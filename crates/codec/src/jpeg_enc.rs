//! JPEG encoding through `jpeg-encoder` (baseline, 4:2:0 chroma
//! subsampling for colour, JFIF header with resolution).

use jpeg_encoder::{ColorType, Encoder, PixelDensity, PixelDensityUnit};

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::check_len;

const CODEC: &str = "jpeg";

/// Sample layout accepted by [`encode_jpeg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegColor {
    /// 8-bit gray.
    Gray,
    /// 8-bit RGB, stored as YCbCr.
    Rgb,
    /// 8-bit DeviceCMYK (0 = no ink). Stored the way Adobe writers do:
    /// samples inverted with an APP14 marker, so a PDF embedding the result
    /// needs `/Decode [1 0 1 0 1 0 1 0]` (see
    /// [`crate::JpegInfo::adobe_inverted_cmyk`]).
    Cmyk,
}

/// Encodes interleaved 8-bit samples as a baseline JPEG. `quality` is
/// clamped to `1..=100`; `dpi`, when given, is written to the JFIF header
/// (rounded to whole dots per inch).
pub fn encode_jpeg(
    data: &[u8],
    width: u32,
    height: u32,
    color: JpegColor,
    quality: u8,
    dpi: Option<(f64, f64)>,
) -> Result<Vec<u8>> {
    check_dimensions(CODEC, width, height)?;
    let (w, h) = match (u16::try_from(width), u16::try_from(height)) {
        (Ok(w), Ok(h)) => (w, h),
        _ => {
            return Err(CodecError::limit(
                CODEC,
                format!("{width}x{height} exceeds the JPEG limit of 65535"),
            ));
        }
    };
    let (channels, color_type) = match color {
        JpegColor::Gray => (1, ColorType::Luma),
        JpegColor::Rgb => (3, ColorType::Rgb),
        JpegColor::Cmyk => (4, ColorType::Cmyk),
    };
    check_len(CODEC, data, width, height, channels, 8)?;
    let mut out = Vec::with_capacity(data.len() / 4 + 1024);
    let mut encoder = Encoder::new(&mut out, quality.clamp(1, 100));
    if let Some((x, y)) = dpi {
        let clamp = |v: f64| v.round().clamp(1.0, f64::from(u16::MAX)) as u16;
        encoder.set_density(PixelDensity {
            density: (clamp(x), clamp(y)),
            unit: PixelDensityUnit::Inches,
        });
    }
    encoder
        .encode(data, w, h, color_type)
        .map_err(|e| CodecError::invalid(CODEC, e.to_string()))?;
    Ok(out)
}
