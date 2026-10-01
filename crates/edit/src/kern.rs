//! Pair kerning in font units: the lookups of the font's GPOS `kern` feature, or else
//! its `kern` table (Microsoft version 0 or Apple version 1, format 0 subtables).

use std::collections::BTreeSet;

use pdf_font::Font;

pub(crate) enum Kerning<'a> {
    None,
    /// The PairPos subtables of each `kern` lookup, in lookup order.
    Gpos(Vec<Vec<&'a [u8]>>),
    /// Format 0 pair lists, from their `nPairs` field.
    Kern(Vec<&'a [u8]>),
}

impl<'a> Kerning<'a> {
    pub(crate) fn of(font: &'a Font) -> Kerning<'a> {
        if let Some(lookups) = font
            .table(b"GPOS")
            .and_then(gpos_lookups)
            .filter(|lookups| lookups.iter().any(|subtables| !subtables.is_empty()))
        {
            return Kerning::Gpos(lookups);
        }
        match font.table(b"kern").and_then(kern_subtables) {
            Some(tables) if !tables.is_empty() => Kerning::Kern(tables),
            _ => Kerning::None,
        }
    }

    /// The advance adjustment between glyphs `left` and `right`; negative draws them closer.
    pub(crate) fn pair(&self, left: u16, right: u16) -> i32 {
        match self {
            Kerning::None => 0,
            // Each lookup applies its first subtable that covers the pair.
            Kerning::Gpos(lookups) => lookups
                .iter()
                .filter_map(|subtables| {
                    subtables
                        .iter()
                        .find_map(|subtable| pair_pos(subtable, left, right))
                })
                .map(i32::from)
                .sum(),
            Kerning::Kern(tables) => tables
                .iter()
                .filter_map(|table| format0(table, left, right))
                .map(i32::from)
                .sum(),
        }
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*data.get(at)?, *data.get(at + 1)?]))
}

fn i16_at(data: &[u8], at: usize) -> Option<i16> {
    u16_at(data, at).map(|v| v as i16)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16_at(data, at)?) << 16 | u32::from(u16_at(data, at + 2)?))
}

/// `data` from the 16-bit offset stored at `at`.
fn offset16(data: &[u8], at: usize) -> Option<&[u8]> {
    data.get(usize::from(u16_at(data, at)?)..)
}

/// The PairPos subtables of every lookup the FeatureList's `kern` features name.
fn gpos_lookups(gpos: &[u8]) -> Option<Vec<Vec<&[u8]>>> {
    let features = offset16(gpos, 6)?;
    let lookups = offset16(gpos, 8)?;
    let mut indices = BTreeSet::new();
    for record in 0..usize::from(u16_at(features, 0)?) {
        let at = 2 + record * 6;
        if features.get(at..at + 4)? != b"kern" {
            continue;
        }
        let feature = offset16(features, at + 4)?;
        for index in 0..usize::from(u16_at(feature, 2)?) {
            indices.insert(u16_at(feature, 4 + index * 2)?);
        }
    }
    let mut out = Vec::new();
    for index in indices {
        let lookup = offset16(lookups, 2 + usize::from(index) * 2)?;
        let kind = u16_at(lookup, 0)?;
        let mut subtables = Vec::new();
        for subtable in 0..usize::from(u16_at(lookup, 4)?) {
            let mut data = offset16(lookup, 6 + subtable * 2)?;
            let mut kind = kind;
            // Extension: the real type and a 32-bit offset from the extension subtable.
            if kind == 9 {
                kind = u16_at(data, 2)?;
                data = data.get(usize::try_from(u32_at(data, 4)?).ok()?..)?;
            }
            if kind == 2 {
                subtables.push(data);
            }
        }
        out.push(subtables);
    }
    Some(out)
}

/// The first glyph's XAdvance from a PairPos subtable, `None` when it does not cover
/// the pair.
fn pair_pos(subtable: &[u8], left: u16, right: u16) -> Option<i16> {
    let covered = coverage(offset16(subtable, 2)?, left)?;
    let (format1, format2) = (u16_at(subtable, 4)?, u16_at(subtable, 6)?);
    let size1 = 2 * format1.count_ones() as usize;
    let record = size1 + 2 * format2.count_ones() as usize;
    match u16_at(subtable, 0)? {
        1 => {
            let set = offset16(subtable, 10 + covered * 2)?;
            let count = usize::from(u16_at(set, 0)?);
            let (mut low, mut high) = (0, count);
            while low < high {
                let middle = (low + high) / 2;
                let at = 2 + middle * (2 + record);
                match u16_at(set, at)?.cmp(&right) {
                    std::cmp::Ordering::Less => low = middle + 1,
                    std::cmp::Ordering::Greater => high = middle,
                    std::cmp::Ordering::Equal => return x_advance(set, at + 2, format1),
                }
            }
            None
        }
        2 => {
            let first = class(offset16(subtable, 8)?, left)?;
            let second = class(offset16(subtable, 10)?, right)?;
            let (count1, count2) = (u16_at(subtable, 12)?, u16_at(subtable, 14)?);
            if first >= count1 || second >= count2 {
                return None;
            }
            let at = 16 + (usize::from(first) * usize::from(count2) + usize::from(second)) * record;
            x_advance(subtable, at, format1)
        }
        _ => None,
    }
}

/// XAdvance of the value record at `at` with `format`; zero when the record has none.
fn x_advance(data: &[u8], at: usize, format: u16) -> Option<i16> {
    if format & 0x0004 == 0 {
        return Some(0);
    }
    // XPlacement and YPlacement come first when present.
    i16_at(data, at + 2 * (format & 0x0003).count_ones() as usize)
}

/// A glyph's coverage index.
fn coverage(table: &[u8], glyph: u16) -> Option<usize> {
    let count = usize::from(u16_at(table, 2)?);
    match u16_at(table, 0)? {
        1 => (0..count).find(|&index| u16_at(table, 4 + index * 2) == Some(glyph)),
        2 => (0..count).find_map(|index| {
            let at = 4 + index * 6;
            let (start, end) = (u16_at(table, at)?, u16_at(table, at + 2)?);
            if !(start..=end).contains(&glyph) {
                return None;
            }
            Some(usize::from(u16_at(table, at + 4)?) + usize::from(glyph - start))
        }),
        _ => None,
    }
}

/// A glyph's class; 0 for glyphs the class definition does not list.
fn class(table: &[u8], glyph: u16) -> Option<u16> {
    match u16_at(table, 0)? {
        1 => {
            let start = u16_at(table, 2)?;
            let count = u16_at(table, 4)?;
            if glyph < start || glyph - start >= count {
                return Some(0);
            }
            u16_at(table, 6 + usize::from(glyph - start) * 2)
        }
        2 => {
            for index in 0..usize::from(u16_at(table, 2)?) {
                let at = 4 + index * 6;
                if (u16_at(table, at)?..=u16_at(table, at + 2)?).contains(&glyph) {
                    return u16_at(table, at + 4);
                }
            }
            Some(0)
        }
        _ => None,
    }
}

/// The horizontal format 0 subtables' pair lists of a `kern` table.
fn kern_subtables(kern: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    if u16_at(kern, 0)? == 0 {
        // Microsoft: version, nTables; each subtable has version, length, coverage.
        let mut at = 4;
        for _ in 0..u16_at(kern, 2)? {
            let coverage = u16_at(kern, at + 4)?;
            // Format 0 and horizontal, neither minimum values nor cross-stream.
            if coverage & 0xFF07 == 0x0001 {
                out.push(kern.get(at + 6..)?);
            }
            at += usize::from(u16_at(kern, at + 2)?);
        }
    } else if u32_at(kern, 0)? == 0x0001_0000 {
        // Apple: version, nTables; each subtable has length, coverage, tupleIndex.
        let mut at = 8;
        for _ in 0..u32_at(kern, 4)? {
            let coverage = u16_at(kern, at + 4)?;
            // Format 0, neither vertical, cross-stream nor variation.
            if coverage & 0xE0FF == 0 {
                out.push(kern.get(at + 8..)?);
            }
            at += usize::try_from(u32_at(kern, at)?).ok()?;
        }
    }
    Some(out)
}

/// A format 0 pair's value: records of left, right and value sorted by the pair.
fn format0(table: &[u8], left: u16, right: u16) -> Option<i16> {
    let key = u32::from(left) << 16 | u32::from(right);
    let (mut low, mut high) = (0, usize::from(u16_at(table, 0)?));
    while low < high {
        let middle = (low + high) / 2;
        let at = 8 + middle * 6;
        match u32_at(table, at)?.cmp(&key) {
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
            std::cmp::Ordering::Equal => return i16_at(table, at + 4),
        }
    }
    None
}
