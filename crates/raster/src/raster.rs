//! MuPDF's "gel" scan converter.
//!
//! Ported from MuPDF `source/fitz/draw-edge.c` and `draw-rasterize.c` (AGPL).
//! Device coordinates are snapped to a 17 × 15 subsample grid (MuPDF
//! anti-aliasing level 8); every edge becomes an integer Bresenham line on
//! that grid, and an active-edge sweep over sub-scanlines accumulates span
//! deltas per pixel column. The running sum of one pixel row is its
//! coverage: 255 subsamples map onto 255 levels, so a pixel's coverage is
//! exactly the number of subsamples inside the shape.

use crate::geom::{IntRect, Point, Transform};
use crate::mask::Mask;
use crate::path::{M32, Path, PathEl, bezier, quadratic};

/// How the interior of a path is decided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// Horizontal subsamples per pixel (`fz_aa_hscale`).
pub(crate) const HSCALE: i32 = 17;
/// Vertical subsamples per pixel (`fz_aa_vscale`).
pub(crate) const VSCALE: i32 = 15;
/// Coordinates are clamped to this many pixels from the origin (`BBOX_MIN/MAX`).
const BBOX_MIN: i32 = -(1 << 20);
const BBOX_MAX: i32 = 1 << 20;

/// Flattening tolerance in device pixels (`fz_draw_fill_path`: `0.3 / expansion`).
pub(crate) const DEVICE_FLATNESS: f32 = 0.3;

/// Floor division (`fz_idiv`).
pub(crate) fn idiv(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

/// Ceiling division (`fz_idiv_up`).
pub(crate) fn idiv_up(a: i32, b: i32) -> i32 {
    -((-a).div_euclid(b))
}

/// One Bresenham edge on the subsample grid (`fz_edge`).
#[derive(Clone, Copy, Debug, Default)]
struct Edge {
    x: i32,
    e: i32,
    h: i32,
    y: i32,
    adj_up: i32,
    adj_down: i32,
    xmove: i32,
    xdir: i32,
    ydir: i32,
}

/// Edge list of one shape, clipped on insertion (`fz_gel` plus the
/// `fz_rasterizer` clip and bounding box, both in subsample units).
#[derive(Debug)]
pub(crate) struct Edges {
    edges: Vec<Edge>,
    clip: [i32; 4],
    bbox: [i32; 4],
    #[cfg(test)]
    quantized_points: usize,
}

impl Default for Edges {
    fn default() -> Edges {
        let mut edges = Edges {
            edges: Vec::new(),
            clip: [0; 4],
            bbox: [0; 4],
            #[cfg(test)]
            quantized_points: 0,
        };
        edges.reset(IntRect::new(BBOX_MIN, BBOX_MIN, BBOX_MAX, BBOX_MAX));
        edges
    }
}

impl Edges {
    /// Starts a new edge list; every later insertion is clipped to the
    /// device-pixel rectangle `clip` (`fz_reset_rasterizer`).
    pub fn reset(&mut self, clip: IntRect) {
        self.edges.clear();
        #[cfg(test)]
        {
            self.quantized_points = 0;
        }
        let clamp = |v: i32| v.clamp(BBOX_MIN, BBOX_MAX);
        self.clip = [
            clamp(clip.x0) * HSCALE,
            clamp(clip.y0) * VSCALE,
            clamp(clip.x1) * HSCALE,
            clamp(clip.y1) * VSCALE,
        ];
        self.bbox = [BBOX_MAX, BBOX_MAX, BBOX_MIN, BBOX_MIN];
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// True when the list is exactly the two vertical edges of an
    /// axis-aligned rectangle (`fz_is_rect_gel`).
    pub fn is_rect(&self) -> bool {
        match self.edges.as_slice() {
            [a, b] => {
                a.y == b.y
                    && a.h == b.h
                    && a.xmove == 0
                    && a.adj_up == 0
                    && b.xmove == 0
                    && b.adj_up == 0
            }
            _ => false,
        }
    }

    /// Device pixels the edges touch, empty when nothing was inserted
    /// (`fz_bound_rasterizer`).
    pub fn bounds(&self) -> IntRect {
        let [x0, y0, x1, y1] = self.bbox;
        if x1 < x0 || y1 < y0 {
            return IntRect::EMPTY;
        }
        IntRect::new(
            idiv(x0, HSCALE),
            idiv(y0, VSCALE),
            idiv_up(x1, HSCALE),
            idiv_up(y1, VSCALE),
        )
    }

    /// The clip given to [`Edges::reset`], in device pixels.
    pub fn clip(&self) -> IntRect {
        let [x0, y0, x1, y1] = self.clip;
        IntRect::new(
            idiv(x0, HSCALE),
            idiv(y0, VSCALE),
            idiv_up(x1, HSCALE),
            idiv_up(y1, VSCALE),
        )
    }

    /// Inserts a device-space line, clipping it to the clip rectangle.
    /// Parts outside horizontally collapse onto the clip edge so the
    /// winding inside stays right (`fz_insert_gel`).
    #[inline]
    pub fn line(&mut self, fx0: f32, fy0: f32, fx1: f32, fy1: f32) {
        let a = self.point([fx0, fy0]);
        let b = self.point([fx1, fy1]);
        self.segment(a, b);
    }

    /// Quantize a vertex once; connected segments share the result.
    #[inline]
    fn point(&mut self, [x, y]: [f32; 2]) -> Option<[i32; 2]> {
        #[cfg(test)]
        {
            self.quantized_points += 1;
        }
        let x = (x * HSCALE as f32).floor();
        let y = (y * VSCALE as f32).floor();
        if x.is_nan() || y.is_nan() {
            return None;
        }
        Some([
            x.clamp((BBOX_MIN * HSCALE) as f32, (BBOX_MAX * HSCALE) as f32) as i32,
            y.clamp((BBOX_MIN * VSCALE) as f32, (BBOX_MAX * VSCALE) as f32) as i32,
        ])
    }

    #[inline]
    fn segment(&mut self, a: Option<[i32; 2]>, b: Option<[i32; 2]>) {
        let (Some([x0, y0]), Some([x1, y1])) = (a, b) else {
            return;
        };
        if y0 == y1 {
            return;
        }
        let [cx0, cy0, cx1, cy1] = self.clip;
        if x0 >= cx0
            && x0 <= cx1
            && x1 >= cx0
            && x1 <= cx1
            && y0 >= cy0
            && y0 <= cy1
            && y1 >= cy0
            && y1 <= cy1
        {
            self.raw(x0, y0, x1, y1);
        } else {
            self.clipped(x0, y0, x1, y1);
        }
    }

    fn clipped(&mut self, mut x0: i32, mut y0: i32, mut x1: i32, mut y1: i32) {
        let [cx0, cy0, cx1, cy1] = self.clip;

        match clip_lerp(cy0, false, y0, x0, y1, x1) {
            Lerp::Outside => return,
            Lerp::Leave(v) => {
                y1 = cy0;
                x1 = v;
            }
            Lerp::Enter(v) => {
                y0 = cy0;
                x0 = v;
            }
            Lerp::Inside => {}
        }
        match clip_lerp(cy1, true, y0, x0, y1, x1) {
            Lerp::Outside => return,
            Lerp::Leave(v) => {
                y1 = cy1;
                x1 = v;
            }
            Lerp::Enter(v) => {
                y0 = cy1;
                x0 = v;
            }
            Lerp::Inside => {}
        }
        match clip_lerp(cx0, false, x0, y0, x1, y1) {
            Lerp::Outside => {
                x0 = cx0;
                x1 = cx0;
            }
            Lerp::Leave(v) => {
                self.raw(cx0, v, cx0, y1);
                x1 = cx0;
                y1 = v;
            }
            Lerp::Enter(v) => {
                self.raw(cx0, y0, cx0, v);
                x0 = cx0;
                y0 = v;
            }
            Lerp::Inside => {}
        }
        match clip_lerp(cx1, true, x0, y0, x1, y1) {
            Lerp::Outside => {
                x0 = cx1;
                x1 = cx1;
            }
            Lerp::Leave(v) => {
                self.raw(cx1, v, cx1, y1);
                x1 = cx1;
                y1 = v;
            }
            Lerp::Enter(v) => {
                self.raw(cx1, y0, cx1, v);
                x0 = cx1;
                y0 = v;
            }
            Lerp::Inside => {}
        }
        self.raw(x0, y0, x1, y1);
    }

    /// Inserts an axis-aligned rectangle as two vertical edges. A side that
    /// would round to nothing is widened to one subsample so thin rules
    /// never drop out (`fz_insert_gel_rect`).
    pub fn rect(&mut self, fx0: f32, fy0: f32, fx1: f32, fy1: f32) {
        let mut fx0 = (fx0 * HSCALE as f32).floor();
        let mut fx1 = (fx1 * HSCALE as f32).floor();
        if fx1 == fx0 {
            fx1 += 1.0;
        }
        let mut fy0 = (fy0 * VSCALE as f32).floor();
        let mut fy1 = (fy1 * VSCALE as f32).floor();
        if fy1 == fy0 {
            fy1 += 1.0;
        }
        if fx0.is_nan() || fx1.is_nan() || fy0.is_nan() || fy1.is_nan() {
            return;
        }
        let [cx0, cy0, cx1, cy1] = self.clip;
        fx0 = fx0.clamp(cx0 as f32, cx1 as f32);
        fx1 = fx1.clamp(cx0 as f32, cx1 as f32);
        fy0 = fy0.clamp(cy0 as f32, cy1 as f32);
        fy1 = fy1.clamp(cy0 as f32, cy1 as f32);
        let x0 = fx0 as i32;
        let y0 = fy0 as i32;
        let x1 = fx1 as i32;
        let y1 = fy1 as i32;
        self.raw(x1, y0, x1, y1);
        self.raw(x0, y1, x0, y0);
    }

    /// Inserts an edge already on the subsample grid (`fz_insert_gel_raw`).
    fn raw(&mut self, mut x0: i32, mut y0: i32, mut x1: i32, mut y1: i32) {
        if y0 == y1 {
            return;
        }
        let winding = if y0 > y1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
            -1
        } else {
            1
        };
        self.bbox[0] = self.bbox[0].min(x0).min(x1);
        self.bbox[2] = self.bbox[2].max(x0).max(x1);
        self.bbox[1] = self.bbox[1].min(y0);
        self.bbox[3] = self.bbox[3].max(y1);

        let dy = y1 - y0;
        let dx = x1 - x0;
        let width = dx.abs();
        let xdir = if dx > 0 { 1 } else { -1 };
        let e = if dx >= 0 { 0 } else { -dy + 1 };
        let (xmove, adj_up) = if dy >= width {
            (0, width)
        } else {
            ((width / dy) * xdir, width % dy)
        };
        self.edges.push(Edge {
            x: x0,
            e,
            h: dy,
            y: y0,
            adj_up,
            adj_down: dy,
            xmove,
            xdir,
            ydir: winding,
        });
    }

    /// Adds the fill outline of `path` under `t`: MuPDF's fill flattening
    /// (`fz_flatten_fill_path`), with the flatness `0.3 / expansion` and an
    /// implicit close of every subpath. An axis-aligned rectangle takes the
    /// anti-dropout rectangle route of `re`.
    pub fn add_path(&mut self, path: &Path, t: &Transform) {
        let ctm = M32::from(t);
        let mut expansion = ctm.expansion();
        if expansion < f32::EPSILON {
            expansion = 1.0;
        }
        let flatness = (DEVICE_FLATNESS / expansion).max(0.001);
        if let Some(r) = path.as_rect() {
            // `flatten_rectto`: each corner product fuses with its offset.
            let (x0, y0, x1, y1) = (r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32);
            if ctm.b == 0.0 && ctm.c == 0.0 {
                self.rect(
                    ctm.a.mul_add(x0, ctm.e),
                    ctm.d.mul_add(y0, ctm.f),
                    ctm.a.mul_add(x1, ctm.e),
                    ctm.d.mul_add(y1, ctm.f),
                );
                return;
            }
            if ctm.a == 0.0 && ctm.d == 0.0 {
                self.rect(
                    ctm.c.mul_add(y0, ctm.e),
                    ctm.b.mul_add(x0, ctm.f),
                    ctm.c.mul_add(y1, ctm.e),
                    ctm.b.mul_add(x1, ctm.f),
                );
                return;
            }
        }
        let mut flat = FillFlattener {
            edges: self,
            ctm,
            flatness,
            b: [0.0; 2],
            c: [0.0; 2],
            device_start: None,
            device_current: None,
        };
        for el in path.elements() {
            match *el {
                PathEl::MoveTo(p) => flat.move_to(pt(p)),
                PathEl::LineTo(p) => flat.line_to(pt(p)),
                PathEl::QuadTo(c, p) => flat.quad_to(pt(c), pt(p)),
                PathEl::CubicTo(c1, c2, p) => flat.cubic_to(pt(c1), pt(c2), pt(p)),
                PathEl::Close => flat.close(),
            }
        }
        flat.close();
    }

    /// Adds a closed polygon mapped through `t`.
    pub fn add_polygon(&mut self, pts: &[Point], t: &Transform) {
        let ctm = M32::from(t);
        let Some(&first) = pts.first() else {
            return;
        };
        let first = self.point(ctm.point(pt(first)));
        let mut prev = first;
        for &p in &pts[1..] {
            let next = self.point(ctm.point(pt(p)));
            self.segment(prev, next);
            prev = next;
        }
        self.segment(prev, first);
    }

    /// Stroker joins and caps share transformed, quantized endpoints.
    pub(crate) fn polyline(&mut self, pts: &[[f32; 2]], ctm: &M32) {
        let Some(&first) = pts.first() else {
            return;
        };
        let first = self.point(ctm.point(first));
        let mut prev = first;
        for &p in &pts[1..] {
            let next = self.point(ctm.point(p));
            self.segment(prev, next);
            prev = next;
        }
    }
}

fn pt(p: Point) -> [f32; 2] {
    [p.x as f32, p.y as f32]
}

enum Lerp {
    Inside,
    Outside,
    Leave(i32),
    Enter(i32),
}

/// Clips the segment against `val` on the primary axis (`clip_lerp_x`);
/// `m` selects the far side. Returns where the segment crosses.
fn clip_lerp(val: i32, m: bool, x0: i32, y0: i32, x1: i32, y1: i32) -> Lerp {
    let v0out = if m { x0 > val } else { x0 < val };
    let v1out = if m { x1 > val } else { x1 < val };
    match (v0out, v1out) {
        (false, false) => Lerp::Inside,
        (true, true) => Lerp::Outside,
        (false, true) => {
            Lerp::Leave(y0 + (((y1 - y0) as f32 * (val - x0) as f32) / (x1 - x0) as f32) as i32)
        }
        (true, false) => {
            Lerp::Enter(y1 + (((y0 - y1) as f32 * (val - x1) as f32) / (x0 - x1) as f32) as i32)
        }
    }
}

/// Fill flattening state (`flatten_proc` in MuPDF `draw-path.c`).
struct FillFlattener<'a> {
    edges: &'a mut Edges,
    ctm: M32,
    flatness: f32,
    /// Start of the current subpath.
    b: [f32; 2],
    /// Current point.
    c: [f32; 2],
    device_start: Option<[i32; 2]>,
    device_current: Option<[i32; 2]>,
}

impl FillFlattener<'_> {
    fn move_to(&mut self, p: [f32; 2]) {
        self.close();
        self.b = p;
        self.c = p;
        self.device_start = self.edges.point(self.ctm.point(p));
        self.device_current = self.device_start;
    }

    fn line_to(&mut self, p: [f32; 2]) {
        let next = self.edges.point(self.ctm.point(p));
        self.edges.segment(self.device_current, next);
        self.device_current = next;
        self.c = p;
    }

    fn quad_to(&mut self, c: [f32; 2], p: [f32; 2]) {
        let from = self.c;
        quadratic(self.flatness, from, c, p, &mut |_, b| self.line_to(b));
    }

    fn cubic_to(&mut self, c1: [f32; 2], c2: [f32; 2], p: [f32; 2]) {
        let from = self.c;
        bezier(self.flatness, from, c1, c2, p, &mut |_, b| self.line_to(b));
    }

    fn close(&mut self) {
        if self.c != self.b {
            self.edges.segment(self.device_current, self.device_start);
        }
        self.c = self.b;
        self.device_current = self.device_start;
    }
}

/// One edge crossing of a sub-scanline, packed as `x << 1 | up`: `x` on
/// the subsample grid as an offset from the edge bounds' left pixel,
/// `up` set when the edge winds +1. Ordering the packed values orders
/// by `x`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Crossing(u32);

impl Crossing {
    fn x(self) -> u32 {
        self.0 >> 1
    }

    fn up(self) -> bool {
        self.0 & 1 != 0
    }

    fn dir(self) -> i32 {
        (self.0 & 1) as i32 * 2 - 1
    }
}

/// Most pixel rows scan-converted per band.
const BAND_ROWS: i32 = 64;
/// Crossings a band may hold before it is cut shorter: bounds the scratch
/// memory of a shape with many tall edges.
const BAND_CROSSINGS: usize = 1 << 18;

/// Reusable scan-conversion scratch (`fz_gel` active list, deltas).
///
/// MuPDF steps every active edge one sub-scanline at a time and shell
/// sorts the list each step. This walks each edge down a band of rows in
/// one go, bucketing its crossings by sub-scanline, then sorts each
/// bucket by `x` and walks it for spans. The coverage is the same: a
/// sub-scanline's spans depend only on its crossings in `x` order, and
/// crossings at equal `x` add zero-width spans whichever way they fall.
#[derive(Default)]
pub(crate) struct Rasterizer {
    active: Vec<Edge>,
    deltas: Vec<i32>,
    scratch: Vec<Edge>,
    counts: Vec<u32>,
    /// Per sub-scanline of the band, where its bucket ends in `cross`.
    ends: Vec<u32>,
    cross: Vec<Crossing>,
    /// Per row of the last mask, the pixel range `lo..hi` (relative to
    /// the mask's left edge) its spans reached; `[0, 0]` for an empty row.
    pub(crate) extents: Vec<[u32; 2]>,
    #[cfg(test)]
    edge_steps: usize,
}

impl Rasterizer {
    /// Scan-converts `edges` into `out`, which afterwards covers the edge
    /// bounds intersected with the edge clip. Returns false when nothing can
    /// be covered (`fz_convert_rasterizer` + `fz_scan_convert_aa`). The
    /// edge list is sorted by start row in place.
    pub fn rasterize(&mut self, edges: &mut Edges, rule: FillRule, out: &mut Mask) -> bool {
        let region = edges.bounds().intersect(&edges.clip());
        if region.is_empty() || edges.is_empty() {
            return false;
        }
        out.reset(region);
        let width = region.width() as usize;
        let mut rows = out.data_mut().chunks_exact_mut(width);

        let [bx0, _, bx1, _] = edges.bbox;
        let xmin = idiv(bx0, HSCALE);
        let xmax = idiv_up(bx1, HSCALE);
        let xofs = xmin * HSCALE;
        let skipx = (region.x0 - xmin) as usize;
        let bcap = (xmax - xmin + 2) as usize;
        self.deltas.clear();
        self.deltas.resize(bcap, 0);
        self.active.clear();
        self.extents.clear();
        // One band when the crossing budget covers the whole shape: then
        // every edge is active in it and the start-row sort is pointless.
        let n = edges.edges.len();
        let height = region.height() as i32;
        let whole =
            height <= BAND_ROWS && BAND_CROSSINGS / (VSCALE as usize * n) >= height as usize;
        if !whole {
            self.sort(edges);
        }
        let sorted = edges.edges.as_slice();
        let mut e = 0usize;
        let mut band_y = region.y0;
        while band_y < region.y1 {
            // Rows in this band: as many as the crossing budget allows for
            // the edges that can be active in it.
            let band_rows = if whole {
                height
            } else {
                let most = BAND_ROWS.min(region.y1 - band_y);
                let starting =
                    sorted[e..].partition_point(|edge| edge.y < (band_y + most) * VSCALE);
                let budget =
                    BAND_CROSSINGS / (VSCALE as usize * (self.active.len() + starting).max(1));
                (budget as i32).clamp(1, most)
            };
            let y0 = band_y * VSCALE;
            let y1 = (band_y + band_rows) * VSCALE;
            while e < sorted.len() && sorted[e].y < y1 {
                self.active.push(sorted[e]);
                e += 1;
            }
            self.bucket(y0, y1, xofs);

            let mut start = 0usize;
            for r in 0..band_rows {
                let Some(row) = rows.next() else {
                    break;
                };
                let buckets = &self.ends[(r * VSCALE) as usize..((r + 1) * VSCALE) as usize];
                let touched = match rule {
                    FillRule::NonZero => {
                        sweep_row::<false>(buckets, &mut self.cross, &mut start, &mut self.deltas)
                    }
                    FillRule::EvenOdd => {
                        sweep_row::<true>(buckets, &mut self.cross, &mut start, &mut self.deltas)
                    }
                };
                let extent = if touched.lo < touched.hi {
                    undelta(row, &mut self.deltas, skipx, touched)
                } else {
                    [0, 0]
                };
                self.extents.push(extent);
            }
            band_y += band_rows;
        }
        true
    }

    /// Walks every active edge down the sub-scanlines `y0..y1`, bucketing
    /// its crossings by sub-scanline, and drops the edges that end there.
    /// Afterwards `ends[s]` is where sub-scanline `s`'s bucket ends.
    fn bucket(&mut self, y0: i32, y1: i32, xofs: i32) {
        let nsub = (y1 - y0) as usize;
        self.ends.clear();
        self.ends.resize(nsub + 1, 0);
        // A difference array: wrapping, since an edge ending where another
        // starts dips below zero before the prefix sum.
        for edge in &self.active {
            let s0 = (edge.y - y0) as usize;
            let s1 = ((edge.y + edge.h).min(y1) - y0) as usize;
            self.ends[s0] = self.ends[s0].wrapping_add(1);
            self.ends[s1] = self.ends[s1].wrapping_sub(1);
        }
        let mut count = 0u32;
        let mut total = 0u32;
        for end in &mut self.ends {
            count = count.wrapping_add(*end);
            *end = total;
            total += count;
        }
        // Every slot below `total` is written below, so the buffer only
        // ever grows; stale entries past it are never read.
        if self.cross.len() < total as usize {
            self.cross.resize(total as usize, Crossing::default());
        }
        for edge in &mut self.active {
            let s0 = (edge.y - y0) as usize;
            let s1 = ((edge.y + edge.h).min(y1) - y0) as usize;
            let mut x = edge.x;
            let mut err = edge.e;
            let up = u32::from(edge.ydir > 0);
            if edge.xmove == 0 && edge.adj_up == 0 {
                let crossing = Crossing((((x - xofs) as u32) << 1) | up);
                for slot in &mut self.ends[s0..s1] {
                    self.cross[*slot as usize] = crossing;
                    *slot += 1;
                }
            } else {
                #[cfg(test)]
                {
                    self.edge_steps += s1 - s0;
                }
                for slot in &mut self.ends[s0..s1] {
                    self.cross[*slot as usize] = Crossing((((x - xofs) as u32) << 1) | up);
                    *slot += 1;
                    x += edge.xmove;
                    err += edge.adj_up;
                    let carry = -i32::from(err > 0);
                    x += edge.xdir & carry;
                    err -= edge.adj_down & carry;
                }
                edge.x = x;
                edge.e = err;
            }
            edge.h -= (s1 - s0) as i32;
            edge.y = y0 + s1 as i32;
        }
        self.active.retain(|edge| edge.h > 0);
    }

    /// Orders `edges` by start row (`sort_gel`), a counting sort over the
    /// rows the shape spans unless that range dwarfs the edge count. Edges
    /// starting on the same row may come in any order: the sweep sorts
    /// them by `x` itself.
    fn sort(&mut self, edges: &mut Edges) {
        let n = edges.edges.len();
        let ymin = edges.bbox[1];
        let range = (edges.bbox[3] - ymin) as usize + 1;
        if range > 16 * n + 1024 {
            edges.edges.sort_unstable_by_key(|edge| edge.y);
            return;
        }
        self.counts.clear();
        self.counts.resize(range + 1, 0);
        for edge in &edges.edges {
            self.counts[(edge.y - ymin) as usize + 1] += 1;
        }
        let mut sum = 0;
        for count in &mut self.counts {
            sum += *count;
            *count = sum;
        }
        self.scratch.clear();
        self.scratch.resize(n, Edge::default());
        for edge in &edges.edges {
            let slot = &mut self.counts[(edge.y - ymin) as usize];
            self.scratch[*slot as usize] = *edge;
            *slot += 1;
        }
        std::mem::swap(&mut edges.edges, &mut self.scratch);
    }
}

/// The `deltas` entries a row's spans wrote: `lo..hi`.
#[derive(Clone, Copy, Debug)]
struct Touched {
    lo: usize,
    hi: usize,
}

impl Default for Touched {
    fn default() -> Touched {
        Touched {
            lo: usize::MAX,
            hi: 0,
        }
    }
}

#[cfg(test)]
thread_local! {
    static INSERTION_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Orders one sub-scanline's crossings by `x` (`sort_active`): an
/// insertion sort, the buckets being a handful of entries.
fn sort_crossings(bucket: &mut [Crossing]) {
    #[cfg(test)]
    INSERTION_SCANS.with(|count| count.set(count.get() + 1));
    for i in 1..bucket.len() {
        let c = bucket[i];
        let mut k = i;
        while k > 0 && bucket[k - 1] > c {
            bucket[k] = bucket[k - 1];
            k -= 1;
        }
        bucket[k] = c;
    }
}

/// Adds the spans of one pixel row's sub-scanlines to `deltas`: the
/// buckets `ends` delimit in `cross` (from `*start`), each sorted by `x`
/// then walked by the fill rule (`non_zero_winding_aa` / `even_odd_aa`).
fn sweep_row<const EVEN_ODD: bool>(
    ends: &[u32],
    cross: &mut [Crossing],
    start: &mut usize,
    deltas: &mut [i32],
) -> Touched {
    let mut touched = Touched::default();
    for &end in ends {
        let end = end as usize;
        let bucket = &mut cross[*start..end];
        *start = end;
        match *bucket {
            [] | [_] => {}
            [a, b] => {
                // Two crossings: one span between them, unless they wind
                // the same way and the interior never returns to zero.
                if EVEN_ODD || a.up() != b.up() {
                    add_span(deltas, a.x().min(b.x()), a.x().max(b.x()), &mut touched);
                }
            }
            [a, b, c, d] => {
                // Two sides of a thin stroke usually produce four crossings.
                // Five compare/exchanges replace insertion searches and the
                // winding loop; each pair becomes branch-free min/max.
                let (a, b) = (a.min(b), a.max(b));
                let (c, d) = (c.min(d), c.max(d));
                let (a, c) = (a.min(c), a.max(c));
                let (b, d) = (b.min(d), b.max(d));
                let (b, c) = (b.min(c), b.max(c));
                if EVEN_ODD || a.up() != b.up() {
                    add_span(deltas, a.x(), b.x(), &mut touched);
                    if EVEN_ODD || c.up() != d.up() {
                        add_span(deltas, c.x(), d.x(), &mut touched);
                    }
                } else if a.dir() + b.dir() + c.dir() + d.dir() == 0 {
                    add_span(deltas, a.x(), d.x(), &mut touched);
                }
            }
            _ => {
                sort_crossings(bucket);
                if EVEN_ODD {
                    // A bucket always holds an even count: every closed
                    // contour crosses a sub-scanline an even number of times.
                    for [open, close] in bucket.as_chunks::<2>().0 {
                        add_span(deltas, open.x(), close.x(), &mut touched);
                    }
                } else {
                    let mut winding = 0;
                    let mut x = 0;
                    for c in bucket.iter() {
                        let next = winding + c.dir();
                        if winding == 0 {
                            x = c.x();
                        } else if next == 0 {
                            add_span(deltas, x, c.x(), &mut touched);
                        }
                        winding = next;
                    }
                }
            }
        }
    }
    touched
}

/// Adds the span `x0..x1` (subsample units from the edge bounds' left
/// pixel, `x0 <= x1`) of one sub-scanline to `deltas` (`add_span_aa`):
/// the pixel it opens in gains the subsamples right of `x0`, the one it
/// closes in loses those right of `x1`, and the prefix sum spreads the
/// full `HSCALE` over the pixels between. The four adds are right as they
/// stand when both ends share a pixel, and cancel when the span is empty,
/// so there is no branch to mispredict.
#[inline]
fn add_span(deltas: &mut [i32], x0: u32, x1: u32, touched: &mut Touched) {
    // Unsigned: cheaper division by the constant.
    let x0pix = (x0 / HSCALE as u32) as usize;
    let x0sub = (x0 % HSCALE as u32) as i32;
    let x1pix = (x1 / HSCALE as u32) as usize;
    let x1sub = (x1 % HSCALE as u32) as i32;
    deltas[x0pix] += HSCALE - x0sub;
    deltas[x0pix + 1] += x0sub;
    deltas[x1pix] += x1sub - HSCALE;
    deltas[x1pix + 1] -= x1sub;
    touched.lo = touched.lo.min(x0pix);
    touched.hi = touched.hi.max(x1pix + 2);
}

/// Running sum of the touched deltas into coverage, clearing them on the
/// way; the scale `0xFF00 / (17 · 15)` is exactly 256, so coverage equals
/// the subsample count (`undelta_aa`). Every span's deltas sum to zero,
/// so the untouched pixels left and right keep the mask's zero. Returns
/// the row's written range.
fn undelta(row: &mut [u8], deltas: &mut [i32], skip: usize, touched: Touched) -> [u32; 2] {
    let Touched { lo, hi } = touched;
    let first = lo.max(skip);
    let end = hi.min(skip + row.len());
    if first >= end {
        deltas[lo..hi].fill(0);
        return [0, 0];
    }
    let mut d = 0i32;
    for delta in &mut deltas[lo..first] {
        d += *delta;
        *delta = 0;
    }
    for (out, delta) in row[first - skip..end - skip]
        .iter_mut()
        .zip(&mut deltas[first..end])
    {
        d += *delta;
        *delta = 0;
        *out = d as u8;
    }
    deltas[end..hi].fill(0);
    [(first - skip) as u32, (end - skip) as u32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_crossings_do_not_search_for_insertion_positions() {
        INSERTION_SCANS.with(|count| count.set(0));
        let mut cross = [Crossing(68), Crossing(1), Crossing(34), Crossing(103)];
        let mut deltas = [0; 5];
        let mut start = 0;
        sweep_row::<false>(&[4], &mut cross, &mut start, &mut deltas);
        assert_eq!(deltas, [17, -17, 17, -17, 0]);
        assert_eq!(INSERTION_SCANS.with(|count| count.get()), 0);
    }

    #[test]
    fn vertical_edges_do_not_recompute_crossings_per_subscanline() {
        let mut edges = Edges::default();
        edges.reset(IntRect::from_size(40, 40));
        edges.rect(2.0, 3.0, 30.0, 35.0);
        let mut raster = Rasterizer::default();
        let mut mask = Mask::default();
        raster.rasterize(&mut edges, FillRule::NonZero, &mut mask);
        assert_eq!(mask.rect(), IntRect::new(2, 3, 30, 35));
        assert!(mask.data().iter().all(|&v| v == 255));
        assert_eq!(
            raster.edge_steps, 0,
            "vertical edges need no Bresenham steps"
        );
    }

    #[test]
    fn polygon_batch_quantizes_each_vertex_once() {
        let mut edges = Edges::default();
        edges.reset(IntRect::from_size(40, 40));
        edges.add_polygon(
            &[
                Point::new(5.0, 5.0),
                Point::new(30.0, 7.0),
                Point::new(28.0, 35.0),
                Point::new(3.0, 32.0),
            ],
            &Transform::IDENTITY,
        );
        let mut mask = Mask::default();
        Rasterizer::default().rasterize(&mut edges, FillRule::NonZero, &mut mask);
        assert_eq!(mask.value(15, 20), 255);
        assert_eq!(mask.value(0, 0), 0);
        assert_eq!(
            edges.quantized_points, 4,
            "shared endpoints are not re-quantized"
        );
    }
}
