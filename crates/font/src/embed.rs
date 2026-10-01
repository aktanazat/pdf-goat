//! Font subset embedding. [`embed_truetype`] takes caller-owned character codes and
//! writes a TrueType subset as a Type0 font using Identity-H: encode each supplied code
//! as two big-endian bytes. Glyph aliases keep separate codes and ToUnicode entries,
//! even when they share a glyph outline. [`embed_font`] takes any outline format
//! (TrueType, CFF, OpenType CFF, Type 1) and chooses the codes itself. No font selection
//! or text layout is performed here.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use pdf_core::{Dict, Document, ObjRef, Object, Stream};

use crate::subset::subset_truetype_with_advances;
use crate::subset_cff::subset_cff;
use crate::subset_type1::subset_type1;
use crate::{
    BaseEncoding, Font, FontError, FontKind, subset_truetype, write_simple_to_unicode_cmap,
    write_to_unicode_cmap,
};

/// One character code and its source-font glyph. Unicode may contain a scalar,
/// a supplementary-plane character, or a ligature; empty text has no ToUnicode entry.
#[derive(Clone, Copy, Debug)]
pub struct GlyphMapping<'a> {
    pub code: u16,
    pub glyph_id: u16,
    pub unicode: &'a str,
}

/// A glyph for [`embed_font`] and the text it stands for; empty text has no ToUnicode
/// entry.
#[derive(Clone, Copy, Debug)]
pub struct EmbedGlyph<'a> {
    pub glyph_id: u16,
    pub unicode: &'a str,
}

/// A font [`embed_font`] added to a document.
#[derive(Debug)]
pub struct EmbeddedFont {
    /// The font dictionary, for a page's /Font resources.
    pub font: ObjRef,
    /// The character code bytes that draw each requested glyph, in request order.
    pub codes: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub enum EmbedError {
    Font(FontError),
    InvalidMapping(&'static str),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Font(error) => error.fmt(f),
            Self::InvalidMapping(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EmbedError {}

impl From<FontError> for EmbedError {
    fn from(error: FontError) -> Self {
        Self::Font(error)
    }
}

/// Adds a Type0 font, its CIDFontType2 descendant, the TrueType subset, widths,
/// descriptor, CIDToGIDMap and ToUnicode streams to `doc`. Codes must be unique
/// (including zero); glyph ids refer to `font`, not the subset. Sparse codes are valid.
/// Only TrueType outlines are supported, just as by [`subset_truetype`].
/// Invalid mappings are rejected before the document is changed.
pub fn embed_truetype(
    doc: &mut Document,
    font: &Font,
    mappings: &[GlyphMapping<'_>],
) -> Result<ObjRef, EmbedError> {
    embed(doc, font, mappings, None)
}

/// Embeds the same glyphs with caller-specified PDF advances (1/1000 em), in the
/// input mapping order. Outlines are shared; different advances for one source
/// glyph receive distinct metric-bearing GIDs. The PDF width values stay exact;
/// font-unit rounding must remain within PDF/A's one-unit consistency tolerance.
/// Invalid widths or mappings leave the document unchanged.
pub fn embed_truetype_with_widths(
    doc: &mut Document,
    font: &Font,
    mappings: &[GlyphMapping<'_>],
    widths: &[f64],
) -> Result<ObjRef, EmbedError> {
    embed(doc, font, mappings, Some(widths))
}

/// Embeds the subset of `font` that draws `glyphs`, whatever its outlines:
/// - TrueType: a Type0 font over a CIDFontType2 subset, as [`embed_truetype`] writes
///   it, with a two-byte code per requested glyph.
/// - CFF, bare or in OpenType: a Type0 font over a CIDFontType0 with a `CIDFontType0C`
///   subset. The two-byte codes are CIDs: the glyph ids of a name-keyed font, the
///   font's own CIDs of a CID-keyed one. Requests for one glyph share its code, and
///   the first request's text is the glyph's ToUnicode entry.
/// - Type 1: a simple font over a subset program, with one-byte codes from 1 for up
///   to 255 distinct glyphs, named by an /Encoding /Differences array. Requests for
///   one glyph share its code as for CFF.
///
/// Every font gets a ToUnicode map. Invalid glyphs are rejected before the document
/// is changed.
pub fn embed_font(
    doc: &mut Document,
    font: &Font,
    glyphs: &[EmbedGlyph<'_>],
) -> Result<EmbeddedFont, EmbedError> {
    if glyphs.is_empty() {
        return Err(EmbedError::InvalidMapping(
            "a font needs at least one character mapping",
        ));
    }
    if glyphs
        .iter()
        .any(|glyph| glyph.glyph_id >= font.num_glyphs())
    {
        return Err(EmbedError::InvalidMapping(
            "font mapping has an invalid glyph",
        ));
    }
    match font.kind() {
        FontKind::TrueType => {
            let mappings = glyphs
                .iter()
                .enumerate()
                .map(|(index, glyph)| {
                    Ok(GlyphMapping {
                        code: u16::try_from(index + 1).map_err(|_| {
                            EmbedError::InvalidMapping("too many glyphs for one font")
                        })?,
                        glyph_id: glyph.glyph_id,
                        unicode: glyph.unicode,
                    })
                })
                .collect::<Result<Vec<_>, EmbedError>>()?;
            let codes = mappings
                .iter()
                .map(|mapping| mapping.code.to_be_bytes().to_vec())
                .collect();
            Ok(EmbeddedFont {
                font: embed(doc, font, &mappings, None)?,
                codes,
            })
        }
        FontKind::OpenTypeCff | FontKind::Cff => embed_cff(doc, font, glyphs),
        FontKind::Type1 => embed_type1(doc, font, glyphs),
    }
}

fn font_advance(width: f64, em: f64) -> Result<u16, EmbedError> {
    let advance = (width / 1000.0 * em).round();
    if !width.is_finite()
        || width < 0.0
        || !(0.0..=f64::from(u16::MAX)).contains(&advance)
        || (advance / em * 1000.0 - width).abs() > 1.0
    {
        return Err(EmbedError::InvalidMapping(
            "font advance cannot be represented in the embedded program",
        ));
    }
    Ok(advance as u16)
}

fn embed(
    doc: &mut Document,
    font: &Font,
    mappings: &[GlyphMapping<'_>],
    pdf_widths: Option<&[f64]>,
) -> Result<ObjRef, EmbedError> {
    if mappings.is_empty() {
        return Err(EmbedError::InvalidMapping(
            "a font needs at least one character mapping",
        ));
    }
    if pdf_widths.is_some_and(|widths| widths.len() != mappings.len()) {
        return Err(EmbedError::InvalidMapping(
            "font advance count must match character mappings",
        ));
    }
    let mut sorted: Vec<usize> = (0..mappings.len()).collect();
    sorted.sort_unstable_by_key(|&index| mappings[index].code);
    let mut previous = None;
    for &index in &sorted {
        let mapping = &mappings[index];
        if previous == Some(mapping.code) {
            return Err(EmbedError::InvalidMapping(
                "font character codes must be unique",
            ));
        }
        if mapping.glyph_id >= font.num_glyphs() {
            return Err(EmbedError::InvalidMapping(
                "font mapping has an invalid glyph",
            ));
        }
        previous = Some(mapping.code);
    }
    let glyph_ids: Vec<u16> = sorted
        .iter()
        .map(|&index| mappings[index].glyph_id)
        .collect();
    let em = units_per_em(font)?;
    let (subset, mapped_glyphs) = match pdf_widths {
        Some(widths) => {
            let advances = sorted
                .iter()
                .map(|&index| font_advance(widths[index], em))
                .collect::<Result<Vec<_>, _>>()?;
            subset_truetype_with_advances(font, &glyph_ids, &advances)?
        }
        None => (subset_truetype(font, &glyph_ids)?, Vec::new()),
    };
    let max_code = previous.ok_or(EmbedError::InvalidMapping("font has no character codes"))?;
    let mut cid_map = vec![0u8; (usize::from(max_code) + 1) * 2];
    let mut widths = Vec::with_capacity(sorted.len());
    for (position, &index) in sorted.iter().enumerate() {
        let mapping = &mappings[index];
        let gid = if pdf_widths.is_some() {
            mapped_glyphs[position]
        } else {
            subset
                .new_gid(mapping.glyph_id)
                .ok_or(EmbedError::InvalidMapping(
                    "font subset lost a mapped glyph",
                ))?
        };
        let offset = usize::from(mapping.code) * 2;
        cid_map[offset..offset + 2].copy_from_slice(&gid.to_be_bytes());
        let width = match pdf_widths {
            Some(widths) => widths[index],
            None => glyph_width(font, mapping.glyph_id, em)?,
        };
        widths.push((mapping.code, width));
    }
    let unicode: Vec<(u16, &str)> = sorted
        .iter()
        .filter_map(|&index| {
            let mapping = &mappings[index];
            (!mapping.unicode.is_empty()).then_some((mapping.code, mapping.unicode))
        })
        .collect();
    let unicode = write_to_unicode_cmap(&unicode);
    let mut file_dict = Dict::new();
    file_dict.insert("Length1", subset.data.len() as i64);
    let file = doc.add(Stream::new(file_dict, subset.data));
    let name = subset_name(font, file);
    let descriptor = add_descriptor(doc, font, &name, em, "FontFile2", file);
    let cid_map = doc.add(Stream::new(Dict::new(), cid_map));
    let mut descendant = Dict::new();
    descendant.insert("Type", Object::name("Font"));
    descendant.insert("Subtype", Object::name("CIDFontType2"));
    descendant.insert("BaseFont", Object::name(name.as_str()));
    descendant.insert("CIDSystemInfo", system_info("Adobe", "Identity", 0));
    descendant.insert("FontDescriptor", descriptor);
    descendant.insert("CIDToGIDMap", cid_map);
    descendant.insert("W", width_runs(&widths));
    let descendant = doc.add(descendant);
    Ok(add_type0(doc, &name, descendant, unicode))
}

/// A Type0 font over a CIDFontType0 whose `CIDFontType0C` program is the CFF subset.
fn embed_cff(
    doc: &mut Document,
    font: &Font,
    glyphs: &[EmbedGlyph<'_>],
) -> Result<EmbeddedFont, EmbedError> {
    let em = units_per_em(font)?;
    let cid_keyed = font.is_cid_keyed();
    let cids = glyphs
        .iter()
        .map(|glyph| match cid_keyed {
            true => font
                .cid_for_glyph(glyph.glyph_id)
                .ok_or(EmbedError::InvalidMapping("font glyph has no CID")),
            false => Ok(glyph.glyph_id),
        })
        .collect::<Result<Vec<u16>, EmbedError>>()?;
    let mut by_cid: BTreeMap<u16, (u16, &str)> = BTreeMap::new();
    for (glyph, &cid) in glyphs.iter().zip(&cids) {
        by_cid.entry(cid).or_insert((glyph.glyph_id, glyph.unicode));
    }
    let widths = by_cid
        .iter()
        .map(|(&cid, &(glyph_id, _))| Ok((cid, glyph_width(font, glyph_id, em)?)))
        .collect::<Result<Vec<_>, EmbedError>>()?;
    let unicode: Vec<(u16, &str)> = by_cid
        .iter()
        .filter(|(_, (_, text))| !text.is_empty())
        .map(|(&cid, &(_, text))| (cid, text))
        .collect();
    let keep: BTreeSet<u16> = std::iter::once(0)
        .chain(glyphs.iter().map(|glyph| glyph.glyph_id))
        .collect();
    let seac_glyph = |code: u8| {
        BaseEncoding::Standard
            .glyph_name(code)
            .and_then(|name| font.glyph_by_name(name))
    };
    let program = subset_cff(font.cff_bytes(), &keep, &seac_glyph)?;
    let mut file_dict = Dict::new();
    file_dict.insert("Subtype", Object::name("CIDFontType0C"));
    let file = doc.add(Stream::new(file_dict, program));
    let name = subset_name(font, file);
    let descriptor = add_descriptor(doc, font, &name, em, "FontFile3", file);
    let system = match font.cid_system_info().filter(|_| cid_keyed) {
        Some((registry, ordering, supplement)) => system_info(&registry, &ordering, supplement),
        None => system_info("Adobe", "Identity", 0),
    };
    let mut descendant = Dict::new();
    descendant.insert("Type", Object::name("Font"));
    descendant.insert("Subtype", Object::name("CIDFontType0"));
    descendant.insert("BaseFont", Object::name(name.as_str()));
    descendant.insert("CIDSystemInfo", system);
    descendant.insert("FontDescriptor", descriptor);
    descendant.insert("W", width_runs(&widths));
    let descendant = doc.add(descendant);
    Ok(EmbeddedFont {
        font: add_type0(doc, &name, descendant, write_to_unicode_cmap(&unicode)),
        codes: cids.iter().map(|cid| cid.to_be_bytes().to_vec()).collect(),
    })
}

/// A simple Type 1 font over the subset program, its codes named by /Differences.
fn embed_type1(
    doc: &mut Document,
    font: &Font,
    glyphs: &[EmbedGlyph<'_>],
) -> Result<EmbeddedFont, EmbedError> {
    let em = units_per_em(font)?;
    // Distinct glyphs in order of first request; a glyph's code is its position + 1.
    let mut order: Vec<u16> = Vec::new();
    let mut codes = Vec::with_capacity(glyphs.len());
    let mut unicode: Vec<(u8, &str)> = Vec::new();
    for glyph in glyphs {
        let position = match order.iter().position(|&id| id == glyph.glyph_id) {
            Some(position) => position,
            None => {
                order.push(glyph.glyph_id);
                order.len() - 1
            }
        };
        let code = u8::try_from(position + 1).map_err(|_| {
            EmbedError::InvalidMapping("a Type 1 font encodes at most 255 distinct glyphs")
        })?;
        if !glyph.unicode.is_empty() && !unicode.iter().any(|&(known, _)| known == code) {
            unicode.push((code, glyph.unicode));
        }
        codes.push(vec![code]);
    }
    let names = order
        .iter()
        .map(|&id| {
            font.glyph_name(id)
                .ok_or(EmbedError::InvalidMapping("Type 1 glyph has no name"))
        })
        .collect::<Result<Vec<String>, EmbedError>>()?;
    let widths = order
        .iter()
        .map(|&id| glyph_width(font, id, em).map(Object::Real))
        .collect::<Result<Vec<_>, EmbedError>>()?;
    let program = subset_type1(
        font.data(),
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    unicode.sort_unstable();
    let mut file_dict = Dict::new();
    for (key, length) in ["Length1", "Length2", "Length3"]
        .into_iter()
        .zip(program.lengths)
    {
        file_dict.insert(key, length as i64);
    }
    let file = doc.add(Stream::new(file_dict, program.data));
    let name = subset_name(font, file);
    let descriptor = add_descriptor(doc, font, &name, em, "FontFile", file);
    let mut differences = vec![Object::Integer(1)];
    differences.extend(names.iter().map(|name| Object::name(name.as_str())));
    let mut encoding = Dict::new();
    encoding.insert("Type", Object::name("Encoding"));
    encoding.insert("Differences", differences);
    let to_unicode = doc.add(Stream::new(
        Dict::new(),
        write_simple_to_unicode_cmap(&unicode),
    ));
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Font"));
    dict.insert("Subtype", Object::name("Type1"));
    dict.insert("BaseFont", Object::name(name.as_str()));
    dict.insert("FirstChar", 1);
    dict.insert("LastChar", order.len() as i64);
    dict.insert("Widths", widths);
    dict.insert("Encoding", encoding);
    dict.insert("FontDescriptor", descriptor);
    dict.insert("ToUnicode", to_unicode);
    Ok(EmbeddedFont {
        font: doc.add(dict),
        codes,
    })
}

fn units_per_em(font: &Font) -> Result<f64, EmbedError> {
    let em = f64::from(font.units_per_em());
    if em == 0.0 {
        return Err(EmbedError::InvalidMapping("font has no units per em"));
    }
    Ok(em)
}

/// A glyph's advance in 1/1000 em.
fn glyph_width(font: &Font, glyph_id: u16, em: f64) -> Result<f64, EmbedError> {
    let advance = font.advance(glyph_id).ok_or(EmbedError::InvalidMapping(
        "mapped font glyph has no advance",
    ))?;
    Ok(f64::from(advance) / em * 1000.0)
}

/// A CIDFont /W array for `(cid, width)` pairs sorted by CID: one run per stretch of
/// consecutive CIDs.
fn width_runs(widths: &[(u16, f64)]) -> Vec<Object> {
    let mut out = Vec::new();
    let mut run = Vec::new();
    let mut start = 0;
    let mut previous: Option<u16> = None;
    for &(cid, width) in widths {
        if previous.is_some_and(|previous| u32::from(cid) != u32::from(previous) + 1) {
            out.push(Object::Integer(i64::from(start)));
            out.push(Object::Array(std::mem::take(&mut run)));
        }
        if run.is_empty() {
            start = cid;
        }
        run.push(Object::Real(width));
        previous = Some(cid);
    }
    if !run.is_empty() {
        out.push(Object::Integer(i64::from(start)));
        out.push(Object::Array(run));
    }
    out
}

/// `ABCDEF+PostScriptName`. Object numbers are document-unique, so a tag taken from
/// the program stream's number is never shared by independent subsets.
fn subset_name(font: &Font, file: ObjRef) -> String {
    let mut tag = [b'A'; 6];
    let mut number = file.num;
    for byte in tag.iter_mut().rev() {
        *byte += (number % 26) as u8;
        number /= 26;
    }
    let tag: String = tag.into_iter().map(char::from).collect();
    format!(
        "{tag}+{}",
        font.postscript_name()
            .unwrap_or_else(|| "Subset".to_owned())
    )
}

fn add_descriptor(
    doc: &mut Document,
    font: &Font,
    name: &str,
    em: f64,
    file_key: &str,
    file: ObjRef,
) -> ObjRef {
    let metrics = font.metrics();
    let scale = |value: f32| Object::Real(f64::from(value) / em * 1000.0);
    let mut descriptor = Dict::new();
    descriptor.insert("Type", Object::name("FontDescriptor"));
    descriptor.insert("FontName", Object::name(name));
    descriptor.insert("Flags", i64::from(metrics.flags));
    descriptor.insert(
        "FontBBox",
        vec![
            scale(metrics.bbox.x_min),
            scale(metrics.bbox.y_min),
            scale(metrics.bbox.x_max),
            scale(metrics.bbox.y_max),
        ],
    );
    descriptor.insert("ItalicAngle", Object::Real(f64::from(metrics.italic_angle)));
    descriptor.insert("Ascent", scale(metrics.ascent));
    descriptor.insert("Descent", scale(metrics.descent));
    descriptor.insert(
        "CapHeight",
        scale(metrics.cap_height.unwrap_or(metrics.ascent)),
    );
    descriptor.insert("StemV", 80);
    descriptor.insert(file_key, file);
    doc.add(descriptor)
}

fn system_info(registry: &str, ordering: &str, supplement: i32) -> Dict {
    let mut system = Dict::new();
    system.insert("Registry", Object::string(registry.as_bytes().to_vec()));
    system.insert("Ordering", Object::string(ordering.as_bytes().to_vec()));
    system.insert("Supplement", i64::from(supplement));
    system
}

fn add_type0(doc: &mut Document, name: &str, descendant: ObjRef, to_unicode: Vec<u8>) -> ObjRef {
    let unicode = doc.add(Stream::new(Dict::new(), to_unicode));
    let mut type0 = Dict::new();
    type0.insert("Type", Object::name("Font"));
    type0.insert("Subtype", Object::name("Type0"));
    type0.insert("BaseFont", Object::name(name));
    type0.insert("Encoding", Object::name("Identity-H"));
    type0.insert("DescendantFonts", vec![Object::Reference(descendant)]);
    type0.insert("ToUnicode", unicode);
    doc.add(type0)
}
