//! Paths as the content stream builds them (user space), and stroke
//! parameters.

use pdf_core::{Matrix, Point, Rect};

/// One path element. Coordinates are in the space the path was built in:
/// user space for content paths, device space for text-clip paths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    MoveTo(Point),
    LineTo(Point),
    /// Control 1, control 2, end.
    CurveTo(Point, Point, Point),
    Close,
}

/// A path: subpaths started by `MoveTo`, as `m l c v y h re` built them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    els: Vec<PathEl>,
    current: Option<Point>,
    start: Option<Point>,
}

impl Path {
    pub fn new() -> Path {
        Path::default()
    }

    pub fn elements(&self) -> &[PathEl] {
        &self.els
    }

    /// True when the path has no element.
    pub fn is_empty(&self) -> bool {
        self.els.is_empty()
    }

    pub fn current_point(&self) -> Option<Point> {
        self.current
    }

    /// `m`: a second move in a row replaces the first.
    pub fn move_to(&mut self, p: Point) {
        if let Some(PathEl::MoveTo(last)) = self.els.last_mut() {
            *last = p;
        } else {
            self.els.push(PathEl::MoveTo(p));
        }
        self.current = Some(p);
        self.start = Some(p);
    }

    /// `l`: without a current point the line starts a subpath at `p`.
    pub fn line_to(&mut self, p: Point) {
        if self.current.is_none() {
            self.move_to(p);
            return;
        }
        self.reopen();
        self.els.push(PathEl::LineTo(p));
        self.current = Some(p);
    }

    /// `c`.
    pub fn curve_to(&mut self, c1: Point, c2: Point, p: Point) {
        if self.current.is_none() {
            self.move_to(c1);
        }
        self.reopen();
        self.els.push(PathEl::CurveTo(c1, c2, p));
        self.current = Some(p);
    }

    /// `h`: closes the open subpath; the current point returns to its start.
    pub fn close(&mut self) {
        if self.current.is_none() {
            return;
        }
        match self.els.last() {
            None | Some(PathEl::Close) => {}
            Some(_) => self.els.push(PathEl::Close),
        }
        self.current = self.start;
    }

    /// `re`: a closed rectangle subpath; the current point is its origin.
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.move_to(Point::new(x, y));
        self.els.push(PathEl::LineTo(Point::new(x + w, y)));
        self.els.push(PathEl::LineTo(Point::new(x + w, y + h)));
        self.els.push(PathEl::LineTo(Point::new(x, y + h)));
        self.els.push(PathEl::Close);
        self.current = Some(Point::new(x, y));
    }

    /// Appends every element of `other` mapped through `m`.
    pub fn append_transformed(&mut self, other: &Path, m: &Matrix) {
        for el in &other.els {
            match *el {
                PathEl::MoveTo(p) => self.move_to(p.transform(m)),
                PathEl::LineTo(p) => self.line_to(p.transform(m)),
                PathEl::CurveTo(a, b, p) => {
                    self.curve_to(a.transform(m), b.transform(m), p.transform(m))
                }
                PathEl::Close => self.close(),
            }
        }
    }

    /// Bounds of every point, control points included, mapped through `m`;
    /// `None` for an empty path.
    pub fn bounds(&self, m: &Matrix) -> Option<Rect> {
        let mut out: Option<Rect> = None;
        let mut add = |p: Point| {
            let q = p.transform(m);
            let r = Rect::new(q.x, q.y, q.x, q.y);
            out = Some(out.map_or(r, |o| o.union(&r)));
        };
        for el in &self.els {
            match *el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => add(p),
                PathEl::CurveTo(a, b, p) => {
                    add(a);
                    add(b);
                    add(p);
                }
                PathEl::Close => {}
            }
        }
        out
    }

    /// Drawing after a `Close` starts a new subpath at the closed one's start.
    fn reopen(&mut self) {
        if matches!(self.els.last(), Some(PathEl::Close))
            && let Some(p) = self.current
        {
            self.els.push(PathEl::MoveTo(p));
            self.start = Some(p);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// `J`: 0 butt, 1 round, 2 square.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

/// `j`: 0 miter, 1 round, 2 bevel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// Stroke parameters in user space (`w J j M d`).
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    pub width: f64,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f64,
    /// Dash lengths; empty for a solid line.
    pub dash: Vec<f64>,
    pub dash_phase: f64,
}

impl Default for StrokeStyle {
    fn default() -> StrokeStyle {
        StrokeStyle {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_phase: 0.0,
        }
    }
}
