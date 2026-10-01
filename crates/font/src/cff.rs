//! Compact Font Format (CFF version 1): bare FontFile3 programs and the OpenType `CFF ` table.

use std::collections::HashMap;

use crate::charstring::{Type2Context, run_type2};
use crate::data::cff_tables::{
    EXPERT_CHARSET, EXPERT_ENCODING_SIDS, EXPERT_SUBSET_CHARSET, STANDARD_ENCODING_SIDS,
    STANDARD_STRINGS,
};
use crate::error::{FontError, Result};
use crate::outline::Outline;
use crate::reader::read_u16;

/// Most operands one DICT operator may take.
const MAX_DICT_OPERANDS: usize = 48;
/// Most Font DICTs accepted in an FDArray.
const MAX_FDS: u32 = 256;

const OP_CHARSET: u16 = 15;
const OP_ENCODING: u16 = 16;
const OP_CHARSTRINGS: u16 = 17;
const OP_PRIVATE: u16 = 18;
const OP_SUBRS: u16 = 19;
const OP_DEFAULT_WIDTH: u16 = 20;
const OP_NOMINAL_WIDTH: u16 = 21;
const OP_FULL_NAME: u16 = 2;
const OP_FAMILY_NAME: u16 = 3;
const OP_WEIGHT: u16 = 4;
const OP_FONT_BBOX: u16 = 5;
const OP_FIXED_PITCH: u16 = 0x0C01;
const OP_ITALIC_ANGLE: u16 = 0x0C02;
const OP_CHARSTRING_TYPE: u16 = 0x0C06;
const OP_FONT_MATRIX: u16 = 0x0C07;
const OP_ROS: u16 = 0x0C1E;
const OP_FDARRAY: u16 = 0x0C24;
const OP_FDSELECT: u16 = 0x0C25;

const DEFAULT_MATRIX: [f64; 6] = [0.001, 0.0, 0.0, 0.001, 0.0, 0.0];

/// A CFF INDEX; positions are relative to the start of the CFF data.
#[derive(Debug, Clone, Default)]
pub(crate) struct Index {
    count: u32,
    off_size: usize,
    offsets: usize,
    data_base: usize,
    end: usize,
}

impl Index {
    pub(crate) fn parse(cff: &[u8], pos: usize) -> Result<Index> {
        let count = u32::from(read_u16(cff, pos).ok_or(FontError::Truncated("CFF INDEX"))?);
        if count == 0 {
            return Ok(Index {
                count: 0,
                off_size: 1,
                offsets: pos + 2,
                data_base: pos + 2,
                end: pos + 2,
            });
        }
        let off_size = usize::from(*cff.get(pos + 2).ok_or(FontError::Truncated("CFF INDEX"))?);
        if !(1..=4).contains(&off_size) {
            return Err(FontError::Malformed("CFF INDEX offset size"));
        }
        let offsets = pos + 3;
        let data_base = offsets + (count as usize + 1) * off_size - 1;
        let mut index = Index {
            count,
            off_size,
            offsets,
            data_base,
            end: 0,
        };
        let last = index
            .offset(cff, count)
            .ok_or(FontError::Truncated("CFF INDEX"))?;
        let end = data_base
            .checked_add(last)
            .ok_or(FontError::Malformed("CFF INDEX"))?;
        if end > cff.len() {
            return Err(FontError::Truncated("CFF INDEX data"));
        }
        index.end = end;
        Ok(index)
    }

    fn offset(&self, cff: &[u8], i: u32) -> Option<usize> {
        let p = self.offsets + i as usize * self.off_size;
        let bytes = cff.get(p..p + self.off_size)?;
        Some(bytes.iter().fold(0usize, |a, &b| a << 8 | usize::from(b)))
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn get<'a>(&self, cff: &'a [u8], i: u32) -> Option<&'a [u8]> {
        if i >= self.count {
            return None;
        }
        let (a, b) = (self.offset(cff, i)?, self.offset(cff, i + 1)?);
        if a == 0 || a > b {
            return None;
        }
        let (s, e) = (self.data_base + a, self.data_base + b);
        if e > self.end {
            return None;
        }
        cff.get(s..e)
    }

    pub(crate) fn end(&self) -> usize {
        self.end
    }
}

/// A parsed Top, Font, or Private DICT.
#[derive(Debug, Default)]
struct Dict {
    entries: Vec<(u16, Vec<f64>)>,
}

fn parse_real(data: &[u8], pos: &mut usize) -> Result<f64> {
    let mut s = String::new();
    'outer: loop {
        let b = *data
            .get(*pos)
            .ok_or(FontError::Truncated("CFF real number"))?;
        *pos += 1;
        for nibble in [b >> 4, b & 0x0F] {
            match nibble {
                0..=9 => s.push(char::from(b'0' + nibble)),
                0xA => s.push('.'),
                0xB => s.push('E'),
                0xC => s.push_str("E-"),
                0xE => s.push('-'),
                0xF => break 'outer,
                _ => {}
            }
        }
        if s.len() > 64 {
            return Err(FontError::Malformed("CFF real number"));
        }
    }
    Ok(s.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0))
}

impl Dict {
    fn parse(data: &[u8]) -> Result<Dict> {
        let mut entries = Vec::new();
        let mut operands: Vec<f64> = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            let b0 = data[pos];
            pos += 1;
            let value = match b0 {
                0..=11 | 13..=21 => {
                    entries.push((u16::from(b0), std::mem::take(&mut operands)));
                    continue;
                }
                12 => {
                    let b1 = *data.get(pos).ok_or(FontError::Truncated("CFF DICT"))?;
                    pos += 1;
                    entries.push((0x0C00 | u16::from(b1), std::mem::take(&mut operands)));
                    continue;
                }
                28 => {
                    let v = data
                        .get(pos..pos + 2)
                        .ok_or(FontError::Truncated("CFF DICT"))?;
                    pos += 2;
                    f64::from(i16::from_be_bytes([v[0], v[1]]))
                }
                29 => {
                    let v = data
                        .get(pos..pos + 4)
                        .ok_or(FontError::Truncated("CFF DICT"))?;
                    pos += 4;
                    f64::from(i32::from_be_bytes([v[0], v[1], v[2], v[3]]))
                }
                30 => parse_real(data, &mut pos)?,
                32..=246 => f64::from(b0) - 139.0,
                247..=250 => {
                    let b1 = *data.get(pos).ok_or(FontError::Truncated("CFF DICT"))?;
                    pos += 1;
                    f64::from(b0 - 247) * 256.0 + f64::from(b1) + 108.0
                }
                251..=254 => {
                    let b1 = *data.get(pos).ok_or(FontError::Truncated("CFF DICT"))?;
                    pos += 1;
                    -f64::from(b0 - 251) * 256.0 - f64::from(b1) - 108.0
                }
                _ => return Err(FontError::Malformed("CFF DICT operand")),
            };
            if operands.len() >= MAX_DICT_OPERANDS {
                return Err(FontError::LimitExceeded("CFF DICT operands"));
            }
            operands.push(value);
        }
        Ok(Dict { entries })
    }

    fn get(&self, op: u16) -> Option<&[f64]> {
        self.entries
            .iter()
            .rev()
            .find(|(o, _)| *o == op)
            .map(|(_, v)| v.as_slice())
    }

    fn number(&self, op: u16) -> Option<f64> {
        self.get(op).and_then(|v| v.first().copied())
    }

    fn offset(&self, op: u16) -> Option<usize> {
        let v = self.number(op)?;
        (v >= 0.0 && v.fract() == 0.0 && v < 1e9).then_some(v as usize)
    }

    fn matrix(&self) -> Option<[f64; 6]> {
        let v = self.get(OP_FONT_MATRIX)?;
        let m: [f64; 6] = v.get(..6)?.try_into().ok()?;
        let det = m[0] * m[3] - m[1] * m[2];
        (det != 0.0 && det.is_finite()).then_some(m)
    }
}

#[derive(Debug, Clone, Default)]
struct PrivateInfo {
    subrs: Option<Index>,
    default_width: f64,
    nominal_width: f64,
}

fn parse_private(cff: &[u8], dict: &Dict) -> Result<PrivateInfo> {
    let Some(v) = dict.get(OP_PRIVATE) else {
        return Ok(PrivateInfo::default());
    };
    let [size, offset] = v else {
        return Err(FontError::Malformed("CFF Private operands"));
    };
    if *size < 0.0 || *offset < 0.0 {
        return Err(FontError::Malformed("CFF Private operands"));
    }
    let (size, offset) = (*size as usize, *offset as usize);
    let data = cff
        .get(offset..offset.saturating_add(size))
        .ok_or(FontError::Truncated("CFF Private DICT"))?;
    let private = Dict::parse(data)?;
    let subrs = match private.offset(OP_SUBRS) {
        Some(rel) if rel > 0 => Some(Index::parse(cff, offset + rel)?),
        _ => None,
    };
    Ok(PrivateInfo {
        subrs,
        default_width: private.number(OP_DEFAULT_WIDTH).unwrap_or(0.0),
        nominal_width: private.number(OP_NOMINAL_WIDTH).unwrap_or(0.0),
    })
}

#[derive(Debug, Clone)]
struct FontDict {
    private: PrivateInfo,
    /// Transform from this dictionary's glyph space into the font's main glyph space.
    to_main: Option<[f64; 6]>,
}

fn multiply(a: &[f64; 6], b: &[f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

fn invert(m: &[f64; 6]) -> Option<[f64; 6]> {
    let det = m[0] * m[3] - m[1] * m[2];
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    Some([a, b, c, d, -(m[4] * a + m[5] * c), -(m[4] * b + m[5] * d)])
}

/// A parsed CFF font (the first font of the FontSet).
#[derive(Debug, Clone)]
pub(crate) struct Cff {
    pub(crate) name: String,
    pub(crate) full_name: Option<String>,
    pub(crate) family_name: Option<String>,
    pub(crate) weight: Option<String>,
    charstrings: Index,
    global_subrs: Index,
    strings: Index,
    /// Glyph id to SID (name-keyed) or CID (CID-keyed).
    charset: Vec<u16>,
    /// SID (name-keyed) or CID (CID-keyed) to glyph id.
    reverse: HashMap<u16, u16>,
    names: HashMap<String, u16>,
    encoding: Option<Box<[u16; 256]>>,
    private: PrivateInfo,
    fds: Vec<FontDict>,
    fd_select: Vec<u8>,
    pub(crate) font_matrix: [f64; 6],
    pub(crate) bbox: [f64; 4],
    pub(crate) italic_angle: f64,
    pub(crate) is_fixed_pitch: bool,
    pub(crate) ros: Option<(String, String, i32)>,
}

impl Cff {
    pub(crate) fn parse(cff: &[u8]) -> Result<Cff> {
        if cff.len() < 4 {
            return Err(FontError::Truncated("CFF header"));
        }
        if cff[0] != 1 {
            return Err(FontError::Unsupported("CFF version other than 1"));
        }
        let header_size = usize::from(cff[2]);
        let names = Index::parse(cff, header_size)?;
        let top_dicts = Index::parse(cff, names.end())?;
        let strings = Index::parse(cff, top_dicts.end())?;
        let global_subrs = Index::parse(cff, strings.end())?;
        let name = names
            .get(cff, 0)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let top = Dict::parse(
            top_dicts
                .get(cff, 0)
                .ok_or(FontError::Missing("CFF Top DICT"))?,
        )?;
        if top.number(OP_CHARSTRING_TYPE).is_some_and(|t| t != 2.0) {
            return Err(FontError::Unsupported("CFF charstring type other than 2"));
        }
        let cs_offset = top
            .offset(OP_CHARSTRINGS)
            .ok_or(FontError::Missing("CFF CharStrings"))?;
        let charstrings = Index::parse(cff, cs_offset)?;
        let num_glyphs = charstrings.count();
        if num_glyphs == 0 || num_glyphs > 65535 {
            return Err(FontError::Malformed("CFF glyph count"));
        }
        let mut font = Cff {
            name,
            full_name: None,
            family_name: None,
            weight: None,
            charstrings,
            global_subrs,
            strings,
            charset: Vec::new(),
            reverse: HashMap::new(),
            names: HashMap::new(),
            encoding: None,
            private: PrivateInfo::default(),
            fds: Vec::new(),
            fd_select: Vec::new(),
            font_matrix: DEFAULT_MATRIX,
            bbox: [0.0; 4],
            italic_angle: top.number(OP_ITALIC_ANGLE).unwrap_or(0.0),
            is_fixed_pitch: top.number(OP_FIXED_PITCH).is_some_and(|v| v != 0.0),
            ros: None,
        };
        let sid_text = |op: u16, font: &Cff| {
            let sid = top.number(op)?;
            (0.0..65536.0)
                .contains(&sid)
                .then(|| font.string(cff, sid as u16))
                .flatten()
        };
        font.full_name = sid_text(OP_FULL_NAME, &font);
        font.family_name = sid_text(OP_FAMILY_NAME, &font);
        font.weight = sid_text(OP_WEIGHT, &font);
        if let Some([a, b, c, d]) = top
            .get(OP_FONT_BBOX)
            .and_then(|v| v.get(..4))
            .and_then(|v| <[f64; 4]>::try_from(v).ok())
        {
            font.bbox = [a, b, c, d];
        }
        if let Some(ros) = top.get(OP_ROS)
            && let [reg, ord, sup] = ros
        {
            let text = |v: f64| {
                font.string(cff, v.clamp(0.0, 65535.0) as u16)
                    .unwrap_or_default()
            };
            font.ros = Some((text(*reg), text(*ord), *sup as i32));
        }
        font.charset = parse_charset(
            cff,
            top.offset(OP_CHARSET).unwrap_or(0),
            num_glyphs as u16,
            font.ros.is_some(),
        )?;
        for (gid, &id) in font.charset.iter().enumerate() {
            font.reverse.entry(id).or_insert(gid as u16);
        }
        if font.ros.is_some() {
            font.parse_cid(cff, &top)?;
        } else {
            font.font_matrix = top.matrix().unwrap_or(DEFAULT_MATRIX);
            font.private = parse_private(cff, &top)?;
            for (gid, &sid) in font.charset.iter().enumerate() {
                if let Some(n) = font.string(cff, sid) {
                    font.names.entry(n).or_insert(gid as u16);
                }
            }
            font.encoding = Some(font.parse_encoding(cff, top.offset(OP_ENCODING).unwrap_or(0))?);
        }
        Ok(font)
    }

    fn parse_cid(&mut self, cff: &[u8], top: &Dict) -> Result<()> {
        let fd_index = Index::parse(
            cff,
            top.offset(OP_FDARRAY)
                .ok_or(FontError::Missing("CFF FDArray"))?,
        )?;
        if fd_index.count() == 0 || fd_index.count() > MAX_FDS {
            return Err(FontError::Malformed("CFF FDArray size"));
        }
        // A Font DICT matrix is concatenated with an explicit Top DICT matrix, used alone when the
        // Top DICT has none, and replaced by the Top DICT (or default) matrix when absent.
        let top_matrix = top.matrix();
        let mut effective = Vec::new();
        for i in 0..fd_index.count() {
            let fd = Dict::parse(
                fd_index
                    .get(cff, i)
                    .ok_or(FontError::Malformed("CFF FDArray entry"))?,
            )?;
            let private = parse_private(cff, &fd)?;
            effective.push(match (fd.matrix(), top_matrix) {
                (Some(f), Some(t)) => multiply(&f, &t),
                (Some(f), None) => f,
                (None, Some(t)) => t,
                (None, None) => DEFAULT_MATRIX,
            });
            self.fds.push(FontDict {
                private,
                to_main: None,
            });
        }
        // The first Font DICT's matrix defines the font's glyph units.
        let main = effective[0];
        self.font_matrix = main;
        if let Some(inv) = invert(&main) {
            for (fd, m) in self.fds.iter_mut().zip(&effective) {
                let rel = multiply(m, &inv);
                let identity = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
                if rel.iter().zip(identity).any(|(a, b)| (a - b).abs() > 1e-9) {
                    fd.to_main = Some(rel);
                }
            }
        }
        let num_glyphs = self.charstrings.count() as usize;
        let sel_offset = top
            .offset(OP_FDSELECT)
            .ok_or(FontError::Missing("CFF FDSelect"))?;
        self.fd_select = parse_fd_select(cff, sel_offset, num_glyphs)?;
        Ok(())
    }

    fn parse_encoding(&self, cff: &[u8], offset: usize) -> Result<Box<[u16; 256]>> {
        let mut map = Box::new([0u16; 256]);
        let predefined = match offset {
            0 => Some(&STANDARD_ENCODING_SIDS),
            1 => Some(&EXPERT_ENCODING_SIDS),
            _ => None,
        };
        if let Some(sids) = predefined {
            for (code, &sid) in sids.iter().enumerate() {
                if sid != 0 {
                    map[code] = self.reverse.get(&sid).copied().unwrap_or(0);
                }
            }
            return Ok(map);
        }
        let byte = |p: usize| {
            cff.get(p)
                .copied()
                .ok_or(FontError::Truncated("CFF Encoding"))
        };
        let format = byte(offset)?;
        let mut pos = offset + 1;
        let num_glyphs = self.charstrings.count();
        match format & 0x7F {
            0 => {
                let n = byte(pos)?;
                pos += 1;
                for gid in 1..=u32::from(n) {
                    let code = byte(pos)?;
                    pos += 1;
                    if gid < num_glyphs {
                        map[usize::from(code)] = gid as u16;
                    }
                }
            }
            1 => {
                let ranges = byte(pos)?;
                pos += 1;
                let mut gid = 1u32;
                for _ in 0..ranges {
                    let first = u32::from(byte(pos)?);
                    let left = u32::from(byte(pos + 1)?);
                    pos += 2;
                    for code in first..=(first + left).min(255) {
                        if gid < num_glyphs {
                            map[code as usize] = gid as u16;
                        }
                        gid += 1;
                    }
                }
            }
            _ => return Err(FontError::Unsupported("CFF encoding format")),
        }
        if format & 0x80 != 0 {
            let n = byte(pos)?;
            pos += 1;
            for _ in 0..n {
                let code = byte(pos)?;
                let sid = read_u16(cff, pos + 1)
                    .ok_or(FontError::Truncated("CFF Encoding supplement"))?;
                pos += 3;
                if let Some(&gid) = self.reverse.get(&sid) {
                    map[usize::from(code)] = gid;
                }
            }
        }
        Ok(map)
    }

    fn string(&self, cff: &[u8], sid: u16) -> Option<String> {
        match STANDARD_STRINGS.get(usize::from(sid)) {
            Some(s) => Some((*s).to_owned()),
            None => {
                let bytes = self
                    .strings
                    .get(cff, u32::from(sid) - STANDARD_STRINGS.len() as u32)?;
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
        }
    }

    pub(crate) fn num_glyphs(&self) -> u16 {
        self.charstrings.count() as u16
    }

    pub(crate) fn is_cid(&self) -> bool {
        self.ros.is_some()
    }

    pub(crate) fn glyph_name(&self, cff: &[u8], gid: u16) -> Option<String> {
        if self.is_cid() {
            return None;
        }
        self.string(cff, *self.charset.get(usize::from(gid))?)
    }

    pub(crate) fn glyph_by_name(&self, name: &str) -> Option<u16> {
        self.names.get(name).copied()
    }

    /// Glyph id for a CID of a CID-keyed font (through the charset).
    pub(crate) fn gid_for_cid(&self, cid: u16) -> Option<u16> {
        if !self.is_cid() {
            return None;
        }
        self.reverse.get(&cid).copied()
    }

    /// CID of a glyph in a CID-keyed font.
    pub(crate) fn cid_for_gid(&self, gid: u16) -> Option<u16> {
        if !self.is_cid() {
            return None;
        }
        self.charset.get(usize::from(gid)).copied()
    }

    /// Glyph for a code in the font's built-in encoding.
    pub(crate) fn gid_for_code(&self, code: u8) -> Option<u16> {
        let gid = self.encoding.as_ref()?[usize::from(code)];
        (gid != 0).then_some(gid)
    }

    /// Glyph names of the built-in encoding by code.
    pub(crate) fn encoding_names(&self, cff: &[u8]) -> Option<Vec<(u8, String)>> {
        let enc = self.encoding.as_ref()?;
        Some(
            (0..=255u8)
                .filter_map(|code| {
                    let gid = enc[usize::from(code)];
                    if gid == 0 {
                        return None;
                    }
                    self.glyph_name(cff, gid).map(|n| (code, n))
                })
                .collect(),
        )
    }

    fn font_dict(&self, gid: u16) -> (&PrivateInfo, Option<&[f64; 6]>) {
        if !self.is_cid() {
            return (&self.private, None);
        }
        let fd = self.fd_select.get(usize::from(gid)).copied().unwrap_or(0);
        match self.fds.get(usize::from(fd)).or_else(|| self.fds.first()) {
            Some(d) => (&d.private, d.to_main.as_ref()),
            None => (&self.private, None),
        }
    }

    /// Outline and advance width of a glyph, in the font's glyph units.
    pub(crate) fn glyph(&self, cff: &[u8], gid: u16) -> Result<(Outline, f64)> {
        self.glyph_inner(cff, gid, true)
    }

    fn glyph_inner(&self, cff: &[u8], gid: u16, allow_seac: bool) -> Result<(Outline, f64)> {
        let code = self
            .charstrings
            .get(cff, u32::from(gid))
            .ok_or(FontError::Missing("CFF glyph"))?;
        let (private, to_main) = self.font_dict(gid);
        let ctx = Type2Context {
            cff,
            global_subrs: &self.global_subrs,
            local_subrs: private.subrs.as_ref(),
        };
        let out = run_type2(&ctx, code)?;
        let mut width = out
            .width
            .map_or(private.default_width, |w| w + private.nominal_width);
        let mut outline = out.outline;
        if let Some(seac) = out.seac
            && allow_seac
            && !self.is_cid()
        {
            let gid_of = |code: u8| {
                let sid = STANDARD_ENCODING_SIDS[usize::from(code)];
                if sid == 0 {
                    None
                } else {
                    self.reverse.get(&sid).copied()
                }
            };
            let base = gid_of(seac.base_code).ok_or(FontError::Missing("seac base glyph"))?;
            let accent = gid_of(seac.accent_code).ok_or(FontError::Missing("seac accent glyph"))?;
            let (base_outline, _) = self.glyph_inner(cff, base, false)?;
            let (mut accent_outline, _) = self.glyph_inner(cff, accent, false)?;
            accent_outline.transform(&[1.0, 0.0, 0.0, 1.0, seac.adx, seac.ady]);
            let mut combined = base_outline;
            combined.append(outline);
            combined.append(accent_outline);
            outline = combined;
        }
        if let Some(m) = to_main {
            outline.transform(m);
            width *= m[0];
        }
        Ok((outline, width))
    }
}

fn parse_charset(cff: &[u8], offset: usize, num_glyphs: u16, cid: bool) -> Result<Vec<u16>> {
    let n = usize::from(num_glyphs);
    if offset <= 2 {
        // CID-keyed fonts need a custom charset; without one CIDs equal glyph ids.
        if cid {
            return Ok((0..n).map(|g| g as u16).collect());
        }
        let table: &[u16] = match offset {
            1 => &EXPERT_CHARSET,
            2 => &EXPERT_SUBSET_CHARSET,
            // ISOAdobe: SID equals glyph id for the first 229 glyphs.
            _ => return Ok((0..n).map(|g| if g < 229 { g as u16 } else { 0 }).collect()),
        };
        return Ok((0..n).map(|g| table.get(g).copied().unwrap_or(0)).collect());
    }
    let mut out = Vec::with_capacity(n);
    out.push(0u16);
    let byte = |p: usize| {
        cff.get(p)
            .copied()
            .ok_or(FontError::Truncated("CFF charset"))
    };
    let word = |p: usize| read_u16(cff, p).ok_or(FontError::Truncated("CFF charset"));
    let format = byte(offset)?;
    let mut pos = offset + 1;
    match format {
        0 => {
            while out.len() < n {
                out.push(word(pos)?);
                pos += 2;
            }
        }
        1 | 2 => {
            while out.len() < n {
                let first = u32::from(word(pos)?);
                let left = if format == 1 {
                    u32::from(byte(pos + 2)?)
                } else {
                    u32::from(word(pos + 2)?)
                };
                pos += if format == 1 { 3 } else { 4 };
                for k in 0..=left {
                    if out.len() >= n {
                        break;
                    }
                    out.push(
                        u16::try_from(first + k)
                            .map_err(|_| FontError::Malformed("CFF charset range"))?,
                    );
                }
            }
        }
        _ => return Err(FontError::Malformed("CFF charset format")),
    }
    Ok(out)
}

pub(crate) fn parse_fd_select(cff: &[u8], offset: usize, num_glyphs: usize) -> Result<Vec<u8>> {
    let format = *cff
        .get(offset)
        .ok_or(FontError::Truncated("CFF FDSelect"))?;
    match format {
        0 => Ok(cff
            .get(offset + 1..offset + 1 + num_glyphs)
            .ok_or(FontError::Truncated("CFF FDSelect"))?
            .to_vec()),
        3 => {
            let n =
                usize::from(read_u16(cff, offset + 1).ok_or(FontError::Truncated("CFF FDSelect"))?);
            let mut out = vec![0u8; num_glyphs];
            let mut pos = offset + 3;
            for _ in 0..n {
                let first =
                    usize::from(read_u16(cff, pos).ok_or(FontError::Truncated("CFF FDSelect"))?);
                let fd = *cff
                    .get(pos + 2)
                    .ok_or(FontError::Truncated("CFF FDSelect"))?;
                let next = usize::from(
                    read_u16(cff, pos + 3).ok_or(FontError::Truncated("CFF FDSelect"))?,
                );
                for slot in out.iter_mut().take(next.min(num_glyphs)).skip(first) {
                    *slot = fd;
                }
                pos += 3;
            }
            Ok(out)
        }
        _ => Err(FontError::Malformed("CFF FDSelect format")),
    }
}
