//! Embed missing font programs without rewriting content strings or changing text advances.
//!
//! Simple fonts become one-byte CID fonts. Their old codes remain CIDs, while a
//! CIDToGIDMap selects the substitute's subset glyphs. Composite fonts keep their
//! original CMap, widths, vertical metrics, and ToUnicode mapping.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use goat_common::GoatError;
use pdf_core::{Dict, Document, ObjRef, Object, Stream};
use pdf_font::embed::{GlyphMapping, embed_truetype_with_widths};
use pdf_font::{
    BaseEncoding, CMap, CharCode, CidCollection, Encoding, Font, FontKind, FontLocator,
    FontRequest, MatchQuality, Script, Standard14, ToUnicodeMap,
};

use crate::doc;

const MAX_DEPTH: usize = 64;
const MAX_CODES: u64 = 1_114_112;
type FontKey = (String, u32, Option<u16>, Script);

struct Substitute {
    font: Font,
    exact: bool,
}

fn failure(message: impl std::fmt::Display) -> GoatError {
    GoatError::message(format!("convert pdfa: {message}"))
}

fn descriptor(document: &Document, font: &Dict) -> Result<Dict, GoatError> {
    match font.get(b"FontDescriptor") {
        Some(value) => Ok(document
            .resolve_dict(value)
            .map_err(doc::pdf_error)?
            .unwrap_or_default()),
        None => Ok(Dict::new()),
    }
}

fn is_embedded(document: &Document, descriptor: &Dict) -> Result<bool, GoatError> {
    for key in [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"] {
        if let Some(value) = descriptor.get(key)
            && let Some(stream) = document.resolve_stream(value).map_err(doc::pdf_error)?
            && !stream.raw().is_empty()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_substitute<'a>(
    cache: &'a mut HashMap<FontKey, Substitute>,
    font: &Dict,
    descriptor: &Dict,
    script: Script,
) -> Result<&'a Substitute, GoatError> {
    let base = font
        .get_name(b"BaseFont")
        .ok_or_else(|| failure("an unembedded font has no BaseFont"))?;
    let base = String::from_utf8_lossy(base).into_owned();
    let standard = Standard14::from_base_font(&base);
    let flags = descriptor
        .get_i64(b"Flags")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or_else(|| standard.map_or(0, |font| font.metrics().flags));
    let weight = descriptor
        .get_i64(b"FontWeight")
        .and_then(|v| u16::try_from(v).ok());
    let key = (base, flags, weight, script);
    let value = match cache.entry(key) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let (base, flags, weight, script) = entry.key();
            let substitute = FontLocator::system()
                .find(&FontRequest {
                    base_font: base,
                    flags: *flags,
                    weight: *weight,
                    script: *script,
                })
                .ok_or_else(|| {
                    failure(format!(
                        "no installed substitute for unembedded font {base}"
                    ))
                })?;
            let font = substitute.load().map_err(failure)?;
            if font.kind() != FontKind::TrueType {
                return Err(failure(format!(
                    "cannot subset the installed {:?} substitute for {base}",
                    font.kind()
                )));
            }
            entry.insert(Substitute {
                font,
                exact: substitute.quality == MatchQuality::Exact,
            })
        }
    };
    Ok(value)
}

fn unicode_map(document: &Document, font: &Dict) -> Result<Option<ToUnicodeMap>, GoatError> {
    let Some(value) = font.get(b"ToUnicode") else {
        return Ok(None);
    };
    let Some(stream) = document.resolve_stream(value).map_err(doc::pdf_error)? else {
        return Ok(None);
    };
    let decoded = document.decode_stream(&stream).map_err(doc::pdf_error)?;
    Ok(Some(ToUnicodeMap::parse(&decoded.data).map_err(failure)?))
}

fn simple_encoding(
    document: &Document,
    font: &Dict,
    standard: Option<Standard14>,
) -> Result<Encoding, GoatError> {
    let value = document
        .resolve_key(font, b"Encoding")
        .map_err(doc::pdf_error)?;
    let base_name = match &value {
        Object::Name(name) => name.as_str(),
        Object::Dict(dict) => dict
            .get_name(b"BaseEncoding")
            .and_then(|bytes| std::str::from_utf8(bytes).ok()),
        _ => None,
    };
    let base = base_name
        .and_then(BaseEncoding::from_pdf_name)
        .or_else(|| standard.map(Standard14::builtin_encoding))
        .unwrap_or(BaseEncoding::Standard);
    let mut encoding = Encoding::new(Some(base));
    if let Object::Dict(dict) = value
        && let Some(value) = dict.get(b"Differences")
        && let Some(differences) = document.resolve_array(value).map_err(doc::pdf_error)?
    {
        let mut code = 0i64;
        for value in differences {
            match value {
                Object::Integer(first) => code = first,
                Object::Name(name) => {
                    if let Ok(byte) = u8::try_from(code) {
                        encoding.set(byte, &String::from_utf8_lossy(name.as_bytes()));
                    }
                    code = code.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    Ok(encoding)
}

fn one_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// A ToUnicode CMap with exactly the original code lengths, including one-byte fonts.
fn write_unicode(entries: &[(CharCode, String)], encoding: &CMap) -> Vec<u8> {
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /GoatUnicode def\n/CMapType 2 def\n",
    );
    let _ = writeln!(out, "{} begincodespacerange", encoding.codespaces().len());
    for range in encoding.codespaces() {
        let n = usize::from(range.len);
        let low = range.low[..n]
            .iter()
            .fold(0u32, |code, byte| code * 256 + u32::from(*byte));
        let high = range.high[..n]
            .iter()
            .fold(0u32, |code, byte| code * 256 + u32::from(*byte));
        let width = n * 2;
        let _ = writeln!(out, "<{low:0width$X}> <{high:0width$X}>");
    }
    out.push_str("endcodespacerange\n");
    for chunk in entries.chunks(100) {
        let _ = writeln!(out, "{} beginbfchar", chunk.len());
        for (code, text) in chunk {
            let width = usize::from(code.len) * 2;
            let _ = write!(out, "<{:0width$X}> <", code.code);
            for unit in text.encode_utf16() {
                let _ = write!(out, "{unit:04X}");
            }
            out.push_str(">\n");
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out.into_bytes()
}

const ONE_BYTE_CMAP: &[u8] = b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def\n/CMapName /GoatOneByte def\n/CMapType 1 def\n/WMode 0 def\n1 begincodespacerange\n<00> <FF>\nendcodespacerange\n1 begincidrange\n<00> <FF> 0\nendcidrange\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";

/// Resolve only the widths needed by the sorted character mappings. Range-form
/// W entries are intersected with those mappings rather than expanded to 65536 CIDs.
fn cid_widths(
    document: &Document,
    descendant: &Dict,
    mappings: &[GlyphMapping<'_>],
) -> Result<Vec<f64>, GoatError> {
    let default = match document
        .resolve_key(descendant, b"DW")
        .map_err(doc::pdf_error)?
    {
        Object::Null => 1000.0,
        value => value
            .as_f64()
            .ok_or_else(|| failure("CID font default width is not a number"))?,
    };
    let mut widths = vec![default; mappings.len()];
    let values = match document
        .resolve_key(descendant, b"W")
        .map_err(doc::pdf_error)?
    {
        Object::Null => return Ok(widths),
        Object::Array(values) => values,
        _ => return Err(failure("CID font widths are not an array")),
    };
    let number = |value: &Object| -> Result<f64, GoatError> {
        document
            .resolve(value)
            .map_err(doc::pdf_error)?
            .as_f64()
            .ok_or_else(|| failure("CID font width is not a number"))
    };
    let mut i = 0;
    while i < values.len() {
        let first = document
            .resolve(&values[i])
            .map_err(doc::pdf_error)?
            .as_i64()
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| failure("invalid first CID in font widths"))?;
        let next = values
            .get(i + 1)
            .ok_or_else(|| failure("incomplete CID font widths"))?;
        let start = mappings.partition_point(|mapping| mapping.code < first);
        match document.resolve(next).map_err(doc::pdf_error)? {
            Object::Array(run) => {
                let end = usize::from(first)
                    .checked_add(run.len())
                    .filter(|end| *end <= usize::from(u16::MAX) + 1)
                    .ok_or_else(|| failure("CID font width run exceeds 65535"))?;
                for (mapping, width) in mappings[start..]
                    .iter()
                    .zip(&mut widths[start..])
                    .take_while(|(mapping, _)| usize::from(mapping.code) < end)
                {
                    *width = number(&run[usize::from(mapping.code - first)])?;
                }
                i += 2;
            }
            last => {
                let last = last
                    .as_i64()
                    .and_then(|value| u16::try_from(value).ok())
                    .filter(|last| *last >= first)
                    .ok_or_else(|| failure("invalid last CID in font widths"))?;
                let width = number(
                    values
                        .get(i + 2)
                        .ok_or_else(|| failure("incomplete CID font width range"))?,
                )?;
                let end = mappings.partition_point(|mapping| mapping.code <= last);
                widths[start..end].fill(width);
                i += 3;
            }
        }
    }
    Ok(widths)
}

/// The shared writer owns the subset, descriptor, and glyph map. Retained PDF fonts
/// own their widths and encoding, so only the program-bearing entries are taken.
fn embed_program(
    document: &mut Document,
    font: &Font,
    glyphs: &BTreeMap<u16, u16>,
    descendant: &mut Dict,
) -> Result<String, GoatError> {
    let mappings: Vec<GlyphMapping<'_>> = glyphs
        .iter()
        .map(|(&code, &glyph_id)| GlyphMapping {
            code,
            glyph_id,
            unicode: "",
        })
        .collect();
    let widths = cid_widths(document, descendant, &mappings)?;
    let root_id =
        embed_truetype_with_widths(document, font, &mappings, &widths).map_err(failure)?;
    let root = document
        .resolve_dict(&Object::Reference(root_id))
        .map_err(doc::pdf_error)?
        .ok_or_else(|| failure("embedded font is not a dictionary"))?;
    let children = root
        .get_array(b"DescendantFonts")
        .ok_or_else(|| failure("embedded font has no descendant"))?;
    let child_id = children
        .first()
        .and_then(Object::as_reference)
        .ok_or_else(|| failure("embedded font has no descendant reference"))?;
    let child = document
        .resolve_dict(&Object::Reference(child_id))
        .map_err(doc::pdf_error)?
        .ok_or_else(|| failure("embedded font descendant is not a dictionary"))?;
    let name = child
        .get_name(b"BaseFont")
        .ok_or_else(|| failure("embedded font has no name"))?;
    let name = String::from_utf8_lossy(name).into_owned();
    for key in [
        "Type",
        "Subtype",
        "BaseFont",
        "FontDescriptor",
        "CIDToGIDMap",
        "CIDSystemInfo",
    ] {
        let value = child
            .get(key.as_bytes())
            .ok_or_else(|| failure(format!("embedded font has no {key}")))?;
        descendant.insert(key, value.clone());
    }
    // These temporary wrappers are replaced by the source font's retained-code wrappers.
    if let Some(unicode) = root.get_ref(b"ToUnicode") {
        document.delete(unicode);
    }
    document.delete(root_id);
    document.delete(child_id);
    Ok(name)
}

fn embed_simple(
    document: &mut Document,
    font: &mut Dict,
    cache: &mut HashMap<FontKey, Substitute>,
) -> Result<bool, GoatError> {
    let descriptor = descriptor(document, font)?;
    if is_embedded(document, &descriptor)? {
        return Ok(false);
    }
    let substitute = load_substitute(cache, font, &descriptor, Script::Latin)?;
    let base = font
        .get_name(b"BaseFont")
        .and_then(|name| std::str::from_utf8(name).ok())
        .unwrap_or("");
    let standard = Standard14::from_base_font(base);
    let encoding = simple_encoding(document, font, standard)?;
    let unicode = unicode_map(document, font)?;
    let symbolic = matches!(
        standard,
        Some(Standard14::Symbol | Standard14::ZapfDingbats)
    );
    let widths = match font.get(b"Widths") {
        Some(value) => document
            .resolve_array(value)
            .map_err(doc::pdf_error)?
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let first = font.get_i64(b"FirstChar").unwrap_or(0);
    let default_width = descriptor.get_f64(b"MissingWidth");
    let mut glyphs = BTreeMap::new();
    let mut mapped = Vec::new();
    let mut output_widths = Vec::with_capacity(256);
    for code in 0..=u8::MAX {
        let name = encoding.glyph_name(code);
        let encoded = name.and_then(pdf_font::glyph_name_to_unicode).or_else(|| {
            (standard == Some(Standard14::ZapfDingbats))
                .then(|| {
                    name.and_then(pdf_font::dingbats_name_to_unicode)
                        .map(|ch| ch.to_string())
                })
                .flatten()
        });
        let text = unicode
            .as_ref()
            .and_then(|map| map.lookup(u32::from(code)))
            .or_else(|| encoded.clone());
        let gid = substitute
            .font
            .simple_glyph(code, name, symbolic)
            .or_else(|| {
                encoded
                    .as_deref()
                    .and_then(one_char)
                    .and_then(|ch| substitute.font.glyph_for_char(ch))
            })
            .or_else(|| {
                text.as_deref()
                    .and_then(one_char)
                    .and_then(|ch| substitute.font.glyph_for_char(ch))
            })
            .unwrap_or(0);
        glyphs.insert(u16::from(code), gid);
        if let Some(text) = text {
            mapped.push((
                CharCode {
                    code: u32::from(code),
                    len: 1,
                },
                text,
            ));
        }
        let explicit = usize::try_from(i64::from(code) - first)
            .ok()
            .and_then(|i| widths.get(i))
            .and_then(Object::as_f64);
        let standard_width = standard
            .and_then(|font| {
                name.and_then(|name| font.glyph_width(name))
                    .or_else(|| symbolic.then(|| font.code_width(code)))
            })
            .map(f64::from);
        let width = explicit
            .or(standard_width)
            .or(default_width)
            .unwrap_or_else(|| {
                f64::from(substitute.font.advance(gid).unwrap_or(0.0)) * 1000.0
                    / f64::from(substitute.font.units_per_em())
            });
        output_widths.push(Object::Real(width));
    }
    let mut descendant = Dict::new();
    descendant.insert(
        "W",
        Object::Array(vec![Object::Integer(0), Object::Array(output_widths)]),
    );
    let name = embed_program(document, &substitute.font, &glyphs, &mut descendant)?;
    let cmap = CMap::parse(ONE_BYTE_CMAP).map_err(failure)?;
    let info = cmap
        .cid_system_info()
        .ok_or_else(|| failure("generated CMap has no character collection"))?;
    let mut system = Dict::new();
    system.insert("Registry", Object::text(&info.registry));
    system.insert("Ordering", Object::text(&info.ordering));
    system.insert("Supplement", Object::Integer(i64::from(info.supplement)));
    let mut encoding_dict = Dict::new();
    encoding_dict.insert("Type", Object::name("CMap"));
    encoding_dict.insert(
        "CMapName",
        Object::name(
            cmap.name()
                .ok_or_else(|| failure("generated CMap has no name"))?,
        ),
    );
    encoding_dict.insert("CIDSystemInfo", system);
    encoding_dict.insert("WMode", Object::Integer(i64::from(cmap.is_vertical())));
    let mut replacement = Dict::new();
    replacement.insert("Type", Object::name("Font"));
    replacement.insert("Subtype", Object::name("Type0"));
    replacement.insert("BaseFont", Object::name(name));
    replacement.insert(
        "Encoding",
        document.add(Stream::new(encoding_dict, ONE_BYTE_CMAP.to_vec())),
    );
    replacement.insert(
        "DescendantFonts",
        Object::Array(vec![Object::Reference(document.add(descendant))]),
    );
    replacement.insert(
        "ToUnicode",
        document.add(Stream::new(Dict::new(), write_unicode(&mapped, &cmap))),
    );
    *font = replacement;
    Ok(true)
}

fn cmap(document: &Document, value: &Object, depth: usize) -> Result<CMap, GoatError> {
    if depth > MAX_DEPTH {
        return Err(failure("CMap inheritance is too deep"));
    }
    match document.resolve(value).map_err(doc::pdf_error)? {
        Object::Name(name) => {
            CMap::predefined(&String::from_utf8_lossy(name.as_bytes())).map_err(failure)
        }
        Object::Stream(stream) => {
            let decoded = document.decode_stream(&stream).map_err(doc::pdf_error)?;
            let map = CMap::parse(&decoded.data).map_err(failure)?;
            match stream.dict.get(b"UseCMap") {
                Some(base) => Ok(map.with_base(&cmap(document, base, depth + 1)?)),
                None => Ok(map),
            }
        }
        _ => Err(failure("an unembedded Type0 font has no usable CMap")),
    }
}

fn embed_composite(
    document: &mut Document,
    font: &mut Dict,
    cache: &mut HashMap<FontKey, Substitute>,
) -> Result<bool, GoatError> {
    let descendants = font
        .get(b"DescendantFonts")
        .ok_or_else(|| failure("Type0 font has no descendant"))?;
    let descendants = document
        .resolve_array(descendants)
        .map_err(doc::pdf_error)?
        .ok_or_else(|| failure("invalid font descendants"))?;
    let mut descendant = document
        .resolve_dict(
            descendants
                .first()
                .ok_or_else(|| failure("empty font descendants"))?,
        )
        .map_err(doc::pdf_error)?
        .ok_or_else(|| failure("invalid CID font"))?;
    let descriptor = descriptor(document, &descendant)?;
    if is_embedded(document, &descriptor)? {
        return Ok(false);
    }
    let info = document
        .resolve_key(&descendant, b"CIDSystemInfo")
        .map_err(doc::pdf_error)?;
    let ordering = info
        .as_dict()
        .and_then(|dict| dict.get_string(b"Ordering"))
        .map(|name| name.to_text())
        .unwrap_or_default();
    let collection = CidCollection::from_ordering(&ordering);
    let substitute = load_substitute(cache, font, &descriptor, Script::from_ordering(&ordering))?;
    let encoding = cmap(
        document,
        font.get(b"Encoding")
            .ok_or_else(|| failure("Type0 font has no Encoding"))?,
        0,
    )?;
    let unicode = unicode_map(document, font)?;
    if unicode.is_none() && collection.is_none() && !substitute.exact {
        return Err(failure(
            "cannot identify glyphs of an unembedded CID font without ToUnicode or a known character collection",
        ));
    }
    let old_map = match document
        .resolve_key(&descendant, b"CIDToGIDMap")
        .map_err(doc::pdf_error)?
    {
        Object::Stream(stream) => Some(
            document
                .decode_stream(&stream)
                .map_err(doc::pdf_error)?
                .data,
        ),
        _ => None,
    };
    let mut work = 0u64;
    let mut glyphs = BTreeMap::new();
    let mut mapped = Vec::new();
    for range in encoding.codespaces() {
        let n = usize::from(range.len);
        if !(1..=4).contains(&n) {
            return Err(failure("invalid CMap code length"));
        }
        let low = range.low[..n]
            .iter()
            .fold(0u32, |code, byte| code * 256 + u32::from(*byte));
        let high = range.high[..n]
            .iter()
            .fold(0u32, |code, byte| code * 256 + u32::from(*byte));
        work = work.saturating_add(u64::from(high).saturating_sub(u64::from(low)) + 1);
        if work > MAX_CODES {
            return Err(failure("font CMap exceeds the conversion code limit"));
        }
        for code in low..=high {
            let code = CharCode {
                code,
                len: range.len,
            };
            let Some(cid) = encoding.lookup(code) else {
                continue;
            };
            let cid = u16::try_from(cid).map_err(|_| failure("CID exceeds 65535"))?;
            let text = unicode
                .as_ref()
                .and_then(|map| map.lookup(code.code))
                .or_else(|| {
                    collection
                        .and_then(|collection| {
                            collection.cid_to_unicode(u32::from(cid), encoding.is_vertical())
                        })
                        .map(|ch| ch.to_string())
                });
            let gid = text
                .as_deref()
                .and_then(one_char)
                .and_then(|ch| substitute.font.glyph_for_char(ch))
                .or_else(|| {
                    if !substitute.exact {
                        return None;
                    }
                    let gid = match &old_map {
                        Some(map) => {
                            let at = usize::from(cid) * 2;
                            let bytes = map.get(at..at + 2)?;
                            u16::from_be_bytes([bytes[0], bytes[1]])
                        }
                        None => cid,
                    };
                    (gid < substitute.font.num_glyphs()).then_some(gid)
                })
                .unwrap_or(0);
            if (gid != 0 || cid == 0)
                && let Some(previous) = glyphs.insert(cid, gid)
                && previous != gid
            {
                return Err(failure(
                    "two character codes require different glyphs for the same CID",
                ));
            }
            if unicode.is_none()
                && let Some(text) = text
            {
                mapped.push((code, text));
            }
        }
    }
    let name = embed_program(document, &substitute.font, &glyphs, &mut descendant)?;
    // Keep the original CID collection: it belongs to the original encoding CMap.
    if !info.is_null() {
        descendant.insert("CIDSystemInfo", info);
    }
    font.insert("BaseFont", Object::name(name));
    font.insert(
        "DescendantFonts",
        Object::Array(vec![Object::Reference(document.add(descendant))]),
    );
    if unicode.is_none() && !mapped.is_empty() {
        font.insert(
            "ToUnicode",
            document.add(Stream::new(Dict::new(), write_unicode(&mapped, &encoding))),
        );
    }
    Ok(true)
}

/// Visit the reachable resource graph, including forms, patterns, annotation
/// appearances, and AcroForm defaults. Cyclic and shared references are visited once.
fn walk(
    document: &mut Document,
    object: &mut Object,
    cache: &mut HashMap<FontKey, Substitute>,
    seen: &mut HashSet<ObjRef>,
    depth: usize,
) -> Result<bool, GoatError> {
    if depth > MAX_DEPTH {
        return Err(failure("font resource graph is too deep"));
    }
    let mut changed = false;
    match object {
        Object::Reference(id) if seen.insert(*id) => {
            let mut target = document.get(*id).map_err(doc::pdf_error)?;
            if walk(document, &mut target, cache, seen, depth + 1)? {
                document.set(*id, target);
            }
        }
        Object::Dict(dict) => {
            if dict.has_type(b"Font") || dict.contains_key(b"BaseFont") {
                match dict.get_name(b"Subtype") {
                    Some(b"Type0") => return embed_composite(document, dict, cache),
                    Some(b"Type1" | b"TrueType" | b"MMType1") => {
                        return embed_simple(document, dict, cache);
                    }
                    _ => {}
                }
            }
            for (_, value) in dict.iter_mut() {
                changed |= walk(document, value, cache, seen, depth + 1)?;
            }
        }
        Object::Array(values) => {
            for value in values {
                changed |= walk(document, value, cache, seen, depth + 1)?;
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                changed |= walk(document, value, cache, seen, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(changed)
}

pub(crate) fn embed_missing(document: &mut Document) -> Result<(), GoatError> {
    let root = document.catalog_ref().map_err(doc::pdf_error)?;
    walk(
        document,
        &mut Object::Reference(root),
        &mut HashMap::new(),
        &mut HashSet::new(),
        0,
    )?;
    Ok(())
}
