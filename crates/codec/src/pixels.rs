//! Sample unpacking and colour helpers for the render, compare, and image
//! extraction verbs.

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::row_stride;

const CODEC: &str = "pixels";

/// Unpacks PDF-convention packed rows (`bpc` bits per sample, MSB first, rows
/// padded to a byte) into one integer per sample, row-major, interleaved.
/// `bpc` must be 1, 2, 4, 8, or 16.
pub fn unpack_samples(
    data: &[u8],
    width: u32,
    height: u32,
    bpc: u8,
    channels: usize,
) -> Result<Vec<u16>> {
    check_dimensions(CODEC, width, height)?;
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(CodecError::invalid(
            CODEC,
            format!("bits per component {bpc}"),
        ));
    }
    if channels == 0 || channels > 32 {
        return Err(CodecError::invalid(CODEC, format!("{channels} channels")));
    }
    let stride = row_stride(width, channels, bpc);
    let need = stride * height as usize;
    if data.len() < need {
        return Err(CodecError::malformed(
            CODEC,
            format!(
                "need {need} bytes for {width}x{height}x{channels}@{bpc}, got {}",
                data.len()
            ),
        ));
    }
    let per_row = width as usize * channels;
    let mut out = Vec::with_capacity(per_row * height as usize);
    for row in data[..need].chunks_exact(stride) {
        match bpc {
            8 => out.extend(row[..per_row].iter().map(|&b| u16::from(b))),
            16 => out.extend(
                row.as_chunks::<2>()
                    .0
                    .iter()
                    .take(per_row)
                    .map(|s| u16::from_be_bytes([s[0], s[1]])),
            ),
            _ => {
                let per_byte = 8 / usize::from(bpc);
                let mask = (1u16 << bpc) - 1;
                for i in 0..per_row {
                    let byte = row[i / per_byte];
                    let shift = 8 - bpc as usize * (i % per_byte + 1);
                    out.push((u16::from(byte) >> shift) & mask);
                }
            }
        }
    }
    Ok(out)
}

/// Maps raw samples to 8-bit values through a PDF `/Decode` array.
///
/// With `decode` `None` the default `[0 1]` per channel applies, so a sample
/// scales linearly to `0..=255`. Otherwise each channel `i` maps through
/// `d[2i] + sample * (d[2i+1] - d[2i]) / (2^bpc - 1)`, clamped to `0..=1`
/// and scaled to `0..=255`. A `decode` shorter than `2 * channels` falls
/// back to the default for the missing channels.
pub fn samples_to_u8(samples: &[u16], bpc: u8, channels: usize, decode: Option<&[f32]>) -> Vec<u8> {
    let max = f32::from((1u32 << bpc).saturating_sub(1).min(65535) as u16);
    let mut out = Vec::with_capacity(samples.len());
    let identity = decode.is_none_or(|d| d.len() < 2 * channels || is_identity_decode(d));
    if identity {
        if bpc == 8 {
            out.extend(samples.iter().map(|&s| s as u8));
        } else if bpc == 16 {
            out.extend(samples.iter().map(|&s| (s >> 8) as u8));
        } else {
            out.extend(
                samples
                    .iter()
                    .map(|&s| (u32::from(s) * 255 / u32::from(max as u16)) as u8),
            );
        }
        return out;
    }
    let d = decode.unwrap_or(&[]);
    for (i, &s) in samples.iter().enumerate() {
        let ch = i % channels;
        let (dmin, dmax) = if d.len() >= 2 * (ch + 1) {
            (d[2 * ch], d[2 * ch + 1])
        } else {
            (0.0, 1.0)
        };
        let v = dmin + f32::from(s) * (dmax - dmin) / max;
        out.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
    }
    out
}

fn is_identity_decode(d: &[f32]) -> bool {
    d.as_chunks::<2>()
        .0
        .iter()
        .all(|p| p[0] == 0.0 && p[1] == 1.0)
}

/// [`unpack_samples`] followed by [`samples_to_u8`].
pub fn unpack_to_u8(
    data: &[u8],
    width: u32,
    height: u32,
    bpc: u8,
    channels: usize,
    decode: Option<&[f32]>,
) -> Result<Vec<u8>> {
    let samples = unpack_samples(data, width, height, bpc, channels)?;
    Ok(samples_to_u8(&samples, bpc, channels, decode))
}

/// Resolves `/Indexed` samples through a PDF `/Decode` array to palette
/// indices. The default decode for an indexed image is `[0 2^bpc-1]`, which
/// leaves the raw sample unchanged; `[2^bpc-1 0]` inverts it. Results are
/// clamped to `0..=hival`.
pub fn indexed_samples(samples: &[u16], bpc: u8, decode: Option<&[f32]>, hival: u16) -> Vec<u16> {
    let max = f32::from((1u32 << bpc).saturating_sub(1).min(65535) as u16);
    let (dmin, dmax) = match decode {
        Some(d) if d.len() >= 2 => (d[0], d[1]),
        _ => (0.0, max),
    };
    samples
        .iter()
        .map(|&s| {
            let v = dmin + f32::from(s) * (dmax - dmin) / max;
            (v.round().max(0.0) as u16).min(hival)
        })
        .collect()
}

/// Expands palette indices to `components` bytes per pixel from a palette of
/// consecutive `components`-byte entries. Out-of-range indices clamp to the
/// last entry, as ISO 32000-1 8.6.6.3 requires.
pub fn expand_palette(indices: &[u16], palette: &[u8], components: usize) -> Vec<u8> {
    let entries = palette.len().checked_div(components).unwrap_or(0);
    let mut out = Vec::with_capacity(indices.len() * components);
    if entries == 0 {
        out.resize(indices.len() * components, 0);
        return out;
    }
    for &i in indices {
        let idx = usize::from(i).min(entries - 1);
        out.extend_from_slice(&palette[idx * components..(idx + 1) * components]);
    }
    out
}

/// Big-endian 16-bit samples to 8-bit by keeping the high byte.
pub fn sixteen_to_eight(data: &[u8]) -> Vec<u8> {
    data.as_chunks::<2>().0.iter().map(|s| s[0]).collect()
}

/// One DeviceCMYK pixel (0 = no ink) to RGB with the additive formula of
/// ISO 32000-1 8.6.4.4: `R = 1 - min(1, c + k)` and so on. Acrobat uses an
/// ICC-based conversion (U.S. Web Coated SWOP), so exact values differ from
/// Acrobat's; this is the formula pdf.js and MuPDF use without colour management.
pub fn cmyk_to_rgb_pixel(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let k = u16::from(k);
    let conv = |v: u8| 255 - (u16::from(v) + k).min(255) as u8;
    [conv(c), conv(m), conv(y)]
}

/// Interleaved DeviceCMYK to interleaved RGB (see [`cmyk_to_rgb_pixel`]).
pub fn cmyk_to_rgb(cmyk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cmyk.len() / 4 * 3);
    for px in cmyk.as_chunks::<4>().0.iter() {
        out.extend_from_slice(&cmyk_to_rgb_pixel(px[0], px[1], px[2], px[3]));
    }
    out
}

/// Rec. 601 luma of an RGB pixel.
pub fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) + 500) / 1000) as u8
}

/// Gray to RGB by replication.
pub fn gray_to_rgb(gray: &[u8]) -> Vec<u8> {
    gray.iter().flat_map(|&g| [g, g, g]).collect()
}

/// RGB to luma gray.
pub fn rgb_to_gray(rgb: &[u8]) -> Vec<u8> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|px| luma(px[0], px[1], px[2]))
        .collect()
}

/// RGB plus an optional alpha plane to straight RGBA; a missing plane means
/// opaque.
pub fn rgb_to_rgba(rgb: &[u8], alpha: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
        let a = alpha.and_then(|a| a.get(i).copied()).unwrap_or(255);
        out.extend_from_slice(&[px[0], px[1], px[2], a]);
    }
    out
}

/// Straight RGBA to RGB by dropping alpha.
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect()
}

/// Straight RGBA composited onto white, as Pillow's paste onto a white
/// background does in `from-images`.
pub fn rgba_to_rgb_on_white(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.as_chunks::<4>().0.iter() {
        let a = u32::from(px[3]);
        for &c in &px[..3] {
            out.push(((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8);
        }
    }
    out
}

/// Straight RGBA to premultiplied RGBA (the rasterizer's pixmap convention).
pub fn premultiply_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0.iter() {
        let a = u32::from(px[3]);
        for &c in &px[..3] {
            out.push(((u32::from(c) * a + 127) / 255) as u8);
        }
        out.push(px[3]);
    }
    out
}

/// Premultiplied RGBA back to straight RGBA.
pub fn unpremultiply_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0.iter() {
        let a = u32::from(px[3]);
        for &c in &px[..3] {
            out.push(
                (u32::from(c) * 255 + a / 2)
                    .checked_div(a)
                    .map_or(0, |v| v.min(255) as u8),
            );
        }
        out.push(px[3]);
    }
    out
}

/// Packed 1-bit rows to one byte per pixel: with `one_is_black` a set bit
/// becomes 0 and a clear bit 255, otherwise the reverse.
pub fn bilevel_to_gray8(
    packed: &[u8],
    width: u32,
    height: u32,
    one_is_black: bool,
) -> Result<Vec<u8>> {
    let samples = unpack_samples(packed, width, height, 1, 1)?;
    let (zero, one) = if one_is_black { (255, 0) } else { (0, 255) };
    Ok(samples
        .into_iter()
        .map(|s| if s == 1 { one } else { zero })
        .collect())
}

/// Bilinear resize of interleaved 8-bit samples with `channels` per pixel.
pub fn resize_bilinear(
    src: &[u8],
    width: u32,
    height: u32,
    channels: usize,
    new_width: u32,
    new_height: u32,
) -> Result<Vec<u8>> {
    check_dimensions(CODEC, width, height)?;
    check_dimensions(CODEC, new_width, new_height)?;
    if channels == 0 || src.len() < width as usize * height as usize * channels {
        return Err(CodecError::invalid(CODEC, "source buffer too small"));
    }
    let (w, h) = (width as usize, height as usize);
    let (nw, nh) = (new_width as usize, new_height as usize);
    let mut out = vec![0u8; nw * nh * channels];
    let sx = w as f32 / nw as f32;
    let sy = h as f32 / nh as f32;
    for oy in 0..nh {
        let fy = ((oy as f32 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy as usize).min(h - 1);
        let y1 = (y0 + 1).min(h - 1);
        let wy = fy - y0 as f32;
        for ox in 0..nw {
            let fx = ((ox as f32 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx as usize).min(w - 1);
            let x1 = (x0 + 1).min(w - 1);
            let wx = fx - x0 as f32;
            for c in 0..channels {
                let p = |x: usize, y: usize| f32::from(src[(y * w + x) * channels + c]);
                let top = p(x0, y0) * (1.0 - wx) + p(x1, y0) * wx;
                let bottom = p(x0, y1) * (1.0 - wx) + p(x1, y1) * wx;
                out[(oy * nw + ox) * channels + c] = (top * (1.0 - wy) + bottom * wy + 0.5) as u8;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CodecError;

    #[test]
    fn sub_byte_samples_unpack_per_row_and_honour_decode() {
        // 2 bpc, 3 pixels per row: rows are padded to a byte.
        let data = [0b1101_0000, 0b0001_1100];
        assert_eq!(
            unpack_samples(&data, 3, 2, 2, 1).unwrap(),
            vec![3, 1, 0, 0, 1, 3]
        );
        assert_eq!(
            unpack_to_u8(&data, 3, 2, 2, 1, None).unwrap(),
            vec![255, 85, 0, 0, 85, 255]
        );
        assert_eq!(
            unpack_to_u8(&data, 3, 2, 2, 1, Some(&[1.0, 0.0])).unwrap(),
            vec![0, 170, 255, 255, 170, 0]
        );
        assert!(matches!(
            unpack_samples(&data[..1], 3, 2, 2, 1),
            Err(CodecError::Malformed { .. })
        ));
    }

    #[test]
    fn indexed_decode_and_palette_clamp() {
        let samples = unpack_samples(&[0b0100_1111], 2, 1, 4, 1).unwrap();
        assert_eq!(samples, vec![4, 15]);
        let idx = indexed_samples(&samples, 4, Some(&[15.0, 0.0]), 5);
        assert_eq!(idx, vec![5, 0], "inverted decode, clamped to hival");
        let palette = [10, 11, 12, 20, 21, 22];
        assert_eq!(
            expand_palette(&[1, 7], &palette, 3),
            vec![20, 21, 22, 20, 21, 22]
        );
    }

    #[test]
    fn cmyk_and_sixteen_bit_conversions() {
        assert_eq!(
            cmyk_to_rgb(&[0, 0, 0, 0, 255, 0, 0, 0, 100, 100, 100, 200]),
            vec![255, 255, 255, 0, 255, 255, 0, 0, 0]
        );
        assert_eq!(
            sixteen_to_eight(&[0x12, 0x34, 0xFF, 0x00]),
            vec![0x12, 0xFF]
        );
        assert_eq!(
            bilevel_to_gray8(&[0b1010_0000], 3, 1, false).unwrap(),
            vec![255, 0, 255]
        );
        assert_eq!(
            bilevel_to_gray8(&[0b1010_0000], 3, 1, true).unwrap(),
            vec![0, 255, 0]
        );
    }
}
