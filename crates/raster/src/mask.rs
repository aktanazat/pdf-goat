//! 8-bit coverage masks: fill coverage, clip masks, and soft masks.

use crate::geom::{IntRect, Transform};
use crate::path::Path;
use crate::pixmap::{Pixmap, RasterError, check_size, luma, mul255};
use crate::raster::{Edges, FillRule, Rasterizer};

/// An 8-bit mask over a device rectangle. `255` means fully covered or
/// opaque. Pixels outside `rect` read as `outside`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mask {
    rect: IntRect,
    data: Vec<u8>,
    outside: u8,
}

impl Mask {
    /// A mask over `rect` with every inside pixel set to `value`.
    pub fn new(rect: IntRect, value: u8, outside: u8) -> Result<Mask, RasterError> {
        let n = if rect.is_empty() {
            0
        } else {
            check_size(rect.width(), rect.height())?
        };
        Ok(Mask {
            rect,
            data: vec![value; n],
            outside,
        })
    }

    /// Anti-aliased coverage of `path` under `transform`, limited to
    /// `bounds`. Outside reads 0.
    pub fn from_path(path: &Path, transform: &Transform, rule: FillRule, bounds: IntRect) -> Mask {
        let mut edges = Edges::default();
        edges.reset(bounds);
        edges.add_path(path, transform);
        let mut out = Mask::default();
        if !Rasterizer::default().rasterize(&mut edges, rule, &mut out) {
            out = Mask::default();
        }
        out
    }

    /// Moves the mask by whole pixels.
    pub fn translate(&mut self, dx: i32, dy: i32) {
        let r = self.rect;
        self.rect = IntRect {
            x0: r.x0.saturating_add(dx),
            y0: r.y0.saturating_add(dy),
            x1: r.x1.saturating_add(dx),
            y1: r.y1.saturating_add(dy),
        };
    }

    /// Wraps row-major data of exactly `rect.width() * rect.height()` bytes.
    pub fn from_data(rect: IntRect, data: Vec<u8>, outside: u8) -> Result<Mask, RasterError> {
        let n = if rect.is_empty() {
            0
        } else {
            check_size(rect.width(), rect.height())?
        };
        if data.len() != n {
            return Err(RasterError::DataLength {
                expected: n,
                actual: data.len(),
            });
        }
        Ok(Mask {
            rect,
            data,
            outside,
        })
    }

    /// Alpha soft mask: each pixel's alpha. Outside the pixmap reads 0.
    pub fn from_alpha(pixmap: &Pixmap) -> Mask {
        Mask {
            rect: pixmap.bounds(),
            data: pixmap
                .data()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[3])
                .collect(),
            outside: 0,
        }
    }

    /// Luminosity soft mask: each pixel composited over the opaque
    /// `backdrop`, then `0.30 R + 0.59 G + 0.11 B`. Outside the pixmap reads
    /// the backdrop's luminosity.
    pub fn from_luminosity(pixmap: &Pixmap, backdrop: [u8; 3]) -> Mask {
        let bg = backdrop.map(u32::from);
        let data = pixmap
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                let inv = 255 - u32::from(p[3]);
                let r = (u32::from(p[0]) + mul255(bg[0], inv)).min(255);
                let g = (u32::from(p[1]) + mul255(bg[1], inv)).min(255);
                let b = (u32::from(p[2]) + mul255(bg[2], inv)).min(255);
                luma(r, g, b)
            })
            .collect();
        Mask {
            rect: pixmap.bounds(),
            data,
            outside: luma(bg[0], bg[1], bg[2]),
        }
    }

    pub fn rect(&self) -> IntRect {
        self.rect
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn outside(&self) -> u8 {
        self.outside
    }

    /// Mask value at a device pixel.
    pub fn value(&self, x: i32, y: i32) -> u8 {
        if !self.rect.contains(x, y) {
            return self.outside;
        }
        let i =
            (y - self.rect.y0) as usize * self.rect.width() as usize + (x - self.rect.x0) as usize;
        self.data.get(i).copied().unwrap_or(self.outside)
    }

    /// Replaces every value `v` (inside and outside) with `lut[v]`, e.g. a
    /// soft-mask transfer function.
    pub fn map_values(&mut self, lut: &[u8; 256]) {
        for v in &mut self.data {
            *v = lut[usize::from(*v)];
        }
        self.outside = lut[usize::from(self.outside)];
    }

    /// Row `y` of the inside data, or `None` when the row is outside `rect`.
    pub(crate) fn row(&self, y: i32) -> Option<&[u8]> {
        if y < self.rect.y0 || y >= self.rect.y1 {
            return None;
        }
        let w = self.rect.width() as usize;
        let start = (y - self.rect.y0) as usize * w;
        self.data.get(start..start + w)
    }

    /// Multiplies `out[i]` by the mask value at `(x0 + i, y)`.
    pub(crate) fn mul_row(&self, x0: i32, y: i32, out: &mut [u8]) {
        let outside = u32::from(self.outside);
        let Some(row) = self.row(y) else {
            scale_all(out, outside);
            return;
        };
        for (i, v) in out.iter_mut().enumerate() {
            if *v == 0 {
                continue;
            }
            let x = x0.saturating_add(i as i32);
            let m = if x >= self.rect.x0 && x < self.rect.x1 {
                u32::from(row[(x - self.rect.x0) as usize])
            } else {
                outside
            };
            *v = mul255(u32::from(*v), m) as u8;
        }
    }

    /// Resets to an all-zero mask over `rect`, reusing the allocation.
    pub(crate) fn reset(&mut self, rect: IntRect) {
        self.rect = rect;
        self.outside = 0;
        self.data.clear();
        self.data
            .resize(rect.width() as usize * rect.height() as usize, 0);
    }

    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Pixel bounds of the nonzero inside values (ignores `outside`).
    pub(crate) fn nonzero_bounds(&self) -> IntRect {
        let w = self.rect.width() as usize;
        if w == 0 {
            return IntRect::EMPTY;
        }
        let mut b = IntRect::EMPTY;
        for (ry, row) in self.data.chunks_exact(w).enumerate() {
            let Some(first) = row.iter().position(|&v| v != 0) else {
                continue;
            };
            let last = row.iter().rposition(|&v| v != 0).unwrap_or(first);
            let y = self.rect.y0 + ry as i32;
            b = b.union(&IntRect::new(
                self.rect.x0 + first as i32,
                y,
                self.rect.x0 + last as i32 + 1,
                y + 1,
            ));
        }
        b
    }
}

pub(crate) fn scale_all(out: &mut [u8], m: u32) {
    match m {
        255 => {}
        0 => out.fill(0),
        _ => {
            for v in out {
                *v = mul255(u32::from(*v), m) as u8;
            }
        }
    }
}
