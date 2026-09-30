//! JPXDecode: JPEG 2000 codestreams and JP2 files through `hayro-jpeg2000`.

use hayro_jpeg2000::{ColorSpace, DecodeSettings, DecoderContext, Image};

use crate::error::{CodecError, Result, check_dimensions};

const CODEC: &str = "jpx";

/// Colour space signalled by the JPEG 2000 data. A PDF image dictionary's
/// `/ColorSpace` overrides it when present (ISO 32000-1 7.4.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpxColorSpace {
    Gray,
    /// sRGB, or sYCC / e-sRGB / CIELab already converted to RGB.
    Rgb,
    Cmyk,
    /// An ICC-profile-based space with the given channel count.
    Icc {
        profile: Vec<u8>,
        channels: u8,
    },
    /// No usable colour specification; the channel count is all that is known.
    Unknown {
        channels: u8,
    },
}

impl JpxColorSpace {
    /// Colour channels, alpha excluded.
    pub fn channels(&self) -> u8 {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
            Self::Icc { channels, .. } | Self::Unknown { channels } => *channels,
        }
    }
}

/// Header information of JPEG 2000 data.
#[derive(Debug, Clone, PartialEq)]
pub struct JpxInfo {
    pub width: u32,
    pub height: u32,
    /// Channels in the decoded output, alpha included; palette indices are
    /// resolved so a palettised image reports its palette's channel count.
    pub channels: u8,
    /// The last channel is opacity (a JP2 `cdef` box says so, or a raw
    /// codestream has one channel more than its colour space needs).
    pub has_alpha: bool,
    pub color_space: JpxColorSpace,
    /// Bit depth of the first component as stored; decoded samples are
    /// always scaled to 8 bits.
    pub bit_depth: u8,
    /// Capture resolution from a JP2 `res`/`resc` box, in dots per inch.
    pub dpi: Option<(f64, f64)>,
}

/// Output of [`decode_jpx`].
#[derive(Debug, Clone, PartialEq)]
pub struct JpxImage {
    pub info: JpxInfo,
    /// Interleaved 8-bit samples, `width * height * channels` bytes, alpha
    /// last when `info.has_alpha`.
    pub data: Vec<u8>,
}

fn settings() -> DecodeSettings {
    DecodeSettings {
        resolve_palette_indices: true,
        strict: false,
        target_resolution: None,
    }
}

fn map_error(err: hayro_jpeg2000::DecodeError) -> CodecError {
    CodecError::malformed(CODEC, format!("{err:?}"))
}

fn info_from(image: &Image<'_>, data: &[u8]) -> Result<JpxInfo> {
    let width = image.width();
    let height = image.height();
    check_dimensions(CODEC, width, height)?;
    let color_space = match image.color_space() {
        ColorSpace::Gray => JpxColorSpace::Gray,
        ColorSpace::RGB => JpxColorSpace::Rgb,
        ColorSpace::CMYK => JpxColorSpace::Cmyk,
        ColorSpace::Icc {
            profile,
            num_channels,
        } => JpxColorSpace::Icc {
            profile: profile.clone(),
            channels: *num_channels,
        },
        ColorSpace::Unknown { num_channels } => JpxColorSpace::Unknown {
            channels: *num_channels,
        },
    };
    let has_alpha = image.has_alpha();
    let channels = color_space.channels() + u8::from(has_alpha);
    Ok(JpxInfo {
        width,
        height,
        channels,
        has_alpha,
        color_space,
        bit_depth: image.original_bit_depth(),
        dpi: capture_resolution(data),
    })
}

/// Reads the headers only.
pub fn jpx_info(data: &[u8]) -> Result<JpxInfo> {
    let image = Image::new(data, &settings()).map_err(map_error)?;
    info_from(&image, data)
}

/// Decodes a JP2 file or a raw J2K codestream to interleaved 8-bit samples.
pub fn decode_jpx(data: &[u8]) -> Result<JpxImage> {
    let image = Image::new(data, &settings()).map_err(map_error)?;
    let mut info = info_from(&image, data)?;
    let mut context = DecoderContext::default();
    let decoded = image.decode(&mut context).map_err(map_error)?;
    let components = decoded.components();
    if components.is_empty() {
        return Err(CodecError::malformed(CODEC, "no components decoded"));
    }
    let pixels = info.width as usize * info.height as usize;
    if components.iter().any(|c| c.samples().len() != pixels) {
        return Err(CodecError::unsupported(
            CODEC,
            "components with differing sample counts",
        ));
    }
    let samples = decoded.data_u8();
    let channels = components.len();
    if samples.len() != pixels * channels {
        return Err(CodecError::malformed(
            CODEC,
            "decoded sample count mismatch",
        ));
    }
    info.channels = u8::try_from(channels)
        .map_err(|_| CodecError::unsupported(CODEC, format!("{channels} channels")))?;
    Ok(JpxImage {
        info,
        data: samples,
    })
}

/// True when the bytes start with a JP2 signature box or a J2K SOC marker.
pub fn is_jpx(data: &[u8]) -> bool {
    data.starts_with(b"\x00\x00\x00\x0CjP  ") || data.starts_with(b"\xFF\x4F\xFF\x51")
}

/// Walks the JP2 box structure for `jp2h/res /resc` and converts the
/// capture resolution (dots per metre) to dots per inch, as Pillow does.
fn capture_resolution(data: &[u8]) -> Option<(f64, f64)> {
    if !data.starts_with(b"\x00\x00\x00\x0CjP  ") {
        return None;
    }
    let jp2h = find_box(data, b"jp2h")?;
    let res = find_box(jp2h, b"res ")?;
    let resc = find_box(res, b"resc")?;
    if resc.len() < 10 {
        return None;
    }
    let field = |i: usize| u32::from(u16::from_be_bytes([resc[i], resc[i + 1]]));
    let (vrn, vrd, hrn, hrd) = (field(0), field(2), field(4), field(6));
    let (vre, hre) = (resc[8] as i8, resc[9] as i8);
    if vrd == 0 || hrd == 0 {
        return None;
    }
    let to_dpi = |n: u32, d: u32, e: i8| {
        254.0 * f64::from(n) * 10f64.powi(i32::from(e)) / (10000.0 * f64::from(d))
    };
    Some((to_dpi(hrn, hrd, hre), to_dpi(vrn, vrd, vre)))
}

/// Returns the payload of the first box of the given type at this level.
fn find_box<'a>(mut data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    while data.len() >= 8 {
        let len32 = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let typ = &data[4..8];
        let (header, len) = match len32 {
            0 => (8usize, data.len()),
            1 => {
                let b = data.get(8..16)?;
                let xl = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                (16, usize::try_from(xl).ok()?)
            }
            n => (8, n as usize),
        };
        if len < header || len > data.len() {
            return None;
        }
        if typ == kind {
            return Some(&data[header..len]);
        }
        data = &data[len..];
    }
    None
}
