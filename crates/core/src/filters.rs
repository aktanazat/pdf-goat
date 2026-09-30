//! Stream filters (ISO 32000-2 7.4): FlateDecode and LZWDecode with PNG and
//! TIFF predictors, ASCIIHexDecode, ASCII85Decode, and RunLengthDecode.
//! Image filters (DCT, JPX, JBIG2, CCITTFax) are not decoded: decoding stops
//! before the first one and reports it.

use std::borrow::Cow;

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

use crate::error::{Error, Result};
use crate::lexer::is_whitespace;
use crate::object::{Dict, Name, Object};

/// Default bound on the decoded size of one stream: 512 MiB.
pub const DEFAULT_DECODE_LIMIT: usize = 512 << 20;

/// The result of running a stream's filter chain.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedStream {
    /// The data after every filter before the first image filter.
    pub data: Vec<u8>,
    /// The image filter decoding stopped at, when there is one.
    pub stopped: Option<StoppedFilter>,
}

/// An image filter left for an image codec.
#[derive(Clone, Debug, PartialEq)]
pub struct StoppedFilter {
    /// The full filter name, abbreviations expanded (`DCTDecode`, not `DCT`).
    pub filter: Name,
    /// Its `/DecodeParms` entry.
    pub parms: Option<Dict>,
}

/// The full name for an inline-image abbreviation; other names unchanged.
pub fn canonical_filter_name(name: &[u8]) -> &[u8] {
    match name {
        b"AHx" => b"ASCIIHexDecode",
        b"A85" => b"ASCII85Decode",
        b"LZW" => b"LZWDecode",
        b"Fl" => b"FlateDecode",
        b"RL" => b"RunLengthDecode",
        b"CCF" => b"CCITTFaxDecode",
        b"DCT" => b"DCTDecode",
        other => other,
    }
}

/// True for filters that produce image samples: DCT, JPX, JBIG2, CCITTFax
/// (abbreviations accepted).
pub fn is_image_filter(name: &[u8]) -> bool {
    matches!(
        canonical_filter_name(name),
        b"DCTDecode" | b"JPXDecode" | b"JBIG2Decode" | b"CCITTFaxDecode"
    )
}

/// Pair each filter with its parameters from direct `/Filter` and
/// `/DecodeParms` values (a name or array; a dictionary, array, or null).
/// References inside them must be resolved by the caller first.
pub fn filter_chain(filter: Option<&Object>, parms: Option<&Object>) -> Vec<(Name, Option<Dict>)> {
    let parms_at = |index: usize, single: bool| -> Option<Dict> {
        match parms {
            Some(Object::Array(items)) => items.get(index).and_then(Object::as_dict).cloned(),
            Some(Object::Dict(dict)) if single || index == 0 => Some(dict.clone()),
            _ => None,
        }
    };
    match filter {
        Some(Object::Name(name)) => vec![(name.clone(), parms_at(0, true))],
        Some(Object::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                item.as_name()
                    .map(|name| (Name::new(name), parms_at(i, false)))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Run `filters` in order over `data`, stopping before an image filter.
/// Output larger than `limit` bytes is an error.
pub fn decode(
    data: &[u8],
    filters: &[(Name, Option<Dict>)],
    limit: usize,
) -> Result<DecodedStream> {
    let mut current: Cow<'_, [u8]> = Cow::Borrowed(data);
    for (name, parms) in filters {
        let canonical = canonical_filter_name(name.as_bytes());
        if is_image_filter(canonical) {
            let stopped = StoppedFilter {
                filter: Name::new(canonical),
                parms: parms.clone(),
            };
            return Ok(DecodedStream {
                data: current.into_owned(),
                stopped: Some(stopped),
            });
        }
        let parms = parms.as_ref();
        let out = match canonical {
            b"FlateDecode" => predict(flate_decode(&current, limit)?, parms, limit)?,
            b"LZWDecode" => {
                let early = parms.and_then(|p| p.get_i64(b"EarlyChange")).unwrap_or(1) != 0;
                predict(lzw_decode(&current, early, limit)?, parms, limit)?
            }
            b"ASCIIHexDecode" => ascii_hex_decode(&current),
            b"ASCII85Decode" => ascii85_decode(&current),
            b"RunLengthDecode" => run_length_decode(&current, limit)?,
            // Decryption happens when the stream is loaded.
            b"Crypt" => continue,
            other => {
                return Err(Error::Unsupported(format!(
                    "stream filter /{}",
                    String::from_utf8_lossy(other)
                )));
            }
        };
        check_limit(out.len(), limit)?;
        current = Cow::Owned(out);
    }
    Ok(DecodedStream {
        data: current.into_owned(),
        stopped: None,
    })
}

fn check_limit(len: usize, limit: usize) -> Result<()> {
    if len > limit {
        return Err(Error::LimitExceeded(format!(
            "decoded stream is larger than {limit} bytes"
        )));
    }
    Ok(())
}

/// zlib-compress at the default level.
pub fn flate_encode(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// Inflate zlib (or raw deflate) data. Corrupt or truncated input returns
/// what decoded before the damage; a bad checksum is ignored.
pub fn flate_decode(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let start = data
        .iter()
        .take(8)
        .position(|&b| !is_whitespace(b))
        .unwrap_or(0);
    let body = &data[start..];
    let zlib_header = body.len() >= 2
        && body[0] & 0x0F == 8
        && body[0] >> 4 <= 7
        && (u16::from(body[0]) << 8 | u16::from(body[1])) % 31 == 0;
    if zlib_header {
        let (out, ok) = inflate_all(body, DataFormat::ZLibIgnoreChecksum, limit)?;
        if ok || !out.is_empty() {
            return Ok(out);
        }
    }
    let (raw, _) = inflate_all(body, DataFormat::Raw, limit)?;
    Ok(raw)
}

/// Returns the output and whether the stream ended cleanly.
fn inflate_all(data: &[u8], format: DataFormat, limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut state = InflateState::new_boxed(format);
    let mut out = Vec::with_capacity(data.len().saturating_mul(3).min(limit));
    let mut buf = vec![0u8; 64 * 1024];
    let mut input = data;
    loop {
        let result = inflate(&mut state, input, &mut buf, MZFlush::None);
        input = input.get(result.bytes_consumed..).unwrap_or(&[]);
        out.extend_from_slice(&buf[..result.bytes_written]);
        check_limit(out.len(), limit)?;
        match result.status {
            Ok(MZStatus::StreamEnd) => return Ok((out, true)),
            Ok(MZStatus::Ok) if result.bytes_consumed > 0 || result.bytes_written > 0 => {}
            _ => return Ok((out, false)),
        }
    }
}

/// Undo a PNG (10-15) or TIFF (2) predictor per `/DecodeParms`.
fn predict(data: Vec<u8>, parms: Option<&Dict>, limit: usize) -> Result<Vec<u8>> {
    let Some(parms) = parms else { return Ok(data) };
    let predictor = parms.get_i64(b"Predictor").unwrap_or(1);
    if predictor <= 1 {
        return Ok(data);
    }
    let colors = parms.get_i64(b"Colors").unwrap_or(1);
    let bpc = parms.get_i64(b"BitsPerComponent").unwrap_or(8);
    let columns = parms.get_i64(b"Columns").unwrap_or(1);
    if !(1..=32).contains(&colors) {
        return Err(Error::invalid(format!("predictor /Colors {colors}")));
    }
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(Error::invalid(format!("predictor /BitsPerComponent {bpc}")));
    }
    let (Ok(colors), Ok(bpc), Ok(columns)) = (
        usize::try_from(colors),
        usize::try_from(bpc),
        usize::try_from(columns),
    ) else {
        return Err(Error::invalid(format!("predictor /Columns {columns}")));
    };
    let row_bits = columns
        .checked_mul(colors * bpc)
        .filter(|&bits| bits / 8 <= limit);
    let Some(row_bits) = row_bits.filter(|&bits| bits > 0) else {
        return Err(Error::invalid(format!("predictor /Columns {columns}")));
    };
    let row_len = row_bits.div_ceil(8);
    let pixel_len = (colors * bpc).div_ceil(8);
    match predictor {
        2 => Ok(tiff_predictor(data, row_len, columns * colors, colors, bpc)),
        10..=15 => Ok(png_predictor(&data, row_len, pixel_len)),
        other => Err(Error::Unsupported(format!("predictor {other}"))),
    }
}

fn png_predictor(data: &[u8], row_len: usize, pixel_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / (row_len + 1) * row_len + row_len);
    let mut prev = vec![0u8; row_len];
    let mut row = vec![0u8; row_len];
    for chunk in data.chunks(row_len + 1) {
        let (kind, src) = (chunk[0], &chunk[1..]);
        let n = src.len();
        row[..n].copy_from_slice(src);
        match kind {
            1 => {
                for i in pixel_len..n {
                    row[i] = row[i].wrapping_add(row[i - pixel_len]);
                }
            }
            2 => {
                for i in 0..n {
                    row[i] = row[i].wrapping_add(prev[i]);
                }
            }
            3 => {
                for i in 0..n {
                    let left = if i >= pixel_len {
                        u16::from(row[i - pixel_len])
                    } else {
                        0
                    };
                    let average = (left + u16::from(prev[i])) / 2;
                    row[i] = row[i].wrapping_add(average as u8);
                }
            }
            4 => {
                for i in 0..n {
                    let (left, up_left) = if i >= pixel_len {
                        (row[i - pixel_len], prev[i - pixel_len])
                    } else {
                        (0, 0)
                    };
                    row[i] = row[i].wrapping_add(paeth(left, prev[i], up_left));
                }
            }
            // 0 is None; an unknown row type is read as None.
            _ => {}
        }
        out.extend_from_slice(&row[..n]);
        prev[..n].copy_from_slice(&row[..n]);
    }
    out
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = (
        (p - i16::from(a)).abs(),
        (p - i16::from(b)).abs(),
        (p - i16::from(c)).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// TIFF predictor 2: each sample is stored as the difference from the same
/// component of the pixel to its left.
fn tiff_predictor(
    mut data: Vec<u8>,
    row_len: usize,
    samples: usize,
    colors: usize,
    bpc: usize,
) -> Vec<u8> {
    for row in data.chunks_mut(row_len) {
        match bpc {
            8 => {
                for i in colors..row.len() {
                    row[i] = row[i].wrapping_add(row[i - colors]);
                }
            }
            16 => {
                for s in colors..row.len() / 2 {
                    let left =
                        u16::from_be_bytes([row[2 * (s - colors)], row[2 * (s - colors) + 1]]);
                    let value = u16::from_be_bytes([row[2 * s], row[2 * s + 1]]).wrapping_add(left);
                    row[2 * s..2 * s + 2].copy_from_slice(&value.to_be_bytes());
                }
            }
            _ => {
                let mask = (1u16 << bpc) - 1;
                let count = samples.min(row.len() * 8 / bpc);
                for s in colors..count {
                    let value = (get_bits(row, s, bpc) + get_bits(row, s - colors, bpc)) & mask;
                    set_bits(row, s, bpc, value);
                }
            }
        }
    }
    data
}

fn get_bits(row: &[u8], index: usize, bpc: usize) -> u16 {
    let bit = index * bpc;
    let shift = 8 - bpc - bit % 8;
    u16::from(row[bit / 8] >> shift) & ((1 << bpc) - 1)
}

fn set_bits(row: &mut [u8], index: usize, bpc: usize, value: u16) {
    let bit = index * bpc;
    let shift = 8 - bpc - bit % 8;
    let mask = (((1u16 << bpc) - 1) << shift) as u8;
    row[bit / 8] = (row[bit / 8] & !mask) | (((value << shift) as u8) & mask);
}

/// LZW with 9- to 12-bit codes, clear (256) and end (257) codes. With
/// `early_change` the code width grows one code early, as most writers do.
fn lzw_decode(data: &[u8], early_change: bool, limit: usize) -> Result<Vec<u8>> {
    const CLEAR: usize = 256;
    const END: usize = 257;
    const MAX_CODES: usize = 4096;
    let early = usize::from(early_change);
    // Each entry is its prefix entry plus one byte.
    let mut prefix: Vec<u16> = Vec::with_capacity(MAX_CODES);
    let mut last: Vec<u8> = Vec::with_capacity(MAX_CODES);
    let mut first: Vec<u8> = Vec::with_capacity(MAX_CODES);
    let mut length: Vec<usize> = Vec::with_capacity(MAX_CODES);
    for byte in 0..=255u8 {
        prefix.push(0);
        last.push(byte);
        first.push(byte);
        length.push(1);
    }
    for _ in [CLEAR, END] {
        prefix.push(0);
        last.push(0);
        first.push(0);
        length.push(0);
    }
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut width = 9;
    let mut previous: Option<usize> = None;
    let mut bits: u32 = 0;
    let mut bit_count = 0;
    let mut input = data.iter();
    loop {
        while bit_count < width {
            let Some(&byte) = input.next() else {
                return Ok(out);
            };
            bits = (bits << 8) | u32::from(byte);
            bit_count += 8;
        }
        let code = ((bits >> (bit_count - width)) & ((1 << width) - 1)) as usize;
        bit_count -= width;
        bits &= (1 << bit_count) - 1;
        if code == CLEAR {
            prefix.truncate(END + 1);
            last.truncate(END + 1);
            first.truncate(END + 1);
            length.truncate(END + 1);
            width = 9;
            previous = None;
            continue;
        }
        if code == END {
            return Ok(out);
        }
        let table_len = prefix.len();
        let head = match previous {
            _ if code < table_len && code != CLEAR && code != END => {
                emit(&mut out, code, &prefix, &last, &length);
                first[code]
            }
            Some(p) if code == table_len => {
                emit(&mut out, p, &prefix, &last, &length);
                out.push(first[p]);
                first[p]
            }
            _ => return Ok(out),
        };
        if let Some(p) = previous.filter(|_| table_len < MAX_CODES) {
            prefix.push(p as u16);
            last.push(head);
            first.push(first[p]);
            length.push(length[p] + 1);
        }
        previous = Some(code);
        if prefix.len() + early >= 1 << width && width < 12 {
            width += 1;
        }
        check_limit(out.len(), limit)?;
    }
}

fn emit(out: &mut Vec<u8>, code: usize, prefix: &[u16], last: &[u8], length: &[usize]) {
    let len = length[code];
    let start = out.len();
    out.resize(start + len, 0);
    let mut entry = code;
    for slot in out[start..].iter_mut().rev() {
        *slot = last[entry];
        entry = usize::from(prefix[entry]);
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Hex digits up to `>`; other bytes are skipped and an odd final digit is
/// padded with zero.
fn ascii_hex_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 1);
    let mut high: Option<u8> = None;
    for &byte in data {
        if byte == b'>' {
            break;
        }
        let Some(value) = hex_value(byte) else {
            continue;
        };
        match high.take() {
            Some(h) => out.push(h << 4 | value),
            None => high = Some(value),
        }
    }
    if let Some(h) = high {
        out.push(h << 4);
    }
    out
}

/// Base-85 groups up to `~>`, `z` for four zero bytes, a short final group
/// padded with `u`. Whitespace and invalid bytes are skipped.
fn ascii85_decode(data: &[u8]) -> Vec<u8> {
    let body = data.strip_prefix(b"<~").unwrap_or(data);
    let mut out = Vec::with_capacity(body.len() / 5 * 4 + 4);
    let mut value: u64 = 0;
    let mut count = 0;
    for &byte in body {
        match byte {
            b'~' => break,
            b'z' if count == 0 => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                value = value * 85 + u64::from(byte - b'!');
                count += 1;
                if count == 5 {
                    out.extend_from_slice(&(value as u32).to_be_bytes());
                    value = 0;
                    count = 0;
                }
            }
            _ => {}
        }
    }
    if count > 1 {
        for _ in count..5 {
            value = value * 85 + 84;
        }
        out.extend_from_slice(&(value as u32).to_be_bytes()[..count - 1]);
    }
    out
}

/// Runs: a length byte 0-127 copies the next length+1 bytes, 129-255 repeats
/// the next byte 257-length times, 128 ends the data.
fn run_length_decode(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut i = 0;
    while let Some(&len) = data.get(i) {
        i += 1;
        match len {
            0..=127 => {
                let end = (i + usize::from(len) + 1).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i = end;
            }
            128 => break,
            _ => {
                if let Some(&byte) = data.get(i) {
                    out.resize(out.len() + 257 - usize::from(len), byte);
                }
                i += 1;
            }
        }
        check_limit(out.len(), limit)?;
    }
    Ok(out)
}
