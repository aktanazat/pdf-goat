//! RGBA8 premultiplied pixel buffers and colours.

use std::fmt;

use crate::geom::IntRect;

/// Largest accepted width or height, in pixels.
pub const MAX_DIMENSION: u32 = 1 << 16;
/// Largest accepted pixel count for one buffer (1 GiB of RGBA8).
pub const MAX_PIXELS: u64 = 1 << 28;

/// Errors from buffer construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RasterError {
    /// Width or height is zero, above [`MAX_DIMENSION`], or the area is above
    /// [`MAX_PIXELS`].
    InvalidSize { width: u32, height: u32 },
    /// A data slice does not match the size its dimensions and format imply.
    DataLength { expected: usize, actual: usize },
    /// The allocator could not reserve the requested buffer.
    AllocationFailed { bytes: usize },
    /// Opening a group would exceed the canvas's total group buffer budget.
    GroupMemoryLimit { requested: usize, limit: usize },
    /// Too many transparency groups are already open.
    GroupDepthLimit { limit: usize },
}

impl fmt::Display for RasterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RasterError::InvalidSize { width, height } => {
                write!(f, "invalid raster size {width}x{height}")
            }
            RasterError::DataLength { expected, actual } => {
                write!(f, "raster data has {actual} bytes, expected {expected}")
            }
            RasterError::AllocationFailed { bytes } => {
                write!(f, "could not allocate {bytes} bytes for raster data")
            }
            RasterError::GroupMemoryLimit { requested, limit } => {
                write!(
                    f,
                    "transparency groups need {requested} bytes, exceeding the {limit}-byte limit"
                )
            }
            RasterError::GroupDepthLimit { limit } => {
                write!(f, "transparency group nesting exceeds the limit of {limit}")
            }
        }
    }
}

impl std::error::Error for RasterError {}

pub(crate) fn check_size(width: u32, height: u32) -> Result<usize, RasterError> {
    let area = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || area > MAX_PIXELS
    {
        return Err(RasterError::InvalidSize { width, height });
    }
    usize::try_from(area).map_err(|_| RasterError::InvalidSize { width, height })
}

pub(crate) fn zeroed_bytes(bytes: usize) -> Result<Vec<u8>, RasterError> {
    let mut data = Vec::new();
    data.try_reserve_exact(bytes)
        .map_err(|_| RasterError::AllocationFailed { bytes })?;
    data.resize(bytes, 0);
    Ok(data)
}

/// Straight-alpha colour with components in `0.0..=1.0`. Out-of-range and NaN
/// components are clamped when the colour is used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const BLACK: Color = Color::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);
    pub const TRANSPARENT: Color = Color::new(0.0, 0.0, 0.0, 0.0);

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Color {
        Color { r, g, b, a }
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Color {
        Color { r, g, b, a: 1.0 }
    }

    pub fn from_rgba8(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            f32::from(a) / 255.0,
        )
    }

    /// Premultiplied RGBA8: `round(255 * a)` and `round(255 * c * a)`.
    pub fn to_premultiplied(self) -> [u8; 4] {
        let a = unit(self.a);
        [
            to_u8(unit(self.r) * a),
            to_u8(unit(self.g) * a),
            to_u8(unit(self.b) * a),
            to_u8(a),
        ]
    }
}

pub(crate) fn unit(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// `round(v * 255)` for `v` in `0..=1`.
pub(crate) fn to_u8(v: f32) -> u8 {
    (unit(v) * 255.0 + 0.5) as u8
}

/// `round(a * b / 255)`, exact for all byte inputs.
#[inline]
pub(crate) fn mul255(a: u32, b: u32) -> u32 {
    let t = a * b + 128;
    (t + (t >> 8)) >> 8
}

/// A width × height RGBA8 buffer with premultiplied alpha, rows top to
/// bottom, `stride = 4 * width` bytes per row.
#[derive(Clone, PartialEq, Eq)]
pub struct Pixmap {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl fmt::Debug for Pixmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pixmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Pixmap {
    /// A transparent pixmap.
    pub fn new(width: u32, height: u32) -> Result<Pixmap, RasterError> {
        let n = check_size(width, height)?;
        Ok(Pixmap {
            width,
            height,
            data: zeroed_bytes(n * 4)?,
        })
    }

    /// Wraps premultiplied RGBA8 data of exactly `width * height * 4` bytes.
    pub fn from_premultiplied(
        width: u32,
        height: u32,
        data: Vec<u8>,
    ) -> Result<Pixmap, RasterError> {
        let n = check_size(width, height)?;
        if data.len() != n * 4 {
            return Err(RasterError::DataLength {
                expected: n * 4,
                actual: data.len(),
            });
        }
        Ok(Pixmap {
            width,
            height,
            data,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row.
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn bounds(&self) -> IntRect {
        IntRect::from_size(self.width, self.height)
    }

    /// Premultiplied RGBA8 bytes, row-major.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Premultiplied RGBA of one pixel, or `None` outside the pixmap.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        let p = self.data.get(i..i + 4)?;
        Some([p[0], p[1], p[2], p[3]])
    }

    /// Fills every pixel with `color`.
    pub fn fill(&mut self, color: Color) {
        let p = color.to_premultiplied();
        for px in self.data.as_chunks_mut::<4>().0 {
            *px = p;
        }
    }

    /// Makes every pixel transparent.
    pub fn clear(&mut self) {
        self.data.fill(0);
    }

    pub(crate) fn row_mut(&mut self, y: u32, x0: u32, x1: u32) -> &mut [u8] {
        let start = (y as usize * self.width as usize + x0 as usize) * 4;
        let end = (y as usize * self.width as usize + x1 as usize) * 4;
        &mut self.data[start..end]
    }

    pub(crate) fn row(&self, y: u32, x0: u32, x1: u32) -> &[u8] {
        let start = (y as usize * self.width as usize + x0 as usize) * 4;
        let end = (y as usize * self.width as usize + x1 as usize) * 4;
        &self.data[start..end]
    }

    /// Straight-alpha RGBA8 (for PNG): each colour is `round(c * 255 / a)`.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len());
        for &[r, g, b, a] in self.data.as_chunks::<4>().0 {
            match a {
                0 => out.extend_from_slice(&[0, 0, 0, 0]),
                255 => out.extend_from_slice(&[r, g, b, 255]),
                _ => out.extend_from_slice(&[unpremul(r, a), unpremul(g, a), unpremul(b, a), a]),
            }
        }
        out
    }

    /// RGB8 composited over an opaque `background`.
    pub fn to_rgb8(&self, background: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() / 4 * 3);
        for p in self.data.as_chunks::<4>().0 {
            let inv = 255 - u32::from(p[3]);
            for c in 0..3 {
                let v = u32::from(p[c]) + mul255(u32::from(background[c]), inv);
                out.push(v.min(255) as u8);
            }
        }
        out
    }

    /// Gray8 composited over an opaque gray `background`, using luma
    /// `0.30 R + 0.59 G + 0.11 B`.
    pub fn to_gray8(&self, background: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() / 4);
        let bg = u32::from(background);
        for p in self.data.as_chunks::<4>().0 {
            let inv = 255 - u32::from(p[3]);
            let r = (u32::from(p[0]) + mul255(bg, inv)).min(255);
            let g = (u32::from(p[1]) + mul255(bg, inv)).min(255);
            let b = (u32::from(p[2]) + mul255(bg, inv)).min(255);
            out.push(luma(r, g, b));
        }
        out
    }
}

/// `round(0.30 r + 0.59 g + 0.11 b)` in 8.8 fixed point.
pub(crate) fn luma(r: u32, g: u32, b: u32) -> u8 {
    ((77 * r + 151 * g + 28 * b + 128) >> 8).min(255) as u8
}

fn unpremul(c: u8, a: u8) -> u8 {
    let a = u32::from(a);
    ((u32::from(c) * 255 + a / 2) / a).min(255) as u8
}
