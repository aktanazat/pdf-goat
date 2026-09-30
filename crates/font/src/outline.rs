use crate::error::{FontError, Result};

/// One path segment of a glyph outline, in font units (y up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathOp {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    /// Quadratic curve: control point, end point.
    QuadTo(f32, f32, f32, f32),
    /// Cubic curve: two control points, end point.
    CurveTo(f32, f32, f32, f32, f32, f32),
    /// Closes the current contour back to its start point.
    Close,
}

/// Axis-aligned rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
}

impl Rect {
    fn point(x: f32, y: f32) -> Rect {
        Rect {
            x_min: x,
            y_min: y,
            x_max: x,
            y_max: y,
        }
    }

    fn include(&mut self, x: f32, y: f32) {
        self.x_min = self.x_min.min(x);
        self.y_min = self.y_min.min(y);
        self.x_max = self.x_max.max(x);
        self.y_max = self.y_max.max(y);
    }
}

/// A glyph outline: contours made of path segments, every contour closed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outline {
    ops: Vec<PathOp>,
}

impl Outline {
    pub fn ops(&self) -> &[PathOp] {
        &self.ops
    }

    pub fn into_ops(self) -> Vec<PathOp> {
        self.ops
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Number of contours (MoveTo operations).
    pub fn contour_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op, PathOp::MoveTo(..)))
            .count()
    }

    /// Exact bounds of the drawn outline, including curve extrema; `None` for an empty outline.
    pub fn bounds(&self) -> Option<Rect> {
        let mut rect: Option<Rect> = None;
        let mut cur = (0.0f32, 0.0f32);
        let add = |rect: &mut Option<Rect>, x: f32, y: f32| match rect {
            Some(r) => r.include(x, y),
            None => *rect = Some(Rect::point(x, y)),
        };
        for op in &self.ops {
            match *op {
                PathOp::MoveTo(x, y) => cur = (x, y),
                PathOp::LineTo(x, y) => {
                    add(&mut rect, cur.0, cur.1);
                    add(&mut rect, x, y);
                    cur = (x, y);
                }
                PathOp::QuadTo(cx, cy, x, y) => {
                    add(&mut rect, cur.0, cur.1);
                    add(&mut rect, x, y);
                    for t in quad_extrema(cur.0, cx, x)
                        .into_iter()
                        .chain(quad_extrema(cur.1, cy, y))
                        .flatten()
                    {
                        let (px, py) = quad_at(cur, (cx, cy), (x, y), t);
                        add(&mut rect, px, py);
                    }
                    cur = (x, y);
                }
                PathOp::CurveTo(c1x, c1y, c2x, c2y, x, y) => {
                    add(&mut rect, cur.0, cur.1);
                    add(&mut rect, x, y);
                    let ts = cubic_extrema(cur.0, c1x, c2x, x)
                        .into_iter()
                        .chain(cubic_extrema(cur.1, c1y, c2y, y));
                    for t in ts.flatten() {
                        let (px, py) = cubic_at(cur, (c1x, c1y), (c2x, c2y), (x, y), t);
                        add(&mut rect, px, py);
                    }
                    cur = (x, y);
                }
                PathOp::Close => {}
            }
        }
        rect
    }

    /// Applies the affine matrix `[a b c d e f]` (x' = a*x + c*y + e, y' = b*x + d*y + f).
    pub fn transform(&mut self, m: &[f64; 6]) {
        let t = |x: f32, y: f32| -> (f32, f32) {
            let (x, y) = (f64::from(x), f64::from(y));
            (
                (m[0] * x + m[2] * y + m[4]) as f32,
                (m[1] * x + m[3] * y + m[5]) as f32,
            )
        };
        for op in &mut self.ops {
            *op = match *op {
                PathOp::MoveTo(x, y) => {
                    let (x, y) = t(x, y);
                    PathOp::MoveTo(x, y)
                }
                PathOp::LineTo(x, y) => {
                    let (x, y) = t(x, y);
                    PathOp::LineTo(x, y)
                }
                PathOp::QuadTo(cx, cy, x, y) => {
                    let (cx, cy) = t(cx, cy);
                    let (x, y) = t(x, y);
                    PathOp::QuadTo(cx, cy, x, y)
                }
                PathOp::CurveTo(ax, ay, bx, by, x, y) => {
                    let (ax, ay) = t(ax, ay);
                    let (bx, by) = t(bx, by);
                    let (x, y) = t(x, y);
                    PathOp::CurveTo(ax, ay, bx, by, x, y)
                }
                PathOp::Close => PathOp::Close,
            };
        }
    }

    pub(crate) fn from_ops(ops: Vec<PathOp>) -> Outline {
        Outline { ops }
    }

    pub(crate) fn append(&mut self, other: Outline) {
        self.ops.extend(other.ops);
    }
}

fn quad_extrema(p0: f32, p1: f32, p2: f32) -> [Option<f32>; 1] {
    let denom = p0 - 2.0 * p1 + p2;
    if denom.abs() < f32::EPSILON {
        return [None];
    }
    let t = (p0 - p1) / denom;
    [(t > 0.0 && t < 1.0).then_some(t)]
}

fn cubic_extrema(p0: f32, p1: f32, p2: f32, p3: f32) -> [Option<f32>; 2] {
    // Derivative: 3[(p1-p0)(1-t)^2 + 2(p2-p1)t(1-t) + (p3-p2)t^2] = a t^2 + b t + c.
    let (p0, p1, p2, p3) = (f64::from(p0), f64::from(p1), f64::from(p2), f64::from(p3));
    let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
    let b = 2.0 * (p0 - 2.0 * p1 + p2);
    let c = p1 - p0;
    let inside = |t: f64| (t > 0.0 && t < 1.0).then_some(t as f32);
    if a.abs() < 1e-12 {
        if b.abs() < 1e-12 {
            return [None, None];
        }
        return [inside(-c / b), None];
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return [None, None];
    }
    let root = disc.sqrt();
    [
        inside((-b + root) / (2.0 * a)),
        inside((-b - root) / (2.0 * a)),
    ]
}

fn quad_at(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), t: f32) -> (f32, f32) {
    let mt = 1.0 - t;
    let f = |a: f32, b: f32, c: f32| mt * mt * a + 2.0 * mt * t * b + t * t * c;
    (f(p0.0, p1.0, p2.0), f(p0.1, p1.1, p2.1))
}

fn cubic_at(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let mt = 1.0 - t;
    let f = |a: f32, b: f32, c: f32, d: f32| {
        mt * mt * mt * a + 3.0 * mt * mt * t * b + 3.0 * mt * t * t * c + t * t * t * d
    };
    (f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1))
}

/// Largest number of path segments one glyph may produce.
const MAX_OPS: usize = 1 << 20;

/// Collects path segments from a charstring interpreter or a contour walker.
pub(crate) struct OutlineBuilder {
    ops: Vec<PathOp>,
    /// Index in `ops` of the current contour's MoveTo, if a contour is open.
    open: Option<usize>,
    cur: (f32, f32),
}

impl OutlineBuilder {
    pub(crate) fn new() -> Self {
        OutlineBuilder {
            ops: Vec::new(),
            open: None,
            cur: (0.0, 0.0),
        }
    }

    fn push(&mut self, op: PathOp) -> Result<()> {
        if self.ops.len() >= MAX_OPS {
            return Err(FontError::LimitExceeded("glyph outline segments"));
        }
        self.ops.push(op);
        Ok(())
    }

    fn ensure_open(&mut self) -> Result<()> {
        if self.open.is_none() {
            self.open = Some(self.ops.len());
            self.push(PathOp::MoveTo(self.cur.0, self.cur.1))?;
        }
        Ok(())
    }

    pub(crate) fn move_to(&mut self, x: f32, y: f32) -> Result<()> {
        match self.open {
            // A contour with no segments yet: move its start instead of leaving a stray point.
            Some(idx) if idx + 1 == self.ops.len() => self.ops[idx] = PathOp::MoveTo(x, y),
            Some(_) => {
                self.push(PathOp::Close)?;
                self.open = Some(self.ops.len());
                self.push(PathOp::MoveTo(x, y))?;
            }
            None => {
                self.open = Some(self.ops.len());
                self.push(PathOp::MoveTo(x, y))?;
            }
        }
        self.cur = (x, y);
        Ok(())
    }

    pub(crate) fn line_to(&mut self, x: f32, y: f32) -> Result<()> {
        self.ensure_open()?;
        self.push(PathOp::LineTo(x, y))?;
        self.cur = (x, y);
        Ok(())
    }

    pub(crate) fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> Result<()> {
        self.ensure_open()?;
        self.push(PathOp::QuadTo(cx, cy, x, y))?;
        self.cur = (x, y);
        Ok(())
    }

    pub(crate) fn curve_to(
        &mut self,
        c1: (f32, f32),
        c2: (f32, f32),
        x: f32,
        y: f32,
    ) -> Result<()> {
        self.ensure_open()?;
        self.push(PathOp::CurveTo(c1.0, c1.1, c2.0, c2.1, x, y))?;
        self.cur = (x, y);
        Ok(())
    }

    /// Closes the open contour. The current point stays at the last drawn point, as in
    /// Type 1 and Type 2 charstrings.
    pub(crate) fn close(&mut self) -> Result<()> {
        if let Some(idx) = self.open.take() {
            if idx + 1 == self.ops.len() {
                // Bare MoveTo with nothing drawn.
                self.ops.pop();
                return Ok(());
            }
            self.push(PathOp::Close)?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<Outline> {
        self.close()?;
        Ok(Outline::from_ops(self.ops))
    }
}
