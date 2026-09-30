//! Signed-area coverage rasterizer.
//!
//! Every edge adds its signed area and cover to an accumulation buffer; a
//! running sum along each row gives the winding-weighted area of every pixel.
//! Nonzero coverage is `min(|sum|, 1)`, even-odd folds the sum with a
//! triangle wave. Pixels whose winding is constant get exact coverage.

use crate::geom::{IntRect, Point, Rect, Transform};
use crate::mask::Mask;
use crate::path::{FlattenSink, Path, flatten};

/// How the interior of a path is decided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// Edge coordinates are clamped to this magnitude before rasterizing.
const COORD_LIMIT: f64 = 1e300;

/// Device-space flattening tolerance in pixels.
pub(crate) const DEVICE_TOLERANCE: f64 = 0.2;

/// Device-space line segments with their bounding box.
#[derive(Default)]
pub(crate) struct Edges {
    lines: Vec<[f64; 4]>,
    bbox: Option<Rect>,
}

impl Edges {
    pub fn clear(&mut self) {
        self.lines.clear();
        self.bbox = None;
    }

    /// Adds one edge. Coordinates are clamped to `±COORD_LIMIT` so later
    /// differences stay finite; an edge with a NaN coordinate is dropped.
    pub fn push(&mut self, p0: Point, p1: Point) {
        let c = [p0.x, p0.y, p1.x, p1.y];
        if c.iter().any(|v| v.is_nan()) {
            return;
        }
        let [x0, y0, x1, y1] = c.map(|v| v.clamp(-COORD_LIMIT, COORD_LIMIT));
        if y0 == y1 {
            return;
        }
        self.lines.push([x0, y0, x1, y1]);
        let r = Rect::new(x0, y0, x1, y1);
        self.bbox = Some(match self.bbox {
            Some(b) => b.union(&r),
            None => r,
        });
    }

    pub fn bbox(&self) -> Option<Rect> {
        self.bbox
    }

    /// Adds `path` mapped through `t`, closing every subpath.
    pub fn add_path(&mut self, path: &Path, t: &Transform) {
        let mut sink = EdgeSink {
            edges: self,
            start: Point::default(),
            current: Point::default(),
            open: false,
        };
        flatten(path, t, DEVICE_TOLERANCE, &mut sink);
        sink.close();
    }

    /// Adds a closed polygon mapped through `t`, traversed backwards when
    /// `reverse` is set.
    pub fn add_polygon(&mut self, pts: &[Point], t: &Transform, reverse: bool) {
        let Some(&first) = pts.first() else {
            return;
        };
        let first = t.apply(first);
        let mut prev = first;
        for &p in &pts[1..] {
            let p = t.apply(p);
            if reverse {
                self.push(p, prev);
            } else {
                self.push(prev, p);
            }
            prev = p;
        }
        if reverse {
            self.push(first, prev);
        } else {
            self.push(prev, first);
        }
    }
}

struct EdgeSink<'a> {
    edges: &'a mut Edges,
    start: Point,
    current: Point,
    open: bool,
}

impl FlattenSink for EdgeSink<'_> {
    fn move_to(&mut self, p: Point) {
        self.close();
        self.start = p;
        self.current = p;
        self.open = true;
    }

    fn line_to(&mut self, p: Point, _smooth: bool) {
        self.edges.push(self.current, p);
        self.current = p;
    }

    fn close(&mut self) {
        if self.open {
            self.edges.push(self.current, self.start);
            self.current = self.start;
        }
    }
}

/// Reusable rasterizer scratch.
#[derive(Default)]
pub(crate) struct Rasterizer {
    acc: Vec<f32>,
}

impl Rasterizer {
    /// Rasterizes `edges` into `out`, restricted to `limit`. Returns false
    /// when nothing can be covered.
    pub fn rasterize(
        &mut self,
        edges: &Edges,
        limit: IntRect,
        rule: FillRule,
        anti_alias: bool,
        out: &mut Mask,
    ) -> bool {
        let Some(bbox) = edges.bbox() else {
            return false;
        };
        let region = bbox.round_out().intersect(&limit);
        if region.is_empty() {
            return false;
        }
        let w = region.width() as usize;
        let h = region.height() as usize;
        let stride = w + 2;
        self.acc.clear();
        self.acc.resize(stride * h, 0.0);
        let mut acc = Acc {
            buf: &mut self.acc,
            stride,
            w,
            h,
        };
        let ox = f64::from(region.x0);
        let oy = f64::from(region.y0);
        for l in &edges.lines {
            acc.clip_line([l[0] - ox, l[1] - oy, l[2] - ox, l[3] - oy]);
        }
        out.reset(region);
        let data = out.data_mut();
        for (acc_row, out_row) in self.acc.chunks_exact(stride).zip(data.chunks_exact_mut(w)) {
            let mut sum = 0.0f32;
            for (a, o) in acc_row.iter().zip(out_row.iter_mut()) {
                sum += *a;
                let c = match rule {
                    FillRule::NonZero => sum.abs().min(1.0),
                    FillRule::EvenOdd => {
                        let t = sum.abs();
                        let t = t - 2.0 * (t * 0.5).floor();
                        if t > 1.0 { 2.0 - t } else { t }
                    }
                };
                *o = if anti_alias {
                    (c * 255.0 + 0.5) as u8
                } else if c >= 0.5 {
                    255
                } else {
                    0
                };
            }
        }
        true
    }
}

/// Accumulation buffer for a `w × h` region, `stride = w + 2` cells per row.
struct Acc<'a> {
    buf: &'a mut [f32],
    stride: usize,
    w: usize,
    h: usize,
}

impl Acc<'_> {
    /// Clips a local-coordinate line to `[0, w] × [0, h]` and accumulates it.
    /// Parts left of the region become vertical edges at `x = 0` (they still
    /// change the winding inside); parts right of it are dropped.
    fn clip_line(&mut self, l: [f64; 4]) {
        let [mut xa, mut ya, mut xb, mut yb] = l;
        let mut dir = 1.0f32;
        if ya > yb {
            std::mem::swap(&mut xa, &mut xb);
            std::mem::swap(&mut ya, &mut yb);
            dir = -1.0;
        }
        let hf = self.h as f64;
        let wf = self.w as f64;
        if yb <= 0.0 || ya >= hf || ya >= yb {
            return;
        }
        // Interpolate by the ratio along y, which stays in 0..=1 and finite.
        let x_at = |y: f64| xa + (xb - xa) * ((y - ya) / (yb - ya));
        let (y0, x0) = if ya < 0.0 { (0.0, x_at(0.0)) } else { (ya, xa) };
        let (y1, x1) = if yb > hf { (hf, x_at(hf)) } else { (yb, xb) };
        if y1 <= y0 {
            return;
        }
        // Break points where the line crosses x = 0 or x = w, in y order.
        let mut breaks = [(y0, x0), (y1, x1), (y1, x1), (y1, x1)];
        let mut n = 1;
        for bound in [0.0, wf] {
            if (x0 < bound) != (x1 < bound) {
                let y = y0 + (y1 - y0) * ((bound - x0) / (x1 - x0));
                if y > y0 && y < y1 {
                    breaks[n] = (y, bound);
                    n += 1;
                }
            }
        }
        breaks[n] = (y1, x1);
        if n == 3 && breaks[1].0 > breaks[2].0 {
            breaks.swap(1, 2);
        }
        for k in 0..n {
            let (ys, xs) = breaks[k];
            let (ye, xe) = breaks[k + 1];
            if ye <= ys {
                continue;
            }
            let mid = 0.5 * (xs + xe);
            if mid > wf {
                continue;
            }
            let (xs, xe) = if mid < 0.0 {
                (0.0, 0.0)
            } else {
                (xs.clamp(0.0, wf), xe.clamp(0.0, wf))
            };
            self.line([xs as f32, ys as f32, xe as f32, ye as f32], dir);
        }
    }

    /// Accumulates a line with `0 <= x <= w` and `0 <= y0 < y1 <= h`.
    fn line(&mut self, l: [f32; 4], dir: f32) {
        let [x0, y0, x1, y1] = l;
        if y1 <= y0 {
            return;
        }
        let wf = self.w as f32;
        let mut x = x0;
        let row_start = y0.floor().max(0.0) as usize;
        let row_end = (y1.ceil() as usize).min(self.h);
        for y in row_start..row_end {
            let top = (y as f32).max(y0);
            let bottom = ((y + 1) as f32).min(y1);
            let dy = bottom - top;
            if dy <= 0.0 {
                continue;
            }
            let xnext = if bottom >= y1 {
                x1
            } else {
                (x0 + (x1 - x0) * ((bottom - y0) / (y1 - y0))).clamp(0.0, wf)
            };
            let d = dy * dir;
            let row = &mut self.buf[y * self.stride..(y + 1) * self.stride];
            let (xl, xr) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let xl_floor = xl.floor();
            let xl_i = xl_floor as usize;
            let xr_ceil = xr.ceil();
            let xr_i = xr_ceil as usize;
            if xr_i <= xl_i + 1 {
                let xmf = 0.5 * (x + xnext) - xl_floor;
                row[xl_i] += d - d * xmf;
                row[xl_i + 1] += d * xmf;
            } else {
                let s = (xr - xl).recip();
                let x0f = xl - xl_floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = xr - xr_ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                row[xl_i] += d * a0;
                if xr_i == xl_i + 2 {
                    row[xl_i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    row[xl_i + 1] += d * (a1 - a0);
                    for v in &mut row[xl_i + 2..xr_i - 1] {
                        *v += d * s;
                    }
                    let a2 = a1 + (xr_i - xl_i - 3) as f32 * s;
                    row[xr_i - 1] += d * (1.0 - a2 - am);
                }
                row[xr_i] += d * am;
            }
            x = xnext;
        }
    }
}
