//! JBIG2Decode: embedded PDF streams (plus `/JBIG2Globals`) and standalone
//! `.jb2` files through the locally bounded hayro-jbig2 decoder.

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::BilevelImage;

const CODEC: &str = "jbig2";

/// Collects the decoder's pixels into packed rows with 1 = white, the
/// convention of the JBIG2Decode filter output (ISO 32000-1 7.4.7: JBIG2
/// encodes 1 for black, the filter delivers the image with 0 for black so it
/// draws correctly as 1-bit DeviceGray).
struct Sink {
    width: u32,
    stride: usize,
    row: usize,
    x: u32,
    rows: usize,
    data: Vec<u8>,
}

impl pdf_codec_jbig2::Decoder for Sink {
    fn push_pixel(&mut self, black: bool) {
        if self.row >= self.rows || self.x >= self.width {
            return;
        }
        if !black {
            let i = self.row * self.stride + (self.x / 8) as usize;
            self.data[i] |= 0x80 >> (self.x % 8);
        }
        self.x += 1;
    }

    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        if self.row >= self.rows {
            return;
        }
        let start = (self.x / 8) as usize;
        let count = (chunk_count as usize).min(self.stride.saturating_sub(start));
        if !black {
            let base = self.row * self.stride;
            self.data[base + start..base + start + count].fill(0xFF);
        }
        self.x = self
            .x
            .saturating_add(chunk_count.saturating_mul(8))
            .min(self.width);
    }

    fn next_line(&mut self) {
        self.row += 1;
        self.x = 0;
    }
}

fn map_error(err: pdf_codec_jbig2::DecodeError) -> CodecError {
    use pdf_codec_jbig2::{DecodeError, OverflowError};
    match err {
        DecodeError::Overflow(OverflowError::PixelBudget | OverflowError::DecodeBudget) => {
            CodecError::limit(CODEC, err.to_string())
        }
        _ => CodecError::malformed(CODEC, format!("{err:?}")),
    }
}

fn run(image: &pdf_codec_jbig2::Image<'_>) -> Result<BilevelImage> {
    let (width, height) = (image.width(), image.height());
    check_dimensions(CODEC, width, height)?;
    let stride = (width as usize).div_ceil(8);
    let mut sink = Sink {
        width,
        stride,
        row: 0,
        x: 0,
        rows: height as usize,
        data: vec![0u8; stride * height as usize],
    };
    image
        .decode(&mut sink, crate::error::MAX_PIXELS)
        .map_err(map_error)?;
    let mut out = BilevelImage {
        width,
        height,
        data: sink.data,
    };
    out.clear_padding();
    Ok(out)
}

/// Decodes the embedded-stream organisation used by PDF (segment headers
/// without a file header), with the optional `/JBIG2Globals` stream first.
/// Generic (arithmetic and MMR), refinement, symbol dictionary and text
/// (arithmetic and Huffman), pattern and halftone regions, end-of-stripe,
/// and unknown-length regions are all handled by the decoder.
pub fn decode_jbig2(data: &[u8], globals: Option<&[u8]>) -> Result<BilevelImage> {
    let image = pdf_codec_jbig2::Image::new_embedded(data, globals).map_err(map_error)?;
    run(&image)
}

/// Decodes a standalone JBIG2 file (sequential or random-access
/// organisation with the `97 4A 42 32 0D 0A 1A 0A` file header).
pub fn decode_jbig2_file(data: &[u8]) -> Result<BilevelImage> {
    let image = pdf_codec_jbig2::Image::new(data).map_err(map_error)?;
    run(&image)
}

/// True when the bytes start with the JBIG2 file header signature.
pub fn is_jbig2_file(data: &[u8]) -> bool {
    data.starts_with(b"\x97JB2\r\n\x1a\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccitt::encode_ccitt_g4;
    use crate::error::CodecError;

    /// An embedded JBIG2 stream (PDF form): a page info segment and one
    /// immediate lossless generic region coded with MMR (T.6), whose
    /// 1 bits are black.
    fn embedded_mmr_stream(bits_black: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mmr = encode_ccitt_g4(bits_black, w, h, true).unwrap();
        let mut out = Vec::new();
        let segment = |out: &mut Vec<u8>, number: u32, kind: u8, body: &[u8]| {
            out.extend_from_slice(&number.to_be_bytes());
            out.push(kind);
            out.push(0); // no referred-to segments
            out.push(1); // page 1
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(body);
        };
        let mut page = Vec::new();
        page.extend_from_slice(&w.to_be_bytes());
        page.extend_from_slice(&h.to_be_bytes());
        page.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // resolution unknown
        page.push(0); // flags: default pixel 0, combination OR
        page.extend_from_slice(&[0, 0]); // not striped
        segment(&mut out, 0, 48, &page);
        let mut region = Vec::new();
        region.extend_from_slice(&w.to_be_bytes());
        region.extend_from_slice(&h.to_be_bytes());
        region.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // x, y
        region.push(0); // combination OR
        region.push(1); // MMR = 1, template 0, no TPGDON
        region.extend_from_slice(&mmr);
        segment(&mut out, 1, 38, &region);
        out
    }

    #[test]
    fn generic_region_mmr_known_answer_is_one_for_white() {
        let (w, h) = (37u32, 9u32);
        let stride = (w as usize).div_ceil(8);
        let mut black = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in (y..w as usize).step_by(5) {
                black[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
        black[3 * stride..4 * stride].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xF8]);
        let stream = embedded_mmr_stream(&black, w, h);
        let img = decode_jbig2(&stream, None).unwrap();
        assert_eq!((img.width, img.height), (w, h));
        let mut expected = BilevelImage {
            width: w,
            height: h,
            data: black.iter().map(|b| !b).collect(),
        };
        expected.clear_padding();
        assert_eq!(img.data, expected.data);
    }

    #[test]
    fn region_larger_than_page_is_clipped_to_the_page() {
        let mut stream = embedded_mmr_stream(&[0xAA, 0x55].repeat(16), 16, 16);
        // The page-info body starts after the eleven-byte segment header.
        stream[11..15].copy_from_slice(&8u32.to_be_bytes());
        stream[15..19].copy_from_slice(&8u32.to_be_bytes());
        let image = decode_jbig2(&stream, None).unwrap();
        assert_eq!((image.width, image.height), (8, 8));
        assert_eq!(image.data, [0x55; 8]);
    }

    #[test]
    fn oversized_intermediate_region_is_rejected_before_allocating_it() {
        let mut stream = embedded_mmr_stream(&[0xFF; 32], 16, 16);
        // Keep a small page but declare a region requiring nearly four billion pixels.
        stream[41..45].copy_from_slice(&65535u32.to_be_bytes());
        stream[45..49].copy_from_slice(&65535u32.to_be_bytes());
        assert!(matches!(
            decode_jbig2(&stream, None),
            Err(CodecError::Limit { .. })
        ));
        let valid = embedded_mmr_stream(&[0x0F; 10], 16, 5);
        assert_eq!(decode_jbig2(&valid, None).unwrap().data, [0xF0; 10]);
    }

    #[test]
    fn truncated_region_is_an_error_not_a_panic() {
        let stream = embedded_mmr_stream(&[0x0F; 10], 16, 5);
        let cut = &stream[..stream.len() - 12];
        assert!(matches!(
            decode_jbig2(cut, None),
            Err(CodecError::Malformed { .. })
        ));
        assert!(matches!(
            decode_jbig2_file(b"\x97JB2\r\n\x1a\n\x01"),
            Err(CodecError::Malformed { .. })
        ));
    }
}
