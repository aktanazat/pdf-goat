//! sfnt container: table directory, TrueType collections, and the small fixed tables.

use crate::data::mac_glyphs::MAC_GLYPH_NAMES;
use crate::error::{FontError, Result};
use crate::reader::{Reader, read_i16, read_u16, read_u32};

pub(crate) const TAG_TTCF: [u8; 4] = *b"ttcf";

/// Largest number of tables accepted in one table directory.
const MAX_TABLES: u16 = 512;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TableRecord {
    pub(crate) tag: [u8; 4],
    pub(crate) offset: usize,
    pub(crate) len: usize,
}

/// A parsed table directory; offsets are absolute in the font data.
#[derive(Debug, Clone)]
pub(crate) struct Sfnt {
    pub(crate) tables: Vec<TableRecord>,
}

pub(crate) fn is_sfnt_version(version: u32) -> bool {
    matches!(version, 0x0001_0000 | 0x7472_7565 | 0x4F54_544F)
}

impl Sfnt {
    /// Parses the table directory at `offset` (0 for a single font, from the TTC header otherwise).
    pub(crate) fn parse(data: &[u8], offset: usize) -> Result<Sfnt> {
        let mut r = Reader::at(data, offset, "sfnt table directory")?;
        let version = r.u32()?;
        if !is_sfnt_version(version) {
            return Err(FontError::Malformed("not an sfnt font"));
        }
        let num_tables = r.u16()?;
        if num_tables > MAX_TABLES {
            return Err(FontError::LimitExceeded("sfnt table count"));
        }
        r.skip(6)?;
        let mut tables = Vec::with_capacity(usize::from(num_tables));
        for _ in 0..num_tables {
            let tag: [u8; 4] = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
            let _checksum = r.u32()?;
            let off = r.u32()? as usize;
            let len = r.u32()? as usize;
            if off >= data.len() {
                // A table that starts past the end of the file is treated as absent.
                continue;
            }
            // Tables whose declared length runs past the end are clamped to the data present.
            let len = len.min(data.len() - off);
            tables.push(TableRecord {
                tag,
                offset: off,
                len,
            });
        }
        Ok(Sfnt { tables })
    }

    pub(crate) fn record(&self, tag: &[u8; 4]) -> Option<TableRecord> {
        self.tables.iter().find(|t| &t.tag == tag).copied()
    }

    pub(crate) fn table<'a>(&self, data: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
        let rec = self.record(tag)?;
        data.get(rec.offset..rec.offset + rec.len)
    }
}

/// Face offsets of a TrueType collection, or `None` when `data` is not a collection.
pub(crate) fn collection_offsets(data: &[u8]) -> Result<Option<Vec<usize>>> {
    if data.get(0..4) != Some(&TAG_TTCF[..]) {
        return Ok(None);
    }
    let mut r = Reader::at(data, 4, "TrueType collection header")?;
    let _version = r.u32()?;
    let count = r.u32()?;
    if count == 0 {
        return Err(FontError::Malformed("empty TrueType collection"));
    }
    if count > 4096 {
        return Err(FontError::LimitExceeded("TrueType collection face count"));
    }
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(r.u32()? as usize);
    }
    Ok(Some(out))
}

/// Fields of head, hhea, maxp, post, and OS/2 that the rest of the crate needs.
#[derive(Debug, Clone, Default)]
pub(crate) struct SfntMetrics {
    pub(crate) units_per_em: u16,
    pub(crate) index_to_loc_long: bool,
    pub(crate) bbox: [i16; 4],
    pub(crate) mac_style: u16,
    pub(crate) num_glyphs: u16,
    pub(crate) ascender: i16,
    pub(crate) descender: i16,
    pub(crate) line_gap: i16,
    pub(crate) num_hmetrics: u16,
    pub(crate) italic_angle: f32,
    pub(crate) is_fixed_pitch: bool,
    pub(crate) os2: Option<Os2>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Os2 {
    pub(crate) weight_class: u16,
    pub(crate) fs_selection: u16,
    pub(crate) family_class: i16,
    pub(crate) typo_ascender: i16,
    pub(crate) typo_descender: i16,
    pub(crate) x_height: Option<i16>,
    pub(crate) cap_height: Option<i16>,
}

impl SfntMetrics {
    pub(crate) fn parse(sfnt: &Sfnt, data: &[u8]) -> Result<SfntMetrics> {
        let head = sfnt
            .table(data, b"head")
            .ok_or(FontError::Missing("head table"))?;
        if head.len() < 54 {
            return Err(FontError::Truncated("head table"));
        }
        let units_per_em = read_u16(head, 18).unwrap_or(0);
        // Values outside the OpenType range 16..16384 are replaced by the common 1000/2048.
        let units_per_em = if (16..=16384).contains(&units_per_em) {
            units_per_em
        } else {
            1000
        };
        let bbox = [
            read_i16(head, 36).unwrap_or(0),
            read_i16(head, 38).unwrap_or(0),
            read_i16(head, 40).unwrap_or(0),
            read_i16(head, 42).unwrap_or(0),
        ];
        let mac_style = read_u16(head, 44).unwrap_or(0);
        let index_to_loc_long = read_i16(head, 50).unwrap_or(0) != 0;
        let num_glyphs = sfnt
            .table(data, b"maxp")
            .and_then(|t| read_u16(t, 4))
            .unwrap_or(0);
        let mut m = SfntMetrics {
            units_per_em,
            index_to_loc_long,
            bbox,
            mac_style,
            num_glyphs,
            ..SfntMetrics::default()
        };
        if let Some(hhea) = sfnt.table(data, b"hhea") {
            m.ascender = read_i16(hhea, 4).unwrap_or(0);
            m.descender = read_i16(hhea, 6).unwrap_or(0);
            m.line_gap = read_i16(hhea, 8).unwrap_or(0);
            m.num_hmetrics = read_u16(hhea, 34).unwrap_or(0);
        }
        if let Some(post) = sfnt.table(data, b"post") {
            m.italic_angle = read_u32(post, 4)
                .map(|v| v as i32 as f32 / 65536.0)
                .unwrap_or(0.0);
            m.is_fixed_pitch = read_u32(post, 12).is_some_and(|v| v != 0);
        }
        if let Some(t) = sfnt.table(data, b"OS/2")
            && t.len() >= 78
        {
            let version = read_u16(t, 0).unwrap_or(0);
            m.os2 = Some(Os2 {
                weight_class: read_u16(t, 4).unwrap_or(400),
                fs_selection: read_u16(t, 62).unwrap_or(0),
                family_class: read_i16(t, 30).unwrap_or(0),
                typo_ascender: read_i16(t, 68).unwrap_or(0),
                typo_descender: read_i16(t, 70).unwrap_or(0),
                x_height: if version >= 2 { read_i16(t, 86) } else { None },
                cap_height: if version >= 2 { read_i16(t, 88) } else { None },
            });
        }
        Ok(m)
    }
}

/// Advance width of `gid` from hmtx, in font units.
pub(crate) fn hmtx_advance(hmtx: &[u8], num_hmetrics: u16, gid: u16) -> Option<u16> {
    if num_hmetrics == 0 {
        return None;
    }
    let idx = gid.min(num_hmetrics - 1);
    read_u16(hmtx, usize::from(idx) * 4)
}

/// Left side bearing of `gid` from hmtx, in font units.
pub(crate) fn hmtx_lsb(hmtx: &[u8], num_hmetrics: u16, gid: u16) -> Option<i16> {
    let (nh, g) = (usize::from(num_hmetrics), usize::from(gid));
    let at = if g < nh {
        g * 4 + 2
    } else {
        nh * 4 + (g - nh) * 2
    };
    read_i16(hmtx, at)
}

/// Glyph names from a post table, indexed by glyph id. `None` when the table carries no names.
pub(crate) fn post_glyph_names(post: &[u8], num_glyphs: u16) -> Option<Vec<Option<String>>> {
    let version = read_u32(post, 0)?;
    let n = usize::from(num_glyphs);
    match version {
        0x0001_0000 => Some(
            (0..n.min(MAC_GLYPH_NAMES.len()))
                .map(|i| Some(MAC_GLYPH_NAMES[i].to_owned()))
                .collect(),
        ),
        0x0002_0000 => {
            let count = usize::from(read_u16(post, 32)?).min(n);
            let mut indices = Vec::with_capacity(count);
            for i in 0..count {
                indices.push(usize::from(read_u16(post, 34 + 2 * i)?));
            }
            // Pascal strings follow the index array.
            let mut custom: Vec<&[u8]> = Vec::new();
            let mut pos = 34 + 2 * count;
            while pos < post.len() {
                let len = usize::from(post[pos]);
                let Some(bytes) = post.get(pos + 1..pos + 1 + len) else {
                    break;
                };
                custom.push(bytes);
                pos += 1 + len;
            }
            let names = indices
                .iter()
                .map(|&idx| {
                    if idx < MAC_GLYPH_NAMES.len() {
                        Some(MAC_GLYPH_NAMES[idx].to_owned())
                    } else {
                        custom
                            .get(idx - MAC_GLYPH_NAMES.len())
                            .map(|b| String::from_utf8_lossy(b).into_owned())
                    }
                })
                .collect();
            Some(names)
        }
        0x0002_5000 => {
            let count = usize::from(read_u16(post, 32)?).min(n);
            let names = (0..count)
                .map(|i| {
                    let offset = i8::from_be_bytes([*post.get(34 + i)?]);
                    let idx = i as i64 + i64::from(offset);
                    usize::try_from(idx)
                        .ok()
                        .and_then(|x| MAC_GLYPH_NAMES.get(x))
                        .map(|s| (*s).to_owned())
                })
                .collect();
            Some(names)
        }
        _ => None,
    }
}

/// One entry of the `name` table decoded to text.
pub(crate) fn name_string(name: &[u8], name_id: u16) -> Option<String> {
    let count = usize::from(read_u16(name, 2)?);
    let storage = usize::from(read_u16(name, 4)?);
    let mut best: Option<(u8, String)> = None;
    for i in 0..count.min(4096) {
        let rec = 6 + 12 * i;
        let platform = read_u16(name, rec)?;
        let encoding = read_u16(name, rec + 2)?;
        let language = read_u16(name, rec + 4)?;
        let id = read_u16(name, rec + 6)?;
        if id != name_id {
            continue;
        }
        let len = usize::from(read_u16(name, rec + 8)?);
        let off = usize::from(read_u16(name, rec + 10)?);
        let Some(bytes) = name.get(storage + off..storage + off + len) else {
            continue;
        };
        let (rank, text) = match (platform, encoding) {
            (3, 1) | (3, 10) | (0, _) => {
                let rank = if platform == 3 && language == 0x409 {
                    0
                } else {
                    1
                };
                (rank, decode_utf16be(bytes))
            }
            (3, 0) => (2, decode_utf16be(bytes)),
            (1, 0) => (3, bytes.iter().map(|&b| mac_roman_char(b)).collect()),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, text));
        }
    }
    best.map(|(_, text)| text)
}

pub(crate) fn decode_utf16be(bytes: &[u8]) -> String {
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_be_bytes(c));
    char::decode_utf16(units).filter_map(|c| c.ok()).collect()
}

fn mac_roman_char(b: u8) -> char {
    if b < 0x80 {
        return char::from(b);
    }
    crate::encoding::BaseEncoding::MacRoman
        .to_unicode(b)
        .unwrap_or('\u{FFFD}')
}

/// One cmap subtable; offsets are absolute in the font data.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CmapSubtable {
    pub(crate) platform: u16,
    pub(crate) encoding: u16,
    pub(crate) format: u16,
    offset: usize,
    end: usize,
}

/// Parses the cmap encoding records. Subtables with unknown formats are skipped.
pub(crate) fn parse_cmap(data: &[u8], rec: TableRecord) -> Vec<CmapSubtable> {
    let base = rec.offset;
    let table_end = rec.offset + rec.len;
    let mut out = Vec::new();
    let Some(count) = read_u16(data, base + 2) else {
        return out;
    };
    for i in 0..usize::from(count).min(256) {
        let r = base + 4 + 8 * i;
        let (Some(platform), Some(encoding), Some(off)) = (
            read_u16(data, r),
            read_u16(data, r + 2),
            read_u32(data, r + 4),
        ) else {
            break;
        };
        let offset = base + off as usize;
        let Some(format) = read_u16(data, offset) else {
            continue;
        };
        let declared = match format {
            0 | 2 | 4 | 6 => read_u16(data, offset + 2).map(usize::from),
            10 | 12 | 13 => read_u32(data, offset + 4).map(|v| v as usize),
            _ => continue,
        };
        let Some(declared) = declared else { continue };
        // Some fonts understate the length (format 4 over 64 KiB); allow up to the table end.
        let end = if offset + declared > table_end || format == 4 {
            table_end
        } else {
            offset + declared
        };
        if end <= offset || end > data.len() {
            continue;
        }
        out.push(CmapSubtable {
            platform,
            encoding,
            format,
            offset,
            end,
        });
    }
    out
}

impl CmapSubtable {
    /// Glyph for `code`; `None` when unmapped (glyph 0 counts as unmapped).
    pub(crate) fn lookup(&self, data: &[u8], code: u32) -> Option<u16> {
        let t = data.get(self.offset..self.end)?;
        let gid = match self.format {
            0 => {
                if code > 255 {
                    return None;
                }
                u16::from(*t.get(6 + code as usize)?)
            }
            2 => lookup_format2(t, code)?,
            4 => lookup_format4(t, code)?,
            6 => {
                let first = u32::from(read_u16(t, 6)?);
                let count = u32::from(read_u16(t, 8)?);
                if code < first || code - first >= count {
                    return None;
                }
                read_u16(t, 10 + 2 * (code - first) as usize)?
            }
            10 => {
                let first = read_u32(t, 12)?;
                let count = read_u32(t, 16)?;
                if code < first || code - first >= count {
                    return None;
                }
                read_u16(t, 20 + 2 * (code - first) as usize)?
            }
            12 | 13 => lookup_groups(t, code, self.format == 13)?,
            _ => return None,
        };
        (gid != 0).then_some(gid)
    }

    /// Every (code, glyph) pair of the subtable, glyph 0 excluded, at most `limit` pairs.
    pub(crate) fn mappings(&self, data: &[u8], limit: usize) -> Vec<(u32, u16)> {
        let mut out = Vec::new();
        let Some(t) = data.get(self.offset..self.end) else {
            return out;
        };
        let mut push = |code: u32, gid: Option<u16>| {
            if let Some(g) = gid
                && g != 0
                && out.len() < limit
            {
                out.push((code, g));
            }
        };
        match self.format {
            0 | 2 | 6 => {
                for code in 0..=0xFFFFu32 {
                    push(code, self.lookup(data, code));
                }
            }
            4 => {
                let seg_count = read_u16(t, 6).map_or(0, |v| usize::from(v / 2));
                for s in 0..seg_count {
                    let (Some(end), Some(start)) = (
                        read_u16(t, 14 + 2 * s),
                        read_u16(t, 16 + 2 * seg_count + 2 * s),
                    ) else {
                        break;
                    };
                    if start > end {
                        continue;
                    }
                    for code in u32::from(start)..=u32::from(end) {
                        push(code, lookup_format4(t, code));
                    }
                }
            }
            10 => {
                let first = read_u32(t, 12).unwrap_or(0);
                let count = read_u32(t, 16).unwrap_or(0).min(0x11_0000);
                for i in 0..count {
                    push(first.saturating_add(i), read_u16(t, 20 + 2 * i as usize));
                }
            }
            12 | 13 => {
                let groups = read_u32(t, 12).unwrap_or(0) as usize;
                let mut budget = 0x11_0000u32;
                for g in 0..groups.min(t.len() / 12) {
                    let base = 16 + 12 * g;
                    let (Some(start), Some(end), Some(glyph)) = (
                        read_u32(t, base),
                        read_u32(t, base + 4),
                        read_u32(t, base + 8),
                    ) else {
                        break;
                    };
                    if start > end || start > 0x10_FFFF {
                        continue;
                    }
                    let end = end.min(0x10_FFFF);
                    for code in start..=end {
                        if budget == 0 {
                            return out;
                        }
                        budget -= 1;
                        let gid = if self.format == 13 {
                            glyph
                        } else {
                            glyph.wrapping_add(code - start)
                        };
                        push(code, u16::try_from(gid).ok());
                    }
                }
            }
            _ => {}
        }
        out
    }
}

fn lookup_format2(t: &[u8], code: u32) -> Option<u16> {
    if code > 0xFFFF {
        return None;
    }
    let (high, low) = if code < 0x100 {
        (0, code)
    } else {
        (code >> 8, code & 0xFF)
    };
    let key_of = |b: u32| read_u16(t, 6 + 2 * b as usize);
    let sub = if code < 0x100 {
        // A lead byte on its own is not a character.
        if key_of(code)? != 0 {
            return None;
        }
        0
    } else {
        let key = key_of(high)?;
        if key == 0 {
            return None;
        }
        usize::from(key / 8)
    };
    let sub_base = 6 + 512 + 8 * sub;
    let first = u32::from(read_u16(t, sub_base)?);
    let count = u32::from(read_u16(t, sub_base + 2)?);
    let delta = read_i16(t, sub_base + 4)?;
    let range_offset = usize::from(read_u16(t, sub_base + 6)?);
    if low < first || low - first >= count {
        return None;
    }
    let pos = sub_base + 6 + range_offset + 2 * (low - first) as usize;
    let g = read_u16(t, pos)?;
    if g == 0 {
        return Some(0);
    }
    Some(g.wrapping_add(delta as u16))
}

fn lookup_format4(t: &[u8], code: u32) -> Option<u16> {
    if code > 0xFFFF {
        return None;
    }
    let code16 = code as u16;
    let seg_count = usize::from(read_u16(t, 6)? / 2);
    let ends = 14;
    let starts = ends + 2 * seg_count + 2;
    let deltas = starts + 2 * seg_count;
    let range_offsets = deltas + 2 * seg_count;
    // Binary search for the first segment whose end code is >= code.
    let (mut lo, mut hi) = (0usize, seg_count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if read_u16(t, ends + 2 * mid)? < code16 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo >= seg_count {
        return None;
    }
    let start = read_u16(t, starts + 2 * lo)?;
    if code16 < start {
        return None;
    }
    let delta = read_u16(t, deltas + 2 * lo)?;
    let ro_pos = range_offsets + 2 * lo;
    let ro = usize::from(read_u16(t, ro_pos)?);
    if ro == 0 {
        return Some(code16.wrapping_add(delta));
    }
    let g = read_u16(t, ro_pos + ro + 2 * usize::from(code16 - start))?;
    if g == 0 {
        return Some(0);
    }
    Some(g.wrapping_add(delta))
}

fn lookup_groups(t: &[u8], code: u32, constant: bool) -> Option<u16> {
    let groups = (read_u32(t, 12)? as usize).min(t.len().saturating_sub(16) / 12);
    let (mut lo, mut hi) = (0usize, groups);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let base = 16 + 12 * mid;
        let start = read_u32(t, base)?;
        let end = read_u32(t, base + 4)?;
        if code < start {
            hi = mid;
        } else if code > end {
            lo = mid + 1;
        } else {
            let glyph = read_u32(t, base + 8)?;
            let gid = if constant {
                glyph
            } else {
                glyph.checked_add(code - start)?
            };
            return u16::try_from(gid).ok();
        }
    }
    None
}
