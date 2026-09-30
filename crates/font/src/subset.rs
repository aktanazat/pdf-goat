//! TrueType subsetting for embedding as a CIDFontType2 with Identity-H encoding.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{FontError, Result};
use crate::font::Font;
use crate::glyf::composite_refs;
use crate::reader::{read_i16, read_u16};
use crate::sfnt;

/// Tables copied unchanged when present.
const COPIED: [&[u8; 4]; 5] = [b"OS/2", b"cvt ", b"fpgm", b"prep", b"gasp"];
/// Name ids kept in the subset's `name` table.
const NAME_IDS: [u16; 5] = [1, 2, 3, 4, 6];

/// A subset TrueType font. Glyphs are renumbered densely in old-id order and old glyph 0
/// (`.notdef`) stays glyph 0, so with Identity-H and `/CIDToGIDMap /Identity` the CIDs are the
/// new glyph ids.
#[derive(Debug, Clone, PartialEq)]
pub struct TrueTypeSubset {
    /// The subset font program, for a FontFile2 stream.
    pub data: Vec<u8>,
    /// (old glyph id, new glyph id), sorted by old id. Includes composite components.
    pub glyph_map: Vec<(u16, u16)>,
    /// Advance width of each new glyph, in font units.
    pub advances: Vec<u16>,
    pub units_per_em: u16,
}

impl TrueTypeSubset {
    /// New glyph id (the CID) of an original glyph id.
    pub fn new_gid(&self, old: u16) -> Option<u16> {
        new_gid(&self.glyph_map, old)
    }

    /// Advance widths of the new glyphs in PDF glyph space (1/1000 em), CID order, for
    /// `/W [0 [w0 w1 ...]]`.
    pub fn pdf_widths(&self) -> Vec<f32> {
        let scale = 1000.0 / f32::from(self.units_per_em);
        self.advances
            .iter()
            .map(|&a| f32::from(a) * scale)
            .collect()
    }
}

fn new_gid(map: &[(u16, u16)], old: u16) -> Option<u16> {
    map.binary_search_by_key(&old, |&(o, _)| o)
        .ok()
        .map(|i| map[i].1)
}

/// Subsets a TrueType (glyf) font to `glyphs` plus `.notdef` and every composite component.
/// Rebuilds glyf, loca (long format), hmtx, cmap, a minimal name table, and a format 3 post
/// table; copies head, hhea, maxp, OS/2, and the hinting tables; fixes all checksums.
pub fn subset_truetype(font: &Font, glyphs: &[u16]) -> Result<TrueTypeSubset> {
    build_subset(font, glyphs, None).map(|(subset, _)| subset)
}

/// Retains canonical component glyphs and returns one metric-bearing GID per
/// requested glyph. Advances are in font units, in the same order as `glyphs`.
pub(crate) fn subset_truetype_with_advances(
    font: &Font,
    glyphs: &[u16],
    advances: &[u16],
) -> Result<(TrueTypeSubset, Vec<u16>)> {
    if advances.len() != glyphs.len() {
        return Err(FontError::Malformed("subset advance count"));
    }
    build_subset(font, glyphs, Some(advances))
}

fn build_subset(
    font: &Font,
    glyphs: &[u16],
    requested: Option<&[u16]>,
) -> Result<(TrueTypeSubset, Vec<u16>)> {
    let face = font.truetype_face().ok_or(FontError::Unsupported(
        "subsetting needs TrueType glyf outlines",
    ))?;
    let glyf = face.glyf.as_ref().ok_or(FontError::Missing("glyf table"))?;
    let data = font.data();
    let num_glyphs = glyf.num_glyphs;
    if num_glyphs == 0 {
        return Err(FontError::Missing("glyphs"));
    }

    let mut keep = BTreeSet::from([0u16]);
    let mut stack = vec![0u16];
    for &g in glyphs {
        if g < num_glyphs && keep.insert(g) {
            stack.push(g);
        }
    }
    while let Some(g) = stack.pop() {
        for child in composite_refs(glyf.glyph_data(data, g)?)?
            .into_iter()
            .map(|c| c.gid)
        {
            if child < num_glyphs && keep.insert(child) {
                stack.push(child);
            }
        }
    }
    let glyph_map: Vec<(u16, u16)> = keep
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new as u16))
        .collect();
    let hmtx = face
        .hmtx
        .and_then(|(o, l)| data.get(o..o + l))
        .unwrap_or(&[]);
    let num_hmetrics = face.metrics.num_hmetrics;
    let mut aliases = Vec::new();
    let mut mapped = Vec::new();
    if let Some(requested) = requested {
        mapped.reserve(glyphs.len());
        let mut alias_ids = BTreeMap::new();
        for (&old, &advance) in glyphs.iter().zip(requested) {
            let canonical =
                new_gid(&glyph_map, old).ok_or(FontError::Malformed("subset glyph id"))?;
            if sfnt::hmtx_advance(hmtx, num_hmetrics, old).unwrap_or(0) == advance {
                mapped.push(canonical);
                continue;
            }
            let gid = match alias_ids.entry((old, advance)) {
                std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let count = u16::try_from(glyph_map.len() + aliases.len() + 1)
                        .map_err(|_| FontError::LimitExceeded("subset glyph count"))?;
                    aliases.push((old, advance));
                    *entry.insert(count - 1)
                }
            };
            mapped.push(gid);
        }
    }
    let count = u16::try_from(glyph_map.len() + aliases.len())
        .map_err(|_| FontError::LimitExceeded("subset glyph count"))?;
    let mut glyf_out = Vec::new();
    let mut loca = Vec::with_capacity((usize::from(count) + 1) * 4);
    let offset =
        |len: usize| u32::try_from(len).map_err(|_| FontError::LimitExceeded("subset glyf size"));
    for &(old, _) in &glyph_map {
        loca.extend_from_slice(&offset(glyf_out.len())?.to_be_bytes());
        let src = glyf.glyph_data(data, old)?;
        let start = glyf_out.len();
        glyf_out.extend_from_slice(src);
        for component in composite_refs(src)? {
            // Components past the glyph count become .notdef.
            let new = new_gid(&glyph_map, component.gid).unwrap_or(0);
            let pos = start + component.gid_offset;
            glyf_out[pos..pos + 2].copy_from_slice(&new.to_be_bytes());
        }
        glyf_out.resize(glyf_out.len().next_multiple_of(4), 0);
    }
    let mut has_compound_alias = false;
    for &(old, _) in &aliases {
        loca.extend_from_slice(&offset(glyf_out.len())?.to_be_bytes());
        let src = glyf.glyph_data(data, old)?;
        if let Some(bounds) = src.get(2..10) {
            // An identity composite shares the canonical outline, but deliberately
            // omits USE_MY_METRICS so its own hmtx advance takes effect.
            glyf_out.extend_from_slice(&(-1i16).to_be_bytes());
            glyf_out.extend_from_slice(bounds);
            glyf_out.extend_from_slice(&3u16.to_be_bytes()); // word arguments, XY offsets
            let canonical =
                new_gid(&glyph_map, old).ok_or(FontError::Malformed("subset alias glyph"))?;
            glyf_out.extend_from_slice(&canonical.to_be_bytes());
            glyf_out.extend_from_slice(&[0; 4]);
            glyf_out.resize(glyf_out.len().next_multiple_of(4), 0);
            has_compound_alias = true;
        }
    }
    loca.extend_from_slice(&offset(glyf_out.len())?.to_be_bytes());

    let mut advances = Vec::with_capacity(usize::from(count));
    let mut hmtx_out = Vec::with_capacity(usize::from(count) * 4);
    for &(old, _) in &glyph_map {
        let advance = sfnt::hmtx_advance(hmtx, num_hmetrics, old).unwrap_or(0);
        let lsb = sfnt::hmtx_lsb(hmtx, num_hmetrics, old).unwrap_or(0);
        advances.push(advance);
        hmtx_out.extend_from_slice(&advance.to_be_bytes());
        hmtx_out.extend_from_slice(&lsb.to_be_bytes());
    }
    let mut alias_min_lsb = i16::MAX;
    let mut alias_min_rsb = i32::MAX;
    let mut alias_max_extent = i32::MIN;
    for &(old, advance) in &aliases {
        let src = glyf.glyph_data(data, old)?;
        let old_lsb = sfnt::hmtx_lsb(hmtx, num_hmetrics, old).unwrap_or(0);
        let lsb = match (
            read_i16(src, 2),
            glyf.phantom_origin(data, hmtx, num_hmetrics, old),
        ) {
            (Some(x_min), Some(origin)) => i16::try_from(i32::from(x_min) - origin)
                .map_err(|_| FontError::LimitExceeded("subset alias bearing"))?,
            _ => old_lsb,
        };
        if let (Some(x_min), Some(x_max)) = (read_i16(src, 2), read_i16(src, 6)) {
            let extent = i32::from(lsb) + i32::from(x_max) - i32::from(x_min);
            alias_min_lsb = alias_min_lsb.min(lsb);
            alias_min_rsb = alias_min_rsb.min(i32::from(advance) - extent);
            alias_max_extent = alias_max_extent.max(extent);
        }
        advances.push(advance);
        hmtx_out.extend_from_slice(&advance.to_be_bytes());
        hmtx_out.extend_from_slice(&lsb.to_be_bytes());
    }

    let table = |tag: &[u8; 4]| face.sfnt.table(data, tag);
    let mut head = table(b"head")
        .ok_or(FontError::Missing("head table"))?
        .to_vec();
    if head.len() < 54 {
        return Err(FontError::Truncated("head table"));
    }
    head[8..12].fill(0);
    head[50..52].copy_from_slice(&1u16.to_be_bytes());
    let mut hhea = table(b"hhea")
        .ok_or(FontError::Missing("hhea table"))?
        .to_vec();
    if hhea.len() < 36 {
        return Err(FontError::Truncated("hhea table"));
    }
    hhea[10..12].copy_from_slice(&advances.iter().copied().max().unwrap_or(0).to_be_bytes());
    hhea[34..36].copy_from_slice(&count.to_be_bytes());
    if has_compound_alias {
        for (at, value, minimum) in [
            (12, i32::from(alias_min_lsb), true),
            (14, alias_min_rsb, true),
            (16, alias_max_extent, false),
        ] {
            let old = i32::from(read_i16(&hhea, at).ok_or(FontError::Truncated("hhea bounds"))?);
            let value = if minimum {
                old.min(value)
            } else {
                old.max(value)
            };
            let value = i16::try_from(value)
                .map_err(|_| FontError::LimitExceeded("subset alias extent"))?;
            hhea[at..at + 2].copy_from_slice(&value.to_be_bytes());
        }
    }
    let mut maxp = table(b"maxp")
        .ok_or(FontError::Missing("maxp table"))?
        .to_vec();
    if maxp.len() < 6 {
        return Err(FontError::Truncated("maxp table"));
    }
    maxp[4..6].copy_from_slice(&count.to_be_bytes());
    if has_compound_alias {
        for (simple, compound) in [(6, 10), (8, 12)] {
            let simple =
                read_u16(&maxp, simple).ok_or(FontError::Truncated("maxp outline limits"))?;
            let old =
                read_u16(&maxp, compound).ok_or(FontError::Truncated("maxp composite limits"))?;
            maxp[compound..compound + 2].copy_from_slice(&old.max(simple).to_be_bytes());
        }
        let elements = read_u16(&maxp, 28)
            .ok_or(FontError::Truncated("maxp component count"))?
            .max(1);
        maxp[28..30].copy_from_slice(&elements.to_be_bytes());
        let depth = read_u16(&maxp, 30)
            .ok_or(FontError::Truncated("maxp component depth"))?
            .checked_add(1)
            .ok_or(FontError::LimitExceeded("subset component depth"))?;
        maxp[30..32].copy_from_slice(&depth.to_be_bytes());
    }

    let mut post = vec![0u8; 32];
    post[0..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());
    if let Some(src) = table(b"post").and_then(|p| p.get(4..16)) {
        post[4..16].copy_from_slice(src);
    }
    let mut tables: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"cmap", build_cmap(font, &glyph_map)),
        (*b"glyf", glyf_out),
        (*b"head", head),
        (*b"hhea", hhea),
        (*b"hmtx", hmtx_out),
        (*b"loca", loca),
        (*b"maxp", maxp),
        (*b"name", build_name(table(b"name"))),
        (*b"post", post),
    ];
    for tag in COPIED {
        if let Some(t) = table(tag) {
            tables.push((*tag, t.to_vec()));
        }
    }
    Ok((
        TrueTypeSubset {
            data: assemble(tables),
            glyph_map,
            advances,
            units_per_em: face.metrics.units_per_em,
        },
        mapped,
    ))
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.chunks(4).fold(0u32, |acc, c| {
        let mut word = [0u8; 4];
        word[..c.len()].copy_from_slice(c);
        acc.wrapping_add(u32::from_be_bytes(word))
    })
}

/// Writes the table directory and tables (sorted by tag, 4-byte aligned) and sets
/// head.checkSumAdjustment.
fn assemble(mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|(tag, _)| *tag);
    let num = tables.len() as u16;
    let entry_selector = 15 - num.max(1).leading_zeros() as u16;
    let search_range = (1u16 << entry_selector) * 16;
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    out.extend_from_slice(&num.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&(num * 16 - search_range).to_be_bytes());
    let mut offset = 12 + tables.len() * 16;
    let mut head_offset = None;
    for (tag, body) in &tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&checksum(body).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        if tag == b"head" {
            head_offset = Some(offset);
        }
        offset += body.len().next_multiple_of(4);
    }
    for (_, body) in &tables {
        out.extend_from_slice(body);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if let Some(h) = head_offset {
        let adjust = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        out[h + 8..h + 12].copy_from_slice(&adjust.to_be_bytes());
    }
    out
}

/// A (3,1) format 4 subtable for the BMP, plus (3,10) format 12 when needed; a (3,0) format 4
/// subtable for symbol fonts without a Unicode cmap.
fn build_cmap(font: &Font, glyph_map: &[(u16, u16)]) -> Vec<u8> {
    let remap = |pairs: Vec<(u32, u16)>| -> Vec<(u32, u16)> {
        let mut out: Vec<(u32, u16)> = pairs
            .into_iter()
            .filter_map(|(c, g)| new_gid(glyph_map, g).filter(|&n| n != 0).map(|n| (c, n)))
            .collect();
        out.sort_unstable();
        out.dedup_by_key(|(c, _)| *c);
        out
    };
    let unicode = remap(
        font.unicode_mappings()
            .into_iter()
            .map(|(c, g)| (u32::from(c), g))
            .collect(),
    );
    let mut subtables: Vec<(u16, u16, Vec<u8>)> = Vec::new();
    if unicode.is_empty() {
        let symbol = remap(font.cmap_mappings(3, 0));
        if let Some(t) = format4(&symbol) {
            subtables.push((3, 0, t));
        }
    } else {
        let bmp: Vec<(u32, u16)> = unicode
            .iter()
            .copied()
            .filter(|&(c, _)| c < 0xFFFF)
            .collect();
        let f4 = format4(&bmp);
        let needs_12 = f4.is_none() || unicode.iter().any(|&(c, _)| c > 0xFFFF);
        if let Some(t) = f4 {
            subtables.push((3, 1, t));
        }
        if needs_12 {
            subtables.push((3, 10, format12(&unicode)));
        }
    }
    if subtables.is_empty()
        && let Some(t) = format4(&[])
    {
        subtables.push((3, 1, t));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(subtables.len() as u16).to_be_bytes());
    let mut offset = 4 + subtables.len() * 8;
    for (platform, encoding, body) in &subtables {
        out.extend_from_slice(&platform.to_be_bytes());
        out.extend_from_slice(&encoding.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        offset += body.len();
    }
    for (_, _, body) in subtables {
        out.extend_from_slice(&body);
    }
    out
}

/// Format 4 subtable from (code, glyph) pairs sorted by code, codes below 0xFFFF. `None` when
/// the segments do not fit the 16-bit length.
fn format4(pairs: &[(u32, u16)]) -> Option<Vec<u8>> {
    // (start, end, delta); consecutive codes with consecutive glyphs share a segment.
    let mut segs: Vec<(u16, u16, u16)> = Vec::new();
    for &(code, gid) in pairs {
        let Ok(code) = u16::try_from(code) else {
            continue;
        };
        if code == 0xFFFF {
            continue;
        }
        let delta = gid.wrapping_sub(code);
        match segs.last_mut() {
            Some(last) if last.1.checked_add(1) == Some(code) && last.2 == delta => last.1 = code,
            _ => segs.push((code, code, delta)),
        }
    }
    segs.push((0xFFFF, 0xFFFF, 1));
    let seg_count = segs.len();
    let len = 16 + seg_count * 8;
    let len = u16::try_from(len).ok()?;
    let seg_x2 = (seg_count * 2) as u16;
    let entry_selector = 15 - (seg_count as u16).leading_zeros() as u16;
    let search_range = 2u16 << entry_selector;
    let mut out = Vec::with_capacity(usize::from(len));
    for v in [
        4u16,
        len,
        0,
        seg_x2,
        search_range,
        entry_selector,
        seg_x2 - search_range,
    ] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    for s in &segs {
        out.extend_from_slice(&s.1.to_be_bytes());
    }
    out.extend_from_slice(&0u16.to_be_bytes());
    for s in &segs {
        out.extend_from_slice(&s.0.to_be_bytes());
    }
    for s in &segs {
        out.extend_from_slice(&s.2.to_be_bytes());
    }
    for _ in &segs {
        out.extend_from_slice(&0u16.to_be_bytes());
    }
    Some(out)
}

/// Format 12 subtable from (code, glyph) pairs sorted by code.
fn format12(pairs: &[(u32, u16)]) -> Vec<u8> {
    let mut groups: Vec<(u32, u32, u32)> = Vec::new();
    for &(code, gid) in pairs {
        let gid = u32::from(gid);
        match groups.last_mut() {
            Some(last) if last.1 + 1 == code && last.2 + (code - last.0) == gid => last.1 = code,
            _ => groups.push((code, code, gid)),
        }
    }
    let mut out = Vec::with_capacity(16 + groups.len() * 12);
    out.extend_from_slice(&12u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&((16 + groups.len() * 12) as u32).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(groups.len() as u32).to_be_bytes());
    for (start, end, gid) in groups {
        for v in [start, end, gid] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
    out
}

/// A format 0 name table with the family, style, unique, full, and PostScript names as
/// Windows Unicode (3,1,0x409) records.
fn build_name(original: Option<&[u8]>) -> Vec<u8> {
    let records: Vec<(u16, Vec<u8>)> = NAME_IDS
        .iter()
        .filter_map(|&id| {
            let text = sfnt::name_string(original?, id)?;
            Some((id, text.encode_utf16().flat_map(u16::to_be_bytes).collect()))
        })
        .collect();
    let count = records.len() as u16;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&(6 + count * 12).to_be_bytes());
    let mut offset = 0usize;
    for (id, bytes) in &records {
        let len = bytes.len().min(0xFFFF);
        for v in [3u16, 1, 0x409, *id, len as u16, offset as u16] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        offset += len;
    }
    for (_, bytes) in &records {
        out.extend_from_slice(&bytes[..bytes.len().min(0xFFFF)]);
    }
    out
}
