//! TrueType subset embedding with caller-owned character codes. The returned
//! Type0 font uses Identity-H: encode each supplied code as two big-endian bytes.
//! Glyph aliases keep separate codes and ToUnicode entries, even when they share
//! a glyph outline. No font selection or text layout is performed here.

use std::fmt;

use pdf_core::{Dict, Document, ObjRef, Object, Stream};

use crate::subset::subset_truetype_with_advances;
use crate::{Font, FontError, subset_truetype, write_to_unicode_cmap};

/// One character code and its source-font glyph. Unicode may contain a scalar,
/// a supplementary-plane character, or a ligature; empty text has no ToUnicode entry.
#[derive(Clone, Copy, Debug)]
pub struct GlyphMapping<'a> {
    pub code: u16,
    pub glyph_id: u16,
    pub unicode: &'a str,
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
    let em = f64::from(font.units_per_em());
    if em == 0.0 {
        return Err(EmbedError::InvalidMapping("font has no units per em"));
    }
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
    let metrics = font.metrics();
    let max_code = previous.ok_or(EmbedError::InvalidMapping("font has no character codes"))?;
    let mut cid_map = vec![0u8; (usize::from(max_code) + 1) * 2];
    let mut widths = Vec::new();
    let mut run = Vec::new();
    let mut run_start = mappings[sorted[0]].code;
    let mut previous = run_start;
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
        if !run.is_empty() && u32::from(mapping.code) != u32::from(previous) + 1 {
            widths.push(Object::Integer(i64::from(run_start)));
            widths.push(Object::Array(std::mem::take(&mut run)));
            run_start = mapping.code;
        }
        let width = match pdf_widths {
            Some(widths) => widths[index],
            None => {
                f64::from(
                    font.advance(mapping.glyph_id)
                        .ok_or(EmbedError::InvalidMapping(
                            "mapped font glyph has no advance",
                        ))?,
                ) / em
                    * 1000.0
            }
        };
        run.push(Object::Real(width));
        previous = mapping.code;
    }
    widths.push(Object::Integer(i64::from(run_start)));
    widths.push(Object::Array(run));
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
    // Object numbers are document-unique, so independent subsets never share a tag.
    let mut tag = [b'A'; 6];
    let mut number = file.num;
    for byte in tag.iter_mut().rev() {
        *byte += (number % 26) as u8;
        number /= 26;
    }
    let tag: String = tag.into_iter().map(char::from).collect();
    let name = format!(
        "{tag}+{}",
        font.postscript_name()
            .unwrap_or_else(|| "Subset".to_owned())
    );
    let scale = |value: f32| Object::Real(f64::from(value) / em * 1000.0);
    let mut descriptor = Dict::new();
    descriptor.insert("Type", Object::name("FontDescriptor"));
    descriptor.insert("FontName", Object::name(name.as_str()));
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
    descriptor.insert("FontFile2", file);
    let descriptor = doc.add(descriptor);
    let mut system = Dict::new();
    system.insert("Registry", Object::string(b"Adobe".to_vec()));
    system.insert("Ordering", Object::string(b"Identity".to_vec()));
    system.insert("Supplement", 0);
    let cid_map = doc.add(Stream::new(Dict::new(), cid_map));
    let mut descendant = Dict::new();
    descendant.insert("Type", Object::name("Font"));
    descendant.insert("Subtype", Object::name("CIDFontType2"));
    descendant.insert("BaseFont", Object::name(name.as_str()));
    descendant.insert("CIDSystemInfo", system);
    descendant.insert("FontDescriptor", descriptor);
    descendant.insert("CIDToGIDMap", cid_map);
    descendant.insert("W", widths);
    let descendant = doc.add(descendant);
    let unicode = doc.add(Stream::new(Dict::new(), unicode));
    let mut type0 = Dict::new();
    type0.insert("Type", Object::name("Font"));
    type0.insert("Subtype", Object::name("Type0"));
    type0.insert("BaseFont", Object::name(name));
    type0.insert("Encoding", Object::name("Identity-H"));
    type0.insert("DescendantFonts", vec![Object::Reference(descendant)]);
    type0.insert("ToUnicode", unicode);
    Ok(doc.add(type0))
}
