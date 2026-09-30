//! Sample containers shared by the decoders.

use crate::error::{CodecError, Result};

/// Channel layout of interleaved samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelLayout {
    /// One gray channel.
    Gray,
    /// Gray plus straight (non-premultiplied) alpha.
    GrayAlpha,
    /// Red, green, blue.
    Rgb,
    /// Red, green, blue plus straight alpha.
    Rgba,
    /// Cyan, magenta, yellow, black; 0 = no ink (PDF DeviceCMYK convention).
    Cmyk,
    /// One index channel into an RGB palette.
    Indexed,
}

impl PixelLayout {
    /// Samples per pixel, alpha included.
    pub fn channels(self) -> usize {
        match self {
            Self::Gray | Self::Indexed => 1,
            Self::GrayAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba | Self::Cmyk => 4,
        }
    }

    /// Colour samples per pixel, alpha excluded.
    pub fn color_channels(self) -> usize {
        match self {
            Self::Gray | Self::GrayAlpha | Self::Indexed => 1,
            Self::Rgb | Self::Rgba => 3,
            Self::Cmyk => 4,
        }
    }

    pub fn has_alpha(self) -> bool {
        matches!(self, Self::GrayAlpha | Self::Rgba)
    }
}

/// A 1-bit-per-pixel image with rows packed most significant bit first and
/// padded to a byte boundary (the PDF sample convention for
/// `BitsPerComponent 1`).
///
/// The meaning of a 1 bit depends on the producer: [`crate::decode_ccitt`]
/// honours `BlackIs1`, [`crate::decode_jbig2`] always yields 1 = white
/// (the JBIG2Decode filter convention), and file decoders say so in their
/// documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BilevelImage {
    pub width: u32,
    pub height: u32,
    /// `height * stride()` bytes.
    pub data: Vec<u8>,
}

impl BilevelImage {
    /// Creates an image filled with `fill` bytes (0x00 or 0xFF).
    pub fn filled(width: u32, height: u32, fill: u8) -> Self {
        let stride = (width as usize).div_ceil(8);
        Self {
            width,
            height,
            data: vec![fill; stride * height as usize],
        }
    }

    /// Bytes per packed row.
    pub fn stride(&self) -> usize {
        (self.width as usize).div_ceil(8)
    }

    /// The bit at (`x`, `y`); `false` outside the image.
    pub fn bit(&self, x: u32, y: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let byte = self.data[y as usize * self.stride() + (x / 8) as usize];
        (byte >> (7 - x % 8)) & 1 == 1
    }

    /// Flips every bit, mapping 1 = black to 1 = white or back.
    pub fn invert(&mut self) {
        for b in &mut self.data {
            *b = !*b;
        }
        self.clear_padding();
    }

    /// Zeroes the padding bits after the last pixel of each row.
    pub fn clear_padding(&mut self) {
        let tail = (self.width % 8) as u8;
        if tail == 0 {
            return;
        }
        let stride = self.stride();
        let mask = 0xFFu8 << (8 - tail);
        for row in self.data.chunks_exact_mut(stride) {
            row[stride - 1] &= mask;
        }
    }

    /// Expands to one byte per pixel: a set bit becomes `one`, a clear bit
    /// becomes `zero`.
    pub fn to_gray8(&self, zero: u8, one: u8) -> Vec<u8> {
        let stride = self.stride();
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize);
        for row in self.data.chunks_exact(stride) {
            for x in 0..self.width as usize {
                let set = (row[x / 8] >> (7 - x % 8)) & 1 == 1;
                out.push(if set { one } else { zero });
            }
        }
        out
    }
}

/// Decoded samples of a raster image file (PNG, JPEG, GIF, BMP, TIFF, PNM,
/// WebP, JPEG 2000) or of a PDF image stream converted to samples.
///
/// `data` holds rows packed in the PDF sample convention: samples of
/// `bit_depth` bits, most significant bit first, each row padded to a byte
/// boundary; 16-bit samples are big-endian. `bit_depth` below 8 only occurs
/// with [`PixelLayout::Gray`] and [`PixelLayout::Indexed`].
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    /// Bits per sample: 1, 2, 4, 8, or 16.
    pub bit_depth: u8,
    pub data: Vec<u8>,
    /// RGB triples for [`PixelLayout::Indexed`], `None` otherwise.
    pub palette: Option<Vec<u8>>,
    /// Embedded ICC profile, if the file carried one.
    pub icc_profile: Option<Vec<u8>>,
    /// Horizontal and vertical resolution in pixels per inch, if the file
    /// stated one.
    pub dpi: Option<(f64, f64)>,
}

impl DecodedImage {
    /// Bytes per packed row.
    pub fn stride(&self) -> usize {
        row_stride(self.width, self.layout.channels(), self.bit_depth)
    }

    pub fn has_alpha(&self) -> bool {
        self.layout.has_alpha()
    }

    /// Straight 8-bit RGBA, palette expanded, deeper samples reduced to 8 bits,
    /// CMYK converted with the naive formula of [`crate::cmyk_to_rgb`].
    pub fn to_rgba8(&self) -> Vec<u8> {
        let samples = self.samples8();
        let n = self.width as usize * self.height as usize;
        let mut out = Vec::with_capacity(n * 4);
        match self.layout {
            PixelLayout::Gray => {
                for &g in &samples {
                    out.extend_from_slice(&[g, g, g, 255]);
                }
            }
            PixelLayout::GrayAlpha => {
                for px in samples.as_chunks::<2>().0.iter() {
                    out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
                }
            }
            PixelLayout::Rgb => {
                for px in samples.as_chunks::<3>().0.iter() {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            PixelLayout::Rgba => out = samples,
            PixelLayout::Cmyk => {
                for px in samples.as_chunks::<4>().0.iter() {
                    let [r, g, b] = crate::pixels::cmyk_to_rgb_pixel(px[0], px[1], px[2], px[3]);
                    out.extend_from_slice(&[r, g, b, 255]);
                }
            }
            PixelLayout::Indexed => {
                let palette = self.palette.as_deref().unwrap_or(&[]);
                for &i in &samples {
                    let base = usize::from(i) * 3;
                    let rgb = palette.get(base..base + 3).unwrap_or(&[0, 0, 0]);
                    out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
            }
        }
        out
    }

    /// 8-bit RGB with any alpha composited onto white, the flattening
    /// `from-images` applies before embedding.
    pub fn to_rgb8_on_white(&self) -> Vec<u8> {
        let rgba = self.to_rgba8();
        let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
        for px in rgba.as_chunks::<4>().0.iter() {
            let a = u32::from(px[3]);
            for &c in &px[..3] {
                out.push(((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8);
            }
        }
        out
    }

    /// 8-bit gray (Rec. 601 luma for colour layouts, alpha ignored).
    pub fn to_gray8(&self) -> Vec<u8> {
        let rgba = self.to_rgba8();
        rgba.as_chunks::<4>()
            .0
            .iter()
            .map(|px| crate::pixels::luma(px[0], px[1], px[2]))
            .collect()
    }

    /// The 8-bit alpha plane, if the image has alpha.
    pub fn alpha8(&self) -> Option<Vec<u8>> {
        if !self.has_alpha() {
            return None;
        }
        let ch = self.layout.channels();
        Some(
            self.samples8()
                .chunks_exact(ch)
                .map(|px| px[ch - 1])
                .collect(),
        )
    }

    /// Every sample as one byte, in row-major interleaved order (no row
    /// padding): 16-bit samples keep their high byte, sub-byte gray samples
    /// scale to 0..=255, and indices stay as they are.
    pub fn samples8(&self) -> Vec<u8> {
        let ch = self.layout.channels();
        let scale_gray = self.layout == PixelLayout::Gray;
        match self.bit_depth {
            8 => self.data.clone(),
            16 => self.data.as_chunks::<2>().0.iter().map(|s| s[0]).collect(),
            bpc => {
                let unpacked =
                    crate::pixels::unpack_samples(&self.data, self.width, self.height, bpc, ch)
                        .unwrap_or_default();
                let max = (1u16 << bpc) - 1;
                unpacked
                    .into_iter()
                    .map(|s| {
                        if scale_gray {
                            (s * 255 / max) as u8
                        } else {
                            s as u8
                        }
                    })
                    .collect()
            }
        }
    }
}

/// Bytes per row for `width` pixels of `channels` samples of `bit_depth` bits,
/// rows padded to a byte boundary.
pub fn row_stride(width: u32, channels: usize, bit_depth: u8) -> usize {
    (width as usize * channels * usize::from(bit_depth)).div_ceil(8)
}

/// Checks that `data` holds exactly `height` packed rows.
pub(crate) fn check_len(
    codec: &'static str,
    data: &[u8],
    width: u32,
    height: u32,
    channels: usize,
    bit_depth: u8,
) -> Result<()> {
    let need = row_stride(width, channels, bit_depth) * height as usize;
    if data.len() != need {
        return Err(CodecError::invalid(
            codec,
            format!(
                "expected {need} bytes for {width}x{height}, got {}",
                data.len()
            ),
        ));
    }
    Ok(())
}
