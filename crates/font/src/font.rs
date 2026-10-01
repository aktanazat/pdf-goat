//! One interface over TrueType, OpenType CFF, bare CFF, and Type 1 font programs.

use std::collections::HashMap;

use crate::cff::Cff;
use crate::encoding::{BaseEncoding, Encoding};
use crate::error::{FontError, Result};
use crate::glyf::GlyfTables;
use crate::glyphnames::glyph_name_to_unicode;
use crate::outline::{Outline, Rect};
use crate::sfnt::{self, CmapSubtable, Sfnt, SfntMetrics};
use crate::type1::Type1;

/// The container and outline format of a font program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontKind {
    /// sfnt with `glyf` outlines (FontFile2, .ttf, .ttc).
    TrueType,
    /// sfnt with a `CFF ` table (FontFile3/OpenType, .otf).
    OpenTypeCff,
    /// Bare CFF (FontFile3 Type1C or CIDFontType0C).
    Cff,
    /// Type 1 (FontFile, .pfa, .pfb).
    Type1,
}

/// How CIDs of a CIDFont select glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CidToGid<'a> {
    Identity,
    /// The bytes of a /CIDToGIDMap stream: two big-endian bytes per CID.
    Map(&'a [u8]),
}

/// Face-wide metrics in font units (see [`Font::units_per_em`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub cap_height: Option<f32>,
    pub x_height: Option<f32>,
    pub italic_angle: f32,
    pub bbox: Rect,
    pub is_fixed_pitch: bool,
    pub is_bold: bool,
    pub is_italic: bool,
    /// Weight class, 100..=900.
    pub weight: u16,
    /// PDF font descriptor flags: FixedPitch 1, Serif 2, Symbolic 4, Nonsymbolic 32, Italic 64.
    pub flags: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SfntFace {
    pub(crate) sfnt: Sfnt,
    pub(crate) metrics: SfntMetrics,
    cmaps: Vec<CmapSubtable>,
    /// Preferred Unicode subtable: index into `cmaps`.
    unicode_cmap: Option<usize>,
    pub(crate) glyf: Option<GlyfTables>,
    pub(crate) hmtx: Option<(usize, usize)>,
    post_names: Vec<Option<String>>,
    post_lookup: HashMap<String, u16>,
}

#[derive(Debug, Clone)]
enum Inner {
    TrueType(Box<SfntFace>),
    OpenTypeCff(Box<SfntFace>, Box<Cff>, (usize, usize)),
    Cff(Box<Cff>),
    Type1(Box<Type1>),
}

/// A parsed font program. Glyph ids are the font's own; for Type 1 they follow the order of the
/// CharStrings dictionary with `.notdef` first.
#[derive(Debug, Clone)]
pub struct Font {
    data: Vec<u8>,
    face_index: u32,
    inner: Inner,
    /// Unicode to glyph for fonts without a cmap, from glyph names.
    by_char: HashMap<char, u16>,
}

fn looks_like_cff(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 1 && data[2] >= 4 && (1..=4).contains(&data[3])
}

impl Font {
    /// Parses a font program, detecting its format. For a TrueType collection this is face 0.
    pub fn parse(data: Vec<u8>) -> Result<Font> {
        Font::parse_face(data, 0)
    }

    /// Parses face `index` of a TrueType collection (index 0 for any other font).
    pub fn parse_face(data: Vec<u8>, index: u32) -> Result<Font> {
        if let Some(offsets) = sfnt::collection_offsets(&data)? {
            let offset = *offsets
                .get(index as usize)
                .ok_or(FontError::Missing("collection face"))?;
            return Font::from_sfnt(data, offset, index);
        }
        if index != 0 {
            return Err(FontError::Missing("collection face"));
        }
        let version = data
            .get(0..4)
            .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]));
        if version.is_some_and(sfnt::is_sfnt_version) {
            return Font::from_sfnt(data, 0, 0);
        }
        if data.starts_with(b"wOFF") || data.starts_with(b"wOF2") {
            return Err(FontError::Unsupported("WOFF font"));
        }
        if looks_like_cff(&data) {
            let cff = Cff::parse(&data)?;
            return Ok(Font::finish(data, 0, Inner::Cff(Box::new(cff))));
        }
        if data.first() == Some(&0x80)
            || data.starts_with(b"%!")
            || data.windows(5).any(|w| w == b"eexec")
        {
            let t1 = Type1::parse(&data)?;
            return Ok(Font::finish(data, 0, Inner::Type1(Box::new(t1))));
        }
        Err(FontError::Unsupported("unrecognized font format"))
    }

    /// Number of faces: the count of a TrueType collection, otherwise 1.
    pub fn face_count(data: &[u8]) -> Result<u32> {
        Ok(sfnt::collection_offsets(data)?.map_or(1, |v| v.len() as u32))
    }

    fn from_sfnt(data: Vec<u8>, offset: usize, face_index: u32) -> Result<Font> {
        let sfnt = Sfnt::parse(&data, offset)?;
        let mut metrics = SfntMetrics::parse(&sfnt, &data)?;
        let cmaps = sfnt
            .record(b"cmap")
            .map(|r| sfnt::parse_cmap(&data, r))
            .unwrap_or_default();
        let unicode_cmap = preferred_unicode(&cmaps);
        let glyf = match (sfnt.record(b"loca"), sfnt.record(b"glyf")) {
            (Some(loca), Some(glyf)) => {
                let entry = if metrics.index_to_loc_long { 4 } else { 2 };
                if metrics.num_glyphs == 0 {
                    metrics.num_glyphs =
                        u16::try_from((loca.len / entry).saturating_sub(1)).unwrap_or(u16::MAX);
                }
                Some(GlyfTables {
                    loca: (loca.offset, loca.len),
                    glyf: (glyf.offset, glyf.len),
                    long_offsets: metrics.index_to_loc_long,
                    num_glyphs: metrics.num_glyphs,
                })
            }
            _ => None,
        };
        let hmtx = sfnt.record(b"hmtx").map(|r| (r.offset, r.len));
        let post_names = sfnt
            .table(&data, b"post")
            .and_then(|p| sfnt::post_glyph_names(p, metrics.num_glyphs))
            .unwrap_or_default();
        let mut post_lookup = HashMap::new();
        for (gid, name) in post_names.iter().enumerate() {
            if let Some(n) = name {
                post_lookup.entry(n.clone()).or_insert(gid as u16);
            }
        }
        let face = Box::new(SfntFace {
            sfnt,
            metrics,
            cmaps,
            unicode_cmap,
            glyf,
            hmtx,
            post_names,
            post_lookup,
        });
        if let Some(rec) = face.sfnt.record(b"CFF ") {
            let range = (rec.offset, rec.len);
            let cff = Cff::parse(&data[rec.offset..rec.offset + rec.len])?;
            return Ok(Font::finish(
                data,
                face_index,
                Inner::OpenTypeCff(face, Box::new(cff), range),
            ));
        }
        if face.sfnt.record(b"CFF2").is_some() && face.glyf.is_none() {
            return Err(FontError::Unsupported("CFF2 outlines"));
        }
        Ok(Font::finish(data, face_index, Inner::TrueType(face)))
    }

    fn finish(data: Vec<u8>, face_index: u32, inner: Inner) -> Font {
        let mut font = Font {
            data,
            face_index,
            inner,
            by_char: HashMap::new(),
        };
        if matches!(font.inner, Inner::Cff(_) | Inner::Type1(_)) {
            for gid in 0..font.num_glyphs() {
                if let Some(name) = font.glyph_name(gid)
                    && let Some(text) = glyph_name_to_unicode(&name)
                    && let Some(ch) = single_char(&text)
                {
                    font.by_char.entry(ch).or_insert(gid);
                }
            }
        }
        font
    }

    pub fn kind(&self) -> FontKind {
        match self.inner {
            Inner::TrueType(_) => FontKind::TrueType,
            Inner::OpenTypeCff(..) => FontKind::OpenTypeCff,
            Inner::Cff(_) => FontKind::Cff,
            Inner::Type1(_) => FontKind::Type1,
        }
    }

    /// The font program bytes this font was parsed from.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn face_index(&self) -> u32 {
        self.face_index
    }

    /// The sfnt face of a TrueType (glyf) font.
    pub(crate) fn truetype_face(&self) -> Option<&SfntFace> {
        match &self.inner {
            Inner::TrueType(f) => Some(f),
            _ => None,
        }
    }

    pub(crate) fn cff_bytes(&self) -> &[u8] {
        match &self.inner {
            Inner::OpenTypeCff(_, _, (off, len)) => &self.data[*off..*off + *len],
            _ => &self.data,
        }
    }

    /// The bytes of sfnt table `tag` of this face; `None` for bare CFF and Type 1 programs
    /// and for a table the face lacks.
    pub fn table(&self, tag: &[u8; 4]) -> Option<&[u8]> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => f.sfnt.table(&self.data, tag),
            Inner::Cff(_) | Inner::Type1(_) => None,
        }
    }

    pub fn num_glyphs(&self) -> u16 {
        match &self.inner {
            Inner::TrueType(f) => f.metrics.num_glyphs,
            Inner::OpenTypeCff(_, c, _) | Inner::Cff(c) => c.num_glyphs(),
            Inner::Type1(t) => t.num_glyphs(),
        }
    }

    /// Glyph units per em. Outlines and advances are in these units.
    pub fn units_per_em(&self) -> u16 {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => f.metrics.units_per_em,
            Inner::Cff(c) => units_from_matrix(&c.font_matrix),
            Inner::Type1(t) => units_from_matrix(&t.header.font_matrix),
        }
    }

    /// Maps glyph units to text space (one em = 1.0). CFF and Type 1 fonts may carry skew here.
    pub fn font_matrix(&self) -> [f64; 6] {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => {
                let s = 1.0 / f64::from(f.metrics.units_per_em);
                [s, 0.0, 0.0, s, 0.0, 0.0]
            }
            Inner::Cff(c) => c.font_matrix,
            Inner::Type1(t) => t.header.font_matrix,
        }
    }

    /// PostScript name: the name table entry 6, the CFF font name, or the Type 1 /FontName.
    pub fn postscript_name(&self) -> Option<String> {
        match &self.inner {
            Inner::TrueType(f) => self.name_entry(f, 6),
            Inner::OpenTypeCff(f, c, _) => self.name_entry(f, 6).or_else(|| Some(c.name.clone())),
            Inner::Cff(c) => Some(c.name.clone()).filter(|n| !n.is_empty()),
            Inner::Type1(t) => t.header.font_name.clone(),
        }
    }

    /// Family name: typographic family (name 16) or family (name 1); CFF and Type 1 FamilyName.
    pub fn family_name(&self) -> Option<String> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => {
                self.name_entry(f, 16).or_else(|| self.name_entry(f, 1))
            }
            Inner::Cff(c) => c.family_name.clone(),
            Inner::Type1(t) => t.header.family_name.clone(),
        }
    }

    /// Subfamily (style) name of an sfnt face, such as "Bold Italic".
    pub fn style_name(&self) -> Option<String> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => {
                self.name_entry(f, 17).or_else(|| self.name_entry(f, 2))
            }
            _ => None,
        }
    }

    fn name_entry(&self, f: &SfntFace, id: u16) -> Option<String> {
        sfnt::name_string(f.sfnt.table(&self.data, b"name")?, id)
    }

    pub fn metrics(&self) -> FontMetrics {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => self.sfnt_metrics(f),
            Inner::Cff(c) => {
                let weight = weight_from_name(c.weight.as_deref());
                ps_metrics(c.bbox, c.italic_angle, c.is_fixed_pitch, weight, self)
            }
            Inner::Type1(t) => {
                let h = &t.header;
                ps_metrics(
                    h.bbox,
                    h.italic_angle,
                    h.is_fixed_pitch,
                    weight_from_name(h.weight.as_deref()),
                    self,
                )
            }
        }
    }

    fn sfnt_metrics(&self, f: &SfntFace) -> FontMetrics {
        let m = &f.metrics;
        let os2 = m.os2.as_ref();
        let (mut ascent, mut descent) = (f32::from(m.ascender), f32::from(m.descender));
        if ascent == 0.0
            && descent == 0.0
            && let Some(o) = os2
        {
            ascent = f32::from(o.typo_ascender);
            descent = f32::from(o.typo_descender);
        }
        let is_bold = m.mac_style & 1 != 0 || os2.is_some_and(|o| o.fs_selection & 0x20 != 0);
        let is_italic = m.mac_style & 2 != 0
            || os2.is_some_and(|o| o.fs_selection & 1 != 0)
            || m.italic_angle != 0.0;
        let weight = os2.map_or(if is_bold { 700 } else { 400 }, |o| {
            o.weight_class.clamp(100, 900)
        });
        // sFamilyClass 1-5 and 7 are serif classes.
        let serif = os2.is_some_and(|o| matches!(o.family_class >> 8, 1..=5 | 7));
        let symbolic = self.glyph_for_char('A').is_none() && self.glyph_for_char('a').is_none();
        let mut flags = if symbolic { 4 } else { 32 };
        if m.is_fixed_pitch {
            flags |= 1;
        }
        if serif {
            flags |= 2;
        }
        if is_italic {
            flags |= 64;
        }
        FontMetrics {
            ascent,
            descent,
            line_gap: f32::from(m.line_gap),
            cap_height: os2.and_then(|o| o.cap_height).map(f32::from),
            x_height: os2.and_then(|o| o.x_height).map(f32::from),
            italic_angle: m.italic_angle,
            bbox: Rect {
                x_min: f32::from(m.bbox[0]),
                y_min: f32::from(m.bbox[1]),
                x_max: f32::from(m.bbox[2]),
                y_max: f32::from(m.bbox[3]),
            },
            is_fixed_pitch: m.is_fixed_pitch,
            is_bold,
            is_italic,
            weight,
            flags,
        }
    }

    /// True for CID-keyed CFF fonts, whose glyphs are selected by CID through the charset.
    pub fn is_cid_keyed(&self) -> bool {
        match &self.inner {
            Inner::OpenTypeCff(_, c, _) | Inner::Cff(c) => c.is_cid(),
            _ => false,
        }
    }

    /// Registry, ordering, and supplement of a CID-keyed CFF font.
    pub fn cid_system_info(&self) -> Option<(String, String, i32)> {
        match &self.inner {
            Inner::OpenTypeCff(_, c, _) | Inner::Cff(c) => c.ros.clone(),
            _ => None,
        }
    }

    /// Outline of a glyph in glyph units. CFF seac and Type 1 seac accents are composed.
    pub fn outline(&self, gid: u16) -> Result<Outline> {
        match &self.inner {
            Inner::TrueType(f) => self.truetype_outline(f, gid),
            Inner::OpenTypeCff(_, c, _) | Inner::Cff(c) => Ok(c.glyph(self.cff_bytes(), gid)?.0),
            Inner::Type1(t) => Ok(t.glyph(gid)?.0),
        }
    }

    /// Bounds in glyph units, or `None` for an empty outline.
    /// TrueType may use its conservative stored bounds; other formats use curve extrema.
    /// Unavailable or invalid glyph data returns an error, not an empty outline.
    pub fn glyph_bounds(&self, gid: u16) -> Result<Option<Rect>> {
        let Inner::TrueType(f) = &self.inner else {
            return Ok(self.outline(gid)?.bounds());
        };
        let glyf = f.glyf.as_ref().ok_or(FontError::Missing("glyf table"))?;
        let Some(mut bounds) = glyf.bounds(&self.data, gid)? else {
            return Ok(None);
        };
        if let Some((off, len)) = f.hmtx
            && let Some(origin) = glyf.phantom_origin(
                &self.data,
                &self.data[off..off + len],
                f.metrics.num_hmetrics,
                gid,
            )
        {
            bounds.x_min -= origin as f32;
            bounds.x_max -= origin as f32;
        }
        Ok(Some(bounds))
    }

    /// Like FreeType, the glyph is shifted so its origin sits at the header xMin minus the hmtx
    /// left side bearing (or at a USE_MY_METRICS component's origin for composites).
    fn truetype_outline(&self, f: &SfntFace, gid: u16) -> Result<Outline> {
        let glyf = f.glyf.as_ref().ok_or(FontError::Missing("glyf table"))?;
        let mut outline = glyf.outline(&self.data, gid)?;
        if let Some((off, len)) = f.hmtx
            && let Some(origin) = glyf.phantom_origin(
                &self.data,
                &self.data[off..off + len],
                f.metrics.num_hmetrics,
                gid,
            )
            && origin != 0
        {
            outline.transform(&[1.0, 0.0, 0.0, 1.0, -f64::from(origin), 0.0]);
        }
        Ok(outline)
    }

    /// Advance width of a glyph in glyph units.
    pub fn advance(&self, gid: u16) -> Option<f32> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => {
                if gid >= f.metrics.num_glyphs && f.metrics.num_glyphs != 0 {
                    return None;
                }
                let (off, len) = f.hmtx?;
                sfnt::hmtx_advance(&self.data[off..off + len], f.metrics.num_hmetrics, gid)
                    .map(f32::from)
            }
            Inner::Cff(c) => c.glyph(&self.data, gid).ok().map(|(_, w)| w as f32),
            Inner::Type1(t) => t.glyph(gid).ok().map(|(_, w)| w as f32),
        }
    }

    /// Glyph name from the post table, the CFF charset, or the Type 1 CharStrings.
    pub fn glyph_name(&self, gid: u16) -> Option<String> {
        match &self.inner {
            Inner::TrueType(f) => f.post_names.get(usize::from(gid)).cloned().flatten(),
            Inner::OpenTypeCff(f, c, _) => c
                .glyph_name(self.cff_bytes(), gid)
                .or_else(|| f.post_names.get(usize::from(gid)).cloned().flatten()),
            Inner::Cff(c) => c.glyph_name(&self.data, gid),
            Inner::Type1(t) => t.glyph_name(gid).map(str::to_owned),
        }
    }

    /// Glyph id for a glyph name.
    pub fn glyph_by_name(&self, name: &str) -> Option<u16> {
        match &self.inner {
            Inner::TrueType(f) => f.post_lookup.get(name).copied(),
            Inner::OpenTypeCff(f, c, _) => c
                .glyph_by_name(name)
                .or_else(|| f.post_lookup.get(name).copied()),
            Inner::Cff(c) => c.glyph_by_name(name),
            Inner::Type1(t) => t.glyph_by_name(name),
        }
    }

    /// Glyph for a Unicode character: the preferred Unicode cmap subtable, a (3,0) symbol
    /// subtable at U+F000 + code for characters below U+0100, or glyph names in CFF and Type 1.
    pub fn glyph_for_char(&self, ch: char) -> Option<u16> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => {
                let code = u32::from(ch);
                if let Some(i) = f.unicode_cmap {
                    return f.cmaps[i].lookup(&self.data, code);
                }
                if code < 0x100 {
                    return self
                        .find_cmap(3, 0)
                        .and_then(|t| t.lookup(&self.data, 0xF000 + code));
                }
                None
            }
            _ => self.by_char.get(&ch).copied(),
        }
    }

    /// Every (Unicode scalar, glyph) pair of the preferred Unicode cmap subtable.
    pub fn unicode_mappings(&self) -> Vec<(char, u16)> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => match f.unicode_cmap {
                Some(i) => f.cmaps[i]
                    .mappings(&self.data, 0x11_0000)
                    .into_iter()
                    .filter_map(|(c, g)| char::from_u32(c).map(|c| (c, g)))
                    .collect(),
                None => Vec::new(),
            },
            _ => {
                let mut v: Vec<(char, u16)> = self.by_char.iter().map(|(&c, &g)| (c, g)).collect();
                v.sort_unstable();
                v
            }
        }
    }

    /// (platform id, encoding id, format) of each cmap subtable.
    pub fn cmap_subtables(&self) -> Vec<(u16, u16, u16)> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => f
                .cmaps
                .iter()
                .map(|t| (t.platform, t.encoding, t.format))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Looks `code` up in the first cmap subtable with the given platform and encoding ids.
    pub fn cmap_lookup(&self, platform: u16, encoding: u16, code: u32) -> Option<u16> {
        self.find_cmap(platform, encoding)?.lookup(&self.data, code)
    }

    /// Every (code, glyph) pair of the first cmap subtable with the given platform and encoding.
    pub fn cmap_mappings(&self, platform: u16, encoding: u16) -> Vec<(u32, u16)> {
        self.find_cmap(platform, encoding)
            .map(|t| t.mappings(&self.data, 0x11_0000))
            .unwrap_or_default()
    }

    fn find_cmap(&self, platform: u16, encoding: u16) -> Option<&CmapSubtable> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => f
                .cmaps
                .iter()
                .find(|t| t.platform == platform && t.encoding == encoding),
            _ => None,
        }
    }

    /// Glyph for a code of a simple (single-byte) font. `name` is the glyph name the font's
    /// /Encoding assigns to the code, with /Differences applied, if any; `symbolic` is the
    /// Symbolic descriptor flag.
    ///
    /// TrueType: a non-symbolic font tries the name first (Unicode cmap through the glyph
    /// list, then the (1,0) cmap through MacRoman, then post names); then the (3,0) cmap at
    /// the code and at U+F000/F100/F200 + code, then the (1,0) cmap at the code. A symbolic font
    /// tries the code-based subtables first. CFF and Type 1: the glyph name, else the font's
    /// built-in encoding.
    pub fn simple_glyph(&self, code: u8, name: Option<&str>, symbolic: bool) -> Option<u16> {
        match &self.inner {
            Inner::TrueType(f) | Inner::OpenTypeCff(f, _, _) => {
                let cff_name = match &self.inner {
                    Inner::OpenTypeCff(_, c, _) => name.and_then(|n| c.glyph_by_name(n)),
                    _ => None,
                };
                if let Some(g) = cff_name {
                    return Some(g);
                }
                let by_name = || name.and_then(|n| self.sfnt_glyph_for_name(f, n));
                let by_code = || self.sfnt_glyph_for_code(code);
                if symbolic {
                    by_code().or_else(by_name)
                } else {
                    by_name().or_else(by_code)
                }
            }
            Inner::Cff(c) => name
                .and_then(|n| c.glyph_by_name(n))
                .or_else(|| c.gid_for_code(code)),
            Inner::Type1(t) => name.and_then(|n| t.glyph_by_name(n)).or_else(|| {
                let builtin = t.header.encoding.get(usize::from(code))?.as_deref()?;
                t.glyph_by_name(builtin)
            }),
        }
    }

    fn sfnt_glyph_for_name(&self, f: &SfntFace, name: &str) -> Option<u16> {
        if let Some(i) = f.unicode_cmap
            && let Some(text) = glyph_name_to_unicode(name)
            && let Some(ch) = single_char(&text)
            && let Some(g) = f.cmaps[i].lookup(&self.data, u32::from(ch))
        {
            return Some(g);
        }
        if let Some(mac) = BaseEncoding::MacRoman.code_for_name(name)
            && let Some(g) = self.cmap_lookup(1, 0, u32::from(mac))
        {
            return Some(g);
        }
        f.post_lookup.get(name).copied()
    }

    fn sfnt_glyph_for_code(&self, code: u8) -> Option<u16> {
        let code = u32::from(code);
        if let Some(t) = self.find_cmap(3, 0) {
            for base in [0, 0xF000, 0xF100, 0xF200] {
                if let Some(g) = t.lookup(&self.data, base + code) {
                    return Some(g);
                }
            }
        }
        self.cmap_lookup(1, 0, code)
    }

    /// The font's built-in encoding (Type 1 /Encoding, CFF encoding) as glyph names by code.
    pub fn builtin_encoding(&self) -> Option<Encoding> {
        match &self.inner {
            Inner::Type1(t) => {
                let mut enc = Encoding::new(None);
                for (code, name) in t.header.encoding.iter().enumerate() {
                    if let (Some(n), Ok(code)) = (name, u8::try_from(code)) {
                        enc.set(code, n);
                    }
                }
                Some(enc)
            }
            Inner::Cff(c) => {
                let mut enc = Encoding::new(None);
                for (code, name) in c.encoding_names(&self.data)? {
                    enc.set(code, &name);
                }
                Some(enc)
            }
            _ => None,
        }
    }

    /// Glyph for a CID of a CIDFont. CID-keyed CFF fonts map through their charset and ignore
    /// `map`; other CFF fonts use the CID as the glyph id; TrueType applies `map`.
    pub fn glyph_for_cid(&self, cid: u32, map: CidToGid<'_>) -> Option<u16> {
        let gid = match &self.inner {
            Inner::OpenTypeCff(_, c, _) | Inner::Cff(c) if c.is_cid() => {
                return c.gid_for_cid(u16::try_from(cid).ok()?);
            }
            Inner::Type1(_) => return None,
            _ => match map {
                CidToGid::Identity => u16::try_from(cid).ok()?,
                CidToGid::Map(bytes) => {
                    let i = usize::try_from(cid).ok()?.checked_mul(2)?;
                    u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?])
                }
            },
        };
        (gid < self.num_glyphs()).then_some(gid)
    }

    /// CID of a glyph in a CID-keyed CFF font.
    pub fn cid_for_glyph(&self, gid: u16) -> Option<u16> {
        match &self.inner {
            Inner::OpenTypeCff(_, c, _) | Inner::Cff(c) => c.cid_for_gid(gid),
            _ => None,
        }
    }
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(ch)
}

/// Preferred Unicode subtable: full-repertoire subtables first, then BMP ones.
fn preferred_unicode(cmaps: &[CmapSubtable]) -> Option<usize> {
    let rank = |t: &CmapSubtable| match (t.platform, t.encoding, t.format) {
        (3, 10, 12 | 13) => Some(0),
        (0, 4 | 6, 12 | 13) => Some(1),
        (3, 1, _) => Some(2),
        (0, 3, _) => Some(3),
        (0, 0..=2, _) => Some(4),
        (0, 4 | 6, _) => Some(5),
        _ => None,
    };
    cmaps
        .iter()
        .enumerate()
        .filter_map(|(i, t)| rank(t).map(|r| (r, i)))
        .min()
        .map(|(_, i)| i)
}

fn units_from_matrix(m: &[f64; 6]) -> u16 {
    let scale = m[0].hypot(m[1]);
    if scale > 0.0 && scale.is_finite() {
        let upem = (1.0 / scale).round();
        if (16.0..=16384.0).contains(&upem) {
            return upem as u16;
        }
    }
    1000
}

fn weight_from_name(weight: Option<&str>) -> u16 {
    let w = weight.unwrap_or("").to_ascii_lowercase();
    if w.contains("black") || w.contains("heavy") {
        900
    } else if w.contains("extrabold") || w.contains("ultrabold") {
        800
    } else if w.contains("semibold") || w.contains("demi") {
        600
    } else if w.contains("bold") {
        700
    } else if w.contains("medium") {
        500
    } else if w.contains("light") {
        300
    } else if w.contains("thin") {
        100
    } else {
        400
    }
}

fn ps_metrics(
    bbox: [f64; 4],
    italic_angle: f64,
    fixed: bool,
    weight: u16,
    font: &Font,
) -> FontMetrics {
    let is_italic = italic_angle != 0.0;
    let symbolic = font.glyph_by_name("A").is_none() && font.glyph_by_name("a").is_none();
    let mut flags = if symbolic { 4 } else { 32 };
    if fixed {
        flags |= 1;
    }
    if is_italic {
        flags |= 64;
    }
    FontMetrics {
        ascent: bbox[3] as f32,
        descent: bbox[1] as f32,
        line_gap: 0.0,
        cap_height: None,
        x_height: None,
        italic_angle: italic_angle as f32,
        bbox: Rect {
            x_min: bbox[0] as f32,
            y_min: bbox[1] as f32,
            x_max: bbox[2] as f32,
            y_max: bbox[3] as f32,
        },
        is_fixed_pitch: fixed,
        is_bold: weight >= 600,
        is_italic,
        weight,
        flags,
    }
}
