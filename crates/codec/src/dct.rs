//! DCTDecode: baseline and progressive JPEG through `jpeg-decoder`, with the
//! PDF colour-transform rules and a marker scanner for metadata.

use jpeg_decoder::{ColorTransform, PixelFormat};

use crate::error::{CodecError, Result, check_dimensions};
use crate::ifd;

const CODEC: &str = "dct";

/// Largest decoded sample buffer accepted from `jpeg-decoder`.
const MAX_DECODE_BYTES: usize = (crate::error::MAX_PIXELS as usize) * 4;

/// `/DecodeParms` of a `/DCTDecode` filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DctParams {
    /// `/ColorTransform`: 0 = none, 1 = YCbCr→RGB (3 components) or
    /// YCCK→CMYK (4 components). Ignored when the data carries an Adobe
    /// APP14 marker, whose transform flag wins (ISO 32000-1 Table 13).
    pub color_transform: Option<u32>,
}

/// Metadata read from JPEG marker segments without decoding scans.
#[derive(Debug, Clone, PartialEq)]
pub struct JpegInfo {
    pub width: u32,
    pub height: u32,
    /// 1 (gray), 3 (YCbCr/RGB), or 4 (CMYK/YCCK).
    pub components: u8,
    /// Sample precision in bits (8 for every PDF-usable JPEG; 12 exists).
    pub precision: u8,
    /// Progressive DCT (SOF2/6/10/14) rather than baseline/sequential.
    pub progressive: bool,
    /// Lossless or hierarchical process, which `DCTDecode` does not allow.
    pub unsupported_process: bool,
    /// The transform flag of an Adobe APP14 segment, when present.
    pub adobe_transform: Option<u8>,
    /// A JFIF APP0 segment is present.
    pub jfif: bool,
    /// Component identifiers from the frame header.
    pub component_ids: Vec<u8>,
    /// Resolution as Pillow reports it: JFIF density when its unit is
    /// inches or centimetres, else the EXIF X resolution for both axes
    /// (72 when EXIF exists without one), else `None`.
    pub dpi: Option<(f64, f64)>,
    /// ICC profile reassembled from APP2 chunks.
    pub icc_profile: Option<Vec<u8>>,
    /// EXIF orientation tag (1..=8) when present.
    pub exif_orientation: Option<u16>,
}

impl JpegInfo {
    /// True when four-component data carries an Adobe marker, the
    /// "Adobe inverted CMYK" convention: sample 255 = no ink, so a PDF
    /// embedding the data as-is needs `/Decode [1 0 1 0 1 0 1 0]`, which is
    /// what img2pdf writes.
    pub fn adobe_inverted_cmyk(&self) -> bool {
        self.components == 4 && self.adobe_transform.is_some()
    }
}

/// Output of [`decode_dct`].
#[derive(Debug, Clone, PartialEq)]
pub struct DctImage {
    pub width: u32,
    pub height: u32,
    /// 1, 3, or 4.
    pub components: u8,
    /// Interleaved 8-bit samples, `width * height * components` bytes, in
    /// the `DCTDecode` filter convention: gray, RGB after the YCbCr
    /// transform, or the CMYK samples as stored in the file (after the
    /// YCCK transform when signalled), with no inversion applied. Adobe
    /// files store CMYK inverted; see [`JpegInfo::adobe_inverted_cmyk`].
    pub data: Vec<u8>,
    pub info: JpegInfo,
}

/// Scans the marker segments up to the first scan.
pub fn jpeg_info(data: &[u8]) -> Result<JpegInfo> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(CodecError::malformed(CODEC, "missing SOI marker"));
    }
    let mut info = JpegInfo {
        width: 0,
        height: 0,
        components: 0,
        precision: 0,
        progressive: false,
        unsupported_process: false,
        adobe_transform: None,
        jfif: false,
        component_ids: Vec::new(),
        dpi: None,
        icc_profile: None,
        exif_orientation: None,
    };
    let mut icc_chunks: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut exif_dpi: Option<(f64, f64)> = None;
    let mut have_exif = false;
    let mut found_sof = false;
    let mut pos = 2usize;
    loop {
        // Skip to the next 0xFF, then over fill bytes.
        while pos < data.len() && data[pos] != 0xFF {
            pos += 1;
        }
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        let Some(&marker) = data.get(pos) else { break };
        pos += 1;
        match marker {
            0x00 | 0x01 | 0xD0..=0xD7 => continue,
            0xD8 => continue,
            0xD9 | 0xDA => break,
            _ => {}
        }
        let len = data
            .get(pos..pos + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated marker segment"))?;
        if len < 2 {
            return Err(CodecError::malformed(
                CODEC,
                "marker segment length below 2",
            ));
        }
        let seg = data
            .get(pos + 2..pos + len)
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated marker segment"))?;
        pos += len;
        match marker {
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if found_sof {
                    return Err(CodecError::malformed(CODEC, "second frame header"));
                }
                found_sof = true;
                if seg.len() < 6 {
                    return Err(CodecError::malformed(CODEC, "short frame header"));
                }
                info.precision = seg[0];
                info.height = u32::from(u16::from_be_bytes([seg[1], seg[2]]));
                info.width = u32::from(u16::from_be_bytes([seg[3], seg[4]]));
                info.components = seg[5];
                info.progressive = matches!(marker, 0xC2 | 0xC6 | 0xCA | 0xCE);
                info.unsupported_process =
                    matches!(marker, 0xC3 | 0xC5..=0xC7 | 0xC9 | 0xCB | 0xCD..=0xCF);
                let n = usize::from(seg[5]);
                if seg.len() < 6 + n * 3 {
                    return Err(CodecError::malformed(CODEC, "short frame header"));
                }
                info.component_ids = (0..n).map(|i| seg[6 + i * 3]).collect();
            }
            0xE0 => {
                if seg.len() >= 14 && &seg[..5] == b"JFIF\0" {
                    info.jfif = true;
                    let unit = seg[7];
                    let x = f64::from(u16::from_be_bytes([seg[8], seg[9]]));
                    let y = f64::from(u16::from_be_bytes([seg[10], seg[11]]));
                    match unit {
                        1 => info.dpi = Some((x, y)),
                        2 => info.dpi = Some((x * 2.54, y * 2.54)),
                        _ => {}
                    }
                }
            }
            0xE1 => {
                if seg.len() >= 6 && &seg[..6] == b"Exif\0\0" && !have_exif {
                    have_exif = true;
                    let (orientation, dpi) = parse_exif(&seg[6..]);
                    info.exif_orientation = orientation;
                    exif_dpi = dpi;
                }
            }
            0xE2 => {
                if seg.len() > 14 && &seg[..12] == b"ICC_PROFILE\0" {
                    icc_chunks.push((seg[12], seg[14..].to_vec()));
                }
            }
            0xEE if seg.len() >= 12 && &seg[..5] == b"Adobe" => {
                info.adobe_transform = Some(seg[11]);
            }
            _ => {}
        }
    }
    if !found_sof {
        return Err(CodecError::malformed(CODEC, "no frame header before scan"));
    }
    if info.dpi.is_none() && have_exif {
        info.dpi = Some(exif_dpi.unwrap_or((72.0, 72.0)));
    }
    if !icc_chunks.is_empty() {
        icc_chunks.sort_by_key(|(seq, _)| *seq);
        let mut profile = Vec::new();
        for (_, chunk) in icc_chunks {
            profile.extend_from_slice(&chunk);
        }
        info.icc_profile = Some(profile);
    }
    Ok(info)
}

/// Orientation and resolution from an EXIF TIFF structure, following
/// Pillow: the X resolution serves both axes, unit 3 means centimetres.
fn parse_exif(tiff: &[u8]) -> (Option<u16>, Option<(f64, f64)>) {
    let Ok((le, first)) = ifd::read_header(CODEC, tiff) else {
        return (None, None);
    };
    let Ok(ifd0) = ifd::read_ifd(CODEC, tiff, first, le) else {
        return (None, None);
    };
    let orientation = ifd0.int(tiff, 0x0112).and_then(|v| u16::try_from(v).ok());
    let dpi = ifd0.rational(tiff, 0x011A).map(|x| {
        let unit = ifd0.int(tiff, 0x0128);
        let scale = if unit == Some(3) { 2.54 } else { 1.0 };
        (x * scale, x * scale)
    });
    (orientation, dpi)
}

/// Decodes a JPEG (baseline, extended sequential, or progressive Huffman;
/// gray, YCbCr/RGB, CMYK/YCCK) to interleaved 8-bit samples.
pub fn decode_dct(data: &[u8], params: &DctParams) -> Result<DctImage> {
    let info = jpeg_info(data)?;
    if info.unsupported_process {
        return Err(CodecError::unsupported(
            CODEC,
            "lossless or hierarchical JPEG process",
        ));
    }
    if info.width == 0 || info.height == 0 {
        return Err(CodecError::malformed(
            CODEC,
            "zero image dimension (DNL not supported)",
        ));
    }
    check_dimensions(CODEC, info.width, info.height)?;

    // Transform selection per ISO 32000-1 Table 13: the Adobe flag wins,
    // then the DecodeParms value, then the default (YCbCr for three
    // components, none for four). Component ids 'R','G','B' mark untransformed
    // RGB, as libjpeg assumes.
    let want_transform = match info.adobe_transform {
        Some(flag) => flag != 0,
        None => match params.color_transform {
            Some(v) => v != 0,
            None => info.components == 3 && info.component_ids != *b"RGB",
        },
    };
    let ycck = info.components == 4 && want_transform;
    // jpeg-decoder's `ColorTransform::None` writes every upsampled line
    // buffer in full and panics on subsampled components, so the untransformed
    // cases go through its RGB (plain interleave) and CMYK (interleave and
    // invert) converters instead.
    let transform = match info.components {
        3 if want_transform => ColorTransform::YCbCr,
        3 => ColorTransform::RGB,
        4 if ycck => ColorTransform::YCCK,
        _ => ColorTransform::CMYK,
    };

    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.set_max_decoding_buffer_size(MAX_DECODE_BYTES);
    if info.components > 1 {
        decoder.set_color_transform(transform);
    }
    let mut pixels = decoder.decode().map_err(map_error)?;
    let decoded = decoder
        .info()
        .ok_or_else(|| CodecError::malformed(CODEC, "decoder reported no frame"))?;
    if decoded.pixel_format == PixelFormat::L16 {
        return Err(CodecError::unsupported(CODEC, "16-bit samples"));
    }
    let width = u32::from(decoded.width);
    let height = u32::from(decoded.height);
    if info.components == 4 {
        // Both four-component converters return 255 - x: YCCK comes back as
        // (R, G, B, 255 - K) where DCTDecode yields (255 - R, 255 - G, 255 - B, K),
        // and the CMYK converter inverts samples that must stay as stored.
        for b in &mut pixels {
            *b = 255 - *b;
        }
    }
    let expected = width as usize * height as usize * usize::from(info.components);
    if pixels.len() != expected {
        return Err(CodecError::malformed(
            CODEC,
            format!("decoded {} bytes, expected {expected}", pixels.len()),
        ));
    }
    Ok(DctImage {
        width,
        height,
        components: info.components,
        data: pixels,
        info,
    })
}

fn map_error(err: jpeg_decoder::Error) -> CodecError {
    match err {
        jpeg_decoder::Error::Unsupported(feature) => {
            CodecError::unsupported(CODEC, format!("{feature:?}"))
        }
        other => CodecError::malformed(CODEC, other.to_string()),
    }
}

/// True when the bytes start with a JPEG SOI marker.
pub fn is_jpeg(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpeg_enc::{JpegColor, encode_jpeg};

    /// Smooth gradients so quantisation error stays small.
    fn gradient(w: u32, h: u32, channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w as usize * h as usize * channels);
        for y in 0..h {
            for x in 0..w {
                for c in 0..channels as u32 {
                    out.push(((x * 3 + y * 2 + c * 40) % 200 + 20) as u8);
                }
            }
        }
        out
    }

    fn max_diff(a: &[u8], b: &[u8]) -> u8 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0)
    }

    macro_rules! round_trip {
        ($name:ident, $color:expr, $channels:expr, $tolerance:expr) => {
            #[test]
            fn $name() {
                let (w, h) = (33u32, 21u32);
                let src = gradient(w, h, $channels);
                let jpeg = encode_jpeg(&src, w, h, $color, 95, Some((150.0, 300.0))).unwrap();
                let img = decode_dct(&jpeg, &DctParams::default()).unwrap();
                assert_eq!(
                    (img.width, img.height, usize::from(img.components)),
                    (w, h, $channels)
                );
                assert_eq!(img.info.dpi, Some((150.0, 300.0)));
                // CMYK is stored inverted with an Adobe marker (img2pdf's `CMYK;I`).
                let data: Vec<u8> = if img.info.adobe_inverted_cmyk() {
                    img.data.iter().map(|b| !b).collect()
                } else {
                    img.data.clone()
                };
                assert_eq!(img.info.adobe_inverted_cmyk(), $channels == 4);
                assert!(
                    max_diff(&data, &src) <= $tolerance,
                    "max diff {}",
                    max_diff(&data, &src)
                );
            }
        };
    }
    round_trip!(gray_round_trip, JpegColor::Gray, 1, 4);
    // The encoder subsamples chroma 4:2:0, so colour edges shift.
    round_trip!(rgb_round_trip, JpegColor::Rgb, 3, 24);
    round_trip!(cmyk_round_trip, JpegColor::Cmyk, 4, 24);

    #[test]
    fn color_transform_zero_keeps_rgb_components_interleaved() {
        // RGB encoded as YCbCr, decoded without the transform: the raw
        // components come back pixel-interleaved, not planar.
        let src = gradient(9, 4, 3);
        let jpeg = encode_jpeg(&src, 9, 4, JpegColor::Rgb, 100, None).unwrap();
        let raw = decode_dct(
            &jpeg,
            &DctParams {
                color_transform: Some(0),
            },
        )
        .unwrap();
        let rgb = decode_dct(&jpeg, &DctParams::default()).unwrap();
        assert_ne!(raw.data, rgb.data);
        // Luma of YCbCr is component 0; the encoder's Y for the first pixel.
        let (r, g, b) = (f32::from(src[0]), f32::from(src[1]), f32::from(src[2]));
        let y = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8;
        assert!(
            raw.data[0].abs_diff(y) <= 3,
            "first raw sample {} vs Y {y}",
            raw.data[0]
        );
    }

    /// Drops the Adobe APP14 segment so the decoder sees a bare 4-component file.
    fn strip_app14(jpeg: &[u8]) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        let mut pos = 2;
        while pos + 4 <= jpeg.len() && jpeg[pos] == 0xFF && jpeg[pos + 1] != 0xDA {
            let end = pos + 2 + usize::from(u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]));
            if jpeg[pos + 1] != 0xEE {
                out.extend_from_slice(&jpeg[pos..end]);
            }
            pos = end;
        }
        out.extend_from_slice(&jpeg[pos..]);
        out
    }

    #[test]
    fn subsampled_untransformed_cmyk_decodes_with_and_without_adobe_marker() {
        // Below quality 90 the encoder codes K at 2x2; that layout made
        // jpeg-decoder's no-transform path overrun its output and panic.
        let (w, h) = (23u32, 10u32);
        // A monotonic ramp (no wrap-around edge) keeps subsampling error small.
        let cmyk: Vec<u8> = (0..h)
            .flat_map(|y| {
                (0..w)
                    .flat_map(move |x| (0..4u32).map(move |c| (30 + x * 4 + y * 3 + c * 20) as u8))
            })
            .collect();
        let jpeg = encode_jpeg(&cmyk, w, h, JpegColor::Cmyk, 80, None).unwrap();
        let bare = strip_app14(&jpeg);
        assert_eq!(bare.len(), jpeg.len() - 16);
        for (label, data, params, adobe) in [
            ("adobe", &jpeg, DctParams::default(), true),
            ("bare", &bare, DctParams::default(), false),
            (
                "bare/0",
                &bare,
                DctParams {
                    color_transform: Some(0),
                },
                false,
            ),
        ] {
            let img = decode_dct(data, &params).unwrap();
            assert_eq!(img.info.adobe_inverted_cmyk(), adobe, "{label}");
            assert_eq!(
                (img.width, img.height, img.components),
                (w, h, 4),
                "{label}"
            );
            // The encoder stores CMYK inverted; samples come back as stored.
            let stored: Vec<u8> = img.data.iter().map(|b| !b).collect();
            let diff = max_diff(&stored, &cmyk);
            assert!(diff <= 24, "{label}: max diff {diff}");
        }
    }

    #[test]
    fn truncated_and_garbage_input_are_errors() {
        let src = gradient(16, 16, 1);
        let jpeg = encode_jpeg(&src, 16, 16, JpegColor::Gray, 80, None).unwrap();
        assert!(matches!(
            decode_dct(&jpeg[..jpeg.len() / 2], &DctParams::default()),
            Err(CodecError::Malformed { .. })
        ));
        assert!(matches!(
            decode_dct(b"\xFF\xD8\xFF\xE0garbage", &DctParams::default()),
            Err(CodecError::Malformed { .. })
        ));
        assert!(matches!(
            jpeg_info(b"not a jpeg"),
            Err(CodecError::Malformed { .. })
        ));
        assert!(matches!(
            crate::jpx::decode_jpx(b"\x00\x00\x00\x0cjP  \r\n\x87\nxx"),
            Err(CodecError::Malformed { .. })
        ));
    }
}
