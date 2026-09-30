//! Windows BMP decoding: 1/4/8-bit palette, 16/24/32-bit RGB, BITFIELDS
//! masks, RLE4/RLE8, top-down and bottom-up rows, resolution.

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::{DecodedImage, PixelLayout, row_stride};

const CODEC: &str = "bmp";

pub(crate) fn is_bmp(data: &[u8]) -> bool {
    data.len() >= 26 && data.starts_with(b"BM")
}

fn u16_at(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

fn u32_at(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

struct Mask {
    mask: u32,
    shift: u32,
    bits: u32,
}

impl Mask {
    fn new(mask: u32) -> Self {
        if mask == 0 {
            return Self {
                mask,
                shift: 0,
                bits: 0,
            };
        }
        let shift = mask.trailing_zeros();
        let bits = (mask >> shift).trailing_ones();
        Self { mask, shift, bits }
    }

    fn extract(&self, v: u32) -> u8 {
        if self.bits == 0 {
            return 0;
        }
        let raw = (v & self.mask) >> self.shift;
        let max = (1u64 << self.bits) - 1;
        ((u64::from(raw) * 255 + max / 2) / max) as u8
    }
}

/// Decodes to indexed 1/4/8-bit with palette, 8-bit RGB, or 8-bit RGBA
/// when a 32-bit image has a non-empty alpha mask.
pub(crate) fn decode_bmp(data: &[u8]) -> Result<DecodedImage> {
    if !is_bmp(data) {
        return Err(CodecError::malformed(CODEC, "missing BM signature"));
    }
    let pixel_offset = u32_at(data, 10) as usize;
    let header_size = u32_at(data, 14) as usize;
    if header_size < 12 || data.len() < 14 + header_size {
        return Err(CodecError::malformed(CODEC, "truncated info header"));
    }
    let (width, height_raw, bpp, compression, colors_used, dpi) = if header_size == 12 {
        (
            i32::from(u16_at(data, 18) as i16),
            i32::from(u16_at(data, 20) as i16),
            u16_at(data, 24),
            0u32,
            0u32,
            None,
        )
    } else {
        let xppm = u32_at(data, 38);
        let yppm = u32_at(data, 42);
        let dpi =
            (xppm != 0 && yppm != 0).then(|| (f64::from(xppm) * 0.0254, f64::from(yppm) * 0.0254));
        (
            u32_at(data, 18) as i32,
            u32_at(data, 22) as i32,
            u16_at(data, 28),
            u32_at(data, 30),
            u32_at(data, 46),
            dpi,
        )
    };
    if width <= 0 || height_raw == 0 || height_raw == i32::MIN {
        return Err(CodecError::malformed(CODEC, "invalid dimensions"));
    }
    let top_down = height_raw < 0;
    let width = width as u32;
    let height = height_raw.unsigned_abs();
    check_dimensions(CODEC, width, height)?;

    let mut masks = None;
    if compression == 3 || compression == 6 {
        let base = if header_size >= 52 {
            54
        } else {
            14 + header_size
        };
        let m = data
            .get(base..base + 16)
            .ok_or_else(|| CodecError::malformed(CODEC, "missing bit masks"))?;
        let alpha = if header_size >= 56 || compression == 6 {
            u32_at(m, 12)
        } else {
            0
        };
        masks = Some([
            Mask::new(u32_at(m, 0)),
            Mask::new(u32_at(m, 4)),
            Mask::new(u32_at(m, 8)),
            Mask::new(alpha),
        ]);
    } else if compression == 0 && bpp == 32 {
        masks = Some([
            Mask::new(0x00FF_0000),
            Mask::new(0x0000_FF00),
            Mask::new(0x0000_00FF),
            Mask::new(0),
        ]);
    } else if compression == 0 && bpp == 16 {
        masks = Some([
            Mask::new(0x7C00),
            Mask::new(0x03E0),
            Mask::new(0x001F),
            Mask::new(0),
        ]);
    } else if !matches!(compression, 0..=2) {
        return Err(CodecError::unsupported(
            CODEC,
            format!("compression {compression}"),
        ));
    }

    let palette = if bpp <= 8 {
        let entry = if header_size == 12 { 3 } else { 4 };
        let n = if colors_used == 0 {
            1usize << bpp
        } else {
            (colors_used as usize).min(1 << bpp)
        };
        let start = 14 + header_size;
        let raw = data
            .get(start..start + n * entry)
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated palette"))?;
        let mut p = Vec::with_capacity(n * 3);
        for c in raw.chunks_exact(entry) {
            p.extend_from_slice(&[c[2], c[1], c[0]]);
        }
        Some(p)
    } else {
        None
    };

    let pixels = data
        .get(pixel_offset..)
        .ok_or_else(|| CodecError::malformed(CODEC, "pixel offset past end"))?;
    let row_index = |y: usize| if top_down { y } else { height as usize - 1 - y };

    match (bpp, compression) {
        (1 | 4 | 8, 0) => {
            let stride = row_stride(width, 1, bpp as u8);
            let src_stride = (width as usize * usize::from(bpp)).div_ceil(32) * 4;
            let mut out = vec![0u8; stride * height as usize];
            for y in 0..height as usize {
                let row = pixels
                    .get(y * src_stride..y * src_stride + stride)
                    .ok_or_else(|| CodecError::malformed(CODEC, "truncated pixel data"))?;
                let dy = row_index(y);
                out[dy * stride..(dy + 1) * stride].copy_from_slice(row);
            }
            Ok(DecodedImage {
                width,
                height,
                layout: PixelLayout::Indexed,
                bit_depth: bpp as u8,
                data: out,
                palette,
                icc_profile: None,
                dpi,
            })
        }
        (8, 1) | (4, 2) => {
            let mut out = vec![0u8; width as usize * height as usize];
            rle_decode(pixels, bpp, width, height, top_down, &mut out)?;
            Ok(DecodedImage {
                width,
                height,
                layout: PixelLayout::Indexed,
                bit_depth: 8,
                data: out,
                palette,
                icc_profile: None,
                dpi,
            })
        }
        (24, 0) => {
            let src_stride = (width as usize * 3).div_ceil(4) * 4;
            let mut out = vec![0u8; width as usize * height as usize * 3];
            for y in 0..height as usize {
                let row = pixels
                    .get(y * src_stride..y * src_stride + width as usize * 3)
                    .ok_or_else(|| CodecError::malformed(CODEC, "truncated pixel data"))?;
                let dy = row_index(y);
                for (dst, src) in out[dy * width as usize * 3..(dy + 1) * width as usize * 3]
                    .as_chunks_mut::<3>()
                    .0
                    .iter_mut()
                    .zip(row.as_chunks::<3>().0.iter())
                {
                    dst.copy_from_slice(&[src[2], src[1], src[0]]);
                }
            }
            Ok(DecodedImage {
                width,
                height,
                layout: PixelLayout::Rgb,
                bit_depth: 8,
                data: out,
                palette: None,
                icc_profile: None,
                dpi,
            })
        }
        (16 | 32, 0 | 3 | 6) => {
            let masks = masks.ok_or_else(|| CodecError::malformed(CODEC, "missing masks"))?;
            let has_alpha = masks[3].bits != 0;
            let channels = if has_alpha { 4 } else { 3 };
            let bytes = usize::from(bpp / 8);
            let src_stride = (width as usize * bytes).div_ceil(4) * 4;
            let mut out = vec![0u8; width as usize * height as usize * channels];
            for y in 0..height as usize {
                let row = pixels
                    .get(y * src_stride..y * src_stride + width as usize * bytes)
                    .ok_or_else(|| CodecError::malformed(CODEC, "truncated pixel data"))?;
                let dy = row_index(y);
                let dst =
                    &mut out[dy * width as usize * channels..(dy + 1) * width as usize * channels];
                for (px, d) in row.chunks_exact(bytes).zip(dst.chunks_exact_mut(channels)) {
                    let v = if bytes == 2 {
                        u32::from(u16::from_le_bytes([px[0], px[1]]))
                    } else {
                        u32_at(px, 0)
                    };
                    d[0] = masks[0].extract(v);
                    d[1] = masks[1].extract(v);
                    d[2] = masks[2].extract(v);
                    if has_alpha {
                        d[3] = masks[3].extract(v);
                    }
                }
            }
            Ok(DecodedImage {
                width,
                height,
                layout: if has_alpha {
                    PixelLayout::Rgba
                } else {
                    PixelLayout::Rgb
                },
                bit_depth: 8,
                data: out,
                palette: None,
                icc_profile: None,
                dpi,
            })
        }
        _ => Err(CodecError::unsupported(
            CODEC,
            format!("{bpp} bits per pixel with compression {compression}"),
        )),
    }
}

fn rle_decode(
    src: &[u8],
    bpp: u16,
    width: u32,
    height: u32,
    top_down: bool,
    out: &mut [u8],
) -> Result<()> {
    let w = width as usize;
    let h = height as usize;
    let (mut x, mut y) = (0usize, 0usize);
    let mut pos = 0usize;
    let put = |out: &mut [u8], x: usize, y: usize, v: u8| {
        if x < w && y < h {
            let row = if top_down { y } else { h - 1 - y };
            out[row * w + x] = v;
        }
    };
    loop {
        let (count, val) = match src.get(pos..pos + 2) {
            Some(b) => (b[0], b[1]),
            None => return Ok(()),
        };
        pos += 2;
        if count > 0 {
            for i in 0..usize::from(count) {
                let v = if bpp == 4 {
                    if i % 2 == 0 { val >> 4 } else { val & 0xF }
                } else {
                    val
                };
                put(out, x, y, v);
                x += 1;
            }
        } else {
            match val {
                0 => {
                    x = 0;
                    y += 1;
                }
                1 => return Ok(()),
                2 => {
                    let d = src
                        .get(pos..pos + 2)
                        .ok_or_else(|| CodecError::malformed(CODEC, "truncated RLE delta"))?;
                    x += usize::from(d[0]);
                    y += usize::from(d[1]);
                    pos += 2;
                }
                n => {
                    let n = usize::from(n);
                    let bytes = if bpp == 4 { n.div_ceil(2) } else { n };
                    let padded = bytes.div_ceil(2) * 2;
                    let lit = src
                        .get(pos..pos + bytes)
                        .ok_or_else(|| CodecError::malformed(CODEC, "truncated RLE literals"))?;
                    for i in 0..n {
                        let v = if bpp == 4 {
                            if i % 2 == 0 {
                                lit[i / 2] >> 4
                            } else {
                                lit[i / 2] & 0xF
                            }
                        } else {
                            lit[i]
                        };
                        put(out, x, y, v);
                        x += 1;
                    }
                    pos += padded;
                }
            }
        }
        if y >= h {
            return Ok(());
        }
    }
}

#[cfg(test)]
pub(crate) mod test_encoder {
    /// Builds a bottom-up 24-bit BMP from RGB rows with a 40-byte info header.
    pub(crate) fn build_bmp24(rgb: &[u8], width: u32, height: u32, ppm: u32) -> Vec<u8> {
        let stride = (width as usize * 3).div_ceil(4) * 4;
        let size = 54 + stride * height as usize;
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(size as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&((stride * height as usize) as u32).to_le_bytes());
        out.extend_from_slice(&ppm.to_le_bytes());
        out.extend_from_slice(&ppm.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        for y in (0..height as usize).rev() {
            let row = &rgb[y * width as usize * 3..(y + 1) * width as usize * 3];
            for px in row.as_chunks::<3>().0.iter() {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            out.resize(out.len() + stride - width as usize * 3, 0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_encoder::build_bmp24;
    use super::*;

    #[test]
    fn bottom_up_24_bit_rows_are_flipped_and_swapped_to_rgb() {
        let (w, h) = (3u32, 2u32);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 13) as u8).collect();
        let bmp = build_bmp24(&rgb, w, h, 2835);
        let img = decode_bmp(&bmp).unwrap();
        assert_eq!((img.layout, img.bit_depth), (PixelLayout::Rgb, 8));
        assert_eq!(img.data, rgb);
        let (dx, dy) = img.dpi.unwrap();
        assert!(
            (dx - 72.0).abs() < 0.01 && (dy - 72.0).abs() < 0.01,
            "{dx} {dy}"
        );
    }

    #[test]
    fn truncated_bmp_is_an_error() {
        let rgb = vec![7u8; 4 * 4 * 3];
        let bmp = build_bmp24(&rgb, 4, 4, 0);
        assert!(decode_bmp(&bmp[..bmp.len() - 3]).is_err());
        assert!(decode_bmp(&bmp[..30]).is_err());
    }
}
