//! Path construction and MuPDF-style curve flattening.

use crate::geom::{Point, Rect, Transform};

/// Deepest midpoint subdivision of one curve (MuPDF `MAX_DEPTH`): at most
/// 256 segments.
const MAX_DEPTH: u32 = 8;

/// One path element in user space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    MoveTo(Point),
    LineTo(Point),
    /// Quadratic Bézier: control point, end point.
    QuadTo(Point, Point),
    /// Cubic Bézier: two control points, end point.
    CubicTo(Point, Point, Point),
    /// Closes the current subpath with a straight line to its start.
    Close,
}

/// An immutable sequence of subpaths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    els: Vec<PathEl>,
}

impl Path {
    /// A closed rectangle through the corners of `r`: `(x0, y0)`, `(x1, y0)`,
    /// `(x1, y1)`, `(x0, y1)`. Built from the corners, so huge coordinates
    /// keep their exact edges (unlike `x, y, w, h`).
    pub fn from_rect(r: Rect) -> Path {
        Path {
            els: vec![
                PathEl::MoveTo(Point::new(r.x0, r.y0)),
                PathEl::LineTo(Point::new(r.x1, r.y0)),
                PathEl::LineTo(Point::new(r.x1, r.y1)),
                PathEl::LineTo(Point::new(r.x0, r.y1)),
                PathEl::Close,
            ],
        }
    }

    pub fn elements(&self) -> &[PathEl] {
        &self.els
    }

    /// True when the path has no drawing element (only moves, or nothing).
    pub fn is_empty(&self) -> bool {
        !self.els.iter().any(|e| !matches!(e, PathEl::MoveTo(_)))
    }

    /// Bounding box of all points including control points, or `None` for a
    /// path without finite points.
    pub fn bounds(&self) -> Option<Rect> {
        let mut pts = Vec::with_capacity(self.els.len());
        for el in &self.els {
            match *el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => pts.push(p),
                PathEl::QuadTo(a, b) => pts.extend([a, b]),
                PathEl::CubicTo(a, b, c) => pts.extend([a, b, c]),
                PathEl::Close => {}
            }
        }
        Rect::from_points(&pts)
    }

    /// The path with every point mapped through `t`.
    pub fn transform(&self, t: &Transform) -> Path {
        let els = self
            .els
            .iter()
            .map(|el| match *el {
                PathEl::MoveTo(p) => PathEl::MoveTo(t.apply(p)),
                PathEl::LineTo(p) => PathEl::LineTo(t.apply(p)),
                PathEl::QuadTo(a, b) => PathEl::QuadTo(t.apply(a), t.apply(b)),
                PathEl::CubicTo(a, b, c) => PathEl::CubicTo(t.apply(a), t.apply(b), t.apply(c)),
                PathEl::Close => PathEl::Close,
            })
            .collect();
        Path { els }
    }

    /// The rectangle this path describes when it is exactly one axis-aligned
    /// rectangle (move plus three or four lines, closed or not).
    pub fn as_rect(&self) -> Option<Rect> {
        let mut pts = [Point::default(); 5];
        let mut n = 0;
        for (i, el) in self.els.iter().enumerate() {
            match *el {
                PathEl::MoveTo(p) if i == 0 => {
                    pts[0] = p;
                    n = 1;
                }
                PathEl::LineTo(p) if n < 5 && i > 0 => {
                    pts[n] = p;
                    n += 1;
                }
                PathEl::Close if i == self.els.len() - 1 => {}
                _ => return None,
            }
        }
        if n == 5 {
            if pts[4] != pts[0] {
                return None;
            }
        } else if n != 4 {
            return None;
        }
        let [p0, p1, p2, p3, _] = pts;
        let horizontal_first = p0.y == p1.y && p1.x == p2.x && p2.y == p3.y && p3.x == p0.x;
        let vertical_first = p0.x == p1.x && p1.y == p2.y && p2.x == p3.x && p3.y == p0.y;
        if !(horizontal_first || vertical_first) {
            return None;
        }
        Rect::from_points(&[p0, p2]).filter(|_| p0.is_finite() && p2.is_finite())
    }
}

/// Builds a [`Path`] with PDF path-construction semantics.
///
/// A drawing operator without a current point starts a subpath at its first
/// point (a line's end point, a curve's first control point). After
/// [`close`](Self::close) the current point is the subpath start, and a
/// following drawing operator opens a new subpath there.
#[derive(Clone, Debug, Default)]
pub struct PathBuilder {
    els: Vec<PathEl>,
    start: Point,
    current: Option<Point>,
    open: bool,
}

impl PathBuilder {
    pub fn new() -> PathBuilder {
        PathBuilder::default()
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> &mut Self {
        let p = Point::new(x, y);
        if let Some(PathEl::MoveTo(last)) = self.els.last_mut() {
            *last = p;
        } else {
            self.els.push(PathEl::MoveTo(p));
        }
        self.start = p;
        self.current = Some(p);
        self.open = true;
        self
    }

    fn ensure_subpath(&mut self, fallback: Point) {
        match self.current {
            None => {
                self.move_to(fallback.x, fallback.y);
            }
            Some(_) if !self.open => {
                let s = self.start;
                self.move_to(s.x, s.y);
            }
            Some(_) => {}
        }
    }

    pub fn line_to(&mut self, x: f64, y: f64) -> &mut Self {
        let p = Point::new(x, y);
        self.ensure_subpath(p);
        self.els.push(PathEl::LineTo(p));
        self.current = Some(p);
        self
    }

    pub fn quad_to(&mut self, x1: f64, y1: f64, x: f64, y: f64) -> &mut Self {
        let c = Point::new(x1, y1);
        let p = Point::new(x, y);
        self.ensure_subpath(c);
        self.els.push(PathEl::QuadTo(c, p));
        self.current = Some(p);
        self
    }

    pub fn cubic_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x: f64, y: f64) -> &mut Self {
        let c1 = Point::new(x1, y1);
        let c2 = Point::new(x2, y2);
        let p = Point::new(x, y);
        self.ensure_subpath(c1);
        self.els.push(PathEl::CubicTo(c1, c2, p));
        self.current = Some(p);
        self
    }

    /// Closes the current subpath; a no-op when no subpath is open.
    pub fn close(&mut self) -> &mut Self {
        if self.open {
            self.els.push(PathEl::Close);
            self.open = false;
            self.current = Some(self.start);
        }
        self
    }

    /// Appends a closed rectangle subpath (PDF `re`).
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> &mut Self {
        self.move_to(x, y)
            .line_to(x + w, y)
            .line_to(x + w, y + h)
            .line_to(x, y + h)
            .close()
    }

    /// The current point, if any.
    pub fn current_point(&self) -> Option<Point> {
        self.current
    }

    pub fn is_empty(&self) -> bool {
        self.els.is_empty()
    }

    /// Finishes the path. A trailing lone move is dropped.
    pub fn finish(mut self) -> Path {
        if let Some(PathEl::MoveTo(_)) = self.els.last() {
            self.els.pop();
        }
        Path { els: self.els }
    }
}

/// A transform in `f32` with the arithmetic MuPDF's compiled code uses:
/// in `x * a + y * c + e` the first product fuses into a multiply-add, the
/// `+ e` does not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct M32 {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl From<&Transform> for M32 {
    fn from(t: &Transform) -> M32 {
        M32 {
            a: t.a as f32,
            b: t.b as f32,
            c: t.c as f32,
            d: t.d as f32,
            e: t.e as f32,
            f: t.f as f32,
        }
    }
}

impl M32 {
    /// `fz_transform_point`.
    pub fn point(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [
            x.mul_add(self.a, y * self.c) + self.e,
            x.mul_add(self.b, y * self.d) + self.f,
        ]
    }

    /// `fz_matrix_expansion`: the geometric mean scale.
    pub fn expansion(&self) -> f32 {
        self.a.mul_add(self.d, -(self.b * self.c)).abs().sqrt()
    }

    /// `fz_matrix_max_expansion`: the largest coefficient magnitude.
    pub fn max_expansion(&self) -> f32 {
        self.a
            .abs()
            .max(self.b.abs())
            .max(self.c.abs())
            .max(self.d.abs())
    }
}

/// Flattens the cubic `a b c d` by midpoint subdivision until the control
/// polygon deviates less than `flatness`, emitting `line(from, to)` for each
/// piece. Ported from MuPDF `draw-path.c` `bezier` (AGPL).
pub(crate) fn bezier(
    flatness: f32,
    a: [f32; 2],
    b: [f32; 2],
    c: [f32; 2],
    d: [f32; 2],
    line: &mut impl FnMut([f32; 2], [f32; 2]),
) {
    bezier_depth(flatness, a, b, c, d, 0, line);
}

fn bezier_depth(
    flatness: f32,
    [xa, ya]: [f32; 2],
    [xb, yb]: [f32; 2],
    [xc, yc]: [f32; 2],
    [xd, yd]: [f32; 2],
    depth: u32,
    line: &mut impl FnMut([f32; 2], [f32; 2]),
) {
    let dmax = (xa - xb)
        .abs()
        .max((ya - yb).abs())
        .max((xd - xc).abs())
        .max((yd - yc).abs());
    if dmax < flatness || depth >= MAX_DEPTH {
        line([xa, ya], [xd, yd]);
        return;
    }
    let mut xab = xa + xb;
    let mut yab = ya + yb;
    let xbc = xb + xc;
    let ybc = yb + yc;
    let mut xcd = xc + xd;
    let mut ycd = yc + yd;
    let mut xabc = xab + xbc;
    let mut yabc = yab + ybc;
    let mut xbcd = xbc + xcd;
    let mut ybcd = ybc + ycd;
    let mut xabcd = xabc + xbcd;
    let mut yabcd = yabc + ybcd;
    xab *= 0.5;
    yab *= 0.5;
    xcd *= 0.5;
    ycd *= 0.5;
    xabc *= 0.25;
    yabc *= 0.25;
    xbcd *= 0.25;
    ybcd *= 0.25;
    xabcd *= 0.125;
    yabcd *= 0.125;
    bezier_depth(
        flatness,
        [xa, ya],
        [xab, yab],
        [xabc, yabc],
        [xabcd, yabcd],
        depth + 1,
        line,
    );
    bezier_depth(
        flatness,
        [xabcd, yabcd],
        [xbcd, ybcd],
        [xcd, ycd],
        [xd, yd],
        depth + 1,
        line,
    );
}

/// Flattens the quadratic `a b c` like [`bezier`]. Ported from MuPDF
/// `draw-path.c` `quad` (AGPL).
pub(crate) fn quadratic(
    flatness: f32,
    a: [f32; 2],
    b: [f32; 2],
    c: [f32; 2],
    line: &mut impl FnMut([f32; 2], [f32; 2]),
) {
    quadratic_depth(flatness, a, b, c, 0, line);
}

fn quadratic_depth(
    flatness: f32,
    [xa, ya]: [f32; 2],
    [xb, yb]: [f32; 2],
    [xc, yc]: [f32; 2],
    depth: u32,
    line: &mut impl FnMut([f32; 2], [f32; 2]),
) {
    let dmax = (xa - xb)
        .abs()
        .max((ya - yb).abs())
        .max((xc - xb).abs())
        .max((yc - yb).abs());
    if dmax < flatness || depth >= MAX_DEPTH {
        line([xa, ya], [xc, yc]);
        return;
    }
    let mut xab = xa + xb;
    let mut yab = ya + yb;
    let mut xbc = xb + xc;
    let mut ybc = yb + yc;
    let mut xabc = xab + xbc;
    let mut yabc = yab + ybc;
    xab *= 0.5;
    yab *= 0.5;
    xbc *= 0.5;
    ybc *= 0.5;
    xabc *= 0.25;
    yabc *= 0.25;
    quadratic_depth(
        flatness,
        [xa, ya],
        [xab, yab],
        [xabc, yabc],
        depth + 1,
        line,
    );
    quadratic_depth(
        flatness,
        [xabc, yabc],
        [xbc, ybc],
        [xc, yc],
        depth + 1,
        line,
    );
}
