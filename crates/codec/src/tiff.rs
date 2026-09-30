//! Baseline and extended TIFF decoding: strips and tiles; uncompressed,
//! PackBits, LZW, Deflate, CCITT (modified Huffman, Group 3, Group 4) and
//! JPEG compression; bilevel, gray, palette, RGB, CMYK and YCbCr-in-JPEG
//! photometrics at 1–16 bits; horizontal predictor; FillOrder 2;
//! multi-page files; resolution and ICC tags.

use crate::ccitt::{CcittParams, decode_ccitt};
use crate::dct::{DctParams, decode_dct};
use crate::error::{CodecError, Result, check_dimensions};
use crate::ifd::{Ifd, read_header, read_ifd};
use crate::image::{DecodedImage, PixelLayout, row_stride};

const CODEC: &str = "tiff";

const TAG_WIDTH: u16 = 256;
const TAG_HEIGHT: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_FILL_ORDER: u16 = 266;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_X_RESOLUTION: u16 = 282;
const TAG_Y_RESOLUTION: u16 = 283;
const TAG_PLANAR: u16 = 284;
const TAG_T4_OPTIONS: u16 = 292;
const TAG_RESOLUTION_UNIT: u16 = 296;
const TAG_PREDICTOR: u16 = 317;
const TAG_COLOR_MAP: u16 = 320;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_EXTRA_SAMPLES: u16 = 338;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TAG_JPEG_TABLES: u16 = 347;
const TAG_ICC_PROFILE: u16 = 34675;

pub(crate) fn is_tiff(data: &[u8]) -> bool {
    data.starts_with(b"II*\0") || data.starts_with(b"MM\0*")
}

/// Facts about one TIFF page that the PDF embedding path needs.
#[derive(Debug, Clone, PartialEq)]
pub struct TiffPageInfo {
    pub width: u32,
    pub height: u32,
    pub bits_per_sample: u16,
    pub samples_per_pixel: u16,
    pub compression: u16,
    pub photometric: u16,
    /// Number of strips or tiles.
    pub segments: usize,
    pub fill_order: u16,
    pub dpi: Option<(f64, f64)>,
    pub icc_profile: Option<Vec<u8>>,
}

fn page_ifd(data: &[u8], page: u32) -> Result<Ifd> {
    let (le, mut offset) = read_header(CODEC, data)?;
    let mut ifd = read_ifd(CODEC, data, offset, le)?;
    for _ in 0..page {
        if ifd.next == 0 {
            return Err(CodecError::invalid(
                CODEC,
                format!("page {page} does not exist"),
            ));
        }
        offset = ifd.next;
        ifd = read_ifd(CODEC, data, offset, le)?;
    }
    Ok(ifd)
}

/// Counts the pages (IFDs) in the file, stopping at loops.
pub fn tiff_page_count(data: &[u8]) -> Result<u32> {
    let (le, mut offset) = read_header(CODEC, data)?;
    let mut count = 0u32;
    let mut seen = Vec::new();
    while offset != 0 && !seen.contains(&offset) && count < 1 << 16 {
        seen.push(offset);
        let ifd = read_ifd(CODEC, data, offset, le)?;
        count += 1;
        offset = ifd.next;
    }
    Ok(count)
}

fn info_from(data: &[u8], ifd: &Ifd) -> Result<TiffPageInfo> {
    let width = ifd
        .int(data, TAG_WIDTH)
        .ok_or_else(|| CodecError::malformed(CODEC, "missing ImageWidth"))?;
    let height = ifd
        .int(data, TAG_HEIGHT)
        .ok_or_else(|| CodecError::malformed(CODEC, "missing ImageLength"))?;
    check_dimensions(CODEC, width, height)?;
    let bits = ifd
        .ints(data, TAG_BITS_PER_SAMPLE)
        .and_then(|v| v.into_iter().max())
        .unwrap_or(1);
    let samples = ifd.int(data, TAG_SAMPLES_PER_PIXEL).unwrap_or(1);
    let compression = ifd.int(data, TAG_COMPRESSION).unwrap_or(1);
    let photometric = ifd
        .int(data, TAG_PHOTOMETRIC)
        .unwrap_or(if samples >= 3 { 2 } else { 1 });
    let segments = ifd
        .ints(data, TAG_STRIP_OFFSETS)
        .or_else(|| ifd.ints(data, TAG_TILE_OFFSETS))
        .map(|v| v.len())
        .unwrap_or(0);
    let unit = ifd.int(data, TAG_RESOLUTION_UNIT).unwrap_or(2);
    let dpi = match (
        ifd.rational(data, TAG_X_RESOLUTION),
        ifd.rational(data, TAG_Y_RESOLUTION),
    ) {
        (Some(x), Some(y)) if x > 0.0 && y > 0.0 => match unit {
            3 => Some((x * 2.54, y * 2.54)),
            1 => None,
            _ => Some((x, y)),
        },
        _ => None,
    };
    Ok(TiffPageInfo {
        width,
        height,
        bits_per_sample: bits.min(u32::from(u16::MAX)) as u16,
        samples_per_pixel: samples.min(u32::from(u16::MAX)) as u16,
        compression: compression.min(u32::from(u16::MAX)) as u16,
        photometric: photometric.min(u32::from(u16::MAX)) as u16,
        segments,
        fill_order: ifd.int(data, TAG_FILL_ORDER).unwrap_or(1).min(2) as u16,
        dpi,
        icc_profile: ifd.bytes(data, TAG_ICC_PROFILE).map(<[u8]>::to_vec),
    })
}

/// Reads the tags of page `page` (0-based).
pub fn tiff_page_info(data: &[u8], page: u32) -> Result<TiffPageInfo> {
    let ifd = page_ifd(data, page)?;
    info_from(data, &ifd)
}

/// A single-strip Group 4 page whose bytes can be embedded as-is with
/// `CCITTFaxDecode` (`/K -1`), the way img2pdf does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group4Strip {
    pub width: u32,
    pub height: u32,
    /// The strip bytes, bit-reversed already when FillOrder was 2.
    pub data: Vec<u8>,
    /// Photometric 0 (WhiteIsZero): img2pdf writes `/BlackIs1 false` for
    /// these and `/BlackIs1 true` for photometric 1.
    pub white_is_zero: bool,
}

/// Returns the raw Group 4 strip of page `page` when the page is a
/// single-strip, uncompressed-layout CCITT T.6 bilevel image with
/// photometric 0 or 1; `None` when it is anything else.
pub fn tiff_group4_strip(data: &[u8], page: u32) -> Result<Option<Group4Strip>> {
    let ifd = page_ifd(data, page)?;
    let info = info_from(data, &ifd)?;
    if info.compression != 4
        || info.bits_per_sample != 1
        || info.samples_per_pixel != 1
        || !matches!(info.photometric, 0 | 1)
    {
        return Ok(None);
    }
    let offsets = ifd.ints(data, TAG_STRIP_OFFSETS).unwrap_or_default();
    let counts = ifd.ints(data, TAG_STRIP_BYTE_COUNTS).unwrap_or_default();
    if offsets.len() != 1 || counts.len() != 1 {
        return Ok(None);
    }
    let start = offsets[0] as usize;
    let bytes = data
        .get(start..start + counts[0] as usize)
        .ok_or_else(|| CodecError::malformed(CODEC, "strip out of range"))?;
    let mut strip = bytes.to_vec();
    if info.fill_order == 2 {
        for b in &mut strip {
            *b = b.reverse_bits();
        }
    }
    Ok(Some(Group4Strip {
        width: info.width,
        height: info.height,
        data: strip,
        white_is_zero: info.photometric == 0,
    }))
}

/// Decodes page `page` (0-based). Bilevel pages come back as 1-bit gray with
/// 1 = white regardless of photometric; palette pages as indexed with an
/// 8-bit RGB palette; gray and RGB keep 8 or 16 bits (sub-byte gray stays
/// packed); CMYK pages are 8-bit [`PixelLayout::Cmyk`]; an alpha extra
/// sample makes gray+alpha or RGBA.
pub fn decode_tiff(data: &[u8], page: u32) -> Result<DecodedImage> {
    let ifd = page_ifd(data, page)?;
    let info = info_from(data, &ifd)?;
    let (width, height) = (info.width, info.height);
    let spp = usize::from(info.samples_per_pixel);
    let bits = info.bits_per_sample;
    if spp == 0 || spp > 4 {
        return Err(CodecError::unsupported(
            CODEC,
            format!("{spp} samples per pixel"),
        ));
    }
    if !matches!(bits, 1 | 2 | 4 | 8 | 16) {
        return Err(CodecError::unsupported(
            CODEC,
            format!("{bits} bits per sample"),
        ));
    }
    if let Some(v) = ifd.ints(data, TAG_BITS_PER_SAMPLE)
        && v.iter().any(|&b| b != u32::from(bits))
    {
        return Err(CodecError::unsupported(CODEC, "differing bits per sample"));
    }
    if ifd.int(data, TAG_PLANAR).unwrap_or(1) != 1 {
        return Err(CodecError::unsupported(CODEC, "planar configuration 2"));
    }
    if ifd
        .ints(data, TAG_SAMPLE_FORMAT)
        .is_some_and(|v| v.iter().any(|&f| f != 1))
    {
        return Err(CodecError::unsupported(CODEC, "non-integer sample format"));
    }
    let extra = ifd.ints(data, TAG_EXTRA_SAMPLES).unwrap_or_default();
    let stride = row_stride(width, spp, bits as u8);
    let mut pixels = vec![0u8; stride * height as usize];

    // Segment geometry: strips or tiles.
    let (seg_w, seg_h, offsets, counts, tiled) =
        if let Some(offsets) = ifd.ints(data, TAG_TILE_OFFSETS) {
            let tw = ifd
                .int(data, TAG_TILE_WIDTH)
                .ok_or_else(|| CodecError::malformed(CODEC, "missing TileWidth"))?;
            let th = ifd
                .int(data, TAG_TILE_LENGTH)
                .ok_or_else(|| CodecError::malformed(CODEC, "missing TileLength"))?;
            if tw == 0 || th == 0 {
                return Err(CodecError::malformed(CODEC, "zero tile size"));
            }
            let counts = ifd
                .ints(data, TAG_TILE_BYTE_COUNTS)
                .ok_or_else(|| CodecError::malformed(CODEC, "missing TileByteCounts"))?;
            (tw, th, offsets, counts, true)
        } else {
            let offsets = ifd
                .ints(data, TAG_STRIP_OFFSETS)
                .ok_or_else(|| CodecError::malformed(CODEC, "missing StripOffsets"))?;
            let rps = ifd
                .int(data, TAG_ROWS_PER_STRIP)
                .unwrap_or(height)
                .clamp(1, height.max(1));
            let counts = match ifd.ints(data, TAG_STRIP_BYTE_COUNTS) {
                Some(c) => c,
                None if offsets.len() == 1 => vec![(data.len() as u32).saturating_sub(offsets[0])],
                None => return Err(CodecError::malformed(CODEC, "missing StripByteCounts")),
            };
            (width, rps, offsets, counts, false)
        };
    if counts.len() < offsets.len() {
        return Err(CodecError::malformed(
            CODEC,
            "fewer byte counts than offsets",
        ));
    }
    let across = if tiled {
        width.div_ceil(seg_w) as usize
    } else {
        1
    };
    let down = height.div_ceil(seg_h) as usize;
    if offsets.len() < across * down {
        return Err(CodecError::malformed(CODEC, "too few strips or tiles"));
    }

    let compression = info.compression;
    let predictor = ifd.int(data, TAG_PREDICTOR).unwrap_or(1);
    let fill_order = info.fill_order;
    let t4_options = ifd.int(data, TAG_T4_OPTIONS).unwrap_or(0);
    let jpeg_tables = ifd.bytes(data, TAG_JPEG_TABLES);
    let seg_stride = row_stride(seg_w, spp, bits as u8);
    let mut reversed = Vec::new();

    for (i, (&off, &count)) in offsets
        .iter()
        .zip(counts.iter())
        .enumerate()
        .take(across * down)
    {
        let (col, row) = (i % across, i / across);
        let rows_here = (height - (row as u32 * seg_h).min(height)).min(seg_h);
        if rows_here == 0 {
            continue;
        }
        let raw = data
            .get(off as usize..(off as usize).saturating_add(count as usize))
            .ok_or_else(|| CodecError::malformed(CODEC, format!("segment {i} out of range")))?;
        let raw: &[u8] = if fill_order == 2 && !matches!(compression, 6 | 7) {
            reversed.clear();
            reversed.extend(raw.iter().map(|b| b.reverse_bits()));
            &reversed
        } else {
            raw
        };
        let expected = seg_stride * rows_here as usize;
        let decoded: Vec<u8> = match compression {
            1 => raw.to_vec(),
            2..=4 => {
                let params = CcittParams {
                    k: match compression {
                        2 => 0,
                        3 if t4_options & 1 != 0 => 4,
                        3 => 0,
                        _ => -1,
                    },
                    columns: seg_w,
                    rows: rows_here,
                    encoded_byte_align: compression == 2
                        || (compression == 3 && t4_options & 4 != 0),
                    end_of_line: false,
                    end_of_block: compression == 4 || compression == 3,
                    // libtiff's buffer convention: CCITT black runs are 1
                    // bits; the photometric handling below interprets them.
                    black_is_1: true,
                    damaged_rows_before_error: 0,
                };
                if bits != 1 || spp != 1 {
                    return Err(CodecError::unsupported(
                        CODEC,
                        "CCITT compression with non-bilevel samples",
                    ));
                }
                decode_ccitt(raw, &params)?.image.data
            }
            5 => lzw_decode(raw, expected)?,
            8 | 32946 => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(raw, expected)
                .map(|mut v| {
                    v.resize(expected, 0);
                    v
                })
                .map_err(|e| CodecError::malformed(CODEC, format!("deflate: {:?}", e.status)))?,
            32773 => packbits_decode(raw, expected),
            7 => {
                if bits != 8 {
                    return Err(CodecError::unsupported(
                        CODEC,
                        "JPEG compression with non-8-bit samples",
                    ));
                }
                let jpeg = match jpeg_tables {
                    Some(t) if t.len() > 4 && raw.len() > 2 => {
                        let mut v = Vec::with_capacity(t.len() + raw.len());
                        v.extend_from_slice(&t[..t.len() - 2]);
                        v.extend_from_slice(&raw[2..]);
                        v
                    }
                    _ => raw.to_vec(),
                };
                let params = DctParams {
                    color_transform: Some(u32::from(info.photometric == 6)),
                };
                let img = decode_dct(&jpeg, &params)?;
                if usize::from(img.components) != spp {
                    return Err(CodecError::malformed(
                        CODEC,
                        "JPEG component count differs from SamplesPerPixel",
                    ));
                }
                let mut v = Vec::with_capacity(expected);
                let jw = img.width as usize * spp;
                for r in 0..rows_here as usize {
                    let line = img
                        .data
                        .get(r * jw..r * jw + seg_stride.min(jw))
                        .unwrap_or(&[]);
                    v.extend_from_slice(line);
                    v.resize((r + 1) * seg_stride, 0);
                }
                v
            }
            c => return Err(CodecError::unsupported(CODEC, format!("compression {c}"))),
        };
        let mut decoded = decoded;
        if decoded.len() < expected {
            if compression == 1 {
                return Err(CodecError::malformed(
                    CODEC,
                    format!("segment {i} shorter than its rows"),
                ));
            }
            decoded.resize(expected, 0);
        }
        if predictor == 2 && matches!(compression, 5 | 8 | 32946) {
            undo_horizontal_predictor(&mut decoded, seg_w as usize, spp, bits, seg_stride);
        }
        // Copy into place.
        let x0 = col as u32 * seg_w;
        let y0 = row as u32 * seg_h;
        let copy_w = (width - x0).min(seg_w);
        for r in 0..rows_here as usize {
            let y = y0 as usize + r;
            let src = &decoded[r * seg_stride..(r + 1) * seg_stride];
            let dst = &mut pixels[y * stride..(y + 1) * stride];
            if bits >= 8 {
                let bpp = spp * usize::from(bits / 8);
                let x = x0 as usize * bpp;
                let n = copy_w as usize * bpp;
                dst[x..x + n].copy_from_slice(&src[..n]);
            } else if x0 == 0 {
                let n = row_stride(copy_w, spp, bits as u8);
                dst[..n].copy_from_slice(&src[..n]);
            } else {
                for px in 0..copy_w as usize {
                    for s in 0..spp {
                        let bit_src = (px * spp + s) * usize::from(bits);
                        let bit_dst = ((x0 as usize + px) * spp + s) * usize::from(bits);
                        let v = (src[bit_src / 8] >> (8 - bits as usize - bit_src % 8))
                            & ((1u8 << bits) - 1);
                        dst[bit_dst / 8] |= v << (8 - bits as usize - bit_dst % 8);
                    }
                }
            }
        }
    }

    let mut image = DecodedImage {
        width,
        height,
        layout: PixelLayout::Gray,
        bit_depth: bits as u8,
        data: pixels,
        palette: None,
        icc_profile: info.icc_profile.clone(),
        dpi: info.dpi,
    };
    let has_alpha =
        spp >= 2 && extra.first().is_some_and(|&e| e == 1 || e == 2) && matches!(spp, 2 | 4);
    match (info.photometric, spp) {
        (0 | 1, 1) => {
            if info.photometric == 0 {
                match bits {
                    16 => {
                        for px in image.data.as_chunks_mut::<2>().0.iter_mut() {
                            let v = !u16::from_be_bytes([px[0], px[1]]);
                            px.copy_from_slice(&v.to_be_bytes());
                        }
                    }
                    _ => {
                        for b in &mut image.data {
                            *b = !*b;
                        }
                    }
                }
            }
            if bits == 16 && ifd.little_endian {
                for px in image.data.as_chunks_mut::<2>().0.iter_mut() {
                    px.swap(0, 1);
                }
            }
        }
        (0 | 1, 2) if has_alpha => {
            image.layout = PixelLayout::GrayAlpha;
            if info.photometric == 0 {
                for px in image.data.chunks_exact_mut(if bits == 16 { 4 } else { 2 }) {
                    let n = if bits == 16 { 2 } else { 1 };
                    for b in &mut px[..n] {
                        *b = !*b;
                    }
                }
            }
            fix_endian(&mut image, ifd.little_endian);
        }
        (2, 3) => {
            image.layout = PixelLayout::Rgb;
            fix_endian(&mut image, ifd.little_endian);
        }
        (2, 4) if has_alpha => {
            image.layout = PixelLayout::Rgba;
            fix_endian(&mut image, ifd.little_endian);
        }
        (3, 1) => {
            let map = ifd
                .ints(data, TAG_COLOR_MAP)
                .ok_or_else(|| CodecError::malformed(CODEC, "palette image without ColorMap"))?;
            let n = 1usize << bits.min(8);
            if map.len() < 3 * n {
                return Err(CodecError::malformed(CODEC, "short ColorMap"));
            }
            let mut palette = Vec::with_capacity(3 * n);
            for i in 0..n {
                palette.extend_from_slice(&[
                    (map[i] >> 8) as u8,
                    (map[n + i] >> 8) as u8,
                    (map[2 * n + i] >> 8) as u8,
                ]);
            }
            if bits == 16 {
                return Err(CodecError::unsupported(CODEC, "16-bit palette indices"));
            }
            image.layout = PixelLayout::Indexed;
            image.palette = Some(palette);
        }
        (5, 4) => {
            image.layout = PixelLayout::Cmyk;
            fix_endian(&mut image, ifd.little_endian);
        }
        (6, 3) if compression == 7 => image.layout = PixelLayout::Rgb,
        (p, s) => {
            return Err(CodecError::unsupported(
                CODEC,
                format!("photometric {p} with {s} samples per pixel"),
            ));
        }
    }
    if has_alpha && extra[0] == 1 {
        // Associated (premultiplied) alpha to straight.
        if image.bit_depth == 8 {
            image.data = match image.layout {
                PixelLayout::Rgba => crate::pixels::unpremultiply_rgba(&image.data),
                _ => unpremultiply_gray_alpha(&image.data),
            };
        }
    }
    Ok(image)
}

fn fix_endian(image: &mut DecodedImage, little_endian: bool) {
    if image.bit_depth == 16 && little_endian {
        for px in image.data.as_chunks_mut::<2>().0.iter_mut() {
            px.swap(0, 1);
        }
    }
}

fn unpremultiply_gray_alpha(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.as_chunks::<2>().0.iter() {
        let a = u32::from(px[1]);
        let g = (u32::from(px[0]) * 255 + a / 2)
            .checked_div(a)
            .map_or(0, |v| v.min(255) as u8);
        out.extend_from_slice(&[g, px[1]]);
    }
    out
}

fn undo_horizontal_predictor(buf: &mut [u8], width: usize, spp: usize, bits: u16, stride: usize) {
    match bits {
        8 => {
            for row in buf.chunks_exact_mut(stride) {
                for i in spp..width * spp {
                    row[i] = row[i].wrapping_add(row[i - spp]);
                }
            }
        }
        16 => {
            for row in buf.chunks_exact_mut(stride) {
                for i in spp..width * spp {
                    let prev = u16::from_be_bytes([row[2 * (i - spp)], row[2 * (i - spp) + 1]]);
                    let cur = u16::from_be_bytes([row[2 * i], row[2 * i + 1]]);
                    let v = cur.wrapping_add(prev).to_be_bytes();
                    row[2 * i] = v[0];
                    row[2 * i + 1] = v[1];
                }
            }
        }
        _ => {}
    }
}

fn packbits_decode(src: &[u8], expected: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(expected);
    let mut pos = 0usize;
    while pos < src.len() && out.len() < expected {
        let n = src[pos] as i8;
        pos += 1;
        if n >= 0 {
            let len = n as usize + 1;
            let end = (pos + len).min(src.len());
            out.extend_from_slice(&src[pos..end]);
            pos = end;
        } else if n != -128 {
            let len = (-(n as i16)) as usize + 1;
            if let Some(&b) = src.get(pos) {
                out.extend(std::iter::repeat_n(b, len));
                pos += 1;
            }
        }
    }
    out.truncate(expected);
    out
}

/// TIFF-variant LZW: MSB-first codes, clear 256, end 257, with the "early
/// change" code-width switch libtiff uses.
fn lzw_decode(src: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    let mut prefix = vec![0u16; 4096];
    let mut suffix = vec![0u8; 4096];
    for (i, s) in suffix.iter_mut().enumerate().take(256) {
        *s = i as u8;
    }
    let mut code_size = 9u32;
    let mut next = 258u16;
    let mut prev: Option<u16> = None;
    let mut bit_pos = 0usize;
    let total = src.len() * 8;
    let mut stack: Vec<u8> = Vec::with_capacity(4096);
    while out.len() < expected && bit_pos + code_size as usize <= total {
        let mut code = 0u32;
        for _ in 0..code_size {
            code = (code << 1) | u32::from((src[bit_pos / 8] >> (7 - bit_pos % 8)) & 1);
            bit_pos += 1;
        }
        let code = code as u16;
        if code == 256 {
            code_size = 9;
            next = 258;
            prev = None;
            continue;
        }
        if code == 257 {
            break;
        }
        if code > next || (code == next && prev.is_none()) {
            return Err(CodecError::malformed(CODEC, "LZW code out of range"));
        }
        stack.clear();
        if code == next {
            let p = prev.unwrap_or(0);
            let mut c = p;
            while c >= 258 {
                stack.push(suffix[usize::from(c)]);
                c = prefix[usize::from(c)];
            }
            stack.push(suffix[usize::from(c)]);
            let first = *stack.last().unwrap_or(&0);
            stack.insert(0, first);
        } else {
            let mut c = code;
            while c >= 258 {
                stack.push(suffix[usize::from(c)]);
                c = prefix[usize::from(c)];
            }
            stack.push(suffix[usize::from(c)]);
        }
        for &b in stack.iter().rev() {
            if out.len() < expected {
                out.push(b);
            }
        }
        if let Some(p) = prev
            && next < 4096
        {
            prefix[usize::from(next)] = p;
            suffix[usize::from(next)] = *stack.last().unwrap_or(&0);
            next += 1;
            if u32::from(next) + 1 == (1 << code_size) && code_size < 12 {
                code_size += 1;
            }
        }
        prev = Some(code);
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod test_encoder {
    //! Builds little-endian TIFF files for decoder tests.

    pub(crate) struct Tag {
        pub tag: u16,
        pub kind: u16,
        pub values: Vec<u32>,
    }

    pub(crate) fn short(tag: u16, v: u32) -> Tag {
        Tag {
            tag,
            kind: 3,
            values: vec![v],
        }
    }

    pub(crate) fn long(tag: u16, v: u32) -> Tag {
        Tag {
            tag,
            kind: 4,
            values: vec![v],
        }
    }

    /// Writes one IFD after the header, then `strip` at the end, patching the
    /// StripOffsets (273) tag; `tags` must include everything else.
    pub(crate) fn build_tiff(mut tags: Vec<Tag>, strip: &[u8]) -> Vec<u8> {
        tags.sort_by_key(|t| t.tag);
        let n = tags.len();
        let ifd_start = 8usize;
        let ifd_len = 2 + 12 * n + 4;
        let mut extra: Vec<u8> = Vec::new();
        let extra_start = ifd_start + ifd_len;
        let mut entries = Vec::new();
        for t in &tags {
            let mut bytes = Vec::new();
            for &v in &t.values {
                if t.kind == 3 {
                    bytes.extend_from_slice(&(v as u16).to_le_bytes());
                } else {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
            }
            let mut entry = Vec::new();
            entry.extend_from_slice(&t.tag.to_le_bytes());
            entry.extend_from_slice(&t.kind.to_le_bytes());
            let count = if t.kind == 5 {
                t.values.len() / 2
            } else {
                t.values.len()
            };
            entry.extend_from_slice(&(count as u32).to_le_bytes());
            if bytes.len() <= 4 {
                bytes.resize(4, 0);
                entry.extend_from_slice(&bytes);
            } else {
                entry.extend_from_slice(&((extra_start + extra.len()) as u32).to_le_bytes());
                extra.extend_from_slice(&bytes);
            }
            entries.push(entry);
        }
        let strip_offset = (extra_start + extra.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"II*\0");
        out.extend_from_slice(&(ifd_start as u32).to_le_bytes());
        out.extend_from_slice(&(n as u16).to_le_bytes());
        for (t, mut e) in tags.iter().zip(entries) {
            if t.tag == 273 {
                e.truncate(8);
                e.extend_from_slice(&strip_offset.to_le_bytes());
            }
            out.extend_from_slice(&e);
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&extra);
        out.extend_from_slice(strip);
        out
    }

    /// PackBits-compresses `data` as literal runs.
    pub(crate) fn packbits(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in data.chunks(128) {
            out.push((chunk.len() - 1) as u8);
            out.extend_from_slice(chunk);
        }
        out
    }

    /// TIFF LZW-compresses `data` with literal codes only, mirroring the
    /// decoder's table growth.
    pub(crate) fn lzw_literal(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u64;
        let mut nbits = 0u32;
        let push = |code: u32, size: u32, acc: &mut u64, nbits: &mut u32, out: &mut Vec<u8>| {
            *acc = (*acc << size) | u64::from(code);
            *nbits += size;
            while *nbits >= 8 {
                out.push(((*acc >> (*nbits - 8)) & 0xFF) as u8);
                *nbits -= 8;
            }
        };
        let mut code_size = 9u32;
        let mut next = 258u32;
        push(256, code_size, &mut acc, &mut nbits, &mut out);
        let mut first = true;
        for &b in data {
            push(u32::from(b), code_size, &mut acc, &mut nbits, &mut out);
            if first {
                first = false;
            } else if next < 4096 {
                next += 1;
                if next + 1 == (1 << code_size) && code_size < 12 {
                    code_size += 1;
                }
            }
        }
        push(257, code_size, &mut acc, &mut nbits, &mut out);
        if nbits > 0 {
            out.push(((acc << (8 - nbits)) & 0xFF) as u8);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_encoder::{Tag, build_tiff, long, lzw_literal, packbits, short};
    use super::*;
    use crate::ccitt::encode_ccitt_g4;

    fn base_tags(
        width: u32,
        height: u32,
        bits: u32,
        spp: u32,
        photometric: u32,
        compression: u32,
        strip_len: usize,
    ) -> Vec<Tag> {
        vec![
            long(TAG_WIDTH, width),
            long(TAG_HEIGHT, height),
            Tag {
                tag: TAG_BITS_PER_SAMPLE,
                kind: 3,
                values: vec![bits; spp as usize],
            },
            short(TAG_COMPRESSION, compression),
            short(TAG_PHOTOMETRIC, photometric),
            long(TAG_STRIP_OFFSETS, 0),
            short(TAG_SAMPLES_PER_PIXEL, spp),
            long(TAG_ROWS_PER_STRIP, height),
            long(TAG_STRIP_BYTE_COUNTS, strip_len as u32),
        ]
    }

    #[test]
    fn uncompressed_rgb_strip_decodes() {
        let (w, h) = (5u32, 3u32);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 17) as u8).collect();
        let tiff = build_tiff(base_tags(w, h, 8, 3, 2, 1, rgb.len()), &rgb);
        let img = decode_tiff(&tiff, 0).unwrap();
        assert_eq!((img.layout, img.bit_depth), (PixelLayout::Rgb, 8));
        assert_eq!(img.data, rgb);
        assert_eq!(tiff_page_count(&tiff).unwrap(), 1);
        assert!(tiff_group4_strip(&tiff, 0).unwrap().is_none());
    }

    #[test]
    fn packbits_and_lzw_with_predictor_match_raw() {
        let (w, h) = (300u32, 2u32);
        let gray: Vec<u8> = (0..w * h).map(|i| ((i * i) % 251) as u8).collect();
        let pb = packbits(&gray);
        let tiff = build_tiff(base_tags(w, h, 8, 1, 1, 32773, pb.len()), &pb);
        assert_eq!(decode_tiff(&tiff, 0).unwrap().data, gray);

        let mut diffed = gray.clone();
        for row in diffed.chunks_exact_mut(w as usize) {
            for i in (1..row.len()).rev() {
                row[i] = row[i].wrapping_sub(row[i - 1]);
            }
        }
        let lzw = lzw_literal(&diffed);
        let mut tags = base_tags(w, h, 8, 1, 1, 5, lzw.len());
        tags.push(short(TAG_PREDICTOR, 2));
        let tiff = build_tiff(tags, &lzw);
        assert_eq!(decode_tiff(&tiff, 0).unwrap().data, gray);
    }

    #[test]
    fn white_is_zero_bilevel_is_inverted_to_one_is_white() {
        let (w, h) = (12u32, 2u32);
        let raw = vec![0b1010_0000, 0b1111_0000, 0b0000_1111, 0b0101_0000];
        let tiff = build_tiff(base_tags(w, h, 1, 1, 0, 1, raw.len()), &raw);
        let img = decode_tiff(&tiff, 0).unwrap();
        assert_eq!(img.bit_depth, 1);
        assert_eq!(
            img.data,
            vec![0b0101_1111, 0b0000_1111, 0b1111_0000, 0b1010_1111]
        );
    }

    #[test]
    fn group4_strip_round_trips_and_is_offered_for_passthrough() {
        let (w, h) = (40u32, 6u32);
        let stride = row_stride(w, 1, 1);
        let mut bits = vec![0xFFu8; stride * h as usize];
        bits[7] = 0x0F;
        bits[13] = 0x00;
        // Photometric 1 (BlackIsZero): 1 bits are white, and libtiff still
        // encodes 1 bits as CCITT black runs.
        let g4 = encode_ccitt_g4(&bits, w, h, true).unwrap();
        let mut tags = base_tags(w, h, 1, 1, 1, 4, g4.len());
        tags.push(short(TAG_FILL_ORDER, 2));
        let reversed: Vec<u8> = g4.iter().map(|b| b.reverse_bits()).collect();
        let tiff = build_tiff(tags, &reversed);
        let img = decode_tiff(&tiff, 0).unwrap();
        assert_eq!(img.data, bits);
        let strip = tiff_group4_strip(&tiff, 0).unwrap().unwrap();
        assert_eq!(strip.data, g4);
        assert!(!strip.white_is_zero);
    }

    #[test]
    fn palette_page_reports_indexed_with_8_bit_palette() {
        let (w, h) = (4u32, 1u32);
        let raw = vec![0x01, 0x23];
        let mut tags = base_tags(w, h, 4, 1, 3, 1, raw.len());
        let mut map = Vec::new();
        for c in 0..3u32 {
            for i in 0..16u32 {
                map.push(((i * 17) << 8) | (c * 100));
            }
        }
        tags.push(Tag {
            tag: TAG_COLOR_MAP,
            kind: 3,
            values: map,
        });
        let tiff = build_tiff(tags, &raw);
        let img = decode_tiff(&tiff, 0).unwrap();
        assert_eq!((img.layout, img.bit_depth), (PixelLayout::Indexed, 4));
        assert_eq!(img.data, raw);
        let palette = img.palette.unwrap();
        assert_eq!(&palette[3..6], &[17, 17, 17]);
    }

    #[test]
    fn resolution_in_centimetres_becomes_dpi() {
        let raw = vec![0u8; 4];
        let mut tags = base_tags(2, 2, 8, 1, 1, 1, raw.len());
        tags.push(Tag {
            tag: TAG_X_RESOLUTION,
            kind: 5,
            values: vec![100, 1],
        });
        tags.push(Tag {
            tag: TAG_Y_RESOLUTION,
            kind: 5,
            values: vec![50, 1],
        });
        tags.push(short(TAG_RESOLUTION_UNIT, 3));
        let tiff = build_tiff(tags, &raw);
        let (dx, dy) = decode_tiff(&tiff, 0).unwrap().dpi.unwrap();
        assert!(
            (dx - 254.0).abs() < 1e-9 && (dy - 127.0).abs() < 1e-9,
            "{dx} {dy}"
        );
    }

    #[test]
    fn truncated_strip_is_an_error() {
        let raw = vec![1u8; 16];
        let tiff = build_tiff(base_tags(4, 4, 8, 1, 1, 1, raw.len()), &raw);
        assert!(decode_tiff(&tiff[..tiff.len() - 4], 0).is_err());
        assert!(decode_tiff(&tiff, 1).is_err());
        assert!(decode_tiff(b"II*\0\x08\0\0\0", 0).is_err());
    }
}
