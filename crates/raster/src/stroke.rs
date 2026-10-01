//! Stroking: turns a path into the edges of its outline. Ported from MuPDF
//! `draw-path.c` (AGPL).
//!
//! Every segment contributes a quad, every join and cap its own small
//! polygon; the pieces overlap and a nonzero fill of the edge list is their
//! union. Geometry is built in user space (so the pen follows non-uniform
//! transforms) and each edge is mapped to device space as it is emitted.
//! Dashing walks the flattened path in user space, clipped to the device
//! scissor so an off-screen path costs nothing.

use std::f32::consts::{PI, SQRT_2};

use crate::geom::{Point, Transform};
use crate::path::{M32, Path, PathEl, bezier, quadratic};
use crate::raster::{DEVICE_FLATNESS, Edges};

/// Shape at the open ends of a stroked subpath (PDF `J`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    /// Projecting square cap: extends half the line width past the end.
    Square,
}

/// Shape at corners of a stroked subpath (PDF `j`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineJoin {
    /// Miter join; becomes a bevel when `1 / sin(φ / 2)` exceeds the miter
    /// limit, `φ` being the angle between the two segments.
    #[default]
    Miter,
    Round,
    Bevel,
}

/// Dash pattern (PDF `d`): alternating on and off lengths in user space and
/// the distance into the pattern at which each subpath starts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dash {
    pub array: Vec<f64>,
    pub phase: f64,
}

/// Stroke parameters in user space.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    /// Line width. Any stroke thinner than [`HAIRLINE_WIDTH`] device
    /// pixels, including `0` and non-finite widths, is widened to that.
    pub width: f64,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f64,
    /// `None`, an empty array, an all-zero array, or one with a negative or
    /// non-finite entry strokes solid. So does a pattern whose period is
    /// under a hundredth of a user unit or half a device pixel.
    pub dash: Option<Dash>,
}

impl Default for Stroke {
    fn default() -> Stroke {
        Stroke {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
        }
    }
}

/// Thinnest stroke drawn, in device pixels: MuPDF's `2 / (aa_level + 2)`
/// with anti-aliasing level 8 (`fz_draw_stroke_path`).
pub const HAIRLINE_WIDTH: f32 = 0.2;

/// `FLT_TINY * FLT_TINY` is about `FLT_EPSILON`.
const FLT_TINY: f32 = 3.4e-4;

/// Adds the stroke outline of `path` under `transform` to `edges`, which
/// must already be reset to the device clip (`do_flatten_stroke`).
pub(crate) fn stroke_edges(path: &Path, transform: &Transform, stroke: &Stroke, edges: &mut Edges) {
    let ctm = M32::from(transform);
    let mut expansion = ctm.expansion();
    if expansion < f32::EPSILON {
        expansion = 1.0;
    }
    let mut linewidth = if stroke.width.is_finite() {
        stroke.width as f32
    } else {
        0.0
    };
    if linewidth * expansion < HAIRLINE_WIDTH {
        linewidth = HAIRLINE_WIDTH / expansion;
    }
    let flatness = (DEVICE_FLATNESS / expansion).max(0.001);
    let mut s = Stroker {
        edges,
        ctm,
        flatness,
        linejoin: stroke.join,
        linewidth: linewidth * 0.5,
        miterlimit: stroke.miter_limit as f32,
        cap: stroke.cap,
        beg: [[0.0; 2]; 2],
        seg: [[0.0; 2]; 2],
        sn: 0,
        not_just_moves: false,
        from_bezier: false,
        cur: [0.0; 2],
        dirn: [0.0; 2],
    };
    let mut dashing = None;
    if let Some(dash) = stroke
        .dash
        .as_ref()
        .filter(|d| !d.array.is_empty() && d.array.iter().all(|v| v.is_finite() && *v >= 0.0))
    {
        let total = dash.array.iter().fold(0.0f32, |t, &v| t + v as f32);
        if total > 0.0 {
            let Some(inv) = try_invert(&ctm) else {
                return;
            };
            let mut rect = transform_rect(clip_rect(s.edges), &inv);
            rect[0] -= linewidth;
            rect[2] += linewidth;
            rect[1] -= linewidth;
            rect[3] += linewidth;
            if total >= 0.01 && total * ctm.max_expansion() >= 0.5 {
                dashing = Some(Dashing {
                    rect,
                    list: &dash.array,
                    start_phase: dash.phase as f32 % total,
                    total,
                    toggle: false,
                    offset: 0,
                    phase: 0.0,
                    cur: [0.0; 2],
                    beg: [0.0; 2],
                });
            }
        }
    }
    match dashing {
        Some(mut d) => {
            for el in path.elements() {
                match *el {
                    PathEl::MoveTo(p) => {
                        let p = pt(p);
                        d.moveto(&mut s, p);
                        d.beg = p;
                        s.cur = p;
                    }
                    PathEl::LineTo(p) => {
                        let p = pt(p);
                        d.lineto(&mut s, p, false);
                        s.cur = p;
                    }
                    PathEl::QuadTo(c, p) => {
                        let (a, c, p) = (s.cur, pt(c), pt(p));
                        d.quad(&mut s, a, c, p);
                        s.cur = p;
                    }
                    PathEl::CubicTo(c1, c2, p) => {
                        let (a, c1, c2, p) = (s.cur, pt(c1), pt(c2), pt(p));
                        d.bezier(&mut s, a, c1, c2, p);
                        s.cur = p;
                    }
                    PathEl::Close => {
                        let beg = d.beg;
                        d.lineto(&mut s, beg, false);
                        s.cur = beg;
                    }
                }
            }
        }
        None => {
            for el in path.elements() {
                match *el {
                    PathEl::MoveTo(p) => {
                        let p = pt(p);
                        s.flush();
                        s.moveto(p);
                        s.cur = p;
                    }
                    PathEl::LineTo(p) => {
                        let p = pt(p);
                        s.lineto(p, false);
                        s.cur = p;
                    }
                    PathEl::QuadTo(c, p) => {
                        let (a, c, p) = (s.cur, pt(c), pt(p));
                        s.quad(a, c, p);
                        s.cur = p;
                    }
                    PathEl::CubicTo(c1, c2, p) => {
                        let (a, c1, c2, p) = (s.cur, pt(c1), pt(c2), pt(p));
                        s.bezier(a, c1, c2, p);
                        s.cur = p;
                    }
                    PathEl::Close => s.closepath(),
                }
            }
        }
    }
    s.flush();
}

fn pt(p: Point) -> [f32; 2] {
    [p.x as f32, p.y as f32]
}

/// `fz_try_invert_matrix`: the inverse in double precision, or `None` when
/// the matrix is singular.
fn try_invert(m: &M32) -> Option<M32> {
    let (a, b, c, d, e, f) = (
        f64::from(m.a),
        f64::from(m.b),
        f64::from(m.c),
        f64::from(m.d),
        f64::from(m.e),
        f64::from(m.f),
    );
    let det = a * d - b * c;
    if (-f64::EPSILON..=f64::EPSILON).contains(&det) {
        return None;
    }
    let det = 1.0 / det;
    let ia = d * det;
    let ib = -b * det;
    let ic = -c * det;
    let id = a * det;
    Some(M32 {
        a: ia as f32,
        b: ib as f32,
        c: ic as f32,
        d: id as f32,
        e: (-e * ia - f * ic) as f32,
        f: (-e * ib - f * id) as f32,
    })
}

/// The device clip of `edges` as a float rectangle (`fz_scissor_rasterizer`).
fn clip_rect(edges: &Edges) -> [f32; 4] {
    let r = edges.clip();
    [r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32]
}

/// Bounding box of the rectangle's corners mapped through `m`
/// (`fz_transform_rect`).
fn transform_rect([x0, y0, x1, y1]: [f32; 4], m: &M32) -> [f32; 4] {
    let corners = [
        m.point([x0, y0]),
        m.point([x0, y1]),
        m.point([x1, y1]),
        m.point([x1, y0]),
    ];
    let mut out = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for [x, y] in corners {
        out[0] = out[0].min(x);
        out[1] = out[1].min(y);
        out[2] = out[2].max(x);
        out[3] = out[3].max(y);
    }
    out
}

/// `find_normal_vectors`: the half-width normal of `(dx, dy)`, or `None`
/// when the segment is too short to have a direction.
fn normal_vectors(dx: f32, dy: f32, linewidth: f32) -> Option<[f32; 2]> {
    if dx == 0.0 {
        if dy < FLT_TINY && dy > -FLT_TINY {
            return None;
        }
        Some([if dy > 0.0 { linewidth } else { -linewidth }, 0.0])
    } else if dy == 0.0 {
        if dx < FLT_TINY && dx > -FLT_TINY {
            return None;
        }
        Some([0.0, if dx > 0.0 { -linewidth } else { linewidth }])
    } else {
        let sq = dx.mul_add(dx, dy * dy);
        if sq < f32::EPSILON {
            return None;
        }
        let scale = linewidth / sq.sqrt();
        Some([dy * scale, -dx * scale])
    }
}

/// `advance`: `a + (b - a) * i / n`, never overrunning `b`.
fn advance(a: f32, b: f32, i: f32, n: f32) -> f32 {
    let d = b - a;
    let target = a + d * i / n;
    if (d < 0.0 && target < b) || (d > 0.0 && target > b) {
        b
    } else {
        target
    }
}

/// Stroker state (`sctx`): the pen, the last two vertices of the current
/// subpath and its first two, so joins and the closing join can be built.
struct Stroker<'a> {
    edges: &'a mut Edges,
    ctm: M32,
    flatness: f32,
    linejoin: LineJoin,
    /// Half the line width.
    linewidth: f32,
    miterlimit: f32,
    cap: LineCap,
    beg: [[f32; 2]; 2],
    seg: [[f32; 2]; 2],
    sn: usize,
    not_just_moves: bool,
    from_bezier: bool,
    cur: [f32; 2],
    /// Direction of the last segment, for the caps of zero-length pieces.
    dirn: [f32; 2],
}

impl Stroker<'_> {
    /// `fz_add_line`: one user-space edge, mapped to device space.
    fn add_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let [tx0, ty0] = self.ctm.point([x0, y0]);
        let [tx1, ty1] = self.ctm.point([x1, y1]);
        self.edges.line(tx0, ty0, tx1, ty1);
    }

    /// `fz_add_horiz_rect`: a horizontal segment's quad; under an
    /// axis-aligned transform it takes the anti-dropout rectangle route.
    fn add_horiz_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let m = self.ctm;
        if m.b == 0.0 && m.c == 0.0 {
            let tx0 = m.a.mul_add(x0, m.e);
            let ty0 = m.d.mul_add(y0, m.f);
            let tx1 = m.a.mul_add(x1, m.e);
            let ty1 = m.d.mul_add(y1, m.f);
            self.edges.rect(tx1, ty1, tx0, ty0);
        } else if m.a == 0.0 && m.d == 0.0 {
            let tx0 = m.c.mul_add(y0, m.e);
            let ty0 = m.b.mul_add(x0, m.f);
            let tx1 = m.c.mul_add(y1, m.e);
            let ty1 = m.b.mul_add(x1, m.f);
            self.edges.rect(tx1, ty0, tx0, ty1);
        } else {
            self.add_line(x0, y0, x1, y0);
            self.add_line(x1, y1, x0, y1);
        }
    }

    /// `fz_add_arc`: the arc of the pen about `c` from offset `p0` to
    /// offset `p1`, walked backwards when `rev`.
    fn add_arc(&mut self, [xc, yc]: [f32; 2], [x0, y0]: [f32; 2], [x1, y1]: [f32; 2], rev: bool) {
        let r = self.linewidth.abs();
        let theta = 2.0 * SQRT_2 * (self.flatness / r).sqrt();
        let mut th0 = y0.atan2(x0);
        let mut th1 = y1.atan2(x1);
        let n = if r > 0.0 {
            if th0 < th1 {
                th0 += PI * 2.0;
            }
            ((th0 - th1) / theta).ceil() as i32
        } else {
            if th1 < th0 {
                th1 += PI * 2.0;
            }
            ((th1 - th0) / theta).ceil() as i32
        };
        let nf = n as f32;
        if rev {
            let (mut ox, mut oy) = (x1, y1);
            for i in (1..n).rev() {
                let theta = th0 + (th1 - th0) * i as f32 / nf;
                let nx = theta.cos() * r;
                let ny = theta.sin() * r;
                self.add_line(xc + nx, yc + ny, xc + ox, yc + oy);
                ox = nx;
                oy = ny;
            }
            self.add_line(xc + x0, yc + y0, xc + ox, yc + oy);
        } else {
            let (mut ox, mut oy) = (x0, y0);
            for i in 1..n {
                let theta = th0 + (th1 - th0) * i as f32 / nf;
                let nx = theta.cos() * r;
                let ny = theta.sin() * r;
                self.add_line(xc + ox, yc + oy, xc + nx, yc + ny);
                ox = nx;
                oy = ny;
            }
            self.add_line(xc + ox, yc + oy, xc + x1, yc + y1);
        }
    }

    /// `fz_add_line_join`: the join at `b` between segments `a→b` and
    /// `b→c`. `join_under` fills the inner side with one edge when both
    /// segments come from the same curve.
    fn add_line_join(
        &mut self,
        [ax, ay]: [f32; 2],
        [bx, by]: [f32; 2],
        [cx, cy]: [f32; 2],
        join_under: bool,
    ) {
        let miterlimit = self.miterlimit;
        let linewidth = self.linewidth;
        let mut linejoin = self.linejoin;
        let mut dx0 = bx - ax;
        let mut dy0 = by - ay;
        let mut dx1 = cx - bx;
        let mut dy1 = cy - by;
        let mut cross = dx1.mul_add(dy0, -(dx0 * dy1));
        let mut rev = false;
        if cross < 0.0 {
            let tmp = dx1;
            dx1 = -dx0;
            dx0 = -tmp;
            let tmp = dy1;
            dy1 = -dy0;
            dy0 = -tmp;
            cross = -cross;
            rev = true;
        }
        let [dlx0, dly0] = normal_vectors(dx0, dy0, linewidth).unwrap_or_else(|| {
            linejoin = LineJoin::Bevel;
            [0.0, 0.0]
        });
        let [dlx1, dly1] = normal_vectors(dx1, dy1, linewidth).unwrap_or_else(|| {
            linejoin = LineJoin::Bevel;
            [0.0, 0.0]
        });
        let mut dmx = (dlx0 + dlx1) * 0.5;
        let mut dmy = (dly0 + dly1) * 0.5;
        let dmr2 = dmx.mul_add(dmx, dmy * dmy);
        if cross * cross < f32::EPSILON && dx0.mul_add(dx1, dy0 * dy1) >= 0.0 {
            linejoin = LineJoin::Bevel;
        }
        if linejoin == LineJoin::Miter && dmr2 * miterlimit * miterlimit < linewidth * linewidth {
            linejoin = LineJoin::Bevel;
        }
        if join_under {
            self.add_line(bx + dlx1, by + dly1, bx + dlx0, by + dly0);
        } else {
            self.edges.polyline(
                &[[bx + dlx1, by + dly1], [bx, by], [bx + dlx0, by + dly0]],
                &self.ctm,
            );
        }
        match linejoin {
            LineJoin::Miter => {
                let scale = linewidth * linewidth / dmr2;
                dmx *= scale;
                dmy *= scale;
                self.edges.polyline(
                    &[
                        [bx - dlx0, by - dly0],
                        [bx - dmx, by - dmy],
                        [bx - dlx1, by - dly1],
                    ],
                    &self.ctm,
                );
            }
            LineJoin::Bevel => {
                self.add_line(bx - dlx0, by - dly0, bx - dlx1, by - dly1);
            }
            LineJoin::Round => {
                self.add_arc([bx, by], [-dlx0, -dly0], [-dlx1, -dly1], rev);
            }
        }
    }

    /// `do_linecap`: the cap at `b` for a segment whose half-width normal
    /// is `(dlx, dly)`.
    fn linecap(&mut self, bx: f32, by: f32, dlx: f32, dly: f32) {
        match self.cap {
            LineCap::Butt => {
                self.add_line(bx - dlx, by - dly, bx + dlx, by + dly);
            }
            LineCap::Round => {
                let n =
                    (PI / (2.0 * SQRT_2 * (self.flatness / self.linewidth).sqrt())).ceil() as i32;
                let mut ox = bx - dlx;
                let mut oy = by - dly;
                for i in 1..n {
                    let theta = PI * i as f32 / n as f32;
                    let cth = theta.cos();
                    let sth = theta.sin();
                    let nx = (-dly).mul_add(sth, (-dlx).mul_add(cth, bx));
                    let ny = dlx.mul_add(sth, (-dly).mul_add(cth, by));
                    self.add_line(ox, oy, nx, ny);
                    ox = nx;
                    oy = ny;
                }
                self.add_line(ox, oy, bx + dlx, by + dly);
            }
            LineCap::Square => {
                self.edges.polyline(
                    &[
                        [bx - dlx, by - dly],
                        [bx - dlx - dly, by - dly + dlx],
                        [bx + dlx - dly, by + dly + dlx],
                        [bx + dlx, by + dly],
                    ],
                    &self.ctm,
                );
            }
        }
    }

    /// `fz_add_line_cap`: the cap at `b` of the segment `a→b`.
    fn add_line_cap(&mut self, [ax, ay]: [f32; 2], [bx, by]: [f32; 2]) {
        let dx = bx - ax;
        let dy = by - ay;
        let scale = self.linewidth / dx.mul_add(dx, dy * dy).sqrt();
        self.linecap(bx, by, dy * scale, -dx * scale);
    }

    /// `fz_add_zero_len_cap`: a cap at `a` facing along (or, when `rev`,
    /// against) the last known direction.
    fn add_zero_len_cap(&mut self, [ax, ay]: [f32; 2], rev: bool) {
        let [dx, dy] = if rev {
            [-self.dirn[0], -self.dirn[1]]
        } else {
            self.dirn
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let scale = self.linewidth / dx.mul_add(dx, dy * dy).sqrt();
        self.linecap(ax, ay, dy * scale, -dx * scale);
    }

    /// `fz_add_line_dot`: the round dot a degenerate subpath gets.
    fn add_line_dot(&mut self, [ax, ay]: [f32; 2]) {
        let linewidth = self.linewidth;
        let n = ((PI / (SQRT_2 * (self.flatness / linewidth).sqrt())).ceil() as i32).max(3);
        let mut ox = ax - linewidth;
        let mut oy = ay;
        for i in 1..n {
            let theta = PI * 2.0 * i as f32 / n as f32;
            let cth = theta.cos();
            let sth = theta.sin();
            let nx = (-cth).mul_add(linewidth, ax);
            let ny = sth.mul_add(linewidth, ay);
            self.add_line(ox, oy, nx, ny);
            ox = nx;
            oy = ny;
        }
        self.add_line(ox, oy, ax - linewidth, ay);
    }

    /// `fz_stroke_flush`: caps the subpath built so far.
    fn flush(&mut self) {
        if self.sn == 1 {
            let [b0, b1] = self.beg;
            let [s0, s1] = self.seg;
            self.add_line_cap(b1, b0);
            self.add_line_cap(s0, s1);
        } else if self.not_just_moves {
            let b0 = self.beg[0];
            if self.cap == LineCap::Round {
                self.add_line_dot(b0);
            } else {
                self.add_zero_len_cap(b0, true);
                self.add_zero_len_cap(b0, false);
            }
        }
    }

    /// `fz_stroke_moveto`.
    fn moveto(&mut self, p: [f32; 2]) {
        self.seg[0] = p;
        self.beg[0] = p;
        self.sn = 0;
        self.not_just_moves = false;
        self.from_bezier = false;
        self.dirn = [0.0; 2];
    }

    /// `fz_stroke_lineto_aux`: extends the subpath to `(x, y)`; `dirn` is
    /// the direction caps of a following zero-length piece align to.
    fn lineto_aux(&mut self, [x, y]: [f32; 2], from_bezier: bool, dirn: [f32; 2]) {
        let [ox, oy] = self.seg[self.sn];
        let dx = x - ox;
        let dy = y - oy;
        self.not_just_moves = true;
        self.dirn = dirn;
        let Some([dlx, dly]) = normal_vectors(dx, dy, self.linewidth) else {
            return;
        };
        if self.sn == 1 {
            let under = self.from_bezier && from_bezier;
            self.add_line_join(self.seg[0], [ox, oy], [x, y], under);
        }
        if dy == 0.0 {
            self.add_horiz_rect(ox, oy - dly, x, y + dly);
        } else {
            self.add_line(ox - dlx, oy - dly, x - dlx, y - dly);
            self.add_line(x + dlx, y + dly, ox + dlx, oy + dly);
        }
        if self.sn == 1 {
            self.seg[0] = self.seg[1];
            self.seg[1] = [x, y];
        } else {
            self.seg[1] = [x, y];
            self.beg[1] = [x, y];
            self.sn = 1;
        }
        self.from_bezier = from_bezier;
    }

    /// `fz_stroke_lineto`.
    fn lineto(&mut self, [x, y]: [f32; 2], from_bezier: bool) {
        let [ox, oy] = self.seg[self.sn];
        self.lineto_aux([x, y], from_bezier, [x - ox, y - oy]);
    }

    /// `fz_stroke_closepath`: closes with a segment back to the start and
    /// the join between it and the first segment.
    fn closepath(&mut self) {
        if self.sn == 1 {
            let [b0, b1] = self.beg;
            self.lineto(b0, false);
            self.add_line_join(self.seg[0], b0, b1, false);
        } else if self.not_just_moves && self.cap == LineCap::Round {
            self.add_line_dot(self.beg[0]);
        }
        self.seg[0] = self.beg[0];
        self.sn = 0;
        self.not_just_moves = false;
        self.from_bezier = false;
        self.dirn = [0.0; 2];
    }

    /// `fz_stroke_bezier`: flattens into `lineto` pieces flagged as curve.
    fn bezier(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2]) {
        let flatness = self.flatness;
        bezier(flatness, a, b, c, d, &mut |_, p| self.lineto(p, true));
    }

    /// `fz_stroke_quad`.
    fn quad(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2]) {
        let flatness = self.flatness;
        quadratic(flatness, a, b, c, &mut |_, p| self.lineto(p, true));
    }
}

/// The dash walker (`fz_dash_*`): tracks the position within the dash
/// pattern along the flattened path and feeds the stroker only the "on"
/// pieces, each as its own capped subpath.
struct Dashing<'a> {
    /// User-space scissor: the device clip mapped back, grown by the line
    /// width. Segments are clipped to it before being dashed.
    rect: [f32; 4],
    list: &'a [f64],
    start_phase: f32,
    total: f32,
    /// Inside an "on" piece.
    toggle: bool,
    offset: usize,
    phase: f32,
    cur: [f32; 2],
    beg: [f32; 2],
}

impl Dashing<'_> {
    fn entry(&self) -> f32 {
        self.list[self.offset] as f32
    }

    fn next_entry(&mut self) {
        self.offset += 1;
        if self.offset == self.list.len() {
            self.offset = 0;
        }
    }

    /// Skips the pattern over `len` units of path, keeping the on/off
    /// state in step (the two "update the position in the dash array"
    /// blocks of `fz_dash_lineto`). Returns the leftover phase.
    fn skip(&mut self, len: f32, strict: bool) -> f32 {
        let mut len = len + self.phase;
        let n = (len / self.total) as i32;
        len = (-(n as f32)).mul_add(self.total, len);
        if (n & self.list.len() as i32 & 1) != 0 {
            self.toggle = !self.toggle;
        }
        while if strict {
            len > self.entry()
        } else {
            len >= self.entry()
        } {
            len -= self.entry();
            self.next_entry();
            self.toggle = !self.toggle;
        }
        len
    }

    /// `fz_dash_moveto`.
    fn moveto(&mut self, s: &mut Stroker<'_>, p: [f32; 2]) {
        self.toggle = true;
        self.offset = 0;
        self.phase = self.start_phase;
        while self.phase > 0.0 && self.phase >= self.entry() {
            self.toggle = !self.toggle;
            self.phase -= self.entry();
            self.next_entry();
        }
        self.cur = p;
        if self.toggle {
            s.flush();
            s.moveto(p);
        }
    }

    /// Either continues the "on" piece to `p` or starts one there. `dirn`
    /// is the direction a following zero-length cap aligns to; `None`
    /// takes it from the pen's last point (`fz_stroke_lineto`).
    fn step(&self, s: &mut Stroker<'_>, p: [f32; 2], from_bezier: bool, dirn: Option<[f32; 2]>) {
        if self.toggle {
            match dirn {
                Some(dirn) => s.lineto_aux(p, from_bezier, dirn),
                None => s.lineto(p, from_bezier),
            }
        } else {
            s.flush();
            s.moveto(p);
        }
    }

    /// `fz_dash_lineto`.
    fn lineto(&mut self, s: &mut Stroker<'_>, [mut bx, mut by]: [f32; 2], from_bezier: bool) {
        let [rx0, ry0, rx1, ry1] = self.rect;
        let [mut ax, mut ay] = self.cur;
        let mut dx = bx - ax;
        let mut dy = by - ay;
        let mut used = 0.0f32;
        let mut tail;
        let mut total = dx.mul_add(dx, dy * dy).sqrt();
        let mut old_b = [0.0f32; 2];

        // Bring `a` onto the screen, first horizontally, then vertically. A
        // segment entirely off screen only advances the pattern.
        let mut off_screen = false;
        let mut d = rx0 - ax;
        let mut moved = false;
        if d > 0.0 {
            if bx < rx0 {
                off_screen = true;
            } else {
                ax = rx0;
                moved = true;
            }
        } else if d < 0.0 {
            d = rx1 - ax;
            if d < 0.0 {
                if bx > rx1 {
                    off_screen = true;
                } else {
                    ax = rx1;
                    moved = true;
                }
            }
        }
        if moved {
            ay = advance(ay, by, d, dx);
            used = total * d / dx;
            total -= used;
            dx = bx - ax;
            dy = by - ay;
        }
        if !off_screen {
            let mut d = ry0 - ay;
            let mut moved = false;
            if d > 0.0 {
                if by < ry0 {
                    off_screen = true;
                } else {
                    ay = ry0;
                    moved = true;
                }
            } else if d < 0.0 {
                d = ry1 - ay;
                if d < 0.0 {
                    if by > ry1 {
                        off_screen = true;
                    } else {
                        ay = ry1;
                        moved = true;
                    }
                }
            }
            if moved {
                ax = advance(ax, bx, d, dy);
                let d = total * d / dy;
                total -= d;
                used += d;
                dx = bx - ax;
                dy = by - ay;
            }
        }
        if off_screen {
            tail = total;
            old_b = [bx, by];
        } else {
            if used != 0.0 {
                self.step(s, [ax, ay], from_bezier, None);
                self.phase = self.skip(used, false);
                self.step(s, [ax, ay], from_bezier, None);
                used = 0.0;
            }

            // Now if `b` is off screen, bring it back.
            tail = 0.0;
            if dx != 0.0 {
                let mut d = bx - rx0;
                let mut moved = false;
                if d < 0.0 {
                    old_b = [bx, by];
                    bx = rx0;
                    moved = true;
                } else if d > 0.0 {
                    d = bx - rx1;
                    if d > 0.0 {
                        old_b = [bx, by];
                        bx = rx1;
                        moved = true;
                    }
                }
                if moved {
                    by = advance(by, ay, d, dx);
                    tail = total * d / dx;
                    total -= tail;
                    dx = bx - ax;
                    dy = by - ay;
                }
            }
            if dy != 0.0 {
                let mut d = by - ry0;
                let mut moved = false;
                if d < 0.0 {
                    old_b = [bx, by];
                    by = ry0;
                    moved = true;
                } else if d > 0.0 {
                    d = by - ry1;
                    if d > 0.0 {
                        old_b = [bx, by];
                        by = ry1;
                        moved = true;
                    }
                }
                if moved {
                    bx = advance(bx, ax, d, dy);
                    let t = total * d / dy;
                    tail += t;
                    total -= t;
                    dx = bx - ax;
                    dy = by - ay;
                }
            }

            while total - used > self.entry() - self.phase {
                used += self.entry() - self.phase;
                let ratio = used / total;
                let mx = ratio.mul_add(dx, ax);
                let my = ratio.mul_add(dy, ay);
                self.step(s, [mx, my], from_bezier, Some([dx, dy]));
                self.toggle = !self.toggle;
                self.phase = 0.0;
                self.next_entry();
            }
            self.phase += total - used;

            if tail == 0.0 {
                self.cur = [bx, by];
                if self.toggle {
                    s.lineto_aux([bx, by], from_bezier, [dx, dy]);
                }
                return;
            }
        }

        // The rest of the segment lies off screen: skip the pattern over it.
        self.cur = old_b;
        self.step(s, old_b, from_bezier, Some([dx, dy]));
        self.phase = self.skip(tail, true);
        self.step(s, old_b, from_bezier, Some([dx, dy]));
    }

    /// `fz_dash_bezier`.
    fn bezier(&mut self, s: &mut Stroker<'_>, a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2]) {
        let flatness = s.flatness;
        bezier(flatness, a, b, c, d, &mut |_, p| self.lineto(s, p, true));
    }

    /// `fz_dash_quad`.
    fn quad(&mut self, s: &mut Stroker<'_>, a: [f32; 2], b: [f32; 2], c: [f32; 2]) {
        let flatness = s.flatness;
        quadratic(flatness, a, b, c, &mut |_, p| self.lineto(s, p, true));
    }
}
