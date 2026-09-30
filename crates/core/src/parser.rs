//! Objects from tokens (ISO 32000-2 7.3): direct objects with a nesting
//! bound, indirect objects `N G obj ... endobj`, stream data with `/Length`
//! recovery, and object streams.

use std::ops::Range;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::lexer::{Lexer, Token, find, is_whitespace};
use crate::object::{Dict, ObjRef, Object, Stream};

/// Deepest nesting of arrays and dictionaries the parser accepts.
pub(crate) const MAX_DEPTH: usize = 256;

/// Keywords that end any open array or dictionary: the object around them
/// is malformed and the rest of the file must stay readable.
fn is_stop_keyword(keyword: &[u8]) -> bool {
    matches!(
        keyword,
        b"endobj" | b"stream" | b"endstream" | b"obj" | b"xref" | b"trailer" | b"startxref"
    )
}

pub(crate) struct Parser<'a> {
    lex: Lexer<'a>,
    /// Read `N G R` as a reference; off in content streams, which have none.
    refs: bool,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(data: &'a [u8], pos: usize) -> Parser<'a> {
        Parser {
            lex: Lexer::new(data, pos),
            refs: true,
        }
    }

    /// A parser for content-stream operands: integers never join into
    /// references.
    pub(crate) fn content(data: &'a [u8], pos: usize) -> Parser<'a> {
        Parser {
            lex: Lexer::new(data, pos),
            refs: false,
        }
    }

    pub(crate) fn pos(&self) -> usize {
        self.lex.pos()
    }

    pub(crate) fn set_pos(&mut self, pos: usize) {
        self.lex.set_pos(pos);
    }

    pub(crate) fn next_token(&mut self) -> Option<Token<'a>> {
        self.lex.next_token()
    }

    pub(crate) fn skip_whitespace(&mut self) {
        self.lex.skip_whitespace();
    }

    /// One direct object.
    pub(crate) fn parse_object(&mut self) -> Result<Object> {
        self.object(0)
    }

    /// The object that starts with `token`, already read from `start`.
    pub(crate) fn object_from_token(&mut self, token: Token<'a>, start: usize) -> Result<Object> {
        self.token_object(token, start, 0)
    }

    fn object(&mut self, depth: usize) -> Result<Object> {
        self.lex.skip_whitespace();
        let start = self.lex.pos();
        let Some(token) = self.lex.next_token() else {
            return Err(Error::syntax(start, "unexpected end of data"));
        };
        self.token_object(token, start, depth)
    }

    fn token_object(&mut self, token: Token<'a>, start: usize, depth: usize) -> Result<Object> {
        match token {
            Token::Integer(value) if self.refs => Ok(self.integer_or_reference(value)),
            Token::Integer(value) => Ok(Object::Integer(value)),
            Token::Real(value) => Ok(Object::Real(value)),
            Token::String(string) => Ok(Object::String(string)),
            Token::Name(name) => Ok(Object::Name(name)),
            Token::ArrayOpen => self.array(start, depth + 1).map(Object::Array),
            Token::DictOpen => self.dict(start, depth + 1).map(Object::Dict),
            Token::Keyword(b"true") => Ok(Object::Bool(true)),
            Token::Keyword(b"false") => Ok(Object::Bool(false)),
            Token::Keyword(b"null") => Ok(Object::Null),
            Token::Keyword(word) => Err(Error::syntax(
                start,
                format!("unexpected keyword `{}`", String::from_utf8_lossy(word)),
            )),
            Token::ArrayClose | Token::DictClose | Token::BraceOpen | Token::BraceClose => {
                Err(Error::syntax(start, "unexpected delimiter"))
            }
        }
    }

    /// `N G R` after the integer `N`, or the integer alone.
    fn integer_or_reference(&mut self, value: i64) -> Object {
        let Ok(num) = u32::try_from(value) else {
            return Object::Integer(value);
        };
        let save = self.lex.pos();
        if let Some(Token::Integer(generation)) = self.lex.next_token()
            && let Ok(generation) = u16::try_from(generation)
            && let Some(Token::Keyword(b"R")) = self.lex.next_token()
        {
            return Object::Reference(ObjRef::new(num, generation));
        }
        self.lex.set_pos(save);
        Object::Integer(value)
    }

    fn check_depth(&self, start: usize, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::LimitExceeded(format!(
                "objects nested deeper than {MAX_DEPTH} levels at byte {start}"
            )));
        }
        Ok(())
    }

    fn array(&mut self, start: usize, depth: usize) -> Result<Vec<Object>> {
        self.check_depth(start, depth)?;
        let mut items = Vec::new();
        loop {
            self.lex.skip_whitespace();
            let item_start = self.lex.pos();
            let Some(token) = self.lex.next_token() else {
                return Err(Error::syntax(start, "unterminated array"));
            };
            match token {
                Token::ArrayClose => return Ok(items),
                Token::Keyword(word) if is_stop_keyword(word) => {
                    self.lex.set_pos(item_start);
                    return Ok(items);
                }
                Token::Keyword(b"true" | b"false" | b"null") => {
                    items.push(self.token_object(token, item_start, depth)?)
                }
                // Stray `>>`, braces, and unknown keywords are skipped.
                Token::Keyword(_) | Token::DictClose | Token::BraceOpen | Token::BraceClose => {}
                token => items.push(self.token_object(token, item_start, depth)?),
            }
        }
    }

    fn dict(&mut self, start: usize, depth: usize) -> Result<Dict> {
        self.check_depth(start, depth)?;
        let mut dict = Dict::new();
        loop {
            self.lex.skip_whitespace();
            let key_start = self.lex.pos();
            let Some(token) = self.lex.next_token() else {
                return Err(Error::syntax(start, "unterminated dictionary"));
            };
            let key = match token {
                Token::DictClose => return Ok(dict),
                Token::Keyword(word) if is_stop_keyword(word) => {
                    self.lex.set_pos(key_start);
                    return Ok(dict);
                }
                Token::Name(name) => name,
                // A value where a key belongs: skip it.
                Token::ArrayOpen | Token::DictOpen => {
                    self.token_object(token, key_start, depth)?;
                    continue;
                }
                _ => continue,
            };
            self.lex.skip_whitespace();
            let value_start = self.lex.pos();
            let Some(token) = self.lex.next_token() else {
                return Err(Error::syntax(start, "unterminated dictionary"));
            };
            match token {
                Token::DictClose => {
                    dict.insert(key, Object::Null);
                    return Ok(dict);
                }
                Token::Keyword(word) if is_stop_keyword(word) => {
                    dict.insert(key, Object::Null);
                    self.lex.set_pos(value_start);
                    return Ok(dict);
                }
                Token::Keyword(word) if !matches!(word, b"true" | b"false" | b"null") => {
                    dict.insert(key, Object::Null);
                }
                token => {
                    let value = self.token_object(token, value_start, depth)?;
                    dict.insert(key, value);
                }
            }
        }
    }

    /// `N G obj`, returning the reference.
    pub(crate) fn object_header(&mut self) -> Result<ObjRef> {
        self.lex.skip_whitespace();
        let start = self.lex.pos();
        let num = match self.lex.next_token() {
            Some(Token::Integer(n)) => u32::try_from(n).ok(),
            _ => None,
        };
        let generation = match self.lex.next_token() {
            Some(Token::Integer(g)) => u16::try_from(g).ok(),
            _ => None,
        };
        let keyword = matches!(self.lex.next_token(), Some(Token::Keyword(b"obj")));
        match (num, generation, keyword) {
            (Some(num), Some(generation), true) => Ok(ObjRef::new(num, generation)),
            _ => Err(Error::syntax(start, "expected `N G obj`")),
        }
    }
}

/// An indirect object read from a buffer.
pub(crate) struct Indirect {
    pub id: ObjRef,
    pub object: Object,
    /// Offset just past the object (after `endobj` when present).
    pub end: usize,
}

/// What [`parse_indirect_parts`] found: an object, or a stream dictionary
/// with the range of its data in the buffer.
enum Body {
    Object(Object),
    Stream { dict: Dict, data: Range<usize> },
}

/// Parse `N G obj <object> [stream ... endstream] endobj` at `pos`.
/// `length_of` resolves an indirect `/Length`; a missing or wrong length is
/// recovered by scanning for `endstream`.
pub(crate) fn parse_indirect(
    buf: &Arc<Vec<u8>>,
    pos: usize,
    length_of: &dyn Fn(ObjRef) -> Option<usize>,
) -> Result<Indirect> {
    let (id, body, end) = parse_indirect_parts(buf, pos, length_of)?;
    let object = match body {
        Body::Object(object) => object,
        Body::Stream { dict, data } => {
            Object::Stream(Stream::shared(dict, Arc::clone(buf), data.start, data.end))
        }
    };
    Ok(Indirect { id, object, end })
}

/// One direct object at `pos` (`N G R` read as a reference) and the offset
/// just past it.
pub fn parse_object_at(data: &[u8], pos: usize) -> Result<(Object, usize)> {
    let mut parser = Parser::new(data, pos);
    let object = parser.parse_object()?;
    Ok((object, parser.pos()))
}

/// The indirect object `N G obj ... endobj` at `pos`: its reference, the
/// object, and the offset just past `endobj` (past the object when `endobj`
/// is missing). Stream data is read with a direct `/Length`, or recovered by
/// scanning for `endstream` when the length is indirect or wrong; the
/// stream holds a copy of its data.
pub fn parse_indirect_at(data: &[u8], pos: usize) -> Result<(ObjRef, Object, usize)> {
    let (id, body, end) = parse_indirect_parts(data, pos, &|_| None)?;
    let object = match body {
        Body::Object(object) => object,
        Body::Stream { dict, data: range } => Object::Stream(Stream::new(
            dict,
            data.get(range).map(<[u8]>::to_vec).unwrap_or_default(),
        )),
    };
    Ok((id, object, end))
}

fn parse_indirect_parts(
    data: &[u8],
    pos: usize,
    length_of: &dyn Fn(ObjRef) -> Option<usize>,
) -> Result<(ObjRef, Body, usize)> {
    let mut parser = Parser::new(data, pos);
    let id = parser.object_header()?;
    parser.skip_whitespace();
    let body_start = parser.pos();
    let object = match parser.next_token() {
        // `N G obj endobj`: an empty object is null.
        Some(Token::Keyword(b"endobj")) => {
            return Ok((id, Body::Object(Object::Null), parser.pos()));
        }
        Some(token) => parser.object_from_token(token, body_start)?,
        None => {
            return Err(Error::syntax(
                body_start,
                "unexpected end of data in object",
            ));
        }
    };
    let after_object = parser.pos();
    parser.skip_whitespace();
    let keyword_pos = parser.pos();
    let next = parser.next_token();
    let dict = match (object, next) {
        (Object::Dict(dict), Some(Token::Keyword(b"stream"))) => dict,
        (object, Some(Token::Keyword(b"endobj"))) => {
            return Ok((id, Body::Object(object), parser.pos()));
        }
        (object, _) => return Ok((id, Body::Object(object), after_object)),
    };
    let data_start = stream_data_start(data, keyword_pos + b"stream".len());
    let declared = match dict.get(b"Length") {
        Some(Object::Integer(n)) => usize::try_from(*n).ok(),
        Some(Object::Reference(r)) => length_of(*r),
        _ => None,
    };
    let (data_end, after) = stream_data_end(data, data_start, declared);
    let mut end = after;
    let mut tail = Parser::new(data, after);
    if let Some(Token::Keyword(b"endobj")) = tail.next_token() {
        end = tail.pos();
    }
    Ok((
        id,
        Body::Stream {
            dict,
            data: data_start..data_end,
        },
        end,
    ))
}

/// Where stream data begins after the `stream` keyword: past CRLF or LF, a
/// lone CR, or spaces before either.
fn stream_data_start(data: &[u8], after_keyword: usize) -> usize {
    let mut p = after_keyword;
    while matches!(data.get(p), Some(b' ' | b'\t')) {
        p += 1;
    }
    match (data.get(p), data.get(p + 1)) {
        (Some(b'\r'), Some(b'\n')) => p + 2,
        (Some(b'\n' | b'\r'), _) => p + 1,
        _ => after_keyword,
    }
}

/// The end of stream data and the offset just past `endstream`. A declared
/// length is used when `endstream` follows it; otherwise the data runs to the
/// end-of-line before the first `endstream`.
fn stream_data_end(data: &[u8], start: usize, declared: Option<usize>) -> (usize, usize) {
    if let Some(end) = declared
        .and_then(|len| start.checked_add(len))
        .filter(|&end| end <= data.len())
    {
        let mut q = end;
        while q < data.len() && q - end < 64 && is_whitespace(data[q]) {
            q += 1;
        }
        if data[q..].starts_with(b"endstream") {
            return (end, q + b"endstream".len());
        }
    }
    if let Some(found) = find(data, b"endstream", start) {
        let mut end = found;
        if end > start && data[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && data[end - 1] == b'\r' {
            end -= 1;
        }
        return (end, found + b"endstream".len());
    }
    let end = declared
        .and_then(|len| start.checked_add(len))
        .map_or(data.len(), |end| end.min(data.len()));
    (end, data.len())
}

/// The object numbers and offsets in an object stream's header: `N` pairs
/// of integers, offsets relative to `/First`.
pub(crate) fn object_stream_index(
    data: &[u8],
    count: usize,
    first: usize,
) -> Result<Vec<(u32, usize)>> {
    // Each pair takes at least four bytes, which bounds a lying /N.
    let count = count.min(data.len() / 4 + 1);
    let mut lex = Lexer::new(data, 0);
    let mut index = Vec::with_capacity(count);
    for _ in 0..count {
        let start = lex.pos();
        let (Some(Token::Integer(num)), Some(Token::Integer(offset))) =
            (lex.next_token(), lex.next_token())
        else {
            return Err(Error::syntax(start, "bad object stream header"));
        };
        let (Ok(num), Ok(offset)) = (u32::try_from(num), usize::try_from(offset)) else {
            return Err(Error::syntax(start, "bad object stream header"));
        };
        let Some(at) = first.checked_add(offset).filter(|&at| at <= data.len()) else {
            return Err(Error::syntax(start, "object stream offset out of range"));
        };
        index.push((num, at));
    }
    Ok(index)
}
