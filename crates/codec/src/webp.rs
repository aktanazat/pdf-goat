//! WebP (lossy, lossless, extended; first frame of animations) through
//! `image-webp`.

use std::io::Cursor;

use image_webp::WebPDecoder;

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::{DecodedImage, PixelLayout};

const CODEC: &str = "webp";

pub(crate) fn is_webp(data: &[u8]) -> bool {
    data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

fn map(err: image_webp::DecodingError) -> CodecError {
    CodecError::malformed(CODEC, err.to_string())
}

/// Decodes to 8-bit RGB, or RGBA when the file carries an alpha channel.
pub(crate) fn decode_webp(data: &[u8]) -> Result<DecodedImage> {
    let mut decoder = WebPDecoder::new(Cursor::new(data)).map_err(map)?;
    let (width, height) = decoder.dimensions();
    check_dimensions(CODEC, width, height)?;
    let has_alpha = decoder.has_alpha();
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| CodecError::limit(CODEC, "output buffer size overflow"))?;
    let mut buf = vec![0u8; size];
    decoder.read_image(&mut buf).map_err(map)?;
    let icc_profile = decoder.icc_profile().unwrap_or(None);
    Ok(DecodedImage {
        width,
        height,
        layout: if has_alpha {
            PixelLayout::Rgba
        } else {
            PixelLayout::Rgb
        },
        bit_depth: 8,
        data: buf,
        palette: None,
        icc_profile,
        dpi: None,
    })
}
