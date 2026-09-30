//! PDF syntax output: the compact form the writer uses and the indented form
//! MuPDF's `xref_object` prints.

use crate::lexer::{is_delimiter, is_regular};
use crate::object::{Dict, ObjRef, Object, PdfString, StringFormat};

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Append `bytes` as a token, with a space first when it would otherwise run
/// into the previous token.
pub(crate) fn write_token(out: &mut Vec<u8>, bytes: &[u8]) {
    if let (Some(&last), Some(&first)) = (out.last(), bytes.first())
        && is_regular(last)
        && is_regular(first)
    {
        out.push(b' ');
    }
    out.extend_from_slice(bytes);
}

/// `/Name` with `#xx` escapes for bytes outside `!`..`~`, `#`, and delimiters.
pub(crate) fn write_name(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'/');
    for &byte in bytes {
        if !(0x21..=0x7E).contains(&byte) || byte == b'#' || is_delimiter(byte) {
            out.push(b'#');
            out.push(HEX_UPPER[usize::from(byte >> 4)]);
            out.push(HEX_UPPER[usize::from(byte & 15)]);
        } else {
            out.push(byte);
        }
    }
}

pub(crate) fn write_string(out: &mut Vec<u8>, string: &PdfString) {
    match string.format {
        StringFormat::Hex => {
            out.push(b'<');
            for &byte in &string.bytes {
                out.push(HEX_UPPER[usize::from(byte >> 4)]);
                out.push(HEX_UPPER[usize::from(byte & 15)]);
            }
            out.push(b'>');
        }
        StringFormat::Literal => {
            out.push(b'(');
            for &byte in &string.bytes {
                match byte {
                    b'(' | b')' | b'\\' => {
                        out.push(b'\\');
                        out.push(byte);
                    }
                    // A bare carriage return would be read back as a line feed.
                    b'\r' => out.extend_from_slice(b"\\r"),
                    _ => out.push(byte),
                }
            }
            out.push(b')');
        }
    }
}

/// A real in plain decimal notation (PDF has no exponents). Integral values
/// keep a `.0` so they read back as reals.
pub(crate) fn format_real(value: f64) -> String {
    if !value.is_finite() || value == 0.0 {
        return "0.0".to_string();
    }
    let shortest = format!("{value:?}");
    if !shortest.contains('e') {
        return shortest;
    }
    let fixed = if value.abs() >= 1.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.20}")
    };
    let trimmed = fixed.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

/// How references are renumbered while writing; `None` writes `null`.
pub(crate) type RefMap<'a> = dyn Fn(ObjRef) -> Option<ObjRef> + 'a;

/// The [`RefMap`] that writes every reference unchanged.
pub(crate) fn keep_ref(id: ObjRef) -> Option<ObjRef> {
    Some(id)
}

/// Compact PDF syntax for any object. A stream is written with its true
/// `/Length`, then `stream`, the raw data, and `endstream`.
pub(crate) fn write_object(out: &mut Vec<u8>, object: &Object) {
    write_object_mapped(out, object, &keep_ref);
}

/// [`write_object`] with every reference passed through `map`.
pub(crate) fn write_object_mapped(out: &mut Vec<u8>, object: &Object, map: &RefMap<'_>) {
    match object {
        Object::Null => write_token(out, b"null"),
        Object::Bool(true) => write_token(out, b"true"),
        Object::Bool(false) => write_token(out, b"false"),
        Object::Integer(value) => write_token(out, value.to_string().as_bytes()),
        Object::Real(value) => write_token(out, format_real(*value).as_bytes()),
        Object::String(string) => write_string(out, string),
        Object::Name(name) => write_name(out, name.as_bytes()),
        Object::Array(items) => {
            out.push(b'[');
            for item in items {
                write_object_mapped(out, item, map);
            }
            out.push(b']');
        }
        Object::Dict(dict) => write_dict(out, dict, None, map),
        Object::Stream(stream) => {
            write_dict(out, &stream.dict, Some(stream.raw().len()), map);
            out.extend_from_slice(b"\nstream\n");
            out.extend_from_slice(stream.raw());
            out.extend_from_slice(b"\nendstream");
        }
        Object::Reference(id) => match map(*id) {
            Some(id) => write_token(out, format!("{} {} R", id.num, id.generation).as_bytes()),
            None => write_token(out, b"null"),
        },
    }
}

/// A dictionary with references passed through `map`; with `length`,
/// `/Length` is written with that value in the key's place (or last when
/// absent).
pub(crate) fn write_dict(out: &mut Vec<u8>, dict: &Dict, length: Option<usize>, map: &RefMap<'_>) {
    out.extend_from_slice(b"<<");
    let mut wrote_length = false;
    for (key, value) in dict.iter() {
        write_name(out, key.as_bytes());
        match length {
            Some(len) if key == "Length" => {
                write_token(out, len.to_string().as_bytes());
                wrote_length = true;
            }
            _ => write_object_mapped(out, value, map),
        }
    }
    if let (Some(len), false) = (length, wrote_length) {
        write_name(out, b"Length");
        write_token(out, len.to_string().as_bytes());
    }
    out.extend_from_slice(b">>");
}

/// MuPDF's indented layout (`pdf_print_obj` untight): one dictionary entry per
/// line indented two spaces per level, arrays as `[ a b ]` wrapped past
/// column 60, a stream as its dictionary.
pub(crate) fn pretty(object: &Object) -> String {
    let mut fmt = Pretty {
        out: String::new(),
        indent: 0,
        col: 0,
    };
    fmt.object(object);
    fmt.out
}

struct Pretty {
    out: String,
    indent: usize,
    col: usize,
}

impl Pretty {
    fn put(&mut self, text: &str) {
        for c in text.chars() {
            self.out.push(c);
            if c == '\n' {
                self.col = 0;
            } else {
                self.col += 1;
            }
        }
    }

    fn put_indent(&mut self) {
        for _ in 0..self.indent {
            self.put("  ");
        }
    }

    fn object(&mut self, object: &Object) {
        match object {
            Object::Null => self.put("null"),
            Object::Bool(value) => self.put(if *value { "true" } else { "false" }),
            Object::Integer(value) => self.put(&value.to_string()),
            Object::Real(value) => {
                let text = if value.is_finite() {
                    format!("{}", *value as f32)
                } else {
                    "0".to_string()
                };
                self.put(&text);
            }
            Object::String(string) => self.string(&string.bytes),
            Object::Name(name) => {
                let mut bytes = Vec::new();
                write_name(&mut bytes, name.as_bytes());
                self.put(&String::from_utf8_lossy(&bytes));
            }
            Object::Array(items) => {
                self.put("[");
                self.indent += 1;
                for item in items {
                    if self.col > 60 {
                        self.put("\n");
                        self.put_indent();
                    } else {
                        self.put(" ");
                    }
                    self.object(item);
                }
                self.indent -= 1;
                self.put(" ]");
            }
            Object::Dict(dict) => self.dict(dict),
            Object::Stream(stream) => self.dict(&stream.dict),
            Object::Reference(id) => self.put(&format!("{} {} R", id.num, id.generation)),
        }
    }

    fn dict(&mut self, dict: &Dict) {
        self.put("<<\n");
        self.indent += 1;
        for (key, value) in dict.iter() {
            self.put_indent();
            self.object(&Object::Name(key.clone()));
            self.put(" ");
            let nested_array = matches!(value, Object::Array(_));
            if nested_array {
                self.indent += 1;
            }
            self.object(value);
            self.put("\n");
            if nested_array {
                self.indent -= 1;
            }
        }
        self.indent -= 1;
        self.put_indent();
        self.put(">>");
    }

    fn string(&mut self, bytes: &[u8]) {
        let bom = bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]);
        let escaped_len: usize = bytes
            .iter()
            .map(|&b| match b {
                b'\n' | b'\r' | b'\t' | 8 | 12 | b'(' | b')' | b'\\' => 2,
                0..32 | 127.. => 4,
                _ => 1,
            })
            .sum();
        if bom || escaped_len > bytes.len() * 2 {
            let mut text = String::with_capacity(bytes.len() * 2 + 2);
            text.push('<');
            for &b in bytes {
                text.push(char::from(HEX_UPPER[usize::from(b >> 4)]));
                text.push(char::from(HEX_UPPER[usize::from(b & 15)]));
            }
            text.push('>');
            self.put(&text);
            return;
        }
        let mut text = String::with_capacity(escaped_len + 2);
        text.push('(');
        for &b in bytes {
            match b {
                b'\n' => text.push_str("\\n"),
                b'\r' => text.push_str("\\r"),
                b'\t' => text.push_str("\\t"),
                8 => text.push_str("\\b"),
                12 => text.push_str("\\f"),
                b'(' => text.push_str("\\("),
                b')' => text.push_str("\\)"),
                b'\\' => text.push_str("\\\\"),
                0..32 | 127.. => text.push_str(&format!("\\{b:03o}")),
                _ => text.push(char::from(b)),
            }
        }
        text.push(')');
        self.put(&text);
    }
}
