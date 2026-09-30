use goat_common::{GoatError, textcache::WordColumns};
use serde_json::Value;

use crate::{Rect, pyre};

#[derive(Clone, Debug)]
pub struct HitPattern(pyre::Pattern);

impl HitPattern {
    pub fn is_match(&self, text: &str) -> Result<bool, GoatError> {
        self.0.is_match(text)
    }
    pub fn replace_all(&self, text: &str, replacement: &str) -> Result<String, GoatError> {
        self.0.sub_literal(text, replacement)
    }
}

/// `_hit_pattern`: expand presentation ligatures in the pattern, then
/// Python `re.compile(pattern, IGNORECASE | MULTILINE)`.
pub fn hit_pattern(pattern: &str) -> Result<HitPattern, GoatError> {
    let mut expanded = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '\u{fb00}' => expanded.push_str("ff"),
            '\u{fb01}' => expanded.push_str("fi"),
            '\u{fb02}' => expanded.push_str("fl"),
            '\u{fb03}' => expanded.push_str("ffi"),
            '\u{fb04}' => expanded.push_str("ffl"),
            '\u{fb05}' | '\u{fb06}' => expanded.push_str("st"),
            _ => expanded.push(c),
        }
    }
    pyre::compile(&expanded, pyre::IGNORECASE | pyre::MULTILINE).map(HitPattern)
}

/// `_page_hits`: matching word spans, coalesced once per source line.
pub fn page_hits(words: &WordColumns, pattern: &HitPattern) -> Result<Vec<Rect>, GoatError> {
    if words.text.is_empty() {
        return Ok(Vec::new());
    }
    let mut starts = vec![0];
    for (i, c) in words.text.chars().enumerate() {
        if c == '\n' {
            starts.push(i + 1);
        }
    }
    let length = words.text.chars().count();
    let count = starts
        .len()
        .min(words.rects.len() / 4)
        .min(words.lines.len() / 2);
    let mut prior = None;
    let mut out = Vec::new();
    for found in pattern.0.finditer(&words.text)? {
        let mut first = starts
            .partition_point(|&start| start <= found.start)
            .saturating_sub(1);
        let last = if found.start == found.end {
            first + 1
        } else {
            let word_end = starts.get(first + 1).map_or(length, |next| next - 1);
            if found.start == word_end {
                first += 1;
            }
            starts.partition_point(|&start| start < found.end)
        };
        if prior == Some((first, last)) {
            continue;
        }
        prior = Some((first, last));
        let mut line = None;
        let mut bbox: Option<Rect> = None;
        for i in first..last.min(count) {
            let key = (words.lines[i * 2], words.lines[i * 2 + 1]);
            let r = &words.rects[i * 4..i * 4 + 4];
            let rect = Rect::new(r[0], r[1], r[2], r[3]);
            if line != Some(key) {
                if let Some(rect) = bbox.take() {
                    out.push(rect);
                }
                line = Some(key);
            }
            bbox = Some(bbox.map_or(rect, |prior| union(prior, rect)));
        }
        if let Some(rect) = bbox {
            out.push(rect);
        }
    }
    Ok(out)
}

pub fn mask_text(value: Value, pattern: Option<&HitPattern>) -> Result<Value, GoatError> {
    let Some(pattern) = pattern else {
        return Ok(value);
    };
    match value {
        Value::String(text) => Ok(Value::String(pattern.replace_all(&text, "[REDACTED]")?)),
        Value::Array(values) => values
            .into_iter()
            .map(|v| mask_text(v, Some(pattern)))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(k, v)| Ok((k, mask_text(v, Some(pattern))?)))
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(Value::Object),
        other => Ok(other),
    }
}

pub(crate) fn union(a: Rect, b: Rect) -> Rect {
    Rect::new(
        a.x0.min(b.x0),
        a.y0.min(b.y0),
        a.x1.max(b.x1),
        a.y1.max(b.y1),
    )
}
pub(crate) fn rounded(rect: Rect) -> [f64; 4] {
    [rect.x0, rect.y0, rect.x1, rect.y1].map(|v| round(v, 1))
}
pub(crate) fn round(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(value)
}
