//! Content streams (ISO 32000-2 8.2): operations as operands and an
//! operator, inline images, and writing operations back out.

use crate::error::{Error, Result};
use crate::lexer::{Token, find, is_delimiter, is_whitespace};
use crate::object::{Dict, Object, Stream};
use crate::parser::Parser;
use crate::serialize;

/// Operands and the operator that consumes them. An inline image is one
/// operation with operator `BI` and a single [`Object::Stream`] operand: its
/// dictionary holds the image's keys as written (abbreviations kept), its
/// data the bytes between `ID` and `EI`.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    pub operator: Vec<u8>,
    pub operands: Vec<Object>,
}

impl Operation {
    pub fn new(operator: impl Into<Vec<u8>>, operands: Vec<Object>) -> Operation {
        Operation {
            operator: operator.into(),
            operands,
        }
    }

    /// The operator as text; an operator that is not UTF-8 gives "".
    pub fn operator_str(&self) -> &str {
        std::str::from_utf8(&self.operator).unwrap_or("")
    }

    /// The inline image of a `BI` operation.
    pub fn inline_image(&self) -> Option<&Stream> {
        match (self.operator.as_slice(), self.operands.as_slice()) {
            (b"BI", [Object::Stream(image)]) => Some(image),
            _ => None,
        }
    }
}

/// Split a decoded content stream into operations. Malformed input is
/// skipped where possible: stray delimiters are dropped, and operands left
/// without an operator at the end are discarded. Fails only when nesting
/// exceeds the parser's depth bound.
pub fn parse_content(data: &[u8]) -> Result<Vec<Operation>> {
    let mut parser = Parser::content(data, 0);
    let mut operands = Vec::new();
    let mut ops = Vec::new();
    loop {
        parser.skip_whitespace();
        let start = parser.pos();
        let Some(token) = parser.next_token() else {
            break;
        };
        match token {
            Token::Keyword(b"true" | b"false" | b"null") => {
                operands.push(parser.object_from_token(token, start)?)
            }
            Token::Keyword(b"BI") => {
                let image = inline_image(data, &mut parser)?;
                operands.clear();
                ops.push(Operation::new(
                    b"BI".as_slice(),
                    vec![Object::Stream(image)],
                ));
            }
            Token::Keyword(word) => ops.push(Operation::new(word, std::mem::take(&mut operands))),
            Token::ArrayClose | Token::DictClose | Token::BraceOpen | Token::BraceClose => {}
            token => match parser.object_from_token(token, start) {
                Ok(object) => operands.push(object),
                Err(err @ Error::LimitExceeded(_)) => return Err(err),
                // An array or dictionary left open at the end of the data.
                Err(_) => break,
            },
        }
    }
    Ok(ops)
}

/// The dictionary after `BI` up to `ID`, then the image data.
fn inline_image(data: &[u8], parser: &mut Parser<'_>) -> Result<Stream> {
    let mut dict = Dict::new();
    loop {
        parser.skip_whitespace();
        let Some(token) = parser.next_token() else {
            return Ok(Stream::new(dict, Vec::new()));
        };
        let key = match token {
            Token::Keyword(b"ID") => break,
            Token::Name(key) => key,
            _ => continue,
        };
        parser.skip_whitespace();
        let value_start = parser.pos();
        match parser.next_token() {
            None => return Ok(Stream::new(dict, Vec::new())),
            Some(Token::Keyword(b"ID")) => {
                dict.insert(key, Object::Null);
                break;
            }
            Some(token) => {
                let value = parser.object_from_token(token, value_start)?;
                dict.insert(key, value);
            }
        }
    }
    // `ID` is followed by exactly one white-space byte before the data.
    let mut start = parser.pos();
    if data.get(start).is_some_and(|&b| is_whitespace(b)) {
        start += 1;
    }
    let (end, after) = inline_image_end(data, start, &dict);
    parser.set_pos(after);
    Ok(Stream::new(dict, data[start..end].to_vec()))
}

/// The end of inline image data starting at `start` and the offset past its
/// `EI`: from `/L` (or `/Length`) when `EI` follows it, then from the size
/// of an unfiltered image, then by scanning for an `EI` between white space
/// that is followed by more content.
fn inline_image_end(data: &[u8], start: usize, dict: &Dict) -> (usize, usize) {
    let declared = dict
        .get(b"L")
        .or_else(|| dict.get(b"Length"))
        .and_then(Object::as_i64);
    let computed = if dict.contains_key(b"F") || dict.contains_key(b"Filter") {
        None
    } else {
        unfiltered_size(dict)
    };
    for len in [declared.and_then(|n| usize::try_from(n).ok()), computed]
        .into_iter()
        .flatten()
    {
        if let Some(end) = start.checked_add(len).filter(|&end| end <= data.len())
            && let Some(after) = ei_at(data, end)
        {
            return (end, after);
        }
    }
    let mut from = start;
    while let Some(at) = find(data, b"EI", from) {
        let preceded = at == start || is_whitespace(data[at - 1]);
        let followed = data
            .get(at + 2)
            .is_none_or(|&b| is_whitespace(b) || is_delimiter(b));
        if preceded && followed && plausible_content(data, at + 2) {
            let end = if at > start && is_whitespace(data[at - 1]) {
                at - 1
            } else {
                at
            };
            return (end, at + 2);
        }
        from = at + 1;
    }
    (data.len(), data.len())
}

/// `EI` at `pos` after optional white space, as a whole token; the offset
/// past it.
fn ei_at(data: &[u8], pos: usize) -> Option<usize> {
    let mut p = pos;
    while p < data.len() && p - pos < 8 && is_whitespace(data[p]) {
        p += 1;
    }
    let rest = data.get(p..)?;
    let whole = rest
        .get(2)
        .is_none_or(|&b| is_whitespace(b) || is_delimiter(b));
    (rest.starts_with(b"EI") && whole).then_some(p + 2)
}

/// Byte length of an unfiltered inline image from its size, depth, and
/// colour space; `None` when those are missing or the space is a resource.
fn unfiltered_size(dict: &Dict) -> Option<usize> {
    let get = |short: &[u8], long: &[u8]| dict.get(short).or_else(|| dict.get(long));
    let width = usize::try_from(get(b"W", b"Width")?.as_i64()?).ok()?;
    let height = usize::try_from(get(b"H", b"Height")?.as_i64()?).ok()?;
    let mask = get(b"IM", b"ImageMask")
        .and_then(Object::as_bool)
        .unwrap_or(false);
    let (bits, components) = if mask {
        (1, 1)
    } else {
        let bits = usize::try_from(get(b"BPC", b"BitsPerComponent")?.as_i64()?).ok()?;
        let components = match get(b"CS", b"ColorSpace")? {
            Object::Name(name) => match name.as_bytes() {
                b"G" | b"DeviceGray" | b"CalGray" => 1,
                b"RGB" | b"DeviceRGB" | b"CalRGB" => 3,
                b"CMYK" | b"DeviceCMYK" => 4,
                _ => return None,
            },
            Object::Array(items)
                if matches!(
                    items.first().and_then(Object::as_name),
                    Some(b"I" | b"Indexed")
                ) =>
            {
                1
            }
            _ => return None,
        };
        (bits, components)
    };
    let row = width
        .checked_mul(bits)?
        .checked_mul(components)?
        .div_ceil(8);
    row.checked_mul(height)
}

/// True when the bytes at `pos` read as operands followed by a known
/// operator, or as the end of the stream.
fn plausible_content(data: &[u8], pos: usize) -> bool {
    let mut parser = Parser::content(data, pos);
    for _ in 0..8 {
        parser.skip_whitespace();
        let start = parser.pos();
        let Some(token) = parser.next_token() else {
            return true;
        };
        match token {
            Token::Keyword(word) => {
                return is_operator(word) || matches!(word, b"true" | b"false" | b"null");
            }
            Token::ArrayClose | Token::DictClose | Token::BraceOpen | Token::BraceClose => {
                return false;
            }
            token => {
                if parser.object_from_token(token, start).is_err() {
                    return false;
                }
            }
        }
    }
    false
}

/// The operators of ISO 32000-2 Annex A.
fn is_operator(word: &[u8]) -> bool {
    matches!(
        word,
        b"b" | b"B"
            | b"b*"
            | b"B*"
            | b"BDC"
            | b"BI"
            | b"BMC"
            | b"BT"
            | b"BX"
            | b"c"
            | b"cm"
            | b"CS"
            | b"cs"
            | b"d"
            | b"d0"
            | b"d1"
            | b"Do"
            | b"DP"
            | b"EI"
            | b"EMC"
            | b"ET"
            | b"EX"
            | b"f"
            | b"F"
            | b"f*"
            | b"G"
            | b"g"
            | b"gs"
            | b"h"
            | b"i"
            | b"ID"
            | b"j"
            | b"J"
            | b"K"
            | b"k"
            | b"l"
            | b"m"
            | b"M"
            | b"MP"
            | b"n"
            | b"q"
            | b"Q"
            | b"re"
            | b"RG"
            | b"rg"
            | b"ri"
            | b"s"
            | b"S"
            | b"SC"
            | b"sc"
            | b"SCN"
            | b"scn"
            | b"sh"
            | b"T*"
            | b"Tc"
            | b"Td"
            | b"TD"
            | b"Tf"
            | b"Tj"
            | b"TJ"
            | b"TL"
            | b"Tm"
            | b"Tr"
            | b"Ts"
            | b"Tw"
            | b"Tz"
            | b"v"
            | b"w"
            | b"W"
            | b"W*"
            | b"y"
            | b"'"
            | b"\""
    )
}

/// Write operations one per line, operands separated by spaces. Inline
/// images are written `BI <dict> ID <data> EI`.
pub fn write_content(ops: &[Operation]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        if let Some(image) = op.inline_image() {
            out.extend_from_slice(b"BI");
            for (key, value) in image.dict.iter() {
                out.push(b' ');
                serialize::write_name(&mut out, key.as_bytes());
                out.push(b' ');
                serialize::write_object(&mut out, value);
            }
            out.extend_from_slice(b" ID ");
            out.extend_from_slice(image.raw());
            out.extend_from_slice(b"\nEI\n");
            continue;
        }
        for operand in &op.operands {
            serialize::write_object(&mut out, operand);
            out.push(b' ');
        }
        out.extend_from_slice(&op.operator);
        out.push(b'\n');
    }
    out
}
