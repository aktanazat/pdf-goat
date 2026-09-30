//! Gouraud triangles for PDF shadings.
//!
//! The painter follows MuPDF's `fz_paint_triangle` so gradients match
//! PyMuPDF output pixel for pixel: a triangle owns the rows from the ceiling
//! of its top vertex up to (excluding) the ceiling of its bottom vertex, a row
//! owns the pixels from its truncated left edge up to (excluding) its
//! truncated right edge, and the vertex values are stepped in 16.16 fixed
//! point and truncated to bytes. Adjoining triangles therefore paint every
//! pixel exactly once, and the last triangle painted wins where they overlap.

use crate::geom::IntRect;
use crate::pixmap::{Pixmap, RasterError};

/// A triangle corner in device space with up to three values scaled to
/// `0..=255`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadeVertex {
    pub x: f32,
    pub y: f32,
    pub value: [f32; 3],
}

/// What the vertex values of a [`ShadePainter`] hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadeSource<'a> {
    /// `value[0]` is a function parameter; the painted byte indexes this
    /// table.
    Parameter(&'a [[u8; 3]; 256]),
    /// `value` is RGB.
    Rgb,
}

/// Paints Gouraud triangles into a pixel rectangle.
pub struct ShadePainter<'a> {
    area: IntRect,
    clip: IntRect,
    source: ShadeSource<'a>,
    pixmap: Pixmap,
}

const MAXN: usize = 3;

struct Edge {
    x: f32,
    dx: f32,
    v: [i32; MAXN],
    dv: [i32; MAXN],
}

impl Edge {
    fn prepare(top: &ShadeVertex, bottom: &ShadeVertex, y: f32, n: usize) -> Edge {
        let r = 1.0 / (bottom.y - top.y);
        let t = (y - top.y) * r;
        let diff = bottom.x - top.x;
        let mut edge = Edge {
            x: top.x + diff * t,
            dx: diff * r,
            v: [0; MAXN],
            dv: [0; MAXN],
        };
        for i in 0..n {
            let diff = bottom.value[i] - top.value[i];
            edge.v[i] = (65536.0 * (top.value[i] + diff * t)) as i32;
            edge.dv[i] = (65536.0 * diff * r) as i32;
        }
        edge
    }

    fn step(&mut self) {
        self.x += self.dx;
        for (v, dv) in self.v.iter_mut().zip(self.dv) {
            *v = v.wrapping_add(dv);
        }
    }
}

impl<'a> ShadePainter<'a> {
    /// A painter whose pixmap covers `area`, initially unpainted; triangles
    /// paint only inside `clip`.
    pub fn new(
        area: IntRect,
        clip: IntRect,
        source: ShadeSource<'a>,
    ) -> Result<ShadePainter<'a>, RasterError> {
        Ok(ShadePainter {
            area,
            clip: clip.intersect(&area),
            source,
            pixmap: Pixmap::new(area.width(), area.height())?,
        })
    }

    pub fn area(&self) -> IntRect {
        self.area
    }

    fn channels(&self) -> usize {
        match self.source {
            ShadeSource::Parameter(_) => 1,
            ShadeSource::Rgb => 3,
        }
    }

    /// Paints one triangle, clipped to the painter's clip.
    pub fn triangle(&mut self, v: [ShadeVertex; 3]) {
        if v.iter().any(|v| !(v.x.is_finite() && v.y.is_finite())) {
            return;
        }
        let n = self.channels();
        let (mut top, mut bot) = (0, 0);
        if v[1].y < v[0].y {
            top = 1;
        } else {
            bot = 1;
        }
        if v[2].y < v[top].y {
            top = 2;
        } else if v[2].y > v[bot].y {
            bot = 2;
        }
        if v[top].y == v[bot].y {
            return;
        }
        let clip = self.clip;
        let (cy0, cy1) = (clip.y0 as f32, clip.y1 as f32);
        if v[bot].y < cy0 || v[top].y > cy1 {
            return;
        }
        let mid = 3 ^ top ^ bot;
        let mut y = cy0.max(v[top].y).ceil();
        let mut y1 = cy1.min(v[mid].y).ceil();
        let mut e0 = Edge::prepare(&v[top], &v[bot], y, n);
        if y < y1 {
            let mut e1 = Edge::prepare(&v[top], &v[mid], y, n);
            loop {
                self.scan(y as i32, &e0, &e1, n);
                e0.step();
                e1.step();
                y += 1.0;
                if y >= y1 {
                    break;
                }
            }
        }
        y1 = cy1.min(v[bot].y).ceil();
        if y < y1 {
            let mut e1 = Edge::prepare(&v[mid], &v[bot], y, n);
            loop {
                self.scan(y as i32, &e0, &e1, n);
                y += 1.0;
                if y >= y1 {
                    break;
                }
                e0.step();
                e1.step();
            }
        }
    }

    fn scan(&mut self, y: i32, e0: &Edge, e1: &Edge, n: usize) {
        let clip = self.clip;
        if y < clip.y0 || y >= clip.y1 {
            return;
        }
        let (mut fx0, mut fx1) = (e0.x as i32, e1.x as i32);
        let (mut v0, mut v1) = (&e0.v, &e1.v);
        if fx0 > fx1 {
            std::mem::swap(&mut fx0, &mut fx1);
            std::mem::swap(&mut v0, &mut v1);
        } else if fx0 == fx1 {
            return;
        }
        if fx0 >= clip.x1 || fx1 <= clip.x0 {
            return;
        }
        let x0 = fx0.max(clip.x0);
        let x1 = fx1.min(clip.x1);
        if x0 >= x1 {
            return;
        }
        let div = 1.0 / (fx1 - fx0) as f32;
        let mul = (x0 - fx0) as f32;
        let mut c = [0i32; MAXN];
        let mut dc = [0i32; MAXN];
        for k in 0..n {
            dc[k] = ((v1[k] - v0[k]) as f32 * div) as i32;
            c[k] = (v0[k] as f32 + dc[k] as f32 * mul) as i32;
        }
        let width = self.area.width() as usize;
        let start = ((y - self.area.y0) as usize * width + (x0 - self.area.x0) as usize) * 4;
        let end = start + (x1 - x0) as usize * 4;
        let Some(row) = self.pixmap.data_mut().get_mut(start..end) else {
            return;
        };
        for pixel in row.as_chunks_mut::<4>().0 {
            for k in 0..n {
                // MuPDF stores the low byte of the 16.16 value.
                pixel[k] = (c[k] >> 16) as u8;
                c[k] = c[k].wrapping_add(dc[k]);
            }
            pixel[3] = 255;
        }
    }

    /// The painted pixels as an opaque premultiplied pixmap over the area;
    /// unpainted pixels take `background` or stay transparent.
    pub fn finish(mut self, background: Option<[u8; 3]>) -> Pixmap {
        for pixel in self.pixmap.data_mut().as_chunks_mut::<4>().0 {
            if pixel[3] == 0 {
                if let Some([r, g, b]) = background {
                    *pixel = [r, g, b, 255];
                }
            } else if let ShadeSource::Parameter(lut) = self.source {
                let [r, g, b] = lut[usize::from(pixel[0])];
                *pixel = [r, g, b, 255];
            }
        }
        self.pixmap
    }
}
