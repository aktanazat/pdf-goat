//! Glyph name to Unicode, following the Adobe Glyph List specification as pdfminer applies it.

use crate::data::agl::AGL;
use crate::data::encodings::DINGBATS_UNICODE;

/// The Adobe Glyph List entry for `name`, exactly as listed (no suffix or `uniXXXX` handling).
pub fn agl_lookup(name: &str) -> Option<&'static str> {
    AGL.binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| AGL[i].1)
}

/// Unicode text for a glyph name.
///
/// The part after the first `.` is dropped; `_` joins ligature components, each of which must
/// map. A component maps through the Adobe Glyph List, then `uniXXXX[XXXX...]` (groups of four
/// hex digits, no surrogates), then `uXXXX` to `uXXXXXX` (four to six hex digits). Returns
/// `None` when any component has no mapping.
pub fn glyph_name_to_unicode(name: &str) -> Option<String> {
    let base = name.split('.').next().unwrap_or("");
    if base.is_empty() {
        return None;
    }
    let mut out = String::new();
    for component in base.split('_') {
        component_to_unicode(component, &mut out)?;
    }
    Some(out)
}

/// Unicode for a ZapfDingbats glyph name (`a1` to `a191`, `space`).
pub fn dingbats_name_to_unicode(name: &str) -> Option<char> {
    if name == "space" {
        return Some(' ');
    }
    DINGBATS_UNICODE
        .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| DINGBATS_UNICODE[i].1)
}

fn component_to_unicode(component: &str, out: &mut String) -> Option<()> {
    if component.is_empty() {
        return None;
    }
    if let Some(text) = agl_lookup(component) {
        out.push_str(text);
        return Some(());
    }
    if let Some(hex) = component.strip_prefix("uni")
        && !hex.is_empty()
        && hex.len().is_multiple_of(4)
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let start = out.len();
        for chunk in hex.as_bytes().chunks(4) {
            let value = parse_hex(chunk)?;
            match char::from_u32(value) {
                Some(ch) => out.push(ch),
                None => {
                    out.truncate(start);
                    return None;
                }
            }
        }
        return Some(());
    }
    if let Some(hex) = component.strip_prefix('u')
        && (4..=6).contains(&hex.len())
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let ch = char::from_u32(parse_hex(hex.as_bytes())?)?;
        out.push(ch);
        return Some(());
    }
    None
}

fn parse_hex(digits: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(digits).ok()?;
    u32::from_str_radix(text, 16).ok()
}
