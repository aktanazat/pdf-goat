//! Loader for the compressed predefined CMaps and CID-to-Unicode tables in `data/predefined_cmaps.bin`.

use crate::data::cmap_index::{BLOB, ENTRIES};
use crate::error::{FontError, Result};

/// One zlib stream inside the predefined data blob.
pub(crate) struct BlobEntry {
    pub(crate) name: &'static str,
    /// 0: code-to-CID CMap; 1: CID-to-Unicode table.
    pub(crate) kind: u8,
    /// 0 Adobe-Japan1, 1 Adobe-GB1, 2 Adobe-CNS1, 3 Adobe-Korea1.
    pub(crate) collection: u8,
    pub(crate) offset: usize,
    pub(crate) len: usize,
    pub(crate) raw_len: usize,
}

fn inflate(entry: &BlobEntry) -> Result<Vec<u8>> {
    let packed = BLOB
        .get(entry.offset..entry.offset + entry.len)
        .ok_or(FontError::Malformed("predefined CMap index"))?;
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(packed, entry.raw_len)
        .map_err(|_| FontError::Malformed("predefined CMap data"))?;
    if raw.len() != entry.raw_len {
        return Err(FontError::Malformed("predefined CMap data length"));
    }
    Ok(raw)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or(FontError::Truncated("predefined CMap data"))?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u32> {
        let mut v = 0u64;
        for shift in (0..35).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return u32::try_from(v)
                    .map_err(|_| FontError::Malformed("predefined CMap varint"));
            }
        }
        Err(FontError::Malformed("predefined CMap varint"))
    }
}

/// Codespace range as stored: code length and per-byte bounds.
pub(crate) type RawCodespace = (u8, [u8; 4], [u8; 4]);

/// A decoded code-to-CID CMap before its base is applied.
pub(crate) struct CodeCMapData {
    pub(crate) vertical: bool,
    pub(crate) codespaces: Vec<RawCodespace>,
    pub(crate) base: Option<usize>,
    pub(crate) collection: u8,
    /// (code length, first code, last code, first CID), sorted by length then code.
    pub(crate) runs: Vec<(u8, u32, u32, u32)>,
}

/// Index into the entry table of a code-to-CID CMap.
pub(crate) fn find_code_cmap(name: &str) -> Option<usize> {
    ENTRIES.iter().position(|e| e.kind == 0 && e.name == name)
}

pub(crate) fn entry_name(index: usize) -> Option<&'static str> {
    ENTRIES.get(index).map(|e| e.name)
}

pub(crate) fn load_code_cmap(index: usize) -> Result<CodeCMapData> {
    let entry = ENTRIES
        .get(index)
        .ok_or(FontError::Missing("predefined CMap"))?;
    if entry.kind != 0 {
        return Err(FontError::Missing("predefined CMap"));
    }
    let raw = inflate(entry)?;
    let mut c = Cursor { data: &raw, pos: 0 };
    let vertical = c.byte()? != 0;
    let n_spaces = c.byte()?;
    let mut codespaces = Vec::with_capacity(usize::from(n_spaces));
    for _ in 0..n_spaces {
        let len = c.byte()?;
        if !(1..=4).contains(&len) {
            return Err(FontError::Malformed("predefined CMap codespace"));
        }
        let mut low = [0u8; 4];
        let mut high = [0u8; 4];
        for slot in low.iter_mut().take(usize::from(len)) {
            *slot = c.byte()?;
        }
        for slot in high.iter_mut().take(usize::from(len)) {
            *slot = c.byte()?;
        }
        codespaces.push((len, low, high));
    }
    let base = match c.varint()? {
        0 => None,
        n => Some(n as usize - 1),
    };
    let count = c.varint()? as usize;
    if count > raw.len() {
        return Err(FontError::Malformed("predefined CMap run count"));
    }
    let mut runs = Vec::with_capacity(count);
    let mut prev_len = 0u8;
    let mut prev_end = 0u64;
    let mut prev_cid_end = 0i64;
    for _ in 0..count {
        let len = c.byte()?;
        if !(1..=4).contains(&len) {
            return Err(FontError::Malformed("predefined CMap code length"));
        }
        if len != prev_len {
            prev_end = 0;
        }
        let lo = prev_end + u64::from(c.varint()?);
        let hi = lo + u64::from(c.varint()?);
        let z = i64::from(c.varint()?);
        let delta = if z & 1 == 0 { z >> 1 } else { -((z + 1) >> 1) };
        let cid = prev_cid_end + delta;
        let (Ok(lo32), Ok(hi32), Ok(cid32)) =
            (u32::try_from(lo), u32::try_from(hi), u32::try_from(cid))
        else {
            return Err(FontError::Malformed("predefined CMap run"));
        };
        runs.push((len, lo32, hi32, cid32));
        prev_len = len;
        prev_end = hi + 1;
        prev_cid_end = cid + (hi - lo) as i64 + 1;
    }
    Ok(CodeCMapData {
        vertical,
        codespaces,
        base,
        collection: entry.collection,
        runs,
    })
}

/// CID-to-Unicode runs of one character collection: (first CID, count, first code point).
pub(crate) struct UnicodeRuns {
    pub(crate) horizontal: Vec<(u32, u32, u32)>,
    /// Entries where vertical writing maps a CID differently (vertical forms).
    pub(crate) vertical: Vec<(u32, u32, u32)>,
}

pub(crate) fn load_unicode_runs(collection: u8) -> Result<UnicodeRuns> {
    let entry = ENTRIES
        .iter()
        .find(|e| e.kind == 1 && e.collection == collection)
        .ok_or(FontError::Missing("CID-to-Unicode table"))?;
    let raw = inflate(entry)?;
    let mut c = Cursor { data: &raw, pos: 0 };
    let horizontal = read_unicode_section(&mut c, raw.len())?;
    let vertical = read_unicode_section(&mut c, raw.len())?;
    Ok(UnicodeRuns {
        horizontal,
        vertical,
    })
}

fn read_unicode_section(c: &mut Cursor<'_>, limit: usize) -> Result<Vec<(u32, u32, u32)>> {
    let count = c.varint()? as usize;
    if count > limit {
        return Err(FontError::Malformed("CID-to-Unicode run count"));
    }
    let mut out = Vec::with_capacity(count);
    let mut prev_end = 0u32;
    for _ in 0..count {
        let cid = prev_end
            .checked_add(c.varint()?)
            .ok_or(FontError::Malformed("CID-to-Unicode run"))?;
        let n = c.varint()?;
        let first = c.varint()?;
        prev_end = cid
            .checked_add(n)
            .ok_or(FontError::Malformed("CID-to-Unicode run"))?;
        out.push((cid, n, first));
    }
    Ok(out)
}
