//! Result helpers shared by verbs: byte sizes for people and masked text.

use serde_json::Value;

/// `human_size(num)`: `1023B`, `1.5KB`, and so on up to `TB`, one decimal past bytes.
pub fn human_size(bytes: i64) -> String {
    if bytes.unsigned_abs() < 1024 {
        return format!("{bytes}B");
    }
    let mut size = bytes as f64 / 1024.0;
    for unit in ["KB", "MB", "GB"] {
        if size.abs() < 1024.0 {
            return format!("{size:.1}{unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1}TB")
}

/// What masked text reads in place of each match.
pub const MASK: &str = "[REDACTED]";

/// `_mask_text(value, pattern)`: every string inside `value`, through lists and object
/// values, passed through `mask`; object keys and other values stay as they are.
///
/// `mask` replaces each match of the caller's pattern with [`MASK`].
pub fn mask_strings(value: Value, mask: &impl Fn(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(mask(&text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| mask_strings(item, mask))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, mask_strings(item, mask)))
                .collect(),
        ),
        other => other,
    }
}
