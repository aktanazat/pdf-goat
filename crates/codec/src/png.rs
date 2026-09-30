//! PNG decoding (every colour type and bit depth, Adam7 interlace, `PLTE`,
//! `tRNS`, `pHYs`, `iCCP`) and encoding (gray, gray+alpha, RGB, RGBA at 8
//! bits and 1-bit gray), over `miniz_oxide` for zlib.

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::{DecodedImage, PixelLayout, check_len, row_stride};

const CODEC: &str = "png";
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];

/// Metadata read from the chunks before the image data.
#[derive(Debug, Clone, PartialEq)]
pub struct PngInfo {
    pub width: u32,
    pub height: u32,
    /// 1, 2, 4, 8, or 16.
    pub bit_depth: u8,
    /// PNG colour type: 0 gray, 2 RGB, 3 indexed, 4 gray+alpha, 6 RGBA.
    pub color_type: u8,
    /// Adam7 interlaced.
    pub interlaced: bool,
    /// The `PLTE` entries as RGB triples.
    pub palette: Option<Vec<u8>>,
    /// A `tRNS` chunk is present: per-index alpha for palettes, or a colour
    /// key for gray and RGB.
    pub has_transparency: bool,
    /// `iCCP` profile after decompression.
    pub icc_profile: Option<Vec<u8>>,
    /// `pHYs` resolution converted to dots per inch when its unit is metres.
    pub dpi: Option<(f64, f64)>,
    /// `pHYs` pixel aspect ratio (x, y) when its unit is unknown, which Pillow
    /// reports as `aspect` and img2pdf turns into a dpi ratio.
    pub aspect: Option<(u32, u32)>,
}

impl PngInfo {
    /// Samples per pixel for the colour type.
    pub fn channels(&self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            _ => 4,
        }
    }
}

struct Chunks<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Chunks<'a> {
    fn next_chunk(&mut self) -> Result<Option<(&'a [u8; 4], &'a [u8])>> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let head = self
            .data
            .get(self.pos..self.pos + 8)
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated chunk header"))?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind: &[u8; 4] = head[4..8]
            .try_into()
            .map_err(|_| CodecError::malformed(CODEC, "chunk type"))?;
        let body_start = self.pos + 8;
        let body = self.data.get(body_start..body_start + len).ok_or_else(|| {
            CodecError::malformed(
                CODEC,
                format!("truncated {} chunk", String::from_utf8_lossy(kind)),
            )
        })?;
        // The CRC is not verified: renderers accept files with bad CRCs.
        self.pos = body_start + len + 4;
        Ok(Some((kind, body)))
    }
}

fn parse_ihdr(body: &[u8]) -> Result<PngInfo> {
    if body.len() < 13 {
        return Err(CodecError::malformed(CODEC, "short IHDR"));
    }
    let width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    check_dimensions(CODEC, width, height)?;
    let bit_depth = body[8];
    let color_type = body[9];
    let valid = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        _ => false,
    };
    if !valid {
        return Err(CodecError::malformed(
            CODEC,
            format!("colour type {color_type} with bit depth {bit_depth}"),
        ));
    }
    if body[10] != 0 || body[11] != 0 {
        return Err(CodecError::unsupported(
            CODEC,
            "compression or filter method",
        ));
    }
    let interlaced = match body[12] {
        0 => false,
        1 => true,
        m => {
            return Err(CodecError::malformed(
                CODEC,
                format!("interlace method {m}"),
            ));
        }
    };
    Ok(PngInfo {
        width,
        height,
        bit_depth,
        color_type,
        interlaced,
        palette: None,
        has_transparency: false,
        icc_profile: None,
        dpi: None,
        aspect: None,
    })
}

struct Parsed {
    info: PngInfo,
    trns: Option<Vec<u8>>,
    idat: Vec<u8>,
}

fn parse(data: &[u8], want_idat: bool) -> Result<Parsed> {
    if !data.starts_with(&SIGNATURE) {
        return Err(CodecError::malformed(CODEC, "missing signature"));
    }
    let mut chunks = Chunks { data, pos: 8 };
    let (kind, body) = chunks
        .next_chunk()?
        .ok_or_else(|| CodecError::malformed(CODEC, "no IHDR"))?;
    if kind != b"IHDR" {
        return Err(CodecError::malformed(CODEC, "first chunk is not IHDR"));
    }
    let mut info = parse_ihdr(body)?;
    let mut trns = None;
    let mut idat = Vec::new();
    let mut seen_iend = false;
    while let Some((kind, body)) = chunks.next_chunk()? {
        match kind {
            b"PLTE" => {
                if body.len() % 3 != 0 || body.is_empty() || body.len() > 256 * 3 {
                    return Err(CodecError::malformed(CODEC, "PLTE length"));
                }
                info.palette = Some(body.to_vec());
            }
            b"tRNS" => {
                info.has_transparency = true;
                trns = Some(body.to_vec());
            }
            b"pHYs" => {
                if body.len() >= 9 {
                    let x = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    let y = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                    if body[8] == 1 {
                        info.dpi = Some((f64::from(x) * 0.0254, f64::from(y) * 0.0254));
                    } else if x != 0 && y != 0 {
                        info.aspect = Some((x, y));
                    }
                }
            }
            b"iCCP" => {
                if let Some(nul) = body.iter().position(|&b| b == 0)
                    && body.len() > nul + 2
                    && body[nul + 1] == 0
                {
                    let compressed = &body[nul + 2..];
                    if let Ok(profile) =
                        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(compressed, 1 << 24)
                    {
                        info.icc_profile = Some(profile);
                    }
                }
            }
            b"IDAT" => {
                if want_idat {
                    idat.extend_from_slice(body);
                }
            }
            b"IEND" => {
                seen_iend = true;
                break;
            }
            _ => {}
        }
    }
    if info.color_type == 3 && info.palette.is_none() {
        return Err(CodecError::malformed(CODEC, "indexed image without PLTE"));
    }
    if want_idat && idat.is_empty() {
        return Err(CodecError::malformed(
            CODEC,
            if seen_iend {
                "no IDAT"
            } else {
                "no IDAT before end of data"
            },
        ));
    }
    Ok(Parsed { info, trns, idat })
}

/// Reads the chunks before the image data.
pub fn png_info(data: &[u8]) -> Result<PngInfo> {
    Ok(parse(data, false)?.info)
}

/// True when the bytes start with the PNG signature.
pub fn is_png(data: &[u8]) -> bool {
    data.starts_with(&SIGNATURE)
}

/// Decodes a PNG. Palette images stay [`PixelLayout::Indexed`] with the
/// palette attached unless the `tRNS` chunk gives per-index alpha, in which
/// case they expand to 8-bit RGBA; a colour-key `tRNS` on gray or RGB images
/// expands to 8-bit gray+alpha or RGBA. 16-bit samples stay 16-bit
/// big-endian, sub-byte gray and indexed samples stay packed.
pub fn decode_png(data: &[u8]) -> Result<DecodedImage> {
    let Parsed { info, trns, idat } = parse(data, true)?;
    let channels = info.channels();
    let bpp_bits = channels * usize::from(info.bit_depth);
    let bpp = bpp_bits.div_ceil(8); // filter byte distance
    let full_stride = row_stride(info.width, channels, info.bit_depth);
    let max_raw = (full_stride + 1) * info.height as usize + 64;
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&idat, max_raw)
        .map_err(|e| CodecError::malformed(CODEC, format!("zlib: {:?}", e.status)))?;

    let mut pixels = vec![0u8; full_stride * info.height as usize];
    if info.interlaced {
        let mut pos = 0usize;
        for &(xs, ys, xo, yo) in &ADAM7 {
            let pw = (info.width as usize + xs - 1 - xo) / xs;
            let ph = (info.height as usize + ys - 1 - yo) / ys;
            if pw == 0 || ph == 0 {
                continue;
            }
            let stride = (pw * bpp_bits).div_ceil(8);
            let mut prev = vec![0u8; stride];
            let mut cur = vec![0u8; stride];
            for py in 0..ph {
                let end = pos + 1 + stride;
                let line = raw
                    .get(pos..end)
                    .ok_or_else(|| CodecError::malformed(CODEC, "image data ends early"))?;
                unfilter(line[0], &line[1..], &prev, bpp, &mut cur)?;
                pos = end;
                let y = yo + py * ys;
                for px in 0..pw {
                    let x = xo + px * xs;
                    copy_pixel(
                        &cur,
                        px,
                        &mut pixels[y * full_stride..(y + 1) * full_stride],
                        x,
                        bpp_bits,
                    );
                }
                std::mem::swap(&mut prev, &mut cur);
            }
        }
    } else {
        let mut prev = vec![0u8; full_stride];
        let mut cur = vec![0u8; full_stride];
        for y in 0..info.height as usize {
            let start = y * (full_stride + 1);
            let line = raw
                .get(start..start + full_stride + 1)
                .ok_or_else(|| CodecError::malformed(CODEC, "image data ends early"))?;
            unfilter(line[0], &line[1..], &prev, bpp, &mut cur)?;
            pixels[y * full_stride..(y + 1) * full_stride].copy_from_slice(&cur);
            std::mem::swap(&mut prev, &mut cur);
        }
    }

    let (width, height) = (info.width, info.height);
    let mut image = DecodedImage {
        width,
        height,
        layout: PixelLayout::Gray,
        bit_depth: info.bit_depth,
        data: pixels,
        palette: None,
        icc_profile: info.icc_profile.clone(),
        dpi: info.dpi,
    };
    match info.color_type {
        0 => match trns {
            Some(t) if t.len() >= 2 => {
                let key = u16::from_be_bytes([t[0], t[1]]);
                image = colour_key_gray(&image, key);
            }
            _ => {}
        },
        2 => {
            image.layout = PixelLayout::Rgb;
            if let Some(t) = trns
                && t.len() >= 6
            {
                let key = [
                    u16::from_be_bytes([t[0], t[1]]),
                    u16::from_be_bytes([t[2], t[3]]),
                    u16::from_be_bytes([t[4], t[5]]),
                ];
                image = colour_key_rgb(&image, key);
            }
        }
        3 => {
            image.layout = PixelLayout::Indexed;
            let palette = info.palette.clone().unwrap_or_default();
            match trns {
                Some(alpha) => image = expand_indexed_alpha(&image, &palette, &alpha),
                None => image.palette = Some(palette),
            }
        }
        4 => image.layout = PixelLayout::GrayAlpha,
        _ => image.layout = PixelLayout::Rgba,
    }
    Ok(image)
}

/// (x step, y step, x offset, y offset) of each Adam7 pass.
const ADAM7: [(usize, usize, usize, usize); 7] = [
    (8, 8, 0, 0),
    (8, 8, 4, 0),
    (4, 8, 0, 4),
    (4, 4, 2, 0),
    (2, 4, 0, 2),
    (2, 2, 1, 0),
    (1, 2, 0, 1),
];

fn copy_pixel(src: &[u8], sx: usize, dst: &mut [u8], dx: usize, bpp_bits: usize) {
    if bpp_bits >= 8 {
        let n = bpp_bits / 8;
        dst[dx * n..dx * n + n].copy_from_slice(&src[sx * n..sx * n + n]);
    } else {
        let per_byte = 8 / bpp_bits;
        let mask = ((1u16 << bpp_bits) - 1) as u8;
        let sshift = 8 - bpp_bits * (sx % per_byte + 1);
        let v = (src[sx / per_byte] >> sshift) & mask;
        let dshift = 8 - bpp_bits * (dx % per_byte + 1);
        let d = &mut dst[dx / per_byte];
        *d = (*d & !(mask << dshift)) | (v << dshift);
    }
}

fn unfilter(kind: u8, line: &[u8], prev: &[u8], bpp: usize, out: &mut [u8]) -> Result<()> {
    match kind {
        0 => out.copy_from_slice(line),
        1 => {
            for i in 0..line.len() {
                let left = if i >= bpp { out[i - bpp] } else { 0 };
                out[i] = line[i].wrapping_add(left);
            }
        }
        2 => {
            for i in 0..line.len() {
                out[i] = line[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..line.len() {
                let left = if i >= bpp { u16::from(out[i - bpp]) } else { 0 };
                out[i] = line[i].wrapping_add(((left + u16::from(prev[i])) / 2) as u8);
            }
        }
        4 => {
            for i in 0..line.len() {
                let a = if i >= bpp { out[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                out[i] = line[i].wrapping_add(paeth(a, b, c));
            }
        }
        f => return Err(CodecError::malformed(CODEC, format!("filter type {f}"))),
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn colour_key_gray(image: &DecodedImage, key: u16) -> DecodedImage {
    let samples =
        crate::pixels::unpack_samples(&image.data, image.width, image.height, image.bit_depth, 1)
            .unwrap_or_default();
    let max = (1u32 << image.bit_depth) - 1;
    let mut data = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let g = if image.bit_depth == 16 {
            (s >> 8) as u8
        } else {
            (u32::from(s) * 255 / max) as u8
        };
        data.push(g);
        data.push(if s == key { 0 } else { 255 });
    }
    DecodedImage {
        layout: PixelLayout::GrayAlpha,
        bit_depth: 8,
        data,
        palette: None,
        ..image.clone()
    }
}

fn colour_key_rgb(image: &DecodedImage, key: [u16; 3]) -> DecodedImage {
    let mut data = Vec::with_capacity(image.width as usize * image.height as usize * 4);
    if image.bit_depth == 16 {
        for px in image.data.as_chunks::<6>().0.iter() {
            let s = [
                u16::from_be_bytes([px[0], px[1]]),
                u16::from_be_bytes([px[2], px[3]]),
                u16::from_be_bytes([px[4], px[5]]),
            ];
            data.extend_from_slice(&[px[0], px[2], px[4], if s == key { 0 } else { 255 }]);
        }
    } else {
        for px in image.data.as_chunks::<3>().0.iter() {
            let s = [u16::from(px[0]), u16::from(px[1]), u16::from(px[2])];
            data.extend_from_slice(&[px[0], px[1], px[2], if s == key { 0 } else { 255 }]);
        }
    }
    DecodedImage {
        layout: PixelLayout::Rgba,
        bit_depth: 8,
        data,
        palette: None,
        ..image.clone()
    }
}

fn expand_indexed_alpha(image: &DecodedImage, palette: &[u8], alpha: &[u8]) -> DecodedImage {
    let indices =
        crate::pixels::unpack_samples(&image.data, image.width, image.height, image.bit_depth, 1)
            .unwrap_or_default();
    let entries = palette.len() / 3;
    let mut data = Vec::with_capacity(indices.len() * 4);
    for i in indices {
        let idx = usize::from(i).min(entries.saturating_sub(1));
        let rgb = palette.get(idx * 3..idx * 3 + 3).unwrap_or(&[0, 0, 0]);
        let a = alpha.get(usize::from(i)).copied().unwrap_or(255);
        data.extend_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
    }
    DecodedImage {
        layout: PixelLayout::Rgba,
        bit_depth: 8,
        data,
        palette: None,
        ..image.clone()
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Sample layout accepted by [`encode_png`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngColor {
    /// 8-bit gray.
    Gray,
    /// 8-bit gray plus alpha.
    GrayAlpha,
    /// 8-bit RGB.
    Rgb,
    /// 8-bit RGBA.
    Rgba,
    /// 1-bit gray, rows packed MSB first and padded to a byte, 1 = white.
    Gray1,
}

impl PngColor {
    fn color_type(self) -> u8 {
        match self {
            Self::Gray | Self::Gray1 => 0,
            Self::GrayAlpha => 4,
            Self::Rgb => 2,
            Self::Rgba => 6,
        }
    }

    fn bit_depth(self) -> u8 {
        if self == Self::Gray1 { 1 } else { 8 }
    }

    fn channels(self) -> usize {
        match self {
            Self::Gray | Self::Gray1 => 1,
            Self::GrayAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
}

/// Encodes packed rows as a non-interlaced PNG, with adaptive per-row
/// filtering and an optional `pHYs` chunk from `dpi`.
pub fn encode_png(
    data: &[u8],
    width: u32,
    height: u32,
    color: PngColor,
    dpi: Option<(f64, f64)>,
) -> Result<Vec<u8>> {
    check_dimensions(CODEC, width, height)?;
    let channels = color.channels();
    let bit_depth = color.bit_depth();
    check_len(CODEC, data, width, height, channels, bit_depth)?;
    let stride = row_stride(width, channels, bit_depth);
    let bpp = if bit_depth == 1 { 1 } else { channels };

    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    let mut prev = vec![0u8; stride];
    let mut candidates: [Vec<u8>; 5] = std::array::from_fn(|_| vec![0u8; stride]);
    for row in data.chunks_exact(stride) {
        let filter = if bit_depth == 1 {
            0
        } else {
            for (kind, buf) in candidates.iter_mut().enumerate() {
                apply_filter(kind as u8, row, &prev, bpp, buf);
            }
            // Heuristic from the PNG spec: the smallest sum of absolute values.
            (0..5)
                .min_by_key(|&k| {
                    candidates[k]
                        .iter()
                        .map(|&b| u64::from((b as i8).unsigned_abs()))
                        .sum::<u64>()
                })
                .unwrap_or(0)
        };
        raw.push(filter as u8);
        if bit_depth == 1 {
            raw.extend_from_slice(row);
        } else {
            raw.extend_from_slice(&candidates[filter]);
        }
        prev.copy_from_slice(row);
    }
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);

    let mut out = Vec::with_capacity(compressed.len() + 128);
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, color.color_type(), 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);
    if let Some((x, y)) = dpi {
        let to_ppm = |v: f64| (v / 0.0254).round().clamp(0.0, f64::from(u32::MAX)) as u32;
        let mut phys = Vec::with_capacity(9);
        phys.extend_from_slice(&to_ppm(x).to_be_bytes());
        phys.extend_from_slice(&to_ppm(y).to_be_bytes());
        phys.push(1);
        write_chunk(&mut out, b"pHYs", &phys);
    }
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

fn apply_filter(kind: u8, row: &[u8], prev: &[u8], bpp: usize, out: &mut [u8]) {
    for i in 0..row.len() {
        let a = if i >= bpp { row[i - bpp] } else { 0 };
        let b = prev[i];
        let c = if i >= bpp { prev[i - bpp] } else { 0 };
        let pred = match kind {
            0 => 0,
            1 => a,
            2 => b,
            3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
            _ => paeth(a, b, c),
        };
        out[i] = row[i].wrapping_sub(pred);
    }
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(body);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let mut c = self.0 ^ u32::from(b);
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            self.0 = c;
        }
    }

    fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32, channels: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(width as usize * height as usize * channels);
        for y in 0..height {
            for x in 0..width {
                for c in 0..channels {
                    v.push(((x * 37 + y * 11 + c as u32 * 71) % 256) as u8);
                }
            }
        }
        v
    }

    macro_rules! round_trip {
        ($name:ident, $color:expr, $layout:expr, $channels:expr) => {
            #[test]
            fn $name() {
                let (w, h) = (23u32, 9u32);
                let data = gradient(w, h, $channels);
                let png = encode_png(&data, w, h, $color, Some((150.0, 300.0))).unwrap();
                let decoded = decode_png(&png).unwrap();
                assert_eq!((decoded.width, decoded.height), (w, h));
                assert_eq!(decoded.layout, $layout);
                assert_eq!(decoded.bit_depth, 8);
                assert_eq!(decoded.data, data);
                let (dx, dy) = decoded.dpi.unwrap();
                assert!(
                    (dx - 150.0).abs() < 0.05 && (dy - 300.0).abs() < 0.05,
                    "dpi {dx} {dy}"
                );
            }
        };
    }

    round_trip!(gray_round_trip, PngColor::Gray, PixelLayout::Gray, 1);
    round_trip!(
        gray_alpha_round_trip,
        PngColor::GrayAlpha,
        PixelLayout::GrayAlpha,
        2
    );
    round_trip!(rgb_round_trip, PngColor::Rgb, PixelLayout::Rgb, 3);
    round_trip!(rgba_round_trip, PngColor::Rgba, PixelLayout::Rgba, 4);

    #[test]
    fn one_bit_round_trip() {
        let (w, h) = (13u32, 5u32);
        let stride = row_stride(w, 1, 1);
        let mut data = vec![0u8; stride * h as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(0x5B) ^ 0x3C;
        }
        for row in data.chunks_exact_mut(stride) {
            row[stride - 1] &= 0xF8;
        }
        let png = encode_png(&data, w, h, PngColor::Gray1, None).unwrap();
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.layout, decoded.bit_depth), (PixelLayout::Gray, 1));
        assert_eq!(decoded.data, data);
        assert_eq!(decoded.dpi, None);
    }

    #[test]
    fn encoder_rejects_wrong_buffer_length() {
        let err = encode_png(&[0; 10], 4, 4, PngColor::Gray, None).unwrap_err();
        assert!(matches!(err, CodecError::InvalidParams { .. }));
    }

    #[test]
    fn truncated_png_is_an_error_not_a_panic() {
        let data = gradient(16, 16, 3);
        let png = encode_png(&data, 16, 16, PngColor::Rgb, None).unwrap();
        for cut in [7, 20, 33, 40, png.len() / 2, png.len() - 5] {
            assert!(decode_png(&png[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn crc_matches_png_reference_value() {
        // The IEND chunk CRC is fixed by the PNG specification.
        let mut crc = Crc32::new();
        crc.update(b"IEND");
        assert_eq!(crc.finish(), 0xAE42_6082);
    }
}
