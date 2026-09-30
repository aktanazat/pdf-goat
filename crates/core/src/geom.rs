//! Points, rectangles, and transformation matrices in PDF user space.

use crate::object::Object;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// The point mapped through `m`.
    pub fn transform(self, m: &Matrix) -> Point {
        Point {
            x: self.x * m.a + self.y * m.c + m.e,
            y: self.x * m.b + self.y * m.d + m.f,
        }
    }
}

/// A rectangle by two corners. [`Rect::from_array`] and
/// [`Rect::normalized`] order them so `x0 <= x1` and `y0 <= y1`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    /// A normalized rectangle from a PDF array of four numbers; `None` when
    /// the array is not four finite numbers.
    pub fn from_array(items: &[Object]) -> Option<Rect> {
        let [a, b, c, d] = items else { return None };
        let values = [a.as_f64()?, b.as_f64()?, c.as_f64()?, d.as_f64()?];
        if values.iter().any(|v| !v.is_finite()) {
            return None;
        }
        Some(Rect::new(values[0], values[1], values[2], values[3]).normalized())
    }

    /// `[x0 y0 x1 y1]`, integers where the values are whole.
    pub fn to_object(&self) -> Object {
        Object::Array(
            [self.x0, self.y0, self.x1, self.y1]
                .into_iter()
                .map(number)
                .collect(),
        )
    }

    /// The same rectangle with `x0 <= x1` and `y0 <= y1`.
    pub fn normalized(&self) -> Rect {
        Rect {
            x0: self.x0.min(self.x1),
            y0: self.y0.min(self.y1),
            x1: self.x0.max(self.x1),
            y1: self.y0.max(self.y1),
        }
    }

    pub fn width(&self) -> f64 {
        (self.x1 - self.x0).abs()
    }

    pub fn height(&self) -> f64 {
        (self.y1 - self.y0).abs()
    }

    /// True when the rectangle has no area.
    pub fn is_empty(&self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    /// The overlap of two normalized rectangles; empty (`is_empty`) when
    /// they do not overlap.
    pub fn intersect(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// The smallest rectangle holding both normalized rectangles.
    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x0 && point.x <= self.x1 && point.y >= self.y0 && point.y <= self.y1
    }

    /// The bounding box of the four corners mapped through `m`.
    pub fn transform(&self, m: &Matrix) -> Rect {
        let corners = [
            Point::new(self.x0, self.y0),
            Point::new(self.x1, self.y0),
            Point::new(self.x0, self.y1),
            Point::new(self.x1, self.y1),
        ]
        .map(|p| p.transform(m));
        let mut out = Rect::new(corners[0].x, corners[0].y, corners[0].x, corners[0].y);
        for p in &corners[1..] {
            out = out.union(&Rect::new(p.x, p.y, p.x, p.y));
        }
        out
    }
}

/// An affine transform `[a b c d e f]`, mapping `(x, y)` to
/// `(a x + c y + e, b x + d y + f)` as PDF's `cm` does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Matrix {
    fn default() -> Matrix {
        Matrix::IDENTITY
    }
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Matrix {
        Matrix { a, b, c, d, e, f }
    }

    pub const fn translate(tx: f64, ty: f64) -> Matrix {
        Matrix::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    pub const fn scale(sx: f64, sy: f64) -> Matrix {
        Matrix::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// Counter-clockwise rotation by `degrees`; multiples of 90 are exact.
    pub fn rotate(degrees: f64) -> Matrix {
        let turns = degrees.rem_euclid(360.0);
        let (sin, cos) = if turns == 0.0 {
            (0.0, 1.0)
        } else if turns == 90.0 {
            (1.0, 0.0)
        } else if turns == 180.0 {
            (0.0, -1.0)
        } else if turns == 270.0 {
            (-1.0, 0.0)
        } else {
            turns.to_radians().sin_cos()
        };
        Matrix::new(cos, sin, -sin, cos, 0.0, 0.0)
    }

    /// A matrix from a PDF array of six numbers.
    pub fn from_array(items: &[Object]) -> Option<Matrix> {
        let [a, b, c, d, e, f] = items else {
            return None;
        };
        Some(Matrix::new(
            a.as_f64()?,
            b.as_f64()?,
            c.as_f64()?,
            d.as_f64()?,
            e.as_f64()?,
            f.as_f64()?,
        ))
    }

    pub fn to_object(&self) -> Object {
        Object::Array(
            [self.a, self.b, self.c, self.d, self.e, self.f]
                .into_iter()
                .map(number)
                .collect(),
        )
    }

    /// `self` applied first, then `other` (PDF's `self × other`).
    pub fn concat(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    /// The inverse, or `None` when the matrix is singular.
    pub fn invert(&self) -> Option<Matrix> {
        let det = self.a * self.d - self.b * self.c;
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let a = self.d / det;
        let b = -self.b / det;
        let c = -self.c / det;
        let d = self.a / det;
        Some(Matrix {
            a,
            b,
            c,
            d,
            e: -(self.e * a + self.f * c),
            f: -(self.e * b + self.f * d),
        })
    }
}

/// An integer object when `value` is whole and in range, a real otherwise.
fn number(value: f64) -> Object {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        Object::Integer(value as i64)
    } else {
        Object::Real(value)
    }
}
