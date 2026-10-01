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

/// `_hit_pattern`: expand presentation ligatures in the pattern, let each run of
/// literal spaces match any whitespace run, then Python
/// `re.compile(pattern, IGNORECASE | MULTILINE)`.
///
/// Page words reach the matcher one per line, so a typed space must also cross a
/// line break: `Acme Corp` finds the two words wherever the page breaks them. For
/// `text --mask` and `compare text --mask` this widens a phrase to match across
/// lines as well. Escaped spaces, spaces in a character class or lookbehind, and
/// patterns that turn on verbose mode inline keep Python's meaning.
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
    let flags = pyre::IGNORECASE | pyre::MULTILINE;
    match spaces_as_whitespace(&expanded) {
        Some(widened) => pyre::compile(&widened, flags)
            // Report a malformed pattern at the positions the caller typed.
            .map_err(|widened_error| {
                pyre::compile(&expanded, flags)
                    .err()
                    .unwrap_or(widened_error)
            }),
        None => pyre::compile(&expanded, flags),
    }
    .map(HitPattern)
}

/// Rewrites each run of unescaped literal spaces outside character classes and
/// lookbehinds to `(?:\s+)`. `None` when the pattern has no such space or turns
/// on verbose mode with an inline flag, where spaces carry no meaning.
fn spaces_as_whitespace(pattern: &str) -> Option<String> {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars().peekable();
    let mut class = false;
    // One entry per open group: whether it sits inside a lookbehind, which
    // must keep a fixed width.
    let mut groups: Vec<bool> = Vec::new();
    let mut rewrote = false;
    while let Some(c) = chars.next() {
        out.push(c);
        match c {
            '\\' => {
                let Some(escaped) = chars.next() else { break };
                out.push(escaped);
                if escaped == 'N' && chars.peek() == Some(&'{') {
                    for named in chars.by_ref() {
                        out.push(named);
                        if named == '}' {
                            break;
                        }
                    }
                }
            }
            '[' if !class => {
                class = true;
                if let Some(negate) = chars.next_if_eq(&'^') {
                    out.push(negate);
                }
                if let Some(bracket) = chars.next_if_eq(&']') {
                    out.push(bracket);
                }
            }
            ']' if class => class = false,
            _ if class => {}
            ' ' if !groups.last().copied().unwrap_or(false) => {
                while chars.next_if_eq(&' ').is_some() {}
                out.pop();
                out.push_str(r"(?:\s+)");
                rewrote = true;
            }
            ')' => {
                groups.pop();
            }
            '(' => {
                let outer = groups.last().copied().unwrap_or(false);
                if chars.next_if_eq(&'?').is_none() {
                    groups.push(outer);
                    continue;
                }
                out.push('?');
                match chars.peek() {
                    Some('#') => {
                        while let Some(comment) = chars.next() {
                            out.push(comment);
                            if comment == '\\' {
                                out.extend(chars.next());
                            } else if comment == ')' {
                                break;
                            }
                        }
                    }
                    Some('<') => {
                        out.push('<');
                        chars.next();
                        let behind = matches!(chars.peek(), Some('=' | '!'));
                        groups.push(outer || behind);
                    }
                    _ => {
                        let mut enabling = true;
                        while let Some(&flag) = chars.peek() {
                            match flag {
                                'x' if enabling => return None,
                                '-' => enabling = false,
                                'a' | 'i' | 'L' | 'm' | 's' | 'u' | 'x' => {}
                                _ => break,
                            }
                            out.push(flag);
                            chars.next();
                        }
                        if chars.next_if_eq(&')').is_some() {
                            out.push(')');
                        } else {
                            groups.push(outer);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    rewrote.then_some(out)
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
