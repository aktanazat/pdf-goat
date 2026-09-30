//! Destinations resolved to pages the way MuPDF resolves outline and link targets.

use std::collections::HashSet;

use goat_common::GoatError;
use pdf_core::{Dict, Document, Object};

use crate::open::pdf_error;

/// Deepest name tree followed.
const MAX_TREE_DEPTH: usize = 32;

/// Where an outline item or link leads, as MuPDF classifies the URI it builds for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// MuPDF builds no URI: the item or link leads nowhere.
    Nothing,
    /// A page of this document, zero-based; `-1` when MuPDF cannot find it.
    Page(i64),
    /// A web address, a launched file, or another document.
    External,
}

pub(crate) fn to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `pdf_lookup_page_number`: a page reference's index, `-1` for anything else.
pub(crate) fn page_number(doc: &Document, object: Option<&Object>) -> Result<i64, GoatError> {
    let Some(Object::Reference(id)) = object else {
        return Ok(-1);
    };
    Ok(doc.page_index(*id).map_err(pdf_error)?.map_or(-1, to_i64))
}

/// An explicit destination `[page /Type ...]`: a page reference gives its index (`-1`
/// outside the page tree), an integer is already an index, and anything else is page 0.
/// An empty array leads nowhere.
fn explicit(doc: &Document, items: &[Object]) -> Result<Target, GoatError> {
    Ok(match items.first() {
        None => Target::Nothing,
        Some(Object::Integer(page)) => Target::Page(*page),
        Some(first @ Object::Reference(_)) => Target::Page(page_number(doc, Some(first))?),
        Some(_) => Target::Page(0),
    })
}

/// A `/Dest` value or a GoTo action's `/D`: an explicit array, or a name or string that
/// names one.
pub(crate) fn dest_target(doc: &Document, dest: &Object) -> Result<Target, GoatError> {
    match doc.resolve(dest).map_err(pdf_error)? {
        Object::Array(items) => explicit(doc, &items),
        Object::Name(name) => named(doc, &String::from_utf8_lossy(name.as_bytes())),
        Object::String(text) => named(doc, &text.to_text()),
        _ => Ok(Target::Nothing),
    }
}

/// A named destination: the page of the explicit destination it names, or `-1` when the
/// name is undefined or names something else.
fn named(doc: &Document, name: &str) -> Result<Target, GoatError> {
    let found = match lookup_dest(doc, name)? {
        Some(Object::Dict(dict)) => doc.resolve_key(&dict, b"D").map_err(pdf_error)?,
        Some(other) => other,
        None => Object::Null,
    };
    let page = match found {
        Object::Array(items) => match explicit(doc, &items)? {
            Target::Page(page) => page,
            Target::Nothing | Target::External => -1,
        },
        _ => -1,
    };
    Ok(Target::Page(page))
}

/// An `/A` action. GoTo follows its `/D`; Named moves among the pages (`current` is the
/// page holding a link, `None` for an outline item); URI, GoToR, and a Launch with a
/// file leave the document; anything else leads nowhere.
pub(crate) fn action_target(
    doc: &Document,
    action: &Object,
    current: Option<usize>,
) -> Result<Target, GoatError> {
    let Some(action) = doc.resolve_dict(action).map_err(pdf_error)? else {
        return Ok(Target::Nothing);
    };
    let kind = doc.resolve_key(&action, b"S").map_err(pdf_error)?;
    Ok(match kind.as_name().unwrap_or_default() {
        b"GoTo" => match action.get(b"D") {
            Some(dest) => dest_target(doc, dest)?,
            None => Target::Nothing,
        },
        b"Named" => named_action(doc, &action, current)?,
        b"URI" | b"GoToR" => Target::External,
        b"Launch" if action.contains_key(b"F") => Target::External,
        _ => Target::Nothing,
    })
}

/// `FirstPage`, `LastPage`, and, on a page, `PrevPage` and `NextPage` clamped to the
/// document.
fn named_action(
    doc: &Document,
    action: &Dict,
    current: Option<usize>,
) -> Result<Target, GoatError> {
    let count = to_i64(doc.page_count().map_err(pdf_error)?);
    let name = doc.resolve_key(action, b"N").map_err(pdf_error)?;
    let page = match (name.as_name().unwrap_or_default(), current) {
        (b"FirstPage", _) => 0,
        (b"LastPage", _) => count - 1,
        (b"PrevPage", Some(page)) => (to_i64(page) - 1).max(0),
        (b"NextPage", Some(page)) => (to_i64(page) + 1).min(count - 1),
        _ => return Ok(Target::Nothing),
    };
    Ok(Target::Page(page))
}

/// `fz_resolve_link` on a destination string read as a link URI: `#page=N` and
/// `#nameddest=` lead into this document, anything else to page `-1`.
pub(crate) fn uri_page(doc: &Document, uri: &str) -> Result<i64, GoatError> {
    let Some(fragment) = uri.strip_prefix('#') else {
        return Ok(-1);
    };
    for part in fragment.split('&') {
        if let Some(number) = part.strip_prefix("page=") {
            return Ok(atoi(number) - 1);
        }
        if let Some(name) = part.strip_prefix("nameddest=") {
            return Ok(match named(doc, &percent_decode(name))? {
                Target::Page(page) => page,
                Target::Nothing | Target::External => -1,
            });
        }
    }
    Ok(-1)
}

/// C `atoi`: an optional sign and the leading digits; 0 when there are none.
fn atoi(text: &str) -> i64 {
    let text = text.trim_start();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let value = digits[..end].parse::<i64>().unwrap_or(0);
    if negative { -value } else { value }
}

/// `fz_decode_uri_component`: `%XX` escapes become bytes; anything else stays.
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = bytes
            .get(index + 1..index + 3)
            .filter(|_| bytes[index] == b'%')
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `pdf_lookup_dest`: the catalog `/Dests` dictionary when it exists, otherwise the
/// `/Names/Dests` name tree. The needle is text, as MuPDF decodes it from a link URI.
pub(crate) fn lookup_dest(doc: &Document, needle: &str) -> Result<Option<Object>, GoatError> {
    let catalog = doc.catalog().map_err(pdf_error)?;
    if let Some(dests) = catalog.get(b"Dests") {
        let Some(dests) = doc.resolve_dict(dests).map_err(pdf_error)? else {
            return Ok(None);
        };
        return match dests.get(&text_string_bytes(needle)) {
            Some(value) => Ok(Some(doc.resolve(value).map_err(pdf_error)?)),
            None => Ok(None),
        };
    }
    let Some(names) = catalog.get(b"Names") else {
        return Ok(None);
    };
    let Some(names) = doc.resolve_dict(names).map_err(pdf_error)? else {
        return Ok(None);
    };
    let Some(tree) = names.get(b"Dests") else {
        return Ok(None);
    };
    find_name(doc, tree, needle, 0, &mut HashSet::new())
}

/// `pdf_new_text_string`: ASCII as is, anything else UTF-16BE behind a byte order mark.
fn text_string_bytes(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        return text.as_bytes().to_vec();
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    bytes
}

/// A name tree searched depth first for a key whose text is `needle`.
fn find_name(
    doc: &Document,
    node: &Object,
    needle: &str,
    depth: usize,
    seen: &mut HashSet<u32>,
) -> Result<Option<Object>, GoatError> {
    if depth > MAX_TREE_DEPTH {
        return Ok(None);
    }
    if let Object::Reference(id) = node
        && !seen.insert(id.num)
    {
        return Ok(None);
    }
    let Some(dict) = doc.resolve_dict(node).map_err(pdf_error)? else {
        return Ok(None);
    };
    if let Some(names) = dict.get(b"Names")
        && let Some(pairs) = doc.resolve_array(names).map_err(pdf_error)?
    {
        for pair in pairs.as_chunks::<2>().0 {
            let key = doc.resolve(&pair[0]).map_err(pdf_error)?;
            if key.as_string().is_some_and(|key| key.to_text() == needle) {
                return Ok(Some(doc.resolve(&pair[1]).map_err(pdf_error)?));
            }
        }
    }
    if let Some(kids) = dict.get(b"Kids")
        && let Some(kids) = doc.resolve_array(kids).map_err(pdf_error)?
    {
        for kid in &kids {
            if let Some(found) = find_name(doc, kid, needle, depth + 1, seen)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}
