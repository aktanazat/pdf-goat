//! Raster file decoding for `from-images`: format sniffing, a uniform
//! [`DecodedImage`] for every supported format, and the checks img2pdf
//! applies before embedding JPEG, JPEG 2000, PNG, or Group 4 data as-is.

use crate::bmp;
use crate::dct::{JpegInfo, jpeg_info};
use crate::error::{CodecError, Result};
use crate::gif;
use crate::image::{DecodedImage, PixelLayout};
use crate::jbig2;
use crate::jpx::{JpxColorSpace, jpx_info};
use crate::png::{PngInfo, png_info};
use crate::pnm;
use crate::tiff;
use crate::webp;

const CODEC: &str = "image";

/// File formats [`detect_format`] recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Png,
    Jpeg,
    /// JP2 file or raw J2K codestream.
    Jpeg2000,
    Gif,
    Bmp,
    Tiff,
    WebP,
    /// PBM, PGM, PPM, or PAM.
    Pnm,
    /// Standalone JBIG2 file (sequential organisation).
    Jbig2,
}

/// Sniffs the format from the leading bytes.
pub fn detect_format(data: &[u8]) -> Option<ImageFormat> {
    if crate::png::is_png(data) {
        Some(ImageFormat::Png)
    } else if crate::dct::is_jpeg(data) {
        Some(ImageFormat::Jpeg)
    } else if crate::jpx::is_jpx(data) {
        Some(ImageFormat::Jpeg2000)
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bmp::is_bmp(data) {
        Some(ImageFormat::Bmp)
    } else if tiff::is_tiff(data) {
        Some(ImageFormat::Tiff)
    } else if webp::is_webp(data) {
        Some(ImageFormat::WebP)
    } else if pnm::is_pnm(data) {
        Some(ImageFormat::Pnm)
    } else if jbig2::is_jbig2_file(data) {
        Some(ImageFormat::Jbig2)
    } else {
        None
    }
}

/// Device colour space a decoded or embedded image maps to in a PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfColorSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
}

impl PdfColorSpace {
    pub fn components(self) -> u8 {
        match self {
            Self::DeviceGray => 1,
            Self::DeviceRGB => 3,
            Self::DeviceCMYK => 4,
        }
    }
}

/// Header facts of an image file, read without decoding pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageProbe {
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    /// Colour channels (alpha excluded), 1 for bilevel, gray, and palette.
    pub channels: u8,
    /// Bits per sample as stored.
    pub bit_depth: u8,
    /// Pages (TIFF) or frames (GIF, WebP animation) the file holds.
    pub pages: u32,
    /// The image carries alpha or a transparency key/index.
    pub has_alpha: bool,
    /// Resolution stated by the file, in dots per inch.
    pub dpi: Option<(f64, f64)>,
    pub icc_profile: Option<Vec<u8>>,
}

fn unknown_format() -> CodecError {
    CodecError::unsupported(CODEC, "unrecognised image format")
}

/// Reads the headers of an image file.
pub fn probe_image_file(data: &[u8]) -> Result<ImageProbe> {
    let format = detect_format(data).ok_or_else(unknown_format)?;
    Ok(match format {
        ImageFormat::Png => {
            let info = png_info(data)?;
            let channels = match info.color_type {
                2 | 6 => 3,
                _ => 1,
            };
            ImageProbe {
                format,
                width: info.width,
                height: info.height,
                channels,
                bit_depth: info.bit_depth,
                pages: 1,
                has_alpha: matches!(info.color_type, 4 | 6) || info.has_transparency,
                dpi: info.dpi,
                icc_profile: info.icc_profile,
            }
        }
        ImageFormat::Jpeg => {
            let info = jpeg_info(data)?;
            ImageProbe {
                format,
                width: info.width,
                height: info.height,
                channels: info.components,
                bit_depth: info.precision,
                pages: 1,
                has_alpha: false,
                dpi: info.dpi,
                icc_profile: info.icc_profile,
            }
        }
        ImageFormat::Jpeg2000 => {
            let info = jpx_info(data)?;
            let icc = match &info.color_space {
                JpxColorSpace::Icc { profile, .. } => Some(profile.clone()),
                _ => None,
            };
            ImageProbe {
                format,
                width: info.width,
                height: info.height,
                channels: info.color_space.channels(),
                bit_depth: info.bit_depth,
                pages: 1,
                has_alpha: info.has_alpha,
                dpi: info.dpi,
                icc_profile: icc,
            }
        }
        ImageFormat::Gif => {
            let info = gif::gif_info(data)?;
            ImageProbe {
                format,
                width: info.width,
                height: info.height,
                channels: 1,
                bit_depth: 8,
                pages: info.frames,
                has_alpha: info.transparent,
                dpi: None,
                icc_profile: None,
            }
        }
        ImageFormat::Bmp => {
            let img = bmp::decode_bmp(data)?;
            probe_from_decoded(format, &img, 1)
        }
        ImageFormat::Tiff => {
            let info = tiff::tiff_page_info(data, 0)?;
            let pages = tiff::tiff_page_count(data)?;
            let channels = match info.photometric {
                2 | 6 => 3,
                5 => 4,
                _ => 1,
            };
            ImageProbe {
                format,
                width: info.width,
                height: info.height,
                channels,
                bit_depth: info.bits_per_sample.min(255) as u8,
                pages,
                has_alpha: info.samples_per_pixel > u16::from(channels),
                dpi: info.dpi,
                icc_profile: info.icc_profile,
            }
        }
        ImageFormat::WebP => {
            let img = webp::decode_webp(data)?;
            probe_from_decoded(format, &img, 1)
        }
        ImageFormat::Pnm => {
            let img = pnm::decode_pnm(data)?;
            probe_from_decoded(format, &img, 1)
        }
        ImageFormat::Jbig2 => {
            let img = jbig2::decode_jbig2_file(data)?;
            ImageProbe {
                format,
                width: img.width,
                height: img.height,
                channels: 1,
                bit_depth: 1,
                pages: 1,
                has_alpha: false,
                dpi: jbig2_file_dpi(data),
                icc_profile: None,
            }
        }
    })
}

fn probe_from_decoded(format: ImageFormat, img: &DecodedImage, pages: u32) -> ImageProbe {
    ImageProbe {
        format,
        width: img.width,
        height: img.height,
        channels: img.layout.color_channels() as u8,
        bit_depth: img.bit_depth,
        pages,
        has_alpha: img.layout.has_alpha(),
        dpi: img.dpi,
        icc_profile: img.icc_profile.clone(),
    }
}

/// Resolution from the page information segment of a sequential JBIG2 file
/// (dots per metre, or dots per inch when below 1000, as img2pdf guesses).
fn jbig2_file_dpi(data: &[u8]) -> Option<(f64, f64)> {
    let b = data.get(24..32)?;
    let x = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let y = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    if x == 0 || y == 0 {
        return None;
    }
    let conv = |v: u32| {
        if v < 1000 {
            f64::from(v)
        } else {
            (f64::from(v) / 39.370_079).floor()
        }
    };
    Some((conv(x), conv(y)))
}

/// Decodes page (or frame) `page` of an image file, 0-based. Formats with a
/// single image ignore `page`. Bilevel output is 1-bit gray with 1 = white.
pub fn decode_image_file(data: &[u8], page: u32) -> Result<DecodedImage> {
    let format = detect_format(data).ok_or_else(unknown_format)?;
    match format {
        ImageFormat::Png => crate::png::decode_png(data),
        ImageFormat::Jpeg => {
            let img = crate::dct::decode_dct(data, &crate::dct::DctParams::default())?;
            let inverted = img.info.adobe_inverted_cmyk();
            let mut data = img.data;
            let layout = match img.components {
                1 => PixelLayout::Gray,
                3 => PixelLayout::Rgb,
                _ => {
                    if inverted {
                        for b in &mut data {
                            *b = !*b;
                        }
                    }
                    PixelLayout::Cmyk
                }
            };
            Ok(DecodedImage {
                width: img.width,
                height: img.height,
                layout,
                bit_depth: 8,
                data,
                palette: None,
                icc_profile: img.info.icc_profile,
                dpi: img.info.dpi,
            })
        }
        ImageFormat::Jpeg2000 => {
            let img = crate::jpx::decode_jpx(data)?;
            let colour = img.info.color_space.channels();
            let layout = match (colour, img.info.has_alpha) {
                (1, false) => PixelLayout::Gray,
                (1, true) => PixelLayout::GrayAlpha,
                (3, false) => PixelLayout::Rgb,
                (3, true) => PixelLayout::Rgba,
                (4, false) => PixelLayout::Cmyk,
                (c, a) => {
                    return Err(CodecError::unsupported(
                        CODEC,
                        format!(
                            "JPEG 2000 with {c} colour channels{}",
                            if a { " and alpha" } else { "" }
                        ),
                    ));
                }
            };
            let icc = match img.info.color_space {
                JpxColorSpace::Icc { profile, .. } => Some(profile),
                _ => None,
            };
            Ok(DecodedImage {
                width: img.info.width,
                height: img.info.height,
                layout,
                bit_depth: 8,
                data: img.data,
                palette: None,
                icc_profile: icc,
                dpi: img.info.dpi,
            })
        }
        ImageFormat::Gif => gif::decode_gif(data, page),
        ImageFormat::Bmp => bmp::decode_bmp(data),
        ImageFormat::Tiff => tiff::decode_tiff(data, page),
        ImageFormat::WebP => webp::decode_webp(data),
        ImageFormat::Pnm => pnm::decode_pnm(data),
        ImageFormat::Jbig2 => {
            let img = jbig2::decode_jbig2_file(data)?;
            Ok(DecodedImage {
                width: img.width,
                height: img.height,
                layout: PixelLayout::Gray,
                bit_depth: 1,
                data: img.data,
                palette: None,
                icc_profile: None,
                dpi: jbig2_file_dpi(data),
            })
        }
    }
}

/// What a PDF writer needs to embed a JPEG file unchanged under
/// `DCTDecode`, following img2pdf's rules.
#[derive(Debug, Clone, PartialEq)]
pub struct JpegPassthrough {
    pub width: u32,
    pub height: u32,
    pub color_space: PdfColorSpace,
    /// Adobe-inverted CMYK: write `/Decode [1 0 1 0 1 0 1 0]`.
    pub inverted_cmyk: bool,
    /// Resolution to lay the page out with: the file's, else `default_dpi`,
    /// rounded to whole dots as img2pdf does.
    pub dpi: (u32, u32),
    /// ICC profile to attach as `ICCBased`; a non-gray profile on a gray JPEG
    /// is dropped, as img2pdf does.
    pub icc_profile: Option<Vec<u8>>,
    /// EXIF orientation 1..=8 when present (6 = rotate 90°, 3 = 180°, 8 = 270°).
    pub exif_orientation: Option<u16>,
    pub info: JpegInfo,
}

/// Checks a JPEG for direct embedding. Baseline and progressive 8-bit
/// gray, YCbCr/RGB, and CMYK/YCCK files pass; lossless, hierarchical,
/// and 12-bit files are an error since `DCTDecode` cannot read them.
pub fn jpeg_passthrough(data: &[u8], default_dpi: u32) -> Result<JpegPassthrough> {
    let info = jpeg_info(data)?;
    if info.unsupported_process {
        return Err(CodecError::unsupported(
            "jpeg",
            "lossless or hierarchical JPEG",
        ));
    }
    if info.precision != 8 {
        return Err(CodecError::unsupported(
            "jpeg",
            format!("{}-bit samples", info.precision),
        ));
    }
    let color_space = match info.components {
        1 => PdfColorSpace::DeviceGray,
        3 => PdfColorSpace::DeviceRGB,
        4 => PdfColorSpace::DeviceCMYK,
        n => return Err(CodecError::unsupported("jpeg", format!("{n} components"))),
    };
    let dpi = round_dpi(info.dpi, default_dpi);
    let icc_profile = match (&info.icc_profile, color_space) {
        (Some(profile), PdfColorSpace::DeviceGray)
            if icc_color_space(profile) != Some(*b"GRAY") =>
        {
            None
        }
        (p, _) => p.clone(),
    };
    Ok(JpegPassthrough {
        width: info.width,
        height: info.height,
        color_space,
        inverted_cmyk: info.adobe_inverted_cmyk(),
        dpi,
        icc_profile,
        exif_orientation: info.exif_orientation,
        info,
    })
}

/// What a PDF writer needs to embed a JP2/J2K file unchanged under
/// `JPXDecode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpxPassthrough {
    pub width: u32,
    pub height: u32,
    /// `None` when the file has alpha (img2pdf omits `/ColorSpace` and lets
    /// the reader take it from the codestream) or an unusual channel count.
    pub color_space: Option<PdfColorSpace>,
    pub has_alpha: bool,
    pub bit_depth: u8,
    pub dpi: (u32, u32),
    pub icc_profile: Option<Vec<u8>>,
}

/// Checks a JPEG 2000 file for direct embedding.
pub fn jpx_passthrough(data: &[u8], default_dpi: u32) -> Result<JpxPassthrough> {
    let info = jpx_info(data)?;
    let color_space = if info.has_alpha {
        None
    } else {
        match info.color_space.channels() {
            1 => Some(PdfColorSpace::DeviceGray),
            3 => Some(PdfColorSpace::DeviceRGB),
            4 => Some(PdfColorSpace::DeviceCMYK),
            _ => None,
        }
    };
    let icc_profile = match info.color_space {
        JpxColorSpace::Icc { profile, .. } => Some(profile),
        _ => None,
    };
    Ok(JpxPassthrough {
        width: info.width,
        height: info.height,
        color_space,
        has_alpha: info.has_alpha,
        bit_depth: info.bit_depth,
        dpi: round_dpi(info.dpi, default_dpi),
        icc_profile,
    })
}

/// What a PDF writer needs to embed a non-interlaced PNG's IDAT stream
/// unchanged under `FlateDecode` with PNG predictors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngPassthrough {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// Colour channels: 1 for gray and palette, 3 for RGB.
    pub channels: u8,
    /// RGB triples for `/Indexed`, empty otherwise.
    pub palette: Vec<u8>,
    /// The concatenated IDAT payload (a zlib stream of filtered rows).
    pub idat: Vec<u8>,
    pub dpi: (u32, u32),
    pub icc_profile: Option<Vec<u8>>,
}

/// Returns the IDAT stream of a PNG that img2pdf embeds as-is: not
/// interlaced, without alpha or transparency, and not both palette-based
/// and ICC-tagged. `None` when the file must be decoded and re-encoded.
pub fn png_passthrough(data: &[u8], default_dpi: u32) -> Result<Option<PngPassthrough>> {
    let info = png_info(data)?;
    if info.interlaced || matches!(info.color_type, 4 | 6) || info.has_transparency {
        return Ok(None);
    }
    if info.color_type == 3 && info.icc_profile.is_some() {
        return Ok(None);
    }
    let idat = concat_idat(data)?;
    let palette = if info.color_type == 3 {
        info.palette.clone().unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(Some(PngPassthrough {
        width: info.width,
        height: info.height,
        bit_depth: info.bit_depth,
        channels: if info.color_type == 2 { 3 } else { 1 },
        palette,
        idat,
        dpi: png_dpi(&info, default_dpi),
        icc_profile: info.icc_profile,
    }))
}

fn concat_idat(data: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 8usize;
    let mut out = Vec::new();
    while let Some(head) = data.get(pos..pos + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let body = data
            .get(pos + 8..pos + 8 + len)
            .ok_or_else(|| CodecError::malformed("png", "truncated chunk"))?;
        if &head[4..8] == b"IDAT" {
            out.extend_from_slice(body);
        } else if &head[4..8] == b"IEND" {
            break;
        }
        pos += 12 + len;
    }
    Ok(out)
}

/// Pillow/img2pdf resolution for a PNG: `pHYs` in metres, else the aspect
/// ratio scaled from `default_dpi` (never below it), else the default.
fn png_dpi(info: &PngInfo, default_dpi: u32) -> (u32, u32) {
    if let Some(d) = info.dpi {
        return round_dpi(Some(d), default_dpi);
    }
    if let Some((ax, ay)) = info.aspect {
        let d = f64::from(default_dpi);
        let (x, y) = if ax > ay {
            (d * f64::from(ax) / f64::from(ay), d)
        } else {
            (d, d * f64::from(ay) / f64::from(ax))
        };
        return round_dpi(Some((x, y)), default_dpi);
    }
    (default_dpi, default_dpi)
}

/// Rounds a stated resolution to whole dots per inch, falling back to
/// `default_dpi` when absent or zero.
pub fn round_dpi(dpi: Option<(f64, f64)>, default_dpi: u32) -> (u32, u32) {
    match dpi {
        Some((x, y)) if x.is_finite() && y.is_finite() => {
            let rx = x.round().clamp(0.0, f64::from(u32::MAX)) as u32;
            let ry = y.round().clamp(0.0, f64::from(u32::MAX)) as u32;
            if rx == 0 || ry == 0 {
                (default_dpi, default_dpi)
            } else {
                (rx, ry)
            }
        }
        _ => (default_dpi, default_dpi),
    }
}

/// The colour space signature at offset 16 of an ICC profile header.
pub fn icc_color_space(profile: &[u8]) -> Option<[u8; 4]> {
    let b = profile.get(16..20)?;
    Some([b[0], b[1], b[2], b[3]])
}
