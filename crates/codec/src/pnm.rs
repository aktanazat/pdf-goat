//! Netpbm decoding: PBM/PGM/PPM in ASCII (P1–P3) and binary (P4–P6) forms
//! and PAM (P7) with GRAYSCALE, GRAYSCALE_ALPHA, RGB, RGB_ALPHA tuple types.

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::{DecodedImage, PixelLayout, row_stride};

const CODEC: &str = "pnm";

pub(crate) fn is_pnm(data: &[u8]) -> bool {
    data.len() >= 3
        && data[0] == b'P'
        && (b'1'..=b'7').contains(&data[1])
        && data[2].is_ascii_whitespace()
}

struct Header {
    kind: u8,
    width: u32,
    height: u32,
    maxval: u32,
    channels: usize,
    body: usize,
}

fn skip_space_and_comments(data: &[u8], mut pos: usize) -> usize {
    while pos < data.len() {
        match data[pos] {
            b'#' => {
                while pos < data.len() && data[pos] != b'\n' && data[pos] != b'\r' {
                    pos += 1;
                }
            }
            b if b.is_ascii_whitespace() => pos += 1,
            _ => break,
        }
    }
    pos
}

fn read_number(data: &[u8], pos: usize) -> Result<(u32, usize)> {
    let start = skip_space_and_comments(data, pos);
    let mut end = start;
    let mut value: u64 = 0;
    while end < data.len() && data[end].is_ascii_digit() {
        value = value * 10 + u64::from(data[end] - b'0');
        if value > u64::from(u32::MAX) {
            return Err(CodecError::malformed(CODEC, "header number too large"));
        }
        end += 1;
    }
    if end == start {
        return Err(CodecError::malformed(
            CODEC,
            "expected a number in the header",
        ));
    }
    Ok((value as u32, end))
}

fn parse_header(data: &[u8]) -> Result<Header> {
    if !is_pnm(data) {
        return Err(CodecError::malformed(CODEC, "missing P1–P7 magic"));
    }
    let kind = data[1] - b'0';
    if kind == 7 {
        return parse_pam_header(data);
    }
    let (width, pos) = read_number(data, 2)?;
    let (height, pos) = read_number(data, pos)?;
    let (maxval, pos) = if kind == 1 || kind == 4 {
        (1, pos)
    } else {
        read_number(data, pos)?
    };
    if maxval == 0 || maxval > 65535 {
        return Err(CodecError::malformed(CODEC, format!("maxval {maxval}")));
    }
    // Exactly one whitespace byte separates the header from binary data.
    let body = if kind >= 4 { pos + 1 } else { pos };
    if body > data.len() {
        return Err(CodecError::malformed(CODEC, "header runs past end of data"));
    }
    let channels = if kind == 3 || kind == 6 { 3 } else { 1 };
    Ok(Header {
        kind,
        width,
        height,
        maxval,
        channels,
        body,
    })
}

fn parse_pam_header(data: &[u8]) -> Result<Header> {
    let mut pos = 2usize;
    let (mut width, mut height, mut depth, mut maxval) = (None, None, None, None);
    let mut tuple_type = String::new();
    loop {
        pos = skip_space_and_comments(data, pos);
        let line_end = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(data.len());
        let line = String::from_utf8_lossy(&data[pos..line_end]);
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        pos = line_end + 1;
        match key {
            "ENDHDR" => break,
            "WIDTH" => width = value.parse::<u32>().ok(),
            "HEIGHT" => height = value.parse::<u32>().ok(),
            "DEPTH" => depth = value.parse::<usize>().ok(),
            "MAXVAL" => maxval = value.parse::<u32>().ok(),
            "TUPLTYPE" => tuple_type = value.to_string(),
            _ => {
                return Err(CodecError::malformed(
                    CODEC,
                    format!("PAM header field {key:?}"),
                ));
            }
        }
        if pos >= data.len() {
            return Err(CodecError::malformed(CODEC, "PAM header without ENDHDR"));
        }
    }
    let (Some(width), Some(height), Some(depth), Some(maxval)) = (width, height, depth, maxval)
    else {
        return Err(CodecError::malformed(CODEC, "incomplete PAM header"));
    };
    if maxval == 0 || maxval > 65535 {
        return Err(CodecError::malformed(CODEC, format!("maxval {maxval}")));
    }
    let channels = match (depth, tuple_type.as_str()) {
        (1, "" | "GRAYSCALE" | "BLACKANDWHITE") => 1,
        (2, "" | "GRAYSCALE_ALPHA" | "BLACKANDWHITE_ALPHA") => 2,
        (3, "" | "RGB") => 3,
        (4, "" | "RGB_ALPHA") => 4,
        _ => {
            return Err(CodecError::unsupported(
                CODEC,
                format!("PAM depth {depth} tuple type {tuple_type:?}"),
            ));
        }
    };
    Ok(Header {
        kind: 7,
        width,
        height,
        maxval,
        channels,
        body: pos,
    })
}

/// Decodes to 1-bit gray (PBM, 1 = white), 8-bit gray/RGB, or 16-bit
/// big-endian samples when maxval exceeds 255; maxval below 255 is scaled
/// up to full range.
pub(crate) fn decode_pnm(data: &[u8]) -> Result<DecodedImage> {
    let h = parse_header(data)?;
    check_dimensions(CODEC, h.width, h.height)?;
    let (w, ht) = (h.width as usize, h.height as usize);
    let layout = match h.channels {
        1 => PixelLayout::Gray,
        2 => PixelLayout::GrayAlpha,
        3 => PixelLayout::Rgb,
        _ => PixelLayout::Rgba,
    };
    let mut image = DecodedImage {
        width: h.width,
        height: h.height,
        layout,
        bit_depth: 8,
        data: Vec::new(),
        palette: None,
        icc_profile: None,
        dpi: None,
    };
    match h.kind {
        1 => {
            let stride = row_stride(h.width, 1, 1);
            let mut out = vec![0u8; stride * ht];
            let mut pos = h.body;
            for y in 0..ht {
                for x in 0..w {
                    pos = skip_space_and_comments(data, pos);
                    let bit = match data.get(pos) {
                        Some(b'0') => 1u8,
                        Some(b'1') => 0u8,
                        _ => return Err(CodecError::malformed(CODEC, "PBM ASCII data ends early")),
                    };
                    pos += 1;
                    out[y * stride + x / 8] |= bit << (7 - x % 8);
                }
            }
            image.bit_depth = 1;
            image.data = out;
        }
        4 => {
            let stride = row_stride(h.width, 1, 1);
            let raw = data
                .get(h.body..h.body + stride * ht)
                .ok_or_else(|| CodecError::malformed(CODEC, "PBM data ends early"))?;
            image.bit_depth = 1;
            let mut out: Vec<u8> = raw.iter().map(|b| !b).collect();
            if w % 8 != 0 {
                let keep = 0xFFu8 << (8 - w % 8);
                for row in out.chunks_exact_mut(stride) {
                    row[stride - 1] &= keep;
                }
            }
            image.data = out;
        }
        2 | 3 => {
            let n = w * ht * h.channels;
            let mut pos = h.body;
            let sixteen = h.maxval > 255;
            let mut out = Vec::with_capacity(if sixteen { n * 2 } else { n });
            for _ in 0..n {
                let (v, next) = read_number(data, pos)
                    .map_err(|_| CodecError::malformed(CODEC, "ASCII sample data ends early"))?;
                pos = next;
                push_sample(&mut out, v.min(h.maxval), h.maxval, sixteen);
            }
            image.bit_depth = if sixteen { 16 } else { 8 };
            image.data = out;
        }
        _ => {
            let n = w * ht * h.channels;
            let sixteen = h.maxval > 255;
            let bytes = if sixteen { 2 } else { 1 };
            let raw = data
                .get(h.body..h.body + n * bytes)
                .ok_or_else(|| CodecError::malformed(CODEC, "binary sample data ends early"))?;
            image.bit_depth = if sixteen { 16 } else { 8 };
            if h.maxval == 255 || h.maxval == 65535 {
                image.data = raw.to_vec();
            } else {
                let mut out = Vec::with_capacity(raw.len());
                for s in raw.chunks_exact(bytes) {
                    let v = if sixteen {
                        u32::from(u16::from_be_bytes([s[0], s[1]]))
                    } else {
                        u32::from(s[0])
                    };
                    push_sample(&mut out, v.min(h.maxval), h.maxval, sixteen);
                }
                image.data = out;
            }
        }
    }
    Ok(image)
}

fn push_sample(out: &mut Vec<u8>, v: u32, maxval: u32, sixteen: bool) {
    if sixteen {
        let s = ((u64::from(v) * 65535 + u64::from(maxval) / 2) / u64::from(maxval)) as u16;
        out.extend_from_slice(&s.to_be_bytes());
    } else {
        out.push(((v * 255 + maxval / 2) / maxval) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_binary_ppm_decode_to_the_same_rgb() {
        let ascii = b"P3\n# comment\n2 2\n255\n255 0 0  0 255 0\n0 0 255  10 20 30\n";
        let binary = b"P6 2 2 255\n\xFF\x00\x00\x00\xFF\x00\x00\x00\xFF\x0A\x14\x1E";
        let a = decode_pnm(ascii).unwrap();
        let b = decode_pnm(binary).unwrap();
        assert_eq!((a.layout, a.bit_depth), (PixelLayout::Rgb, 8));
        assert_eq!(a.data, b.data);
        assert_eq!(a.data, vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 20, 30]);
    }

    #[test]
    fn pbm_bits_are_inverted_to_one_is_white() {
        // P4: 1 bits are black in PBM; the decoder reports 1 = white.
        let binary = b"P4\n10 1\n\xA5\x40";
        let img = decode_pnm(binary).unwrap();
        assert_eq!((img.layout, img.bit_depth), (PixelLayout::Gray, 1));
        assert_eq!(img.data, vec![0x5A, 0x80]);
        let ascii = b"P1\n10 1\n1 0 1 0 0 1 0 1 0 1\n";
        assert_eq!(decode_pnm(ascii).unwrap().data, vec![0x5A, 0x80]);
    }

    #[test]
    fn small_maxval_scales_to_full_range() {
        let pgm = b"P5 3 1 3\n\x00\x01\x03";
        assert_eq!(decode_pnm(pgm).unwrap().data, vec![0, 85, 255]);
    }

    #[test]
    fn sixteen_bit_pgm_keeps_big_endian_samples() {
        let pgm = b"P5 2 1 65535\n\x12\x34\xAB\xCD";
        let img = decode_pnm(pgm).unwrap();
        assert_eq!(img.bit_depth, 16);
        assert_eq!(img.data, vec![0x12, 0x34, 0xAB, 0xCD]);
    }

    #[test]
    fn pam_rgba_decodes() {
        let pam = b"P7\nWIDTH 1\nHEIGHT 2\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n\x01\x02\x03\x04\x05\x06\x07\x08";
        let img = decode_pnm(pam).unwrap();
        assert_eq!(img.layout, PixelLayout::Rgba);
        assert_eq!(img.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn short_data_is_an_error() {
        assert!(decode_pnm(b"P6 4 4 255\n\x00\x00").is_err());
        assert!(decode_pnm(b"P3 2 1 255\n1 2 3 4").is_err());
        assert!(decode_pnm(b"P5 0 1 255\n").is_err());
    }
}
