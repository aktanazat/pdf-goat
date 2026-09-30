//! PostScript lexer for CMap files and Type 1 font programs.

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token<'a> {
    Int(i64),
    Real(f64),
    /// A literal name, without the leading slash.
    Name(&'a [u8]),
    /// An executable name or operator.
    Word(&'a [u8]),
    Str(Vec<u8>),
    Hex(Vec<u8>),
    OpenArray,
    CloseArray,
    OpenDict,
    CloseDict,
    OpenProc,
    CloseProc,
}

pub(crate) struct Lexer<'a> {
    data: &'a [u8],
    pub(crate) pos: usize,
}

fn is_white(b: u8) -> bool {
    matches!(b, 0 | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Deepest `(` nesting accepted inside one string literal.
const MAX_STRING_NESTING: u32 = 256;

impl<'a> Lexer<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Lexer { data, pos: 0 }
    }

    fn skip_space(&mut self) {
        while let Some(&b) = self.data.get(self.pos) {
            if is_white(b) {
                self.pos += 1;
            } else if b == b'%' {
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

    /// Raw bytes following the current position, as used after `RD` in Type 1 programs.
    pub(crate) fn raw(&mut self, len: usize) -> Option<&'a [u8]> {
        let out = self.data.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += len;
        Some(out)
    }

    fn regular_end(&self, start: usize) -> usize {
        let mut end = start;
        while let Some(&b) = self.data.get(end) {
            if is_white(b) || is_delim(b) {
                break;
            }
            end += 1;
        }
        end
    }

    pub(crate) fn next_token(&mut self) -> Option<Token<'a>> {
        self.skip_space();
        let b = *self.data.get(self.pos)?;
        match b {
            b'[' => {
                self.pos += 1;
                Some(Token::OpenArray)
            }
            b']' => {
                self.pos += 1;
                Some(Token::CloseArray)
            }
            b'{' => {
                self.pos += 1;
                Some(Token::OpenProc)
            }
            b'}' => {
                self.pos += 1;
                Some(Token::CloseProc)
            }
            b'/' => {
                let start = self.pos + 1;
                // `//name` is an immediately evaluated name; treat it like a literal.
                let start = if self.data.get(start) == Some(&b'/') {
                    start + 1
                } else {
                    start
                };
                let end = self.regular_end(start);
                self.pos = end;
                Some(Token::Name(&self.data[start..end]))
            }
            b'(' => Some(Token::Str(self.string())),
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    return Some(Token::OpenDict);
                }
                if self.data.get(self.pos + 1) == Some(&b'~') {
                    // ASCII85 strings do not occur in CMaps or font programs; skip to `~>`.
                    let rest = &self.data[self.pos..];
                    let end = rest
                        .windows(2)
                        .position(|w| w == b"~>")
                        .map_or(rest.len(), |p| p + 2);
                    self.pos += end;
                    return Some(Token::Str(Vec::new()));
                }
                Some(Token::Hex(self.hex_string()))
            }
            b'>' => {
                self.pos += 1;
                if self.data.get(self.pos) == Some(&b'>') {
                    self.pos += 1;
                    return Some(Token::CloseDict);
                }
                Some(Token::Word(b">"))
            }
            b')' => {
                self.pos += 1;
                Some(Token::Word(b")"))
            }
            _ => {
                let start = self.pos;
                let end = self.regular_end(start).max(start + 1);
                self.pos = end;
                let text = &self.data[start..end];
                Some(parse_number(text).unwrap_or(Token::Word(text)))
            }
        }
    }

    fn string(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut depth = 1u32;
        self.pos += 1;
        while let Some(&b) = self.data.get(self.pos) {
            self.pos += 1;
            match b {
                b'(' => {
                    depth = (depth + 1).min(MAX_STRING_NESTING);
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                b'\\' => {
                    let Some(&e) = self.data.get(self.pos) else {
                        break;
                    };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'\r' => {
                            if self.data.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.data.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => {
                                        v = v * 8 + u32::from(d - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((v & 0xFF) as u8);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(b),
            }
        }
        out
    }

    fn hex_string(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut high: Option<u8> = None;
        self.pos += 1;
        while let Some(&b) = self.data.get(self.pos) {
            self.pos += 1;
            if b == b'>' {
                break;
            }
            if let Some(v) = hex_value(b) {
                match high.take() {
                    Some(h) => out.push(h << 4 | v),
                    None => high = Some(v),
                }
            }
        }
        if let Some(h) = high {
            out.push(h << 4);
        }
        out
    }
}

fn parse_number(text: &[u8]) -> Option<Token<'static>> {
    let s = std::str::from_utf8(text).ok()?;
    let first = *text.first()?;
    if !(first.is_ascii_digit() || first == b'-' || first == b'+' || first == b'.') {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        return Some(Token::Int(v));
    }
    if let Some((radix, digits)) = s.split_once('#') {
        let radix: u32 = radix.parse().ok()?;
        if (2..=36).contains(&radix) {
            return i64::from_str_radix(digits, radix).ok().map(Token::Int);
        }
        return None;
    }
    let v: f64 = s.parse().ok()?;
    v.is_finite().then_some(Token::Real(v))
}
