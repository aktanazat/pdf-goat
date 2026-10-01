//! PDF font dictionaries, encodings, metrics and glyph programs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use pdf_core::{Dict, Document, Matrix, ObjRef, Object, Point, Rect, Stream};
use pdf_font::{
    BaseEncoding, CMap, CharCode, CidCollection, CidToGid, Font, FontKind, FontLocator,
    FontRequest, PathOp, Script, Standard14, ToUnicodeMap,
};

use crate::InterpError;
use crate::device::GlyphText;
use crate::font_data::{AGL_DUPLICATES, GLYPH_NAMES_EXTRA, TRUETYPE_UCS2};
use crate::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontSubtype {
    Type1,
    MMType1,
    TrueType,
    Type3,
    Type0,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontFlags {
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub mono: bool,
}

#[derive(Debug)]
enum Program {
    Face(Arc<Font>),
    Base14(Standard14, Arc<Font>),
    Type3,
}

#[derive(Debug)]
enum Charmap {
    None,
    Builtin(Box<[u32; 256]>),
    Names,
    Sfnt(u16, u16),
}

/// A glyph program's outline in its own units and the matrix to one-em
/// glyph space.
#[derive(Clone, Debug)]
pub struct GlyphOutline {
    pub ops: Vec<PathOp>,
    pub to_em: Matrix,
    /// Shared original program and its actual glyph id, for hinted outlines
    /// and font-specific stroke rendering.
    pub font: Arc<Font>,
    pub gid: u16,
}

#[derive(Clone, Debug)]
enum UnicodeMap {
    Stream(Arc<ToUnicodeMap>),
    Collection(CidCollection),
    TrueType,
}

impl UnicodeMap {
    fn lookup(&self, code: u32, cid: u32) -> Option<GlyphText> {
        match self {
            Self::Stream(map) => {
                let text = map.lookup(code)?;
                let mut chars = ['\0'; 8];
                let mut len = 0;
                for (slot, c) in chars.iter_mut().zip(text.chars()) {
                    *slot = c;
                    len += 1;
                }
                (len != 0).then(|| GlyphText::new(&chars[..len]))
            }
            Self::Collection(c) => c.cid_to_unicode(cid, false).map(|c| GlyphText::new(&[c])),
            Self::TrueType => TRUETYPE_UCS2
                .get(cid as usize)
                .and_then(|v| char::from_u32(u32::from(*v)))
                .map(|c| GlyphText::new(&[c])),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HMetric {
    lo: u32,
    hi: u32,
    w: i32,
}
#[derive(Clone, Copy, Debug)]
struct VMetric {
    lo: u32,
    hi: u32,
    x: i32,
    y: i32,
    w: i32,
}

#[derive(Debug)]
pub(crate) struct CharProc {
    pub(crate) object: ObjRef,
    pub(crate) stream: Stream,
}
#[derive(Debug)]
pub(crate) struct Type3 {
    pub(crate) resources: Dict,
    pub(crate) procs: Vec<Option<CharProc>>,
    widths: [f64; 256],
}

/// A font dictionary and its loaded glyph program. Shared by all its text runs.
#[derive(Debug)]
pub struct PdfFont {
    id: u64,
    object: Option<ObjRef>,
    name: String,
    base_font: String,
    subtype: FontSubtype,
    embedded: bool,
    flags: FontFlags,
    descriptor_flags: i64,
    ascender: f64,
    descender: f64,
    bbox: Rect,
    wmode: u8,
    type3_matrix: Option<Matrix>,
    program: Program,
    charmap: Charmap,
    encoding: Option<CMap>,
    cid_to_gid: Vec<u32>,
    cid_to_ucs: Vec<char>,
    to_unicode: Option<UnicodeMap>,
    to_ttf: Option<UnicodeMap>,
    hmtx: Vec<HMetric>,
    vmtx: Vec<VMetric>,
    default_width: i32,
    default_vertical: (i32, i32),
    substitute: bool,
    stretch: bool,
    widths: Vec<i16>,
    glyph_bounds: Mutex<HashMap<u32, Rect>>,
    pub(crate) type3: Option<Type3>,
}

impl PdfFont {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn object(&self) -> Option<ObjRef> {
        self.object
    }
    /// The font name, including its subset tag.
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn base_font(&self) -> &str {
        &self.base_font
    }
    pub fn subtype(&self) -> FontSubtype {
        self.subtype
    }
    pub fn is_embedded(&self) -> bool {
        self.embedded
    }
    pub fn flags(&self) -> FontFlags {
        self.flags
    }
    pub fn descriptor_flags(&self) -> i64 {
        self.descriptor_flags
    }
    pub fn ascender(&self) -> f64 {
        self.ascender
    }
    pub fn descender(&self) -> f64 {
        self.descender
    }
    pub fn bbox(&self) -> Rect {
        self.bbox
    }
    pub fn wmode(&self) -> u8 {
        self.wmode
    }
    pub fn type3_matrix(&self) -> Option<Matrix> {
        self.type3_matrix
    }

    /// The glyph program's outline in font units, with the matrix that
    /// maps it to one-em glyph space (the font matrix and the substitute
    /// width adjustment). TrueType quadratics stay quadratics, so a
    /// rasterizer can walk them the way FreeType does.
    pub fn glyph_outline(&self, gid: u32) -> Option<GlyphOutline> {
        let (face, real_gid, to_em) = self.outline_mapping(gid)?;
        let outline = face.outline(real_gid).ok()?;
        if outline.is_empty() {
            return None;
        }
        Some(GlyphOutline {
            ops: outline.ops().to_vec(),
            to_em,
            font: Arc::clone(face),
            gid: real_gid,
        })
    }

    /// Outline in one-em glyph space, including substitute-font width
    /// adjustment, as a cubic path.
    pub fn glyph_path(&self, gid: u32) -> Option<Path> {
        let GlyphOutline { ops, to_em, .. } = self.glyph_outline(gid)?;
        let mut path = Path::new();
        let p = |x: f32, y: f32| Point::new(f64::from(x), f64::from(y)).transform(&to_em);
        for op in ops {
            match op {
                PathOp::MoveTo(x, y) => path.move_to(p(x, y)),
                PathOp::LineTo(x, y) => path.line_to(p(x, y)),
                PathOp::QuadTo(cx, cy, x, y) => {
                    let start = path.current_point().unwrap_or(Point::new(0.0, 0.0));
                    let control = p(cx, cy);
                    let end = p(x, y);
                    path.curve_to(
                        Point::new(
                            start.x + (control.x - start.x) * 2.0 / 3.0,
                            start.y + (control.y - start.y) * 2.0 / 3.0,
                        ),
                        Point::new(
                            end.x + (control.x - end.x) * 2.0 / 3.0,
                            end.y + (control.y - end.y) * 2.0 / 3.0,
                        ),
                        end,
                    );
                }
                PathOp::CurveTo(x1, y1, x2, y2, x, y) => {
                    path.curve_to(p(x1, y1), p(x2, y2), p(x, y))
                }
                PathOp::Close => path.close(),
            }
        }
        Some(path)
    }

    /// Glyph bounds in one-em space, with the same substitution transform
    /// as `glyph_path`. Empty glyphs have a zero rectangle; an unavailable
    /// outline uses the font box. TrueType boxes may conservatively include
    /// off-curve control points. Results are cached by PDF glyph id.
    pub fn glyph_bounds(&self, gid: u32) -> Option<Rect> {
        let mut cache = self
            .glyph_bounds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(rect) = cache.get(&gid) {
            return Some(*rect);
        }
        let Some((face, real_gid, matrix)) = self.outline_mapping(gid) else {
            return Some(self.bbox);
        };
        let rect = match face.glyph_bounds(real_gid) {
            Ok(Some(r)) => Rect::new(
                f64::from(r.x_min),
                f64::from(r.y_min),
                f64::from(r.x_max),
                f64::from(r.y_max),
            )
            .transform(&matrix),
            Ok(None) => Rect::new(0.0, 0.0, 0.0, 0.0),
            Err(_) => self.bbox,
        };
        cache.insert(gid, rect);
        Some(rect)
    }

    fn outline_mapping(&self, gid: u32) -> Option<(&Arc<Font>, u16, Matrix)> {
        let (face, real_gid, target) = match &self.program {
            Program::Face(face) => (
                face,
                face_gid(face, gid)?,
                self.stretch.then(|| self.stretched_width(gid)),
            ),
            Program::Base14(base, face) => {
                let name = base_name(*base, gid)?;
                let real_gid = face.glyph_by_name(name).or_else(|| {
                    let c = if *base == Standard14::ZapfDingbats {
                        pdf_font::dingbats_name_to_unicode(name)
                    } else {
                        Some(unicode_name(name))
                    };
                    c.and_then(|c| face.glyph_for_char(c))
                })?;
                let target = if self.stretch {
                    self.stretched_width(gid)
                } else {
                    base_width(*base, gid) / 1000.0
                };
                (face, real_gid, Some(target))
            }
            _ => return None,
        };
        let [a, b, c, d, e, f] = face.font_matrix();
        let mut matrix = Matrix::new(a, b, c, d, e, f);
        if let Some(target) = target {
            let real = f64::from(face.advance(real_gid).unwrap_or(0.0))
                / f64::from(face.units_per_em().max(1));
            if real > 0.0 && target > 0.0 {
                matrix = matrix.concat(&Matrix::scale(target / real, 1.0));
            }
        }
        Some((face, real_gid, matrix))
    }

    /// Glyph selected through the program's Unicode cmap (Type 3 uses the scalar).
    pub fn encode_character(&self, unicode: u32) -> u32 {
        match &self.program {
            Program::Face(face) => char::from_u32(unicode)
                .and_then(|c| face.glyph_for_char(c))
                .map_or(0, u32::from),
            Program::Base14(base, _) => base_unicode(*base, unicode),
            Program::Type3 => unicode,
        }
    }

    /// The glyph program's advance, in em; independent of text character spacing.
    pub fn advance_glyph(&self, gid: u32, wmode: u8) -> f64 {
        if let Some(t3) = &self.type3 {
            return if wmode != 0 {
                1.0
            } else {
                t3.widths.get(gid as usize).copied().unwrap_or(0.0)
            };
        }
        if self.stretch && !self.widths.is_empty() {
            return self.stretched_width(gid);
        }
        match &self.program {
            Program::Base14(base, _) => {
                if wmode == 0 {
                    base_width(*base, gid) / 1000.0
                } else {
                    self.ascender - self.descender
                }
            }
            Program::Face(face) => {
                let Some(gid) = face_gid(face, gid) else {
                    return f64::from(self.default_width) / 1000.0;
                };
                if wmode != 0 {
                    return vertical_advance(face, gid);
                }
                f64::from(face.advance(gid).unwrap_or(0.0)) / f64::from(face.units_per_em().max(1))
            }
            Program::Type3 => 0.0,
        }
    }

    fn stretched_width(&self, gid: u32) -> f64 {
        f64::from(
            self.widths
                .get(gid as usize)
                .copied()
                .unwrap_or(self.default_width as i16),
        ) / 1000.0
    }

    pub(crate) fn decode(&self, bytes: &[u8]) -> Option<CharCode> {
        let &first = bytes.first()?;
        let Some(cmap) = &self.encoding else {
            return Some(CharCode {
                code: u32::from(first),
                len: 1,
            });
        };
        let mut code = 0u32;
        for (i, &byte) in bytes.iter().take(4).enumerate() {
            code = (code << 8) | u32::from(byte);
            let len = (i + 1) as u8;
            if cmap.codespaces().iter().any(|s| {
                s.len == len
                    && code >= be_number(&s.low[..usize::from(len)])
                    && code <= be_number(&s.high[..usize::from(len)])
            }) {
                return Some(CharCode { code, len });
            }
        }
        Some(CharCode { code: 0, len: 1 })
    }

    pub(crate) fn cid(&self, code: CharCode) -> Option<u32> {
        match &self.encoding {
            None => Some(code.code),
            Some(cmap) => cmap.lookup(code).or_else(|| {
                (1..=4).find_map(|len| {
                    cmap.lookup(CharCode {
                        code: code.code,
                        len,
                    })
                })
            }),
        }
    }

    pub(crate) fn gid(&self, cid: u32) -> u32 {
        if let Some(map) = &self.to_ttf {
            let Some(mut u) = map.lookup(cid, cid).map(|s| u32::from(s.first())) else {
                return 0;
            };
            if self.substitute && self.wmode != 0 {
                u = vertical_form(u);
            }
            return self.char_index(u);
        }
        self.cid_to_gid.get(cid as usize).copied().unwrap_or(cid)
    }

    pub(crate) fn unicode(&self, code: u32, cid: u32) -> GlyphText {
        if let Some(text) = self.to_unicode.as_ref().and_then(|m| m.lookup(code, cid)) {
            if text.as_slice().len() != 1 {
                return text;
            }
            let u = u32::from(text.first());
            if (8..=13).contains(&u) {
                return GlyphText::new(&[' ']);
            }
            if u >= 32 && !(127..160).contains(&u) {
                return text;
            }
        }
        self.cid_to_ucs
            .get(cid as usize)
            .filter(|c| **c != '\0')
            .map_or(GlyphText::REPLACEMENT, |c| GlyphText::new(&[*c]))
    }

    pub(crate) fn horizontal(&self, cid: u32) -> f32 {
        metric(&self.hmtx, cid).map_or(self.default_width, |m| m.w) as f32 * 0.001
    }
    pub(crate) fn vertical(&self, cid: u32) -> (i32, i32, i32) {
        let found = self.vmtx.iter().find(|m| cid >= m.lo && cid <= m.hi);
        found.map_or_else(
            || {
                (
                    metric(&self.hmtx, cid).map_or(self.default_width, |m| m.w) / 2,
                    self.default_vertical.0,
                    self.default_vertical.1,
                )
            },
            |m| (m.x, m.y, m.w),
        )
    }

    fn char_index(&self, code: u32) -> u32 {
        let lookup = |code| match &self.charmap {
            Charmap::None => 0,
            Charmap::Builtin(table) => table.get(code as usize).copied().unwrap_or(0),
            Charmap::Names => self.encode_character(code),
            Charmap::Sfnt(p, e) => match &self.program {
                Program::Face(face) => face.cmap_lookup(*p, *e, code).map_or(0, u32::from),
                _ => 0,
            },
        };
        let gid = lookup(code);
        if gid != 0 {
            return gid;
        }
        let gid = lookup(code.saturating_add(0xf000));
        if gid == 0 && code == 0x22ef {
            lookup(0x2026)
        } else {
            gid
        }
    }

    fn name_index(&self, name: &str) -> u32 {
        let lookup = |name: &str| match &self.program {
            Program::Face(face) => face.glyph_by_name(name).map_or(0, u32::from),
            Program::Base14(base, _) => base
                .metrics()
                .widths
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
                .map_or(0, |i| i as u32 + 1),
            Program::Type3 => 0,
        };
        let gid = lookup(name);
        if gid != 0 {
            return gid;
        }
        let u = u32::from(unicode_name(name));
        if let Ok(i) = AGL_DUPLICATES.binary_search_by_key(&u, |(u, _)| *u) {
            for alternate in AGL_DUPLICATES[i].1 {
                let gid = lookup(alternate);
                if gid != 0 {
                    return gid;
                }
            }
        }
        lookup(&format!("uni{u:04X}"))
    }

    fn make_widths(&mut self) -> Result<(), InterpError> {
        if !self.stretch {
            return Ok(());
        }
        let mut count = 0u64;
        let mut widths = Vec::<i16>::new();
        for range in &self.hmtx {
            count += u64::from(range.hi.saturating_sub(range.lo)) + 1;
            if count > 4_000_000 {
                return Err(InterpError::Limit(
                    "font width table exceeds work bound".into(),
                ));
            }
            for c in range.lo..=range.hi {
                let Some(cid) = self.cid(CharCode { code: c, len: 2 }) else {
                    continue;
                };
                let gid = self.gid(cid) as usize;
                if gid > 0x10ffff {
                    continue;
                }
                if widths.len() <= gid {
                    widths.resize(gid + 1, -1);
                }
                widths[gid] = i32::from(widths[gid]).max(range.w) as i16;
            }
        }
        for w in &mut widths {
            if *w == -1 {
                *w = self.default_width as i16;
            }
        }
        self.widths = widths;
        Ok(())
    }
}

pub(crate) fn load(
    doc: &Document,
    object: &Object,
    resources: &Dict,
    id: u64,
) -> Result<PdfFont, InterpError> {
    let dict = doc.resolve_dict(object)?.unwrap_or_default();
    let subtype = match dict.get_name(b"Subtype") {
        Some(b"Type0") => FontSubtype::Type0,
        Some(b"TrueType") => FontSubtype::TrueType,
        Some(b"MMType1") => FontSubtype::MMType1,
        Some(b"Type3") => FontSubtype::Type3,
        _ if dict.contains_key(b"CharProcs") => FontSubtype::Type3,
        _ if dict.contains_key(b"DescendantFonts") => FontSubtype::Type0,
        _ => FontSubtype::Type1,
    };
    let mut font = empty_font(id, object.as_reference(), name(&dict, b"BaseFont"), subtype);
    match subtype {
        FontSubtype::Type3 => load_type3(doc, &dict, resources, &mut font)?,
        FontSubtype::Type0 => load_type0(doc, &dict, &mut font)?,
        _ => load_simple(doc, &dict, &mut font)?,
    }
    font.hmtx.sort_by_key(|m| m.lo);
    font.vmtx.sort_by_key(|m| m.lo);
    font.make_widths()?;
    Ok(font)
}

pub(crate) fn fallback(id: u64) -> PdfFont {
    let mut font = empty_font(id, None, String::new(), FontSubtype::Type1);
    builtin(&mut font, Standard14::TimesRoman);
    font.charmap = select_cmap(&font.program, false, false);
    let mut names = encoding_names(Some(BaseEncoding::Standard));
    finish_simple(&mut font, &mut names);
    for code in 0..256 {
        let gid = font.gid(code);
        font.hmtx.push(HMetric {
            lo: code,
            hi: code,
            w: (font.advance_glyph(gid, 0) * 1000.0) as i32,
        });
    }
    font
}

fn empty_font(id: u64, object: Option<ObjRef>, base_font: String, subtype: FontSubtype) -> PdfFont {
    PdfFont {
        id,
        object,
        name: truncate(&base_font, 31),
        base_font,
        subtype,
        embedded: false,
        flags: FontFlags::default(),
        descriptor_flags: 0,
        ascender: 0.8,
        descender: -0.2,
        bbox: Rect::new(0.0, 0.0, 1.0, 1.0),
        wmode: 0,
        type3_matrix: None,
        program: Program::Type3,
        charmap: Charmap::None,
        encoding: None,
        cid_to_gid: Vec::new(),
        cid_to_ucs: Vec::new(),
        to_unicode: None,
        to_ttf: None,
        hmtx: Vec::new(),
        vmtx: Vec::new(),
        default_width: 0,
        default_vertical: (880, -1000),
        substitute: false,
        stretch: false,
        widths: Vec::new(),
        glyph_bounds: Mutex::new(HashMap::new()),
        type3: None,
    }
}

fn load_simple(doc: &Document, dict: &Dict, font: &mut PdfFont) -> Result<(), InterpError> {
    if let Some(descriptor) = dict.get(b"FontDescriptor") {
        let d = doc.resolve_dict(descriptor)?.unwrap_or_default();
        descriptor_load(
            doc,
            &d,
            None,
            font,
            if font.subtype == FontSubtype::TrueType {
                b"FontFile2"
            } else {
                b"FontFile"
            },
        )?;
    } else {
        builtin(
            font,
            clean_name(&font.base_font).unwrap_or(Standard14::TimesRoman),
        );
    }
    let symbolic = font.descriptor_flags & 4 != 0 && font.descriptor_flags & 32 == 0;
    font.charmap = select_cmap(&font.program, symbolic, false);
    let mut names = load_encoding(
        doc,
        dict.get(b"Encoding"),
        (!font.embedded && !symbolic).then_some(BaseEncoding::Standard),
    )?;
    finish_simple(font, &mut names);
    font.to_unicode = load_unicode(doc, dict.get(b"ToUnicode"));
    let widths = doc.resolve_key(dict, b"Widths")?;
    if let Some(widths) = widths.as_array() {
        let first = integer(&doc.resolve_key(dict, b"FirstChar")?).clamp(0, 255) as u32;
        let last = integer(&doc.resolve_key(dict, b"LastChar")?).clamp(0, 255) as u32;
        for code in first..=last {
            font.hmtx.push(HMetric {
                lo: code,
                hi: code,
                w: widths.get((code - first) as usize).map_or(0, integer),
            });
        }
    } else {
        for code in 0..256 {
            let gid = font.gid(code);
            font.hmtx.push(HMetric {
                lo: code,
                hi: code,
                w: (font.advance_glyph(gid, 0) * 1000.0) as i32,
            });
        }
    }
    Ok(())
}

fn finish_simple(font: &mut PdfFont, names: &mut [Option<String>]) {
    let symbolic = font.descriptor_flags & 4 != 0 && font.descriptor_flags & 32 == 0;
    let truetype = font.subtype == FontSubtype::TrueType;
    let mut table = Vec::with_capacity(256);
    for (code, estr) in names.iter_mut().enumerate() {
        let mut gid = font.char_index(code as u32);
        if let Some(name) = estr.as_deref() {
            let named = if !truetype {
                font.name_index(name)
            } else if !symbolic && matches!(font.charmap, Charmap::Sfnt(3, _)) {
                let strict = strict_unicode(name).map_or(0, |u| font.char_index(u));
                if strict != 0 {
                    strict
                } else {
                    let n = font.name_index(name);
                    if n != 0 {
                        n
                    } else {
                        font.char_index(u32::from(unicode_name(name)))
                    }
                }
            } else if !symbolic && matches!(font.charmap, Charmap::Sfnt(1, _)) {
                mac_code(name)
                    .filter(|c| *c != 0)
                    .map_or_else(|| font.name_index(name), |c| font.char_index(u32::from(c)))
            } else if !matches!(font.charmap, Charmap::Sfnt(3, 0)) {
                font.name_index(name)
            } else {
                0
            };
            if named != 0 {
                gid = named;
            }
        }
        if gid != 0 && estr.is_none() {
            *estr = match &font.program {
                Program::Face(face)
                    if face.kind() != FontKind::TrueType || face.glyph_name(0).is_some() =>
                {
                    face.glyph_name(gid as u16)
                        .map(|n| truncate(&n, 31))
                        .filter(|n| !n.is_empty())
                }
                Program::Base14(base, _) => base_name(*base, gid).map(str::to_owned),
                _ => encoding_name(BaseEncoding::WinAnsi, code as u8).map(str::to_owned),
            };
        }
        table.push(gid);
    }
    font.cid_to_gid = table;
    font.cid_to_ucs = names
        .iter()
        .map(|n| n.as_deref().map_or('\u{fffd}', unicode_name))
        .collect();
}

fn load_type0(doc: &Document, dict: &Dict, font: &mut PdfFont) -> Result<(), InterpError> {
    let descendants = doc.resolve_key(dict, b"DescendantFonts")?;
    let first = descendants
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| InterpError::Limit("cid font is missing descendant fonts".into()))?;
    let child = doc
        .resolve_dict(first)?
        .ok_or_else(|| InterpError::Limit("invalid descendant font".into()))?;
    let fftype: &[u8] = match child.get_name(b"Subtype") {
        Some(b"CIDFontType0") => b"FontFile3",
        Some(b"CIDFontType2") => b"FontFile2",
        _ => return Err(InterpError::Limit("unknown cid font type".into())),
    };
    let encoding = dict
        .get(b"Encoding")
        .ok_or_else(|| InterpError::Limit("font missing encoding".into()))?;
    let cmap = load_cmap(doc, encoding, 0)?;
    font.wmode = u8::from(cmap.is_vertical());
    let identity = cmap.name().is_some_and(|n| n.contains("Identity-"));
    font.encoding = Some(cmap);
    let info = doc.resolve_key(&child, b"CIDSystemInfo")?;
    let collection = info
        .as_dict()
        .map(|d| {
            let text = |key: &[u8]| {
                d.get_string(key)
                    .map(|s| String::from_utf8_lossy(&s.bytes).into_owned())
                    .unwrap_or_default()
            };
            format!("{}-{}", text(b"Registry"), text(b"Ordering"))
        })
        .unwrap_or_else(|| "Adobe-Identity".into());
    let desc = child
        .get(b"FontDescriptor")
        .ok_or_else(|| InterpError::Limit("missing font descriptor".into()))?;
    let desc = doc.resolve_dict(desc)?.unwrap_or_default();
    let child_name = name(&child, b"BaseFont");
    if !child_name.is_empty() {
        font.name = truncate(&child_name, 31);
    }
    descriptor_load(doc, &desc, Some(&collection), font, fftype)?;
    font.charmap = select_cmap(&font.program, false, font.substitute);
    if let Some(stream) = child
        .get(b"CIDToGIDMap")
        .map(|o| doc.resolve_stream(o))
        .transpose()?
        .flatten()
    {
        let bytes = doc.decode_stream(&stream)?.data;
        font.cid_to_gid = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u32::from(u16::from_be_bytes(*b)))
            .collect();
    } else if font.substitute {
        font.to_ttf = collection_map(&collection);
    }
    font.to_unicode =
        load_unicode(doc, dict.get(b"ToUnicode")).or_else(|| collection_map(&collection));
    if identity && font.substitute {
        if font.to_ttf.is_none() {
            font.to_ttf = Some(font.to_unicode.clone().unwrap_or(UnicodeMap::TrueType));
        }
        if font.to_unicode.is_none() {
            font.to_unicode = font.to_ttf.clone();
        }
    }
    font.default_width = child.get(b"DW").map_or(1000, integer);
    let widths = doc.resolve_key(&child, b"W")?;
    if let Some(a) = widths.as_array() {
        parse_metrics(doc, a, false, font)?;
    } else {
        font.stretch = false;
    }
    if font.wmode != 0 {
        if let Some(a) = doc.resolve_key(&child, b"DW2")?.as_array() {
            font.default_vertical = (a.first().map_or(0, integer), a.get(1).map_or(0, integer));
        }
        if let Some(a) = doc.resolve_key(&child, b"W2")?.as_array() {
            parse_metrics(doc, a, true, font)?;
        }
    }
    Ok(())
}

fn parse_metrics(
    doc: &Document,
    a: &[Object],
    vertical: bool,
    font: &mut PdfFont,
) -> Result<(), InterpError> {
    let mut i = 0;
    while i + 1 < a.len() {
        let first = integer(&doc.resolve(&a[i])?) as u16 as u32;
        let second = doc.resolve(&a[i + 1])?;
        if let Some(values) = second.as_array() {
            let step = if vertical { 3 } else { 1 };
            for (k, v) in values.chunks(step).enumerate() {
                let code = first.saturating_add(k as u32);
                if code > 65535 {
                    break;
                }
                if vertical {
                    font.vmtx.push(VMetric {
                        lo: code,
                        hi: code,
                        w: v.first().map_or(0, integer) as i16 as i32,
                        x: v.get(1).map_or(0, integer) as i16 as i32,
                        y: v.get(2).map_or(0, integer) as i16 as i32,
                    });
                } else {
                    font.hmtx.push(HMetric {
                        lo: code,
                        hi: code,
                        w: integer(&v[0]),
                    });
                }
            }
            i += 2;
        } else {
            let last = integer(&second) as u16 as u32;
            let w = a.get(i + 2).map_or(0, integer);
            if vertical {
                font.vmtx.push(VMetric {
                    lo: first,
                    hi: last,
                    w: w as i16 as i32,
                    x: a.get(i + 3).map_or(0, integer) as i16 as i32,
                    y: a.get(i + 4).map_or(0, integer) as i16 as i32,
                });
            } else {
                font.hmtx.push(HMetric {
                    lo: first,
                    hi: last,
                    w,
                });
            }
            i += if vertical { 5 } else { 3 };
        }
    }
    Ok(())
}

fn load_type3(
    doc: &Document,
    dict: &Dict,
    resources: &Dict,
    font: &mut PdfFont,
) -> Result<(), InterpError> {
    let matrix = doc
        .resolve_key(dict, b"FontMatrix")?
        .as_array()
        .and_then(Matrix::from_array)
        .unwrap_or(Matrix::IDENTITY);
    font.type3_matrix = Some(matrix);
    font.name = dict
        .get_name(b"Name")
        .map(|n| truncate(&String::from_utf8_lossy(n), 31))
        .unwrap_or_else(|| format!("Type3 ({} 0 R)", font.object.map_or(0, |r| r.num)));
    font.bbox = doc
        .resolve_key(dict, b"FontBBox")?
        .as_array()
        .and_then(Rect::from_array)
        .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0))
        .transform(&matrix);
    font.ascender = font.bbox.y1;
    font.descender = font.bbox.y0;
    let enc = dict
        .get(b"Encoding")
        .ok_or_else(|| InterpError::Limit("Type3 font missing Encoding".into()))?;
    let names = load_encoding(doc, Some(enc), None)?;
    font.cid_to_ucs = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let c = n.as_deref().map_or('\u{fffd}', unicode_name);
            if c == '\u{fffd}' && (32..=126).contains(&i) {
                char::from(i as u8)
            } else {
                c
            }
        })
        .collect();
    font.to_unicode = load_unicode(doc, dict.get(b"ToUnicode"));
    let widths = doc.resolve_key(dict, b"Widths")?;
    let widths = widths
        .as_array()
        .ok_or_else(|| InterpError::Limit("Type3 font missing Widths".into()))?;
    let first = dict.get(b"FirstChar").map_or(0, integer).clamp(0, 255) as usize;
    let last = dict.get(b"LastChar").map_or(0, integer).clamp(0, 255) as usize;
    let mut advances = [0.0; 256];
    for (i, advance) in advances.iter_mut().enumerate().take(last + 1).skip(first) {
        let w = widths
            .get(i - first)
            .and_then(Object::as_f64)
            .unwrap_or(0.0)
            * matrix.a
            * 1000.0;
        *advance = w * 0.001;
        font.hmtx.push(HMetric {
            lo: i as u32,
            hi: i as u32,
            w: w as i32,
        });
    }
    let procs = doc.resolve_key(dict, b"CharProcs")?;
    let procs = procs
        .as_dict()
        .ok_or_else(|| InterpError::Limit("Type3 font missing CharProcs".into()))?;
    let procs = names
        .iter()
        .map(|name| {
            name.as_ref()
                .and_then(|n| procs.get(n.as_bytes()))
                .and_then(|o| {
                    doc.resolve_stream(o).ok().flatten().map(|stream| CharProc {
                        object: o.as_reference().unwrap_or(ObjRef {
                            num: 0,
                            generation: 0,
                        }),
                        stream,
                    })
                })
        })
        .collect();
    let resources = dict
        .get(b"Resources")
        .map(|o| doc.resolve_dict(o))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| resources.clone());
    font.type3 = Some(Type3 {
        resources,
        procs,
        widths: advances,
    });
    Ok(())
}

fn descriptor_load(
    doc: &Document,
    d: &Dict,
    collection: Option<&str>,
    font: &mut PdfFont,
    preferred: &[u8],
) -> Result<(), InterpError> {
    font.descriptor_flags = d.get(b"Flags").map_or(0, |o| i64::from(integer(o)));
    font.default_width = d.get(b"MissingWidth").map_or(0, integer);
    let file = [preferred, b"FontFile", b"FontFile2", b"FontFile3"]
        .into_iter()
        .find_map(|k| d.get(k));
    let face = file
        .filter(|o| o.as_reference().is_some())
        .and_then(|o| doc.resolve_stream(o).ok().flatten())
        .and_then(|s| doc.decode_stream(&s).ok())
        .and_then(|s| extract_cff(s.data))
        .and_then(|data| Font::parse(data).ok());
    if let Some(face) = face {
        set_face(font, Arc::new(face));
        font.embedded = true;
    } else if collection.is_none()
        && let Some(base) = clean_name(&font.base_font)
    {
        builtin(font, base);
    } else {
        substitute(font, collection)?;
    }
    let mut ascent = if let Program::Face(face) = &font.program {
        if face.kind() == FontKind::TrueType {
            f64::from(face.metrics().ascent) * 1000.0 / f64::from(face.units_per_em().max(1))
        } else {
            800.0
        }
    } else {
        font.ascender * 1000.0
    };
    let mut descent = if let Program::Face(face) = &font.program {
        if face.kind() == FontKind::TrueType {
            f64::from(face.metrics().descent) * 1000.0 / f64::from(face.units_per_em().max(1))
        } else {
            -200.0
        }
    } else {
        font.descender * 1000.0
    };
    if let Some(a) = d.get_f64(b"Ascent") {
        ascent = a;
    }
    if let Some(a) = d.get_f64(b"Descent") {
        descent = -a.abs();
    }
    if ascent <= 0.0 || ascent > 8000.0 || descent < -2000.0 {
        font.ascender = 0.8;
        font.descender = -0.2;
    } else {
        font.ascender = ascent / 1000.0;
        font.descender = descent / 1000.0;
    }
    Ok(())
}

fn set_face(font: &mut PdfFont, face: Arc<Font>) {
    let m = face.metrics();
    let upem = f64::from(face.units_per_em().max(1));
    font.flags = FontFlags {
        mono: m.is_fixed_pitch,
        serif: sfnt_table(&face, b"OS/2")
            .and_then(|t| be_u16(t, 30))
            .is_none_or(|class| class & 2048 == 0),
        bold: m.is_bold || font.name.contains("Bold") || font.name.contains("Semibold"),
        italic: m.is_italic || font.name.contains("Italic") || font.name.contains("Oblique"),
    };
    font.ascender = f64::from(m.ascent) / upem;
    font.descender = -f64::from(m.descent).abs() / upem;
    if !(0.0..=8.0).contains(&font.ascender) || font.ascender == 0.0 {
        font.ascender = 0.8;
    }
    if font.descender < -2.0 {
        font.descender = -0.2;
    }
    font.bbox = Rect::new(
        f64::from(m.bbox.x_min) / upem,
        f64::from(m.bbox.y_min) / upem,
        f64::from(m.bbox.x_max) / upem,
        f64::from(m.bbox.y_max) / upem,
    );
    if font.bbox.is_empty() {
        font.bbox = Rect::new(0.0, 0.0, 1.0, 1.0);
    }
    font.program = Program::Face(face);
}

fn builtin(font: &mut PdfFont, base: Standard14) {
    let name = base.name();
    font.flags = FontFlags {
        mono: name.starts_with("Courier"),
        serif: name.starts_with("Times"),
        bold: name.contains("Bold"),
        italic: name.contains("Italic") || name.contains("Oblique"),
    };
    if matches!(base, Standard14::Symbol | Standard14::ZapfDingbats) {
        font.descriptor_flags |= 4;
    }
    let (a, d, b) = match name {
        "Courier" => (0.932, -0.317, [-0.031, -0.317, 0.683, 0.932]),
        "Courier-Bold" => (1.007, -0.393, [-0.082, -0.393, 0.698, 1.007]),
        "Courier-Oblique" => (0.920, -0.317, [-0.062, -0.317, 0.792, 0.920]),
        "Courier-BoldOblique" => (0.997, -0.393, [-0.093, -0.393, 0.844, 0.997]),
        "Helvetica" => (1.075, -0.299, [-0.210, -0.299, 1.032, 1.075]),
        "Helvetica-Bold" => (1.070, -0.307, [-0.188, -0.307, 1.069, 1.070]),
        "Helvetica-Oblique" => (1.070, -0.284, [-0.123, -0.284, 1.154, 1.070]),
        "Helvetica-BoldOblique" => (1.073, -0.309, [-0.122, -0.309, 1.196, 1.073]),
        "Times-Roman" => (1.053, -0.281, [-0.168, -0.281, 1.000, 1.053]),
        "Times-Bold" => (1.044, -0.341, [-0.168, -0.341, 1.079, 1.044]),
        "Times-Italic" => (0.951, -0.270, [-0.169, -0.270, 1.085, 0.951]),
        "Times-BoldItalic" => (0.972, -0.324, [-0.200, -0.324, 1.154, 0.972]),
        "Symbol" => (1.010, -0.293, [-0.180, -0.293, 1.090, 1.010]),
        _ => (0.819, -0.144, [-0.001, -0.144, 0.981, 0.819]),
    };
    font.ascender = a;
    font.descender = d;
    font.bbox = Rect::new(b[0], b[1], b[2], b[3]);
    // Unmodified URW CFF programs from MuPDF 1.27.2 resources/fonts/urw.
    // Copyright 2016 (URW)++ Design & Development, SIL OFL 1.1; see fonts/OFL.txt.
    // Bundling the same outlines makes Standard 14 rendering independent
    // of which substitute fonts happen to be installed on the host.
    static FACES: [OnceLock<Arc<Font>>; 14] = [const { OnceLock::new() }; 14];
    let shape = FACES[base as usize]
        .get_or_init(|| {
            let bytes: &[u8] = match base {
                Standard14::Courier => include_bytes!("fonts/NimbusMonoPS-Regular.cff"),
                Standard14::CourierBold => include_bytes!("fonts/NimbusMonoPS-Bold.cff"),
                Standard14::CourierBoldOblique => {
                    include_bytes!("fonts/NimbusMonoPS-BoldItalic.cff")
                }
                Standard14::CourierOblique => include_bytes!("fonts/NimbusMonoPS-Italic.cff"),
                Standard14::Helvetica => include_bytes!("fonts/NimbusSans-Regular.cff"),
                Standard14::HelveticaBold => include_bytes!("fonts/NimbusSans-Bold.cff"),
                Standard14::HelveticaBoldOblique => {
                    include_bytes!("fonts/NimbusSans-BoldItalic.cff")
                }
                Standard14::HelveticaOblique => include_bytes!("fonts/NimbusSans-Italic.cff"),
                Standard14::Symbol => include_bytes!("fonts/StandardSymbolsPS.cff"),
                Standard14::TimesBold => include_bytes!("fonts/NimbusRoman-Bold.cff"),
                Standard14::TimesBoldItalic => include_bytes!("fonts/NimbusRoman-BoldItalic.cff"),
                Standard14::TimesItalic => include_bytes!("fonts/NimbusRoman-Italic.cff"),
                Standard14::TimesRoman => include_bytes!("fonts/NimbusRoman-Regular.cff"),
                Standard14::ZapfDingbats => include_bytes!("fonts/Dingbats.cff"),
            };
            Arc::new(Font::parse(bytes.to_vec()).expect("bundled Standard 14 font"))
        })
        .clone();
    font.program = Program::Base14(base, shape);
}

fn substitute(font: &mut PdfFont, collection: Option<&str>) -> Result<(), InterpError> {
    let f = font.descriptor_flags;
    let flags = FontFlags {
        mono: f & 1 != 0,
        serif: f & 2 != 0,
        bold: f & 262144 != 0 || font.name.contains("Bold"),
        italic: f & 64 != 0 || font.name.contains("Italic") || font.name.contains("Oblique"),
    };
    let mut script = collection
        .and_then(|c| c.strip_prefix("Adobe-"))
        .map(Script::from_ordering)
        .unwrap_or(Script::Latin);
    if collection.is_some() && script == Script::Latin {
        for (needle, s) in [
            ("SimFang", Script::SimplifiedChinese),
            ("SimHei", Script::SimplifiedChinese),
            ("SimKai", Script::SimplifiedChinese),
            ("SimLi", Script::SimplifiedChinese),
            ("SimSun", Script::SimplifiedChinese),
            ("Song", Script::SimplifiedChinese),
            ("MingLiU", Script::TraditionalChinese),
            ("Gothic", Script::Japanese),
            ("Mincho", Script::Japanese),
            ("Batang", Script::Korean),
            ("Gulim", Script::Korean),
            ("Dotum", Script::Korean),
        ] {
            if font.name.contains(needle) {
                script = s;
                break;
            }
        }
    }
    if script != Script::Latin {
        let face = FontLocator::system()
            .find(&FontRequest {
                base_font: &font.name,
                flags: f as u32,
                weight: None,
                script,
            })
            .and_then(|s| s.load().ok())
            .ok_or_else(|| InterpError::Limit("no CJK substitute font found".into()))?;
        set_face(font, Arc::new(face));
    } else {
        let family = if flags.mono {
            "Courier"
        } else if flags.serif {
            "Times"
        } else {
            "Helvetica"
        };
        let suffix = match (flags.bold, flags.italic, flags.serif && !flags.mono) {
            (false, false, true) => "-Roman",
            (false, false, false) => "",
            (true, false, _) => "-Bold",
            (false, true, true) => "-Italic",
            (true, true, true) => "-BoldItalic",
            (false, true, false) => "-Oblique",
            (true, true, false) => "-BoldOblique",
        };
        let base = Standard14::ALL
            .into_iter()
            .find(|b| b.name() == format!("{family}{suffix}"))
            .unwrap_or(Standard14::Helvetica);
        builtin(font, base);
        font.flags = flags;
        font.stretch = true;
    }
    font.substitute = true;
    Ok(())
}

fn select_cmap(program: &Program, symbolic: bool, unicode: bool) -> Charmap {
    match program {
        Program::Type3 => Charmap::None,
        Program::Base14(base, _) => {
            if unicode {
                return Charmap::Names;
            }
            Charmap::Builtin(Box::new(std::array::from_fn(|i| {
                base.builtin_encoding()
                    .glyph_name(i as u8)
                    .and_then(|n| {
                        base.metrics()
                            .widths
                            .binary_search_by(|(k, _)| k.as_bytes().cmp(n.as_bytes()))
                            .ok()
                    })
                    .map_or(0, |i| i as u32 + 1)
            })))
        }
        Program::Face(face) => {
            let cmaps = face.cmap_subtables();
            if !cmaps.is_empty() {
                let selected = if unicode {
                    cmaps
                        .iter()
                        .rev()
                        .find(|(p, e, _)| (*p == 3 && *e == 10) || (*p == 0 && matches!(e, 4 | 6)))
                        .or_else(|| {
                            cmaps
                                .iter()
                                .rev()
                                .find(|(p, e, _)| *p == 0 || (*p == 3 && matches!(e, 1 | 10)))
                        })
                } else if face.kind() == FontKind::TrueType {
                    (symbolic
                        .then(|| cmaps.iter().find(|(p, e, _)| *p == 3 && *e == 0))
                        .flatten())
                    .or_else(|| cmaps.iter().find(|(p, e, _)| *p == 3 && *e == 1))
                    .or_else(|| cmaps.iter().find(|(p, e, _)| *p == 1 && *e == 0))
                    .or(cmaps.first())
                } else {
                    cmaps.first()
                };
                return selected.map_or(Charmap::None, |(p, e, _)| Charmap::Sfnt(*p, *e));
            }
            if !unicode && let Some(enc) = face.builtin_encoding() {
                return Charmap::Builtin(Box::new(std::array::from_fn(|i| {
                    enc.glyph_name(i as u8)
                        .and_then(|n| face.glyph_by_name(n))
                        .map_or(0, u32::from)
                })));
            }
            if face.is_cid_keyed() {
                Charmap::None
            } else {
                Charmap::Names
            }
        }
    }
}

fn load_encoding(
    doc: &Document,
    object: Option<&Object>,
    default: Option<BaseEncoding>,
) -> Result<Vec<Option<String>>, InterpError> {
    let object = object
        .map(|o| doc.resolve(o))
        .transpose()?
        .unwrap_or(Object::Null);
    let base = match &object {
        Object::Name(n) => pdf_encoding(n.as_bytes()),
        Object::Dict(d) => d.get_name(b"BaseEncoding").map_or(default, pdf_encoding),
        _ => default,
    };
    let mut names = encoding_names(base);
    if let Object::Dict(dict) = object
        && let Some(a) = doc.resolve_key(&dict, b"Differences")?.as_array()
    {
        let mut code = 0i32;
        for o in a {
            match o {
                Object::Integer(_) | Object::Real(_) => code = integer(o),
                Object::Name(n) => {
                    if (0..256).contains(&code) {
                        names[code as usize] =
                            Some(String::from_utf8_lossy(n.as_bytes()).into_owned());
                    }
                    code = code.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    Ok(names)
}
fn pdf_encoding(name: &[u8]) -> Option<BaseEncoding> {
    match name {
        b"StandardEncoding" => Some(BaseEncoding::Standard),
        b"MacRomanEncoding" => Some(BaseEncoding::MacRoman),
        b"MacExpertEncoding" => Some(BaseEncoding::MacExpert),
        b"WinAnsiEncoding" => Some(BaseEncoding::WinAnsi),
        _ => None,
    }
}
fn encoding_names(base: Option<BaseEncoding>) -> Vec<Option<String>> {
    (0..=255)
        .map(|i| base.and_then(|b| encoding_name(b, i)).map(str::to_owned))
        .collect()
}
fn encoding_name(base: BaseEncoding, code: u8) -> Option<&'static str> {
    if base == BaseEncoding::WinAnsi && matches!(code, 127 | 129 | 141 | 143 | 144 | 157) {
        Some("bullet")
    } else {
        base.glyph_name(code)
    }
}

fn load_unicode(doc: &Document, object: Option<&Object>) -> Option<UnicodeMap> {
    let stream = doc.resolve_stream(object?).ok()??;
    let bytes = doc.decode_stream(&stream).ok()?.data;
    ToUnicodeMap::parse(&bytes)
        .ok()
        .map(|m| UnicodeMap::Stream(Arc::new(m)))
}
fn collection_map(name: &str) -> Option<UnicodeMap> {
    CidCollection::from_ordering(name.strip_prefix("Adobe-")?).map(UnicodeMap::Collection)
}
fn load_cmap(doc: &Document, object: &Object, depth: usize) -> Result<CMap, InterpError> {
    if depth >= 32 {
        return Err(InterpError::Limit("recursive CMap".into()));
    }
    let object = doc.resolve(object)?;
    match object {
        Object::Name(n) => CMap::predefined(&String::from_utf8_lossy(n.as_bytes()))
            .map_err(|e| InterpError::Limit(e.to_string())),
        Object::Stream(s) => {
            let mut cmap = CMap::parse(&doc.decode_stream(&s)?.data)
                .map_err(|e| InterpError::Limit(e.to_string()))?;
            if let Some(base) = s.dict.get(b"UseCMap") {
                cmap = cmap.with_base(&load_cmap(doc, base, depth + 1)?);
            }
            Ok(cmap)
        }
        _ => Err(InterpError::Limit("font missing encoding".into())),
    }
}
fn metric(table: &[HMetric], cid: u32) -> Option<&HMetric> {
    table
        .binary_search_by(|m| {
            if cid < m.lo {
                std::cmp::Ordering::Greater
            } else if cid > m.hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
        .map(|i| &table[i])
}
fn face_gid(face: &Font, gid: u32) -> Option<u16> {
    if face.is_cid_keyed() {
        face.glyph_for_cid(gid, CidToGid::Identity)
    } else {
        u16::try_from(gid).ok().filter(|g| *g < face.num_glyphs())
    }
}
fn base_name(base: Standard14, gid: u32) -> Option<&'static str> {
    gid.checked_sub(1)
        .and_then(|g| base.metrics().widths.get(g as usize))
        .map(|(n, _)| *n)
}
fn base_width(base: Standard14, gid: u32) -> f64 {
    base_name(base, gid)
        .and_then(|n| base.glyph_width(n))
        .map_or_else(
            || {
                if base.name().starts_with("Courier") {
                    600.0
                } else if base.name().starts_with("Helvetica") || base == Standard14::ZapfDingbats {
                    278.0
                } else {
                    250.0
                }
            },
            f64::from,
        )
}
fn base_unicode(base: Standard14, u: u32) -> u32 {
    base.metrics()
        .widths
        .iter()
        .position(|(n, _)| strict_unicode(n) == Some(u))
        .map_or(0, |i| i as u32 + 1)
}
fn be_number(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |a, b| (a << 8) | u32::from(*b))
}
fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
fn sfnt_table<'a>(face: &'a Font, tag: &[u8; 4]) -> Option<&'a [u8]> {
    let data = face.data();
    let start = if data.starts_with(b"ttcf") {
        be_u32(data, 12 + face.face_index() as usize * 4)? as usize
    } else {
        0
    };
    let count = usize::from(be_u16(data, start + 4)?);
    for i in 0..count {
        let at = start + 12 + i * 16;
        if data.get(at..at + 4)? == tag {
            let offset = be_u32(data, at + 8)? as usize;
            let size = be_u32(data, at + 12)? as usize;
            return data.get(offset..offset.checked_add(size)?);
        }
    }
    None
}
fn vertical_advance(face: &Font, gid: u16) -> f64 {
    if let (Some(header), Some(metrics)) = (sfnt_table(face, b"vhea"), sfnt_table(face, b"vmtx"))
        && let Some(count) = be_u16(header, 34)
        && count > 0
        && let Some(advance) = be_u16(metrics, usize::from(gid.min(count - 1)) * 4)
    {
        return f64::from(advance) / f64::from(face.units_per_em().max(1));
    }
    let m = face.metrics();
    f64::from(m.ascent - m.descent) / f64::from(face.units_per_em().max(1))
}
fn extract_cff(data: Vec<u8>) -> Option<Vec<u8>> {
    if data.starts_with(b"OTTO") && data.len() > 12 {
        let n = usize::from(be_u16(&data, 4)?);
        data.get(..12 + n * 16)?;
        for i in 0..n {
            let at = 12 + i * 16;
            if data.get(at..at + 4)? == b"CFF " {
                let off = be_u32(&data, at + 8)? as usize;
                let len = be_u32(&data, at + 12)? as usize;
                return data.get(off..off.checked_add(len)?).map(<[u8]>::to_vec);
            }
        }
    }
    Some(data)
}
fn name(d: &Dict, key: &[u8]) -> String {
    d.get_name(key)
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_default()
}
pub(crate) fn integer(o: &Object) -> i32 {
    match o {
        Object::Integer(i) => *i as i32,
        Object::Real(f) => (f + 0.5).floor() as i32,
        _ => 0,
    }
}
fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].into()
}
fn strict_unicode(name: &str) -> Option<u32> {
    if let Some(s) = pdf_font::agl_lookup(name) {
        let mut c = s.chars();
        if let Some(first) = c.next()
            && c.next().is_none()
        {
            return Some(u32::from(first));
        }
    }
    GLYPH_NAMES_EXTRA
        .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| GLYPH_NAMES_EXTRA[i].1)
}
fn unicode_name(name: &str) -> char {
    let n = truncate(name, 63);
    let n = n.split('.').next().unwrap_or("");
    let n = match n {
        "f_f" => "ff",
        "f_f_i" => "ffi",
        "f_f_l" => "ffl",
        "f_i" => "fi",
        "f_l" => "fl",
        _ => n.split('_').next().unwrap_or(""),
    };
    if let Some(u) = strict_unicode(n).and_then(char::from_u32) {
        return u;
    }
    let (digits, radix) = if n.starts_with("uni") && n.len() == 7 {
        (&n[3..], 16)
    } else if let Some(n) = n.strip_prefix('u') {
        (n, 16)
    } else if n.starts_with('a') && n.len() >= 3 {
        (&n[1..], 10)
    } else {
        (n, 10)
    };
    let digits = digits
        .trim_start()
        .strip_prefix('+')
        .unwrap_or(digits.trim_start());
    let digits = if radix == 16 {
        digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
            .unwrap_or(digits)
    } else {
        digits
    };
    u32::from_str_radix(digits, radix)
        .ok()
        .filter(|u| *u != 0)
        .and_then(char::from_u32)
        .unwrap_or('\u{fffd}')
}
fn mac_code(name: &str) -> Option<u8> {
    let extra = [
        (173, "notequal"),
        (176, "infinity"),
        (178, "lessequal"),
        (179, "greaterequal"),
        (182, "partialdiff"),
        (183, "summation"),
        (184, "product"),
        (185, "pi"),
        (186, "integral"),
        (189, "Omega"),
        (195, "radical"),
        (197, "approxequal"),
        (198, "Delta"),
        (215, "lozenge"),
        (219, "Euro"),
        (240, "apple"),
    ];
    extra
        .into_iter()
        .find(|(_, n)| *n == name)
        .map(|(c, _)| c)
        .or_else(|| BaseEncoding::MacRoman.code_for_name(name))
}
fn vertical_form(u: u32) -> u32 {
    match u {
        0x21 | 0xff01 => 0xfe15,
        0x28 | 0xff08 => 0xfe35,
        0x29 | 0xff09 => 0xfe36,
        0x2c | 0xff0c => 0xfe10,
        0x3a | 0xff1a => 0xfe13,
        0x3b | 0xff1b => 0xfe14,
        0x3f | 0xff1f => 0xfe16,
        0x5b | 0xff3b => 0xfe47,
        0x5d | 0xff3d => 0xfe48,
        0x5f | 0xff3f => 0xfe33,
        0x7b | 0xff5b => 0xfe37,
        0x7d | 0xff5d => 0xfe38,
        0x2013 => 0xfe32,
        0x2014 | 0x30fc | 0xff0d => 0xfe31,
        0x2025 => 0xfe30,
        0x2026 => 0xfe19,
        0x3001 => 0xfe11,
        0x3002 => 0xfe12,
        0x3008 => 0xfe3f,
        0x3009 => 0xfe40,
        0x300a => 0xfe3d,
        0x300b => 0xfe3e,
        0x300c => 0xfe41,
        0x300d => 0xfe42,
        0x300e => 0xfe43,
        0x300f => 0xfe44,
        0x3010 => 0xfe3b,
        0x3011 => 0xfe3c,
        0x3014 => 0xfe39,
        0x3015 => 0xfe3a,
        0x3016 => 0xfe17,
        0x3017 => 0xfe18,
        _ => u,
    }
}

fn clean_name(name: &str) -> Option<Standard14> {
    let name: String = name.chars().filter(|c| *c != ' ').collect();
    let canonical = match name.as_str() {
        "CourierNew" | "CourierNewPSMT" => "Courier",
        "CourierNew,Bold" | "Courier,Bold" | "CourierNewPS-BoldMT" | "CourierNew-Bold" => {
            "Courier-Bold"
        }
        "CourierNew,Italic" | "Courier,Italic" | "CourierNewPS-ItalicMT" | "CourierNew-Italic" => {
            "Courier-Oblique"
        }
        "CourierNew,BoldItalic"
        | "Courier,BoldItalic"
        | "CourierNewPS-BoldItalicMT"
        | "CourierNew-BoldItalic" => "Courier-BoldOblique",
        "ArialMT" | "Arial" => "Helvetica",
        "Arial-BoldMT" | "Arial,Bold" | "Arial-Bold" | "Helvetica,Bold" => "Helvetica-Bold",
        "Arial-ItalicMT" | "Arial,Italic" | "Arial-Italic" | "Helvetica,Italic"
        | "Helvetica-Italic" => "Helvetica-Oblique",
        "Arial-BoldItalicMT"
        | "Arial,BoldItalic"
        | "Arial-BoldItalic"
        | "Helvetica,BoldItalic"
        | "Helvetica-BoldItalic" => "Helvetica-BoldOblique",
        "TimesNewRomanPSMT" | "TimesNewRoman" | "TimesNewRomanPS" => "Times-Roman",
        "TimesNewRomanPS-BoldMT"
        | "TimesNewRoman,Bold"
        | "TimesNewRomanPS-Bold"
        | "TimesNewRoman-Bold" => "Times-Bold",
        "TimesNewRomanPS-ItalicMT"
        | "TimesNewRoman,Italic"
        | "TimesNewRomanPS-Italic"
        | "TimesNewRoman-Italic" => "Times-Italic",
        "TimesNewRomanPS-BoldItalicMT"
        | "TimesNewRoman,BoldItalic"
        | "TimesNewRomanPS-BoldItalic"
        | "TimesNewRoman-BoldItalic" => "Times-BoldItalic",
        "Symbol,Italic"
        | "Symbol,Bold"
        | "Symbol,BoldItalic"
        | "SymbolMT"
        | "SymbolMT,Italic"
        | "SymbolMT,Bold"
        | "SymbolMT,BoldItalic" => "Symbol",
        n => n,
    };
    Standard14::ALL
        .into_iter()
        .find(|font| font.name() == canonical)
}
