//! MuPDF's single-precision geometry. The stext device and PyMuPDF's output
//! functions compute in `float`; doing the same here keeps rounded
//! coordinates identical to the MuPDF reference.

use pdf_core::{Matrix, Point, Rect};

use crate::Quad;

/// `FZ_MIN_INF_RECT`: the smallest coordinate of MuPDF's infinite rectangle.
const MIN_INF: f32 = -2_147_483_648.0;
/// `FZ_MAX_INF_RECT`.
const MAX_INF: f32 = 2_147_483_520.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct P {
    pub x: f32,
    pub y: f32,
}

impl P {
    pub const fn new(x: f32, y: f32) -> P {
        P { x, y }
    }

    pub fn from_point(p: Point) -> P {
        P::new(p.x as f32, p.y as f32)
    }

    pub fn point(self) -> Point {
        Point::new(f64::from(self.x), f64::from(self.y))
    }

    /// `fz_transform_point`.
    pub fn transform(self, m: &M) -> P {
        P::new(
            self.x * m.a + self.y * m.c + m.e,
            self.x * m.b + self.y * m.d + m.f,
        )
    }

    /// `fz_transform_vector`: the linear part only.
    pub fn transform_vector(self, m: &M) -> P {
        P::new(self.x * m.a + self.y * m.c, self.x * m.b + self.y * m.d)
    }

    /// `fz_normalize_vector`.
    pub fn normalize(self) -> P {
        let len = self.x * self.x + self.y * self.y;
        if len != 0.0 {
            let len = len.sqrt();
            P::new(self.x / len, self.y / len)
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct M {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl M {
    pub const fn new(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> M {
        M { a, b, c, d, e, f }
    }

    pub fn from_matrix(m: &Matrix) -> M {
        M::new(
            m.a as f32, m.b as f32, m.c as f32, m.d as f32, m.e as f32, m.f as f32,
        )
    }

    /// `fz_matrix_expansion`.
    pub fn expansion(&self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct R {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl R {
    pub const EMPTY: R = R {
        x0: MAX_INF,
        y0: MAX_INF,
        x1: MIN_INF,
        y1: MIN_INF,
    };
    pub const INFINITE: R = R {
        x0: MIN_INF,
        y0: MIN_INF,
        x1: MAX_INF,
        y1: MAX_INF,
    };

    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> R {
        R { x0, y0, x1, y1 }
    }

    pub fn from_rect(r: &Rect) -> R {
        R::new(r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32)
    }

    pub fn rect(self) -> Rect {
        Rect::new(
            f64::from(self.x0),
            f64::from(self.y0),
            f64::from(self.x1),
            f64::from(self.y1),
        )
    }

    /// `fz_is_empty_rect`.
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    /// `fz_is_valid_rect`.
    pub fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    /// `fz_is_infinite_rect`.
    pub fn is_infinite(&self) -> bool {
        *self == R::INFINITE
    }

    /// `fz_union_rect`.
    pub fn union(self, b: R) -> R {
        if !b.is_valid() {
            return self;
        }
        if !self.is_valid() {
            return b;
        }
        if self.is_infinite() {
            return self;
        }
        if b.is_infinite() {
            return b;
        }
        R::new(
            self.x0.min(b.x0),
            self.y0.min(b.y0),
            self.x1.max(b.x1),
            self.y1.max(b.y1),
        )
    }

    /// `fz_intersect_rect`.
    pub fn intersect(self, b: R) -> R {
        if b.is_infinite() {
            return self;
        }
        if self.is_infinite() {
            return b;
        }
        R::new(
            self.x0.max(b.x0),
            self.y0.max(b.y0),
            self.x1.min(b.x1),
            self.y1.min(b.y1),
        )
    }

    /// `fz_contains_rect`.
    pub fn contains(&self, b: &R) -> bool {
        if !self.is_valid() {
            return false;
        }
        if !b.is_valid() {
            return true;
        }
        self.x0 <= b.x0 && self.y0 <= b.y0 && self.x1 >= b.x1 && self.y1 >= b.y1
    }

    /// PyMuPDF's `JM_rects_overlap`.
    pub fn overlaps(&self, b: &R) -> bool {
        !(self.x0 >= b.x1 || self.y0 >= b.y1 || self.x1 <= b.x0 || self.y1 <= b.y0)
    }

    /// `fz_transform_rect`, including its axis-aligned fast paths.
    pub fn transform(self, m: &M) -> R {
        if self.is_infinite() {
            return self;
        }
        let mut r = self;
        if m.b.abs() < f32::EPSILON && m.c.abs() < f32::EPSILON {
            if m.a < 0.0 {
                std::mem::swap(&mut r.x0, &mut r.x1);
            }
            if m.d < 0.0 {
                std::mem::swap(&mut r.y0, &mut r.y1);
            }
            let s = P::new(r.x0, r.y0).transform(m);
            let t = P::new(r.x1, r.y1).transform(m);
            return R::new(s.x, s.y, t.x, t.y);
        }
        if m.a.abs() < f32::EPSILON && m.d.abs() < f32::EPSILON {
            if m.b < 0.0 {
                std::mem::swap(&mut r.x0, &mut r.x1);
            }
            if m.c < 0.0 {
                std::mem::swap(&mut r.y0, &mut r.y1);
            }
            let s = P::new(r.x0, r.y0).transform(m);
            let t = P::new(r.x1, r.y1).transform(m);
            return R::new(s.x, s.y, t.x, t.y);
        }
        let invalid = r.x0 > r.x1 || r.y0 > r.y1;
        let pts = [
            P::new(r.x0, r.y0).transform(m),
            P::new(r.x0, r.y1).transform(m),
            P::new(r.x1, r.y1).transform(m),
            P::new(r.x1, r.y0).transform(m),
        ];
        let mut out = R::new(
            min4(pts.map(|p| p.x)),
            min4(pts.map(|p| p.y)),
            max4(pts.map(|p| p.x)),
            max4(pts.map(|p| p.y)),
        );
        if invalid {
            std::mem::swap(&mut out.x0, &mut out.x1);
            std::mem::swap(&mut out.y0, &mut out.y1);
        }
        out
    }
}

/// A quad in MuPDF corner order, single precision.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Q {
    pub ul: P,
    pub ur: P,
    pub ll: P,
    pub lr: P,
}

impl Q {
    pub fn from_quad(q: &Quad) -> Q {
        Q {
            ul: P::from_point(q.ul),
            ur: P::from_point(q.ur),
            ll: P::from_point(q.ll),
            lr: P::from_point(q.lr),
        }
    }

    pub fn quad(self) -> Quad {
        Quad {
            ul: self.ul.point(),
            ur: self.ur.point(),
            ll: self.ll.point(),
            lr: self.lr.point(),
        }
    }

    fn is_valid(&self) -> bool {
        [self.ul, self.ur, self.ll, self.lr]
            .iter()
            .all(|p| !p.x.is_nan() && !p.y.is_nan())
    }

    /// `fz_rect_from_quad`.
    pub fn rect(&self) -> R {
        if !self.is_valid() {
            return R::new(0.0, 0.0, -1.0, -1.0);
        }
        let pts = [self.ll, self.lr, self.ul, self.ur];
        R::new(
            min4(pts.map(|p| p.x)),
            min4(pts.map(|p| p.y)),
            max4(pts.map(|p| p.x)),
            max4(pts.map(|p| p.y)),
        )
    }

    /// `fz_transform_quad`.
    pub fn transform(self, m: &M) -> Q {
        if !self.is_valid() {
            return self;
        }
        Q {
            ul: self.ul.transform(m),
            ur: self.ur.transform(m),
            ll: self.ll.transform(m),
            lr: self.lr.transform(m),
        }
    }
}

/// MuPDF's `MIN4`: nested ternaries, so a NaN falls through like C's.
fn min4(v: [f32; 4]) -> f32 {
    let ab = if v[0] < v[1] { v[0] } else { v[1] };
    let cd = if v[2] < v[3] { v[2] } else { v[3] };
    if ab < cd { ab } else { cd }
}

fn max4(v: [f32; 4]) -> f32 {
    let ab = if v[0] > v[1] { v[0] } else { v[1] };
    let cd = if v[2] > v[3] { v[2] } else { v[3] };
    if ab > cd { ab } else { cd }
}
