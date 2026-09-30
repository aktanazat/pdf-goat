//! Path construction and curve flattening.

use crate::geom::{Point, Rect, Transform};

/// Upper bound on line segments produced for one curve, so a curve with huge
/// control points cannot allocate without bound.
const MAX_CURVE_SEGMENTS: usize = 512;

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

/// Receiver for flattened geometry.
pub(crate) trait FlattenSink {
    fn move_to(&mut self, p: Point);
    /// `smooth` marks a vertex inside a flattened curve.
    fn line_to(&mut self, p: Point, smooth: bool);
    fn close(&mut self);
}

/// Flattens `path` after mapping it through `t`, with at most `tolerance`
/// distance between the curve and its polyline in the destination space.
pub(crate) fn flatten(path: &Path, t: &Transform, tolerance: f64, sink: &mut impl FlattenSink) {
    let tol = if tolerance.is_finite() && tolerance > 1e-6 {
        tolerance
    } else {
        0.25
    };
    let mut current = Point::default();
    let mut start = Point::default();
    let mut has_current = false;
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                current = t.apply(p);
                start = current;
                has_current = true;
                sink.move_to(current);
            }
            PathEl::LineTo(p) => {
                let p = t.apply(p);
                if !has_current {
                    start = p;
                    has_current = true;
                    sink.move_to(p);
                } else {
                    sink.line_to(p, false);
                }
                current = p;
            }
            PathEl::QuadTo(c, p) => {
                let c = t.apply(c);
                let p = t.apply(p);
                if !has_current {
                    start = c;
                    current = c;
                    has_current = true;
                    sink.move_to(c);
                }
                let dd = (current - c * 2.0 + p).length();
                let n = segment_count(0.25 * dd, tol);
                for i in 1..n {
                    let s = i as f64 / n as f64;
                    let a = current.lerp(c, s);
                    let b = c.lerp(p, s);
                    sink.line_to(a.lerp(b, s), true);
                }
                sink.line_to(p, false);
                current = p;
            }
            PathEl::CubicTo(c1, c2, p) => {
                let c1 = t.apply(c1);
                let c2 = t.apply(c2);
                let p = t.apply(p);
                if !has_current {
                    start = c1;
                    current = c1;
                    has_current = true;
                    sink.move_to(c1);
                }
                let dd1 = (current - c1 * 2.0 + c2).length();
                let dd2 = (c1 - c2 * 2.0 + p).length();
                let n = segment_count(0.75 * dd1.max(dd2), tol);
                let p0 = current;
                for i in 1..n {
                    let s = i as f64 / n as f64;
                    let u = 1.0 - s;
                    let q = p0 * (u * u * u)
                        + c1 * (3.0 * u * u * s)
                        + c2 * (3.0 * u * s * s)
                        + p * (s * s * s);
                    sink.line_to(q, true);
                }
                sink.line_to(p, false);
                current = p;
            }
            PathEl::Close => {
                if has_current {
                    sink.close();
                    current = start;
                }
            }
        }
    }
}

/// Wang's formula: segments needed so the chord error stays under `tol`,
/// given `k * M` where `M` is the largest second difference.
fn segment_count(km: f64, tol: f64) -> usize {
    let n = (km / tol).sqrt().ceil();
    if n.is_finite() && n >= 1.0 {
        (n as usize).min(MAX_CURVE_SEGMENTS)
    } else if n.is_finite() {
        1
    } else {
        MAX_CURVE_SEGMENTS
    }
}

/// A flattened subpath: points plus per-vertex smoothness flags.
#[derive(Clone, Debug, Default)]
pub(crate) struct Polyline {
    pub points: Vec<Point>,
    pub smooth: Vec<bool>,
    pub closed: bool,
    /// Set when the subpath contains any drawing element, even if all its
    /// points coincide (a zero-length subpath that still gets caps).
    pub has_segments: bool,
    /// Orientation for square caps when the subpath has zero length.
    pub zero_dir: Point,
}

impl Polyline {
    fn starting_at(p: Point) -> Polyline {
        Polyline {
            points: vec![p],
            smooth: vec![false],
            closed: false,
            has_segments: false,
            zero_dir: Point::new(1.0, 0.0),
        }
    }
}

/// Collects flattened subpaths as polylines.
#[derive(Default)]
pub(crate) struct PolylineCollector {
    pub lines: Vec<Polyline>,
}

impl FlattenSink for PolylineCollector {
    fn move_to(&mut self, p: Point) {
        if let Some(last) = self.lines.last_mut()
            && !last.has_segments
        {
            *last = Polyline::starting_at(p);
            return;
        }
        self.lines.push(Polyline::starting_at(p));
    }

    fn line_to(&mut self, p: Point, smooth: bool) {
        if let Some(last) = self.lines.last_mut() {
            last.points.push(p);
            last.smooth.push(smooth);
            last.has_segments = true;
        }
    }

    fn close(&mut self) {
        if let Some(last) = self.lines.last_mut() {
            last.closed = true;
            last.has_segments = true;
            let start = last.points[0];
            // A following drawing element continues from the start in a new subpath.
            self.lines.push(Polyline::starting_at(start));
        }
    }
}
