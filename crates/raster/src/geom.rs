//! Points, affine transforms, and rectangles.

use std::ops::{Add, Mul, Neg, Sub};

/// A point or vector in user or device space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    pub fn length(self) -> f64 {
        self.x.hypot(self.y)
    }

    pub fn dot(self, other: Point) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// Z component of the 3D cross product.
    pub fn cross(self, other: Point) -> f64 {
        self.x * other.y - self.y * other.x
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    pub fn lerp(self, other: Point, t: f64) -> Point {
        Point::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}

impl Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

impl Mul<f64> for Point {
    type Output = Point;
    fn mul(self, s: f64) -> Point {
        Point::new(self.x * s, self.y * s)
    }
}

/// Affine transform in PDF matrix order `[a b c d e f]`:
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Transform {
    fn default() -> Transform {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform::new(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);

    pub const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Transform {
        Transform { a, b, c, d, e, f }
    }

    pub const fn translate(tx: f64, ty: f64) -> Transform {
        Transform::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    pub const fn scale(sx: f64, sy: f64) -> Transform {
        Transform::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// Counter-clockwise rotation in a y-up space (clockwise on a y-down device).
    pub fn rotate(radians: f64) -> Transform {
        let (s, c) = radians.sin_cos();
        Transform::new(c, s, -s, c, 0.0, 0.0)
    }

    /// The transform that applies `self` first and `next` second
    /// (the PDF matrix product `self × next`).
    pub fn then(&self, next: &Transform) -> Transform {
        Transform {
            a: self.a * next.a + self.b * next.c,
            b: self.a * next.b + self.b * next.d,
            c: self.c * next.a + self.d * next.c,
            d: self.c * next.b + self.d * next.d,
            e: self.e * next.a + self.f * next.c + next.e,
            f: self.e * next.b + self.f * next.d + next.f,
        }
    }

    /// PDF `cm` semantics: `m` is applied before `self` (`m × self`).
    pub fn pre_concat(&self, m: &Transform) -> Transform {
        m.then(self)
    }

    pub fn determinant(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    /// Inverse transform, or `None` when the matrix is singular or not finite.
    pub fn invert(&self) -> Option<Transform> {
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = 1.0 / det;
        let t = Transform {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        };
        t.is_finite().then_some(t)
    }

    pub fn is_finite(&self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f]
            .iter()
            .all(|v| v.is_finite())
    }

    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            self.a * p.x + self.c * p.y + self.e,
            self.b * p.x + self.d * p.y + self.f,
        )
    }

    /// Applies the linear part only (no translation).
    pub fn apply_vector(&self, v: Point) -> Point {
        Point::new(self.a * v.x + self.c * v.y, self.b * v.x + self.d * v.y)
    }

    /// Largest singular value of the linear part: the most a unit length can grow.
    pub fn max_scale(&self) -> f64 {
        let (s_max, _) = self.singular_values();
        s_max
    }

    /// Smallest singular value of the linear part.
    pub fn min_scale(&self) -> f64 {
        let (_, s_min) = self.singular_values();
        s_min
    }

    fn singular_values(&self) -> (f64, f64) {
        let p = self.a * self.a + self.b * self.b;
        let q = self.a * self.c + self.b * self.d;
        let r = self.c * self.c + self.d * self.d;
        let mean = 0.5 * (p + r);
        let diff = (0.25 * (p - r) * (p - r) + q * q).sqrt();
        let hi = (mean + diff).max(0.0).sqrt();
        let lo = (mean - diff).max(0.0).sqrt();
        (hi, lo)
    }

    /// True when axis-aligned rectangles stay axis-aligned (no rotation or skew
    /// other than multiples of 90 degrees).
    pub fn is_axis_aligned(&self) -> bool {
        (self.b == 0.0 && self.c == 0.0) || (self.a == 0.0 && self.d == 0.0)
    }

    /// Bounding box of `rect` after the transform.
    pub fn map_rect(&self, rect: Rect) -> Rect {
        let pts = [
            self.apply(Point::new(rect.x0, rect.y0)),
            self.apply(Point::new(rect.x1, rect.y0)),
            self.apply(Point::new(rect.x1, rect.y1)),
            self.apply(Point::new(rect.x0, rect.y1)),
        ];
        Rect::from_points(&pts).unwrap_or(Rect::EMPTY)
    }
}

/// Axis-aligned rectangle with `x0 <= x1` and `y0 <= y1` once normalized.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub const EMPTY: Rect = Rect::new(0.0, 0.0, 0.0, 0.0);

    /// Builds a rectangle from two corners in any order.
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
        let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        let (y0, y1) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        Rect { x0, y0, x1, y1 }
    }

    pub fn from_xywh(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::new(x, y, x + w, y + h)
    }

    /// Bounding box of the finite points; `None` when there are none.
    pub fn from_points(points: &[Point]) -> Option<Rect> {
        let mut it = points.iter().filter(|p| p.is_finite());
        let first = it.next()?;
        let mut r = Rect {
            x0: first.x,
            y0: first.y,
            x1: first.x,
            y1: first.y,
        };
        for p in it {
            r.x0 = r.x0.min(p.x);
            r.y0 = r.y0.min(p.y);
            r.x1 = r.x1.max(p.x);
            r.y1 = r.y1.max(p.y);
        }
        Some(r)
    }

    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    pub fn is_empty(&self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let r = Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        };
        if r.is_empty() { Rect::EMPTY } else { r }
    }

    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Smallest pixel rectangle containing this rectangle, saturated to `i32`.
    pub fn round_out(&self) -> IntRect {
        IntRect::new(
            saturate_i32(self.x0.floor()),
            saturate_i32(self.y0.floor()),
            saturate_i32(self.x1.ceil()),
            saturate_i32(self.y1.ceil()),
        )
    }
}

pub(crate) fn saturate_i32(v: f64) -> i32 {
    if v.is_nan() {
        0
    } else {
        v.clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

/// Half-open pixel rectangle `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IntRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl IntRect {
    pub const EMPTY: IntRect = IntRect {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
    };

    /// Builds the rectangle; an inverted input becomes empty.
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> IntRect {
        let r = IntRect { x0, y0, x1, y1 };
        if r.is_empty() { IntRect::EMPTY } else { r }
    }

    pub fn from_size(width: u32, height: u32) -> IntRect {
        IntRect::new(
            0,
            0,
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub fn width(&self) -> u32 {
        if self.is_empty() {
            0
        } else {
            self.x1.abs_diff(self.x0)
        }
    }

    pub fn height(&self) -> u32 {
        if self.is_empty() {
            0
        } else {
            self.y1.abs_diff(self.y0)
        }
    }

    pub fn intersect(&self, other: &IntRect) -> IntRect {
        IntRect::new(
            self.x0.max(other.x0),
            self.y0.max(other.y0),
            self.x1.min(other.x1),
            self.y1.min(other.y1),
        )
    }

    /// Smallest rectangle containing both; an empty side is ignored.
    pub fn union(&self, other: &IntRect) -> IntRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        IntRect::new(
            self.x0.min(other.x0),
            self.y0.min(other.y0),
            self.x1.max(other.x1),
            self.y1.max(other.y1),
        )
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    pub fn to_rect(&self) -> Rect {
        Rect::new(
            f64::from(self.x0),
            f64::from(self.y0),
            f64::from(self.x1),
            f64::from(self.y1),
        )
    }
}
