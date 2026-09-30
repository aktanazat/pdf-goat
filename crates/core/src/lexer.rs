//! Tokens of PDF syntax (ISO 32000-2 7.2 and 7.3): numbers, strings, names,
//! delimiters, and keywords, with comments skipped as whitespace.

use crate::object::{Name, PdfString};

pub(crate) fn is_whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

pub(crate) fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

pub(crate) fn is_regular(byte: u8) -> bool {
    !is_whitespace(byte) && !is_delimiter(byte)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Token<'a> {
    Integer(i64),
    Real(f64),
    String(PdfString),
    Name(Name),
    ArrayOpen,
    ArrayClose,
    DictOpen,
    DictClose,
    BraceOpen,
    BraceClose,
    /// A run of regular characters that is not a number: `obj`, `R`,
    /// `true`, a content operator, and so on.
    Keyword(&'a [u8]),
}

pub(crate) struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(data: &'a [u8], pos: usize) -> Lexer<'a> {
        Lexer {
            data,
            pos: pos.min(data.len()),
        }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    /// Skip whitespace and `%` comments.
    pub(crate) fn skip_whitespace(&mut self) {
        while let Some(&byte) = self.data.get(self.pos) {
            if is_whitespace(byte) {
                self.pos += 1;
            } else if byte == b'%' {
                while let Some(&c) = self.data.get(self.pos) {
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    /// The next token, or `None` at the end of the data. Stray `)` and `>`
    /// bytes are skipped.
    pub(crate) fn next_token(&mut self) -> Option<Token<'a>> {
        loop {
            self.skip_whitespace();
            let byte = *self.data.get(self.pos)?;
            match byte {
                b'[' => {
                    self.pos += 1;
                    return Some(Token::ArrayOpen);
                }
                b']' => {
                    self.pos += 1;
                    return Some(Token::ArrayClose);
                }
                b'{' => {
                    self.pos += 1;
                    return Some(Token::BraceOpen);
                }
                b'}' => {
                    self.pos += 1;
                    return Some(Token::BraceClose);
                }
                b'<' => {
                    if self.data.get(self.pos + 1) == Some(&b'<') {
                        self.pos += 2;
                        return Some(Token::DictOpen);
                    }
                    return Some(Token::String(self.hex_string()));
                }
                b'>' => {
                    if self.data.get(self.pos + 1) == Some(&b'>') {
                        self.pos += 2;
                        return Some(Token::DictClose);
                    }
                    self.pos += 1;
                }
                b'(' => return Some(Token::String(self.literal_string())),
                b')' => self.pos += 1,
                b'/' => return Some(Token::Name(self.name())),
                b'+' | b'-' | b'.' | b'0'..=b'9' => return Some(self.number_or_keyword()),
                _ => return Some(Token::Keyword(self.keyword())),
            }
        }
    }

    fn keyword(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self.data.get(self.pos).is_some_and(|&b| is_regular(b)) {
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    /// Numbers are lenient the way readers are: repeated signs count as one
    /// minus, `.5` and `5.` are reals, and a non-standard exponent is read.
    fn number_or_keyword(&mut self) -> Token<'a> {
        let start = self.pos;
        let data = self.data;
        let mut p = start;
        let mut negative = false;
        while let Some(&b) = data.get(p) {
            match b {
                b'-' => negative = true,
                b'+' => {}
                _ => break,
            }
            p += 1;
        }
        let digits_start = p;
        let mut int_value: u64 = 0;
        let mut overflow = false;
        while let Some(&b) = data.get(p).filter(|b| b.is_ascii_digit()) {
            match int_value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(b - b'0')))
            {
                Some(v) => int_value = v,
                None => overflow = true,
            }
            p += 1;
        }
        let int_digits = p - digits_start;
        let mut is_real = false;
        let mut frac_digits = 0;
        if data.get(p) == Some(&b'.') {
            is_real = true;
            p += 1;
            while data.get(p).is_some_and(|b| b.is_ascii_digit()) {
                p += 1;
                frac_digits += 1;
            }
        }
        if int_digits + frac_digits == 0 {
            // `-`, `+`, or `.` alone: a keyword such as a stray operator.
            return Token::Keyword(self.keyword());
        }
        if matches!(data.get(p), Some(b'e' | b'E')) {
            let mut q = p + 1;
            if matches!(data.get(q), Some(b'+' | b'-')) {
                q += 1;
            }
            if data.get(q).is_some_and(|b| b.is_ascii_digit()) {
                while data.get(q).is_some_and(|b| b.is_ascii_digit()) {
                    q += 1;
                }
                is_real = true;
                p = q;
            }
        }
        self.pos = p;
        if !is_real
            && !overflow
            && let Ok(value) = i64::try_from(int_value)
        {
            return Token::Integer(if negative { -value } else { value });
        }
        let text = std::str::from_utf8(&data[digits_start..p]).unwrap_or("0");
        let value: f64 = text.parse().unwrap_or(0.0);
        Token::Real(if negative { -value } else { value })
    }

    fn name(&mut self) -> Name {
        self.pos += 1;
        let mut out = Vec::new();
        while let Some(&byte) = self.data.get(self.pos) {
            if !is_regular(byte) {
                break;
            }
            self.pos += 1;
            if byte == b'#' {
                let hi = self.data.get(self.pos).copied().and_then(hex_value);
                let lo = self.data.get(self.pos + 1).copied().and_then(hex_value);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(hi << 4 | lo);
                    self.pos += 2;
                    continue;
                }
            }
            out.push(byte);
        }
        Name::new(out)
    }

    /// A literal string: balanced parentheses, backslash escapes, octal
    /// codes, line continuations, and any bare end-of-line read as LF.
    fn literal_string(&mut self) -> PdfString {
        self.pos += 1;
        let data = self.data;
        let mut out = Vec::new();
        let mut depth = 1usize;
        while let Some(&byte) = data.get(self.pos) {
            self.pos += 1;
            match byte {
                b'\\' => {
                    let Some(&escaped) = data.get(self.pos) else {
                        break;
                    };
                    self.pos += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut value = u32::from(escaped - b'0');
                            for _ in 0..2 {
                                match data.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => {
                                        value = value * 8 + u32::from(d - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((value & 0xFF) as u8);
                        }
                        b'\r' => {
                            if data.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(byte);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(byte);
                }
                b'\r' => {
                    out.push(b'\n');
                    if data.get(self.pos) == Some(&b'\n') {
                        self.pos += 1;
                    }
                }
                _ => out.push(byte),
            }
        }
        PdfString::literal(out)
    }

    /// A hex string: whitespace and stray characters ignored, an odd final
    /// digit padded with zero.
    fn hex_string(&mut self) -> PdfString {
        self.pos += 1;
        let mut out = Vec::new();
        let mut high: Option<u8> = None;
        while let Some(&byte) = self.data.get(self.pos) {
            self.pos += 1;
            if byte == b'>' {
                break;
            }
            let Some(value) = hex_value(byte) else {
                continue;
            };
            match high.take() {
                Some(h) => out.push(h << 4 | value),
                None => high = Some(value),
            }
        }
        if let Some(h) = high {
            out.push(h << 4);
        }
        PdfString::hex(out)
    }
}

/// First index at or after `from` where `needle` occurs.
pub(crate) fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let first = *needle.first()?;
    let mut pos = from;
    while pos + needle.len() <= haystack.len() {
        let offset = haystack[pos..=haystack.len() - needle.len()]
            .iter()
            .position(|&b| b == first)?;
        let candidate = pos + offset;
        if haystack[candidate..].starts_with(needle) {
            return Some(candidate);
        }
        pos = candidate + 1;
    }
    None
}

/// Last index where `needle` starts at or before `before - needle.len()`.
pub(crate) fn rfind(haystack: &[u8], needle: &[u8], before: usize) -> Option<usize> {
    let end = before.min(haystack.len());
    if needle.is_empty() || needle.len() > end {
        return None;
    }
    (0..=end - needle.len())
        .rev()
        .find(|&i| haystack[i..].starts_with(needle))
}
