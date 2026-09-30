//! Python-compatible text: what CPython 3.12 prints for `json.dumps`, `repr`, and `str`, and
//! what it accepts in `int()` and `float()`.
//!
//! One gap remains in [`repr_str`]: code points Unicode leaves unassigned print as themselves,
//! where Python escapes them.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use serde_json::{Number, Value};

use crate::error::GoatError;

/// `json.dumps(value, indent=2)`: ASCII-only, two-space indent, `","` between items.
pub fn json_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, Some(0));
    out
}

/// `json.dumps(value)`: ASCII-only, one line, `", "` and `": "` separators.
pub fn json_compact(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, None);
    out
}

fn write_json(out: &mut String, value: &Value, level: Option<usize>) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => write_json_string(out, text),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                separate(out, position, level);
                write_json(out, item, level.map(|depth| depth + 1));
            }
            close(out, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (position, (key, item)) in map.iter().enumerate() {
                separate(out, position, level);
                write_json_string(out, key);
                out.push_str(": ");
                write_json(out, item, level.map(|depth| depth + 1));
            }
            close(out, level);
            out.push('}');
        }
    }
}

fn separate(out: &mut String, position: usize, level: Option<usize>) {
    if position > 0 {
        out.push(',');
        if level.is_none() {
            out.push(' ');
        }
    }
    if let Some(depth) = level {
        indent(out, depth + 1);
    }
}

fn close(out: &mut String, level: Option<usize>) {
    if let Some(depth) = level {
        indent(out, depth);
    }
}

fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_number(out: &mut String, number: &Number) {
    if let Some(value) = number.as_i64() {
        push_display(out, value);
    } else if let Some(value) = number.as_u64() {
        push_display(out, value);
    } else if let Some(value) = number.as_f64() {
        out.push_str(&float_repr(value));
    }
}

fn push_display(out: &mut String, value: impl fmt::Display) {
    // Formatting into a `String` cannot fail.
    let _ = write!(out, "{value}");
}

fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0_u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    push_display(out, format_args!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python `repr(float)`: shortest round-trip digits, `72.0`, `1e-05`, `1e+16`, `inf`, `nan`.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    // `{:e}` prints the same shortest digits as Python, only laid out differently.
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let Ok(exponent) = exponent.parse::<i64>() else {
        return scientific;
    };
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|ch| *ch != '.').collect();
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    // Python switches to exponent form when the decimal point would sit more than four
    // places left of the digits or more than sixteen places right of their start.
    let point = exponent + 1;
    if !(-3..=16).contains(&point) {
        let (first, rest) = digits.split_at(1);
        out.push_str(first);
        if !rest.is_empty() {
            out.push('.');
            out.push_str(rest);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        push_display(&mut out, format_args!("{:02}", exponent.unsigned_abs()));
    } else if point <= 0 {
        out.push_str("0.");
        for _ in 0..point.unsigned_abs() {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let point = point.unsigned_abs() as usize;
        if point >= digits.len() {
            out.push_str(&digits);
            for _ in digits.len()..point {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            let (whole, fraction) = digits.split_at(point);
            out.push_str(whole);
            out.push('.');
            out.push_str(fraction);
        }
    }
    out
}

/// Python `repr(str)`.
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            '\0'..='\u{1f}' | '\u{7f}' => {
                push_display(&mut out, format_args!("\\x{:02x}", u32::from(ch)))
            }
            _ if ch.is_ascii() || is_printable(ch) => out.push(ch),
            _ => {
                let code = u32::from(ch);
                if code <= 0xff {
                    push_display(&mut out, format_args!("\\x{code:02x}"));
                } else if code <= 0xffff {
                    push_display(&mut out, format_args!("\\u{code:04x}"));
                } else {
                    push_display(&mut out, format_args!("\\U{code:08x}"));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// Python `repr(bytes)`: `b'...'` with `\xNN` for bytes outside printable ASCII.
pub fn repr_bytes(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::with_capacity(bytes.len() + 3);
    out.push('b');
    out.push(char::from(quote));
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if byte == quote => {
                out.push('\\');
                out.push(char::from(byte));
            }
            b' '..=b'~' => out.push(char::from(byte)),
            _ => push_display(&mut out, format_args!("\\x{byte:02x}")),
        }
    }
    out.push(char::from(quote));
    out
}

/// Python `str()` of a JSON value: `None`, `True`, `72.0`, text as is, and element reprs
/// inside lists and dicts.
pub fn str_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => repr_value(value),
    }
}

/// Python `repr()` of a JSON value.
pub fn repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => {
            let mut out = String::new();
            write_number(&mut out, number);
            out
        }
        Value::String(text) => repr_str(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(repr_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, item)| format!("{}: {}", repr_str(key), repr_value(item)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python `str.isspace()` for one character: Unicode white space plus the four ASCII
/// information separators.
pub fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python `str.strip()`.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Python `str.rstrip()`.
pub fn rstrip(text: &str) -> &str {
    text.trim_end_matches(is_space)
}

/// A Python `int`: an `i64` when it fits, else the canonical decimal text of the huge value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PyInt {
    Small(i64),
    Huge(String),
}

impl PyInt {
    /// The value, when it fits in an `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Small(value) => Some(*value),
            Self::Huge(_) => None,
        }
    }

    /// Whether the value is below zero.
    pub fn is_negative(&self) -> bool {
        match self {
            Self::Small(value) => *value < 0,
            Self::Huge(text) => text.starts_with('-'),
        }
    }
}

impl fmt::Display for PyInt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Small(value) => write!(formatter, "{value}"),
            Self::Huge(text) => formatter.write_str(text),
        }
    }
}

/// Python `int(text)`: surrounding white space, a sign, Unicode decimal digits, and single
/// underscores between digits. Fails with Python's `ValueError`.
pub fn parse_int(text: &str) -> Result<PyInt, GoatError> {
    let invalid = || {
        GoatError::value_error(format!(
            "invalid literal for int() with base 10: {}",
            repr_str(text)
        ))
    };
    let (negative, digits) = int_digits(text, 10).ok_or_else(invalid)?;
    let signed = if negative {
        format!("-{digits}")
    } else {
        digits
    };
    Ok(match signed.parse::<i64>() {
        Ok(value) => PyInt::Small(value),
        Err(_) => PyInt::Huge(signed),
    })
}

/// Python `int(text, 16)` when the value fits an `i64`. Fails with Python's `ValueError`.
pub(crate) fn parse_hex(text: &str) -> Result<i64, GoatError> {
    let invalid = || {
        GoatError::value_error(format!(
            "invalid literal for int() with base 16: {}",
            repr_str(text)
        ))
    };
    let (negative, digits) = int_digits(text, 16).ok_or_else(invalid)?;
    let magnitude = i64::from_str_radix(&digits, 16).map_err(|_| invalid())?;
    Ok(if negative { -magnitude } else { magnitude })
}

/// The sign and canonical digits (no underscores, no leading zeros) of an `int()` literal.
fn int_digits(text: &str, radix: u32) -> Option<(bool, String)> {
    let ascii = ascii_digits_and_spaces(text)?;
    let literal = ascii.trim_matches(' ');
    let (negative, mut body) = match literal.as_bytes().first() {
        Some(b'-') => (true, &literal[1..]),
        Some(b'+') => (false, &literal[1..]),
        _ => (false, literal),
    };
    if radix == 16
        && let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X"))
    {
        body = rest.strip_prefix('_').unwrap_or(rest);
    }
    let mut digits = String::with_capacity(body.len());
    let mut previous_digit = false;
    let mut pending_underscore = false;
    for ch in body.chars() {
        if ch == '_' {
            if !previous_digit {
                return None;
            }
            previous_digit = false;
            pending_underscore = true;
        } else if ch.is_digit(radix) {
            digits.push(ch.to_ascii_lowercase());
            previous_digit = true;
            pending_underscore = false;
        } else {
            return None;
        }
    }
    if digits.is_empty() || pending_underscore {
        return None;
    }
    let canonical = digits.trim_start_matches('0');
    let canonical = if canonical.is_empty() { "0" } else { canonical };
    Some((negative && canonical != "0", canonical.to_owned()))
}

/// Python `float(text)`. Fails with Python's `ValueError`.
pub fn parse_float(text: &str) -> Result<f64, GoatError> {
    let invalid = || {
        GoatError::value_error(format!(
            "could not convert string to float: {}",
            repr_str(text)
        ))
    };
    let ascii = ascii_digits_and_spaces(text).ok_or_else(invalid)?;
    let literal = ascii.trim_matches(' ');
    let literal = if literal.contains('_') {
        Cow::Owned(remove_underscores(literal).ok_or_else(invalid)?)
    } else {
        Cow::Borrowed(literal)
    };
    // Rust's grammar matches `PyOS_string_to_double`: optional sign, decimal digits with
    // optional point and exponent, or `inf`, `infinity`, and `nan` in any case.
    literal.parse::<f64>().map_err(|_| invalid())
}

/// CPython's `_Py_string_to_number_with_underscores`: each underscore sits between digits.
fn remove_underscores(literal: &str) -> Option<String> {
    let mut out = String::with_capacity(literal.len());
    let mut previous = '\0';
    for ch in literal.chars() {
        if ch == '_' {
            if !previous.is_ascii_digit() {
                return None;
            }
        } else {
            if previous == '_' && !ch.is_ascii_digit() {
                return None;
            }
            out.push(ch);
        }
        previous = ch;
    }
    (previous != '_').then_some(out)
}

/// CPython's `_PyUnicode_TransformDecimalAndSpaceToASCII`: white space becomes `' '`, Unicode
/// decimal digits become ASCII, and any other non-ASCII character makes the literal invalid.
fn ascii_digits_and_spaces(text: &str) -> Option<String> {
    text.chars()
        .map(|ch| {
            if is_space(ch) {
                Some(' ')
            } else if ch.is_ascii() {
                Some(ch)
            } else {
                decimal_value(ch).map(|digit| char::from(b'0' + digit))
            }
        })
        .collect()
}

/// The first code point of every Unicode 15.0 decimal-digit run (`Nd`); each run holds ten
/// digits, zero through nine, in order.
const DECIMAL_ZEROS: [u32; 68] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
    0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80,
    0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
    0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0,
    0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60,
    0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0,
    0x1E950, 0x1FBF0,
];

fn decimal_value(ch: char) -> Option<u8> {
    let code = u32::from(ch);
    let run = DECIMAL_ZEROS.partition_point(|zero| *zero <= code);
    let zero = DECIMAL_ZEROS.get(run.checked_sub(1)?)?;
    u8::try_from(code - zero).ok().filter(|digit| *digit < 10)
}

/// Non-ASCII code points Python's `str.isprintable()` rejects among assigned characters:
/// categories Cc, Cf, Co, Zs, Zl, and Zp in Unicode 15.0.
const NON_PRINTABLE: [(u32, u32); 27] = [
    (0x0080, 0x00A0),
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x1680, 0x1680),
    (0x180E, 0x180E),
    (0x2000, 0x200F),
    (0x2028, 0x202F),
    (0x205F, 0x2064),
    (0x2066, 0x206F),
    (0x3000, 0x3000),
    (0xE000, 0xF8FF),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xF0000, 0xFFFFD),
    (0x100000, 0x10FFFD),
];

fn is_printable(ch: char) -> bool {
    let code = u32::from(ch);
    let run = NON_PRINTABLE.partition_point(|(start, _)| *start <= code);
    match run
        .checked_sub(1)
        .and_then(|index| NON_PRINTABLE.get(index))
    {
        Some(&(start, end)) => !(start..=end).contains(&code),
        None => true,
    }
}
