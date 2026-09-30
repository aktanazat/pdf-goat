//! GIF87a/89a decoding: LZW, global and local colour tables, interlace,
//! transparency, and frame compositing for multi-frame files.

use crate::error::{CodecError, Result, check_dimensions};
use crate::image::{DecodedImage, PixelLayout};

const CODEC: &str = "gif";

/// Header facts of a GIF file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GifInfo {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    /// The first frame's colour table (RGB triples) is an identity gray ramp,
    /// which Pillow reports as mode `L` instead of `P`.
    pub gray_palette: bool,
    /// The first frame has a transparent index.
    pub transparent: bool,
}

struct Frame<'a> {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    interlaced: bool,
    palette: &'a [u8],
    transparent: Option<u8>,
    disposal: u8,
    min_code_size: u8,
    data: Vec<u8>,
}

struct Parsed<'a> {
    width: u32,
    height: u32,
    background: u8,
    global_palette: &'a [u8],
    frames: Vec<Frame<'a>>,
}

fn parse<'a>(data: &'a [u8], max_frames: usize) -> Result<Parsed<'a>> {
    if data.len() < 13 || !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        return Err(CodecError::malformed(CODEC, "missing GIF header"));
    }
    let width = u32::from(u16::from_le_bytes([data[6], data[7]]));
    let height = u32::from(u16::from_le_bytes([data[8], data[9]]));
    check_dimensions(CODEC, width, height)?;
    let packed = data[10];
    let background = data[11];
    let mut pos = 13usize;
    let mut global_palette: &[u8] = &[];
    if packed & 0x80 != 0 {
        let n = 3 * (1usize << ((packed & 7) + 1));
        global_palette = data
            .get(pos..pos + n)
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated global colour table"))?;
        pos += n;
    }
    let mut frames = Vec::new();
    let mut transparent: Option<u8> = None;
    let mut disposal = 0u8;
    while pos < data.len() && frames.len() < max_frames {
        match data[pos] {
            0x3B => break,
            0x21 => {
                let label = *data
                    .get(pos + 1)
                    .ok_or_else(|| CodecError::malformed(CODEC, "truncated extension"))?;
                pos += 2;
                if label == 0xF9
                    && let Some(block) = data.get(pos + 1..pos + 5)
                    && data[pos] == 4
                {
                    disposal = (block[0] >> 2) & 7;
                    transparent = (block[0] & 1 == 1).then_some(block[3]);
                }
                pos = skip_sub_blocks(data, pos)?;
            }
            0x2C => {
                let d = data
                    .get(pos + 1..pos + 10)
                    .ok_or_else(|| CodecError::malformed(CODEC, "truncated image descriptor"))?;
                let left = u32::from(u16::from_le_bytes([d[0], d[1]]));
                let top = u32::from(u16::from_le_bytes([d[2], d[3]]));
                let fw = u32::from(u16::from_le_bytes([d[4], d[5]]));
                let fh = u32::from(u16::from_le_bytes([d[6], d[7]]));
                let fpacked = d[8];
                pos += 10;
                let mut palette = global_palette;
                if fpacked & 0x80 != 0 {
                    let n = 3 * (1usize << ((fpacked & 7) + 1));
                    palette = data.get(pos..pos + n).ok_or_else(|| {
                        CodecError::malformed(CODEC, "truncated local colour table")
                    })?;
                    pos += n;
                }
                let min_code_size = *data
                    .get(pos)
                    .ok_or_else(|| CodecError::malformed(CODEC, "missing LZW code size"))?;
                pos += 1;
                let mut lzw = Vec::new();
                pos = collect_sub_blocks(data, pos, &mut lzw)?;
                if fw == 0 || fh == 0 {
                    return Err(CodecError::malformed(CODEC, "empty frame"));
                }
                frames.push(Frame {
                    left,
                    top,
                    width: fw,
                    height: fh,
                    interlaced: fpacked & 0x40 != 0,
                    palette,
                    transparent,
                    disposal,
                    min_code_size,
                    data: lzw,
                });
                transparent = None;
                disposal = 0;
            }
            _ => {
                return Err(CodecError::malformed(
                    CODEC,
                    format!("unknown block 0x{:02X}", data[pos]),
                ));
            }
        }
    }
    if frames.is_empty() {
        return Err(CodecError::malformed(CODEC, "no image frames"));
    }
    Ok(Parsed {
        width,
        height,
        background,
        global_palette,
        frames,
    })
}

fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Result<usize> {
    loop {
        let n = *data
            .get(pos)
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated sub-blocks"))?;
        pos += 1;
        if n == 0 {
            return Ok(pos);
        }
        pos += usize::from(n);
    }
}

fn collect_sub_blocks(data: &[u8], mut pos: usize, out: &mut Vec<u8>) -> Result<usize> {
    loop {
        let n = *data
            .get(pos)
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated image data"))?;
        pos += 1;
        if n == 0 {
            return Ok(pos);
        }
        let block = data
            .get(pos..pos + usize::from(n))
            .ok_or_else(|| CodecError::malformed(CODEC, "truncated image data"))?;
        out.extend_from_slice(block);
        pos += usize::from(n);
    }
}

/// GIF-variant LZW (LSB-first codes, clear and end codes). Missing data
/// leaves the remaining pixels at index 0, as libgif and Pillow do.
fn lzw_decode(data: &[u8], min_code_size: u8, out: &mut [u8]) -> Result<()> {
    if !(1..=11).contains(&min_code_size) {
        return Err(CodecError::malformed(
            CODEC,
            format!("LZW code size {min_code_size}"),
        ));
    }
    let clear = 1u16 << min_code_size;
    let end = clear + 1;
    let mut code_size = u32::from(min_code_size) + 1;
    let mut prefix = vec![0u16; 4096];
    let mut suffix = vec![0u8; 4096];
    for (i, s) in suffix.iter_mut().enumerate().take(usize::from(clear)) {
        *s = i as u8;
    }
    let mut next = end + 1;
    let mut prev: Option<u16> = None;
    let mut bit_pos = 0usize;
    let total_bits = data.len() * 8;
    let mut written = 0usize;
    let mut stack = Vec::with_capacity(4096);

    while written < out.len() {
        if bit_pos + code_size as usize > total_bits {
            break;
        }
        let mut code = 0u32;
        for i in 0..code_size as usize {
            let p = bit_pos + i;
            code |= u32::from((data[p / 8] >> (p % 8)) & 1) << i;
        }
        bit_pos += code_size as usize;
        let code = code as u16;
        if code == clear {
            code_size = u32::from(min_code_size) + 1;
            next = end + 1;
            prev = None;
            continue;
        }
        if code == end {
            break;
        }
        // A code equal to `next` is the KwKwK case: the previous string
        // followed by its own first character.
        if code > next || (code == next && prev.is_none()) {
            return Err(CodecError::malformed(CODEC, "LZW code out of range"));
        }
        let entry = code;
        // Expand the string for `entry` (or prev + first(prev) when entry == next).
        stack.clear();
        if entry == next {
            let p = prev.unwrap_or(0);
            let mut c = p;
            while c >= clear + 2 {
                stack.push(suffix[usize::from(c)]);
                c = prefix[usize::from(c)];
            }
            stack.push(suffix[usize::from(c)]);
            let first = *stack.last().unwrap_or(&0);
            stack.insert(0, first);
        } else {
            let mut c = entry;
            while c >= clear + 2 {
                stack.push(suffix[usize::from(c)]);
                c = prefix[usize::from(c)];
            }
            stack.push(suffix[usize::from(c)]);
        }
        for &b in stack.iter().rev() {
            if written < out.len() {
                out[written] = b;
                written += 1;
            }
        }
        if let Some(p) = prev
            && next < 4096
        {
            prefix[usize::from(next)] = p;
            suffix[usize::from(next)] = *stack.last().unwrap_or(&0);
            next += 1;
            if next == (1 << code_size) && code_size < 12 {
                code_size += 1;
            }
        }
        prev = Some(entry);
    }
    Ok(())
}

fn frame_indices(frame: &Frame<'_>) -> Result<Vec<u8>> {
    let n = frame.width as usize * frame.height as usize;
    let mut raw = vec![0u8; n];
    lzw_decode(&frame.data, frame.min_code_size, &mut raw)?;
    if !frame.interlaced {
        return Ok(raw);
    }
    let w = frame.width as usize;
    let h = frame.height as usize;
    let mut out = vec![0u8; n];
    let mut src_row = 0usize;
    for (start, step) in [(0usize, 8usize), (4, 8), (2, 4), (1, 2)] {
        let mut y = start;
        while y < h {
            out[y * w..(y + 1) * w].copy_from_slice(&raw[src_row * w..(src_row + 1) * w]);
            src_row += 1;
            y += step;
        }
    }
    Ok(out)
}

/// Reads the header, counts frames, and inspects the first frame.
pub(crate) fn gif_info(data: &[u8]) -> Result<GifInfo> {
    let parsed = parse(data, usize::MAX)?;
    let first = &parsed.frames[0];
    Ok(GifInfo {
        width: parsed.width,
        height: parsed.height,
        frames: parsed.frames.len() as u32,
        gray_palette: is_gray_ramp(first.palette),
        transparent: first.transparent.is_some(),
    })
}

fn is_gray_ramp(palette: &[u8]) -> bool {
    palette
        .as_chunks::<3>()
        .0
        .iter()
        .enumerate()
        .all(|(i, c)| c[0] == c[1] && c[1] == c[2] && usize::from(c[0]) == i)
}

/// Decodes frame `index` composited onto the logical screen the way viewers
/// show it (earlier frames drawn with their disposal honoured).
///
/// The first frame comes back as [`PixelLayout::Indexed`] with its colour
/// table, or 8-bit RGBA when it has a transparent index (Pillow's
/// `transparency`); later frames are always RGBA because compositing may
/// mix colour tables.
pub(crate) fn decode_gif(data: &[u8], index: u32) -> Result<DecodedImage> {
    let parsed = parse(data, index as usize + 1)?;
    let frame = parsed.frames.get(index as usize).ok_or_else(|| {
        CodecError::invalid(CODEC, format!("frame {index} of {}", parsed.frames.len()))
    })?;
    let (sw, sh) = (parsed.width, parsed.height);
    let base = DecodedImage {
        width: sw,
        height: sh,
        layout: PixelLayout::Indexed,
        bit_depth: 8,
        data: Vec::new(),
        palette: None,
        icc_profile: None,
        dpi: None,
    };
    if index == 0
        && frame.transparent.is_none()
        && frame.left == 0
        && frame.top == 0
        && frame.width == sw
        && frame.height == sh
    {
        let indices = frame_indices(frame)?;
        let n = (frame.palette.len() / 3).clamp(1, 256);
        let data: Vec<u8> = indices.into_iter().map(|i| i.min((n - 1) as u8)).collect();
        let palette = if frame.palette.len() >= 3 {
            frame.palette[..n * 3].to_vec()
        } else {
            vec![0, 0, 0]
        };
        return Ok(DecodedImage {
            data,
            palette: Some(palette),
            ..base
        });
    }
    if index == 0 && frame.transparent.is_some() {
        // First frame with transparency: expand through the colour table.
        let indices = frame_indices(frame)?;
        let mut canvas = vec![0u8; sw as usize * sh as usize * 4];
        blit(&mut canvas, sw, sh, frame, &indices);
        return Ok(DecodedImage {
            layout: PixelLayout::Rgba,
            data: canvas,
            ..base
        });
    }

    // General case: composite every frame up to `index`.
    let bg = if parsed.frames[0].transparent.is_some() {
        [0, 0, 0, 0]
    } else {
        let p = parsed.global_palette;
        let i = usize::from(parsed.background) * 3;
        match p.get(i..i + 3) {
            Some(c) => [c[0], c[1], c[2], 255],
            None => [0, 0, 0, 0],
        }
    };
    let mut canvas: Vec<u8> = bg
        .iter()
        .copied()
        .cycle()
        .take(sw as usize * sh as usize * 4)
        .collect();
    for (i, f) in parsed.frames.iter().enumerate().take(index as usize + 1) {
        let indices = frame_indices(f)?;
        let before = if f.disposal == 3 {
            Some(canvas.clone())
        } else {
            None
        };
        blit(&mut canvas, sw, sh, f, &indices);
        if i < index as usize {
            match f.disposal {
                2 => clear_rect(&mut canvas, sw, sh, f, bg),
                3 => {
                    if let Some(b) = before {
                        canvas = b;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(DecodedImage {
        layout: PixelLayout::Rgba,
        data: canvas,
        ..base
    })
}

fn blit(canvas: &mut [u8], sw: u32, sh: u32, f: &Frame<'_>, indices: &[u8]) {
    let n = f.palette.len() / 3;
    for y in 0..f.height {
        let cy = f.top + y;
        if cy >= sh {
            break;
        }
        for x in 0..f.width {
            let cx = f.left + x;
            if cx >= sw {
                break;
            }
            let idx = indices[(y * f.width + x) as usize];
            if f.transparent == Some(idx) {
                continue;
            }
            let i = usize::from(idx).min(n.saturating_sub(1));
            let rgb = f.palette.get(i * 3..i * 3 + 3).unwrap_or(&[0, 0, 0]);
            let o = ((cy * sw + cx) * 4) as usize;
            canvas[o..o + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
}

fn clear_rect(canvas: &mut [u8], sw: u32, sh: u32, f: &Frame<'_>, bg: [u8; 4]) {
    for y in f.top..(f.top + f.height).min(sh) {
        for x in f.left..(f.left + f.width).min(sw) {
            let o = ((y * sw + x) * 4) as usize;
            canvas[o..o + 4].copy_from_slice(&bg);
        }
    }
}

#[cfg(test)]
pub(crate) mod test_encoder {
    //! Minimal GIF writer (uncompressed-style LZW with periodic clears) for
    //! decoder tests.

    /// Emits every index as a literal LZW code with `min_code_size` 8,
    /// tracking the table growth exactly as a decoder does so the code size
    /// stays in step. Valid GIF LZW, if unoptimised.
    fn lzw_literal(indices: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut nbits = 0u32;
        let mut code_size = 9u32;
        let mut next = 258u32;
        let push = |code: u32, size: u32, acc: &mut u32, nbits: &mut u32, out: &mut Vec<u8>| {
            *acc |= code << *nbits;
            *nbits += size;
            while *nbits >= 8 {
                out.push((*acc & 0xFF) as u8);
                *acc >>= 8;
                *nbits -= 8;
            }
        };
        push(256, code_size, &mut acc, &mut nbits, &mut out);
        let mut first = true;
        for &i in indices {
            push(u32::from(i), code_size, &mut acc, &mut nbits, &mut out);
            if first {
                first = false;
            } else if next < 4096 {
                next += 1;
                if next == (1 << code_size) && code_size < 12 {
                    code_size += 1;
                }
            }
        }
        push(257, code_size, &mut acc, &mut nbits, &mut out);
        if nbits > 0 {
            out.push((acc & 0xFF) as u8);
        }
        out
    }

    pub(crate) struct TestFrame<'a> {
        pub left: u16,
        pub top: u16,
        pub width: u16,
        pub height: u16,
        pub indices: &'a [u8],
        pub transparent: Option<u8>,
        pub disposal: u8,
        pub interlaced: bool,
    }

    /// Builds a GIF89a with a 256-entry global palette and the given frames.
    pub(crate) fn build_gif(
        width: u16,
        height: u16,
        palette: &[u8; 768],
        frames: &[TestFrame<'_>],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GIF89a");
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&[0xF7, 0, 0]);
        out.extend_from_slice(palette);
        for f in frames {
            if f.transparent.is_some() || f.disposal != 0 {
                let packed = (f.disposal << 2) | u8::from(f.transparent.is_some());
                out.extend_from_slice(&[
                    0x21,
                    0xF9,
                    4,
                    packed,
                    0,
                    0,
                    f.transparent.unwrap_or(0),
                    0,
                ]);
            }
            out.push(0x2C);
            out.extend_from_slice(&f.left.to_le_bytes());
            out.extend_from_slice(&f.top.to_le_bytes());
            out.extend_from_slice(&f.width.to_le_bytes());
            out.extend_from_slice(&f.height.to_le_bytes());
            out.push(if f.interlaced { 0x40 } else { 0 });
            out.push(8);
            let indices: Vec<u8> = if f.interlaced {
                let w = usize::from(f.width);
                let h = usize::from(f.height);
                let mut rows = Vec::with_capacity(f.indices.len());
                for (start, step) in [(0usize, 8usize), (4, 8), (2, 4), (1, 2)] {
                    let mut y = start;
                    while y < h {
                        rows.extend_from_slice(&f.indices[y * w..(y + 1) * w]);
                        y += step;
                    }
                }
                rows
            } else {
                f.indices.to_vec()
            };
            let lzw = lzw_literal(&indices);
            for chunk in lzw.chunks(255) {
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
            out.push(0);
        }
        out.push(0x3B);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_encoder::{TestFrame, build_gif};
    use super::*;

    fn palette() -> [u8; 768] {
        let mut p = [0u8; 768];
        for i in 0..256 {
            p[i * 3] = i as u8;
            p[i * 3 + 1] = (i * 7 % 256) as u8;
            p[i * 3 + 2] = (255 - i) as u8;
        }
        p
    }

    #[test]
    fn first_frame_keeps_indices_and_palette() {
        let (w, h) = (300u16, 3u16);
        let indices: Vec<u8> = (0..u32::from(w) * u32::from(h))
            .map(|i| (i * 31 % 256) as u8)
            .collect();
        let gif = build_gif(
            w,
            h,
            &palette(),
            &[TestFrame {
                left: 0,
                top: 0,
                width: w,
                height: h,
                indices: &indices,
                transparent: None,
                disposal: 0,
                interlaced: false,
            }],
        );
        let img = decode_gif(&gif, 0).unwrap();
        assert_eq!((img.layout, img.bit_depth), (PixelLayout::Indexed, 8));
        assert_eq!(img.data, indices);
        assert_eq!(img.palette.as_deref(), Some(&palette()[..]));
        let info = gif_info(&gif).unwrap();
        assert_eq!(
            (info.frames, info.gray_palette, info.transparent),
            (1, false, false)
        );
    }

    #[test]
    fn interlaced_rows_are_reordered() {
        let (w, h) = (4u16, 11u16);
        let indices: Vec<u8> = (0..u32::from(w) * u32::from(h))
            .map(|i| (i / u32::from(w)) as u8)
            .collect();
        let gif = build_gif(
            w,
            h,
            &palette(),
            &[TestFrame {
                left: 0,
                top: 0,
                width: w,
                height: h,
                indices: &indices,
                transparent: None,
                disposal: 0,
                interlaced: true,
            }],
        );
        let img = decode_gif(&gif, 0).unwrap();
        assert_eq!(img.data, indices);
    }

    #[test]
    fn transparent_first_frame_expands_to_rgba() {
        let indices = [5u8, 9, 9, 5];
        let gif = build_gif(
            2,
            2,
            &palette(),
            &[TestFrame {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
                indices: &indices,
                transparent: Some(9),
                disposal: 0,
                interlaced: false,
            }],
        );
        let img = decode_gif(&gif, 0).unwrap();
        assert_eq!(img.layout, PixelLayout::Rgba);
        let p = palette();
        assert_eq!(&img.data[..4], &[p[15], p[16], p[17], 255]);
        assert_eq!(&img.data[4..8], &[0, 0, 0, 0]);
        assert!(gif_info(&gif).unwrap().transparent);
    }

    #[test]
    fn second_frame_composites_over_first_with_disposal() {
        let first = [1u8, 1, 1, 1];
        let second = [2u8];
        let gif = build_gif(
            2,
            2,
            &palette(),
            &[
                TestFrame {
                    left: 0,
                    top: 0,
                    width: 2,
                    height: 2,
                    indices: &first,
                    transparent: None,
                    disposal: 0,
                    interlaced: false,
                },
                TestFrame {
                    left: 1,
                    top: 1,
                    width: 1,
                    height: 1,
                    indices: &second,
                    transparent: None,
                    disposal: 0,
                    interlaced: false,
                },
            ],
        );
        assert_eq!(gif_info(&gif).unwrap().frames, 2);
        let img = decode_gif(&gif, 1).unwrap();
        let p = palette();
        assert_eq!(&img.data[..4], &[p[3], p[4], p[5], 255]);
        assert_eq!(&img.data[12..16], &[p[6], p[7], p[8], 255]);
    }

    #[test]
    fn truncated_gif_is_an_error() {
        let indices = [1u8; 16];
        let gif = build_gif(
            4,
            4,
            &palette(),
            &[TestFrame {
                left: 0,
                top: 0,
                width: 4,
                height: 4,
                indices: &indices,
                transparent: None,
                disposal: 0,
                interlaced: false,
            }],
        );
        for cut in [5, 12, 700, 790] {
            assert!(
                decode_gif(&gif[..cut.min(gif.len() - 1)], 0).is_err(),
                "cut {cut}"
            );
        }
    }
}
