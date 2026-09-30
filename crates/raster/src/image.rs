//! Source images, sampling, and shaders.

use crate::geom::{Point, Transform};
use crate::pixmap::{Color, Pixmap, RasterError, check_size, mul255};

/// Per-pixel colour source in device space.
pub trait Shader {
    /// Writes premultiplied RGBA8 for pixels `(x + i, y)`, sampled at the
    /// pixel centres `(x + i + 0.5, y + 0.5)`.
    fn shade_row(&self, x: i32, y: i32, out: &mut [[u8; 4]]);
}

/// Adapts a closure `f(device_x, device_y) -> Color` (straight alpha,
/// evaluated at pixel centres) into a [`Shader`].
pub struct FnShader<F>(pub F);

impl<F: Fn(f64, f64) -> Color> Shader for FnShader<F> {
    fn shade_row(&self, x: i32, y: i32, out: &mut [[u8; 4]]) {
        let cy = f64::from(y) + 0.5;
        for (i, o) in out.iter_mut().enumerate() {
            *o = (self.0)(f64::from(x) + i as f64 + 0.5, cy).to_premultiplied();
        }
    }
}

/// Sample layout of an [`Image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Gray8,
    Rgb8,
    /// RGBA8 with straight (non-premultiplied) alpha.
    Rgba8,
    Rgba8Premultiplied,
}

impl ImageFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            ImageFormat::Gray8 => 1,
            ImageFormat::Rgb8 => 3,
            ImageFormat::Rgba8 | ImageFormat::Rgba8Premultiplied => 4,
        }
    }
}

/// Borrowed colour image, rows top to bottom, tightly packed.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    width: u32,
    height: u32,
    format: ImageFormat,
    data: &'a [u8],
}

impl<'a> Image<'a> {
    /// Checks that `data` holds exactly `width * height` pixels of `format`.
    pub fn new(
        width: u32,
        height: u32,
        format: ImageFormat,
        data: &'a [u8],
    ) -> Result<Image<'a>, RasterError> {
        let n = check_size(width, height)?;
        let expected = n * format.bytes_per_pixel();
        if data.len() != expected {
            return Err(RasterError::DataLength {
                expected,
                actual: data.len(),
            });
        }
        Ok(Image {
            width,
            height,
            format,
            data,
        })
    }

    /// Views a pixmap as a premultiplied image.
    pub fn from_pixmap(pixmap: &'a Pixmap) -> Image<'a> {
        Image {
            width: pixmap.width(),
            height: pixmap.height(),
            format: ImageFormat::Rgba8Premultiplied,
            data: pixmap.data(),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> ImageFormat {
        self.format
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Premultiplied RGBA of the sample at `(x, y)`; callers keep indices in range.
    #[inline]
    fn fetch(&self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.width as usize + x) * self.format.bytes_per_pixel();
        let d = self.data;
        match self.format {
            ImageFormat::Gray8 => [d[i], d[i], d[i], 255],
            ImageFormat::Rgb8 => [d[i], d[i + 1], d[i + 2], 255],
            ImageFormat::Rgba8 => {
                let a = u32::from(d[i + 3]);
                [
                    mul255(u32::from(d[i]), a) as u8,
                    mul255(u32::from(d[i + 1]), a) as u8,
                    mul255(u32::from(d[i + 2]), a) as u8,
                    d[i + 3],
                ]
            }
            ImageFormat::Rgba8Premultiplied => [d[i], d[i + 1], d[i + 2], d[i + 3]],
        }
    }
}

/// Borrowed 8-bit mask image (soft mask or stencil), `255` = opaque.
#[derive(Clone, Copy, Debug)]
pub struct MaskImage<'a> {
    width: u32,
    height: u32,
    data: &'a [u8],
}

impl<'a> MaskImage<'a> {
    /// Checks that `data` holds exactly `width * height` bytes.
    pub fn new(width: u32, height: u32, data: &'a [u8]) -> Result<MaskImage<'a>, RasterError> {
        let n = check_size(width, height)?;
        if data.len() != n {
            return Err(RasterError::DataLength {
                expected: n,
                actual: data.len(),
            });
        }
        Ok(MaskImage {
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

    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

/// Image sampling filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Filter {
    Nearest,
    /// Bilinear interpolation; images shrunk below half size are first
    /// box-filtered so they do not alias.
    #[default]
    Bilinear,
}

/// Pixel grid position of unit-square coordinates: `u` across, `v` up
/// (PDF image space, sample row 0 at `v = 1`).
#[inline]
fn grid(u: f64, v: f64, w: u32, h: u32) -> (f64, f64) {
    (u * f64::from(w), (1.0 - v) * f64::from(h))
}

#[inline]
fn clamp_index(i: f64, n: u32) -> usize {
    if i <= 0.0 || i.is_nan() {
        0
    } else {
        (i as usize).min(n as usize - 1)
    }
}

/// Bilinear taps: indices and weight of the second tap along one axis.
#[inline]
fn taps(s: f64, n: u32) -> (usize, usize, f32) {
    let f = s - 0.5;
    let i0 = f.floor();
    let t = (f - i0) as f32;
    (
        clamp_index(i0, n),
        clamp_index(i0 + 1.0, n),
        if t.is_finite() { t } else { 0.0 },
    )
}

/// Premultiplied colour source for image drawing.
pub(crate) enum Pixels<'a> {
    Borrowed(Image<'a>),
    Owned {
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
}

impl Pixels<'_> {
    fn size(&self) -> (u32, u32) {
        match self {
            Pixels::Borrowed(img) => (img.width, img.height),
            Pixels::Owned { width, height, .. } => (*width, *height),
        }
    }

    #[inline]
    fn fetch(&self, x: usize, y: usize) -> [u8; 4] {
        match self {
            Pixels::Borrowed(img) => img.fetch(x, y),
            Pixels::Owned { width, data, .. } => {
                let i = (y * *width as usize + x) * 4;
                [data[i], data[i + 1], data[i + 2], data[i + 3]]
            }
        }
    }

    fn sample(&self, u: f64, v: f64, filter: Filter) -> [u8; 4] {
        let (w, h) = self.size();
        let (sx, sy) = grid(u, v, w, h);
        match filter {
            Filter::Nearest => self.fetch(clamp_index(sx.floor(), w), clamp_index(sy.floor(), h)),
            Filter::Bilinear => {
                let (x0, x1, tx) = taps(sx, w);
                let (y0, y1, ty) = taps(sy, h);
                let p00 = self.fetch(x0, y0);
                let p10 = self.fetch(x1, y0);
                let p01 = self.fetch(x0, y1);
                let p11 = self.fetch(x1, y1);
                let mut out = [0u8; 4];
                for k in 0..4 {
                    let top = f32::from(p00[k]) + (f32::from(p10[k]) - f32::from(p00[k])) * tx;
                    let bottom = f32::from(p01[k]) + (f32::from(p11[k]) - f32::from(p01[k])) * tx;
                    out[k] = (top + (bottom - top) * ty + 0.5).clamp(0.0, 255.0) as u8;
                }
                out
            }
        }
    }
}

pub(crate) fn sample_mask(m: &MaskImage<'_>, u: f64, v: f64, filter: Filter) -> u8 {
    let (sx, sy) = grid(u, v, m.width, m.height);
    let at = |x: usize, y: usize| f32::from(m.data[y * m.width as usize + x]);
    match filter {
        Filter::Nearest => at(
            clamp_index(sx.floor(), m.width),
            clamp_index(sy.floor(), m.height),
        ) as u8,
        Filter::Bilinear => {
            let (x0, x1, tx) = taps(sx, m.width);
            let (y0, y1, ty) = taps(sy, m.height);
            let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
            let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
            (top + (bottom - top) * ty + 0.5).clamp(0.0, 255.0) as u8
        }
    }
}

/// Shader that draws an image placed by a unit-square transform.
pub(crate) struct ImageShader<'a> {
    pub pixels: Pixels<'a>,
    pub soft_mask: Option<MaskImage<'a>>,
    /// Device space to unit square.
    pub inverse: Transform,
    pub filter: Filter,
}

impl Shader for ImageShader<'_> {
    fn shade_row(&self, x: i32, y: i32, out: &mut [[u8; 4]]) {
        let start = self
            .inverse
            .apply(Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5));
        for (i, o) in out.iter_mut().enumerate() {
            let u = start.x + self.inverse.a * i as f64;
            let v = start.y + self.inverse.b * i as f64;
            let m = match &self.soft_mask {
                Some(mask) => sample_mask(mask, u, v, self.filter),
                None => 255,
            };
            *o = match m {
                0 => [0; 4],
                255 => self.pixels.sample(u, v, self.filter),
                _ => self
                    .pixels
                    .sample(u, v, self.filter)
                    .map(|c| mul255(u32::from(c), u32::from(m)) as u8),
            };
        }
    }
}

/// Box-filters an image (with its soft mask folded into alpha) by integer
/// factors, returning premultiplied RGBA8.
pub(crate) fn reduce(
    image: &Image<'_>,
    soft_mask: Option<&MaskImage<'_>>,
    kx: u32,
    ky: u32,
) -> Pixels<'static> {
    let w = image.width.div_ceil(kx);
    let h = image.height.div_ceil(ky);
    let mut data = Vec::with_capacity(w as usize * h as usize * 4);
    for by in 0..h {
        let y0 = by * ky;
        let y1 = (y0 + ky).min(image.height);
        for bx in 0..w {
            let x0 = bx * kx;
            let x1 = (x0 + kx).min(image.width);
            let mut sum = [0u64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let mut p = image.fetch(x as usize, y as usize);
                    if let Some(m) = soft_mask {
                        let mx = ((u64::from(x) * 2 + 1) * u64::from(m.width)
                            / (u64::from(image.width) * 2))
                            as usize;
                        let my = ((u64::from(y) * 2 + 1) * u64::from(m.height)
                            / (u64::from(image.height) * 2))
                            as usize;
                        let mv = u32::from(
                            m.data[my.min(m.height as usize - 1) * m.width as usize
                                + mx.min(m.width as usize - 1)],
                        );
                        p = p.map(|c| mul255(u32::from(c), mv) as u8);
                    }
                    for k in 0..4 {
                        sum[k] += u64::from(p[k]);
                    }
                }
            }
            let n = u64::from(x1 - x0) * u64::from(y1 - y0);
            data.extend(sum.map(|s| ((s + n / 2) / n) as u8));
        }
    }
    Pixels::Owned {
        width: w,
        height: h,
        data,
    }
}

/// Shader for tiling patterns and other pixmap sources: samples `pixmap`
/// placed by `transform` (pixmap pixel space to device space).
pub struct PixmapShader<'a> {
    pixmap: &'a Pixmap,
    inverse: Transform,
    filter: Filter,
    repeat: bool,
}

impl<'a> PixmapShader<'a> {
    /// `repeat` tiles the pixmap across the plane; otherwise samples outside
    /// it are transparent. `None` when `transform` is not invertible.
    pub fn new(
        pixmap: &'a Pixmap,
        transform: &Transform,
        filter: Filter,
        repeat: bool,
    ) -> Option<PixmapShader<'a>> {
        Some(PixmapShader {
            pixmap,
            inverse: transform.invert()?,
            filter,
            repeat,
        })
    }

    fn fetch(&self, x: i64, y: i64) -> [u8; 4] {
        let w = i64::from(self.pixmap.width());
        let h = i64::from(self.pixmap.height());
        let (x, y) = if self.repeat {
            (x.rem_euclid(w), y.rem_euclid(h))
        } else if x < 0 || y < 0 || x >= w || y >= h {
            return [0; 4];
        } else {
            (x, y)
        };
        self.pixmap.pixel(x as u32, y as u32).unwrap_or([0; 4])
    }

    fn sample(&self, sx: f64, sy: f64) -> [u8; 4] {
        if !(sx.is_finite() && sy.is_finite()) || sx.abs() > 1e15 || sy.abs() > 1e15 {
            return [0; 4];
        }
        match self.filter {
            Filter::Nearest => self.fetch(sx.floor() as i64, sy.floor() as i64),
            Filter::Bilinear => {
                let fx = sx - 0.5;
                let fy = sy - 0.5;
                let x0 = fx.floor();
                let y0 = fy.floor();
                let tx = (fx - x0) as f32;
                let ty = (fy - y0) as f32;
                let (x0, y0) = (x0 as i64, y0 as i64);
                let p00 = self.fetch(x0, y0);
                let p10 = self.fetch(x0 + 1, y0);
                let p01 = self.fetch(x0, y0 + 1);
                let p11 = self.fetch(x0 + 1, y0 + 1);
                let mut out = [0u8; 4];
                for k in 0..4 {
                    let top = f32::from(p00[k]) + (f32::from(p10[k]) - f32::from(p00[k])) * tx;
                    let bottom = f32::from(p01[k]) + (f32::from(p11[k]) - f32::from(p01[k])) * tx;
                    out[k] = (top + (bottom - top) * ty + 0.5).clamp(0.0, 255.0) as u8;
                }
                out
            }
        }
    }
}

impl Shader for PixmapShader<'_> {
    fn shade_row(&self, x: i32, y: i32, out: &mut [[u8; 4]]) {
        let start = self
            .inverse
            .apply(Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5));
        for (i, o) in out.iter_mut().enumerate() {
            let sx = start.x + self.inverse.a * i as f64;
            let sy = start.y + self.inverse.b * i as f64;
            *o = self.sample(sx, sy);
        }
    }
}
