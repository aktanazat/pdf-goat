//! The PDF object model: null, booleans, numbers, strings, names, arrays,
//! dictionaries, streams, and indirect references (ISO 32000-2 7.3).

use std::fmt;
use std::sync::Arc;

use crate::serialize;
use crate::text;

/// An indirect reference `num generation R`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjRef {
    pub num: u32,
    pub generation: u16,
}

impl ObjRef {
    pub const fn new(num: u32, generation: u16) -> ObjRef {
        ObjRef { num, generation }
    }
}

impl fmt::Display for ObjRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} R", self.num, self.generation)
    }
}

/// A name's bytes after `#xx` escapes are decoded, without the leading `/`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(Vec<u8>);

impl Name {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Name {
        Name(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The name as UTF-8 text, when it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl From<&str> for Name {
    fn from(value: &str) -> Name {
        Name(value.as_bytes().to_vec())
    }
}

impl From<String> for Name {
    fn from(value: String) -> Name {
        Name(value.into_bytes())
    }
}

impl From<&[u8]> for Name {
    fn from(value: &[u8]) -> Name {
        Name(value.to_vec())
    }
}

impl<const N: usize> From<&[u8; N]> for Name {
    fn from(value: &[u8; N]) -> Name {
        Name(value.to_vec())
    }
}

impl From<Vec<u8>> for Name {
    fn from(value: Vec<u8>) -> Name {
        Name(value)
    }
}

impl From<&Name> for Name {
    fn from(value: &Name) -> Name {
        value.clone()
    }
}

impl PartialEq<[u8]> for Name {
    fn eq(&self, other: &[u8]) -> bool {
        self.0 == other
    }
}

impl PartialEq<&[u8]> for Name {
    fn eq(&self, other: &&[u8]) -> bool {
        self.0 == *other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for Name {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.0 == other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for Name {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self.0 == *other
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl fmt::Display for Name {
    /// PDF syntax: `/` followed by the bytes, `#xx`-escaped where required.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = Vec::new();
        serialize::write_name(&mut out, &self.0);
        f.write_str(&String::from_utf8_lossy(&out))
    }
}

/// How a string was written: `(literal)` or `<hex>`. The writer keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StringFormat {
    #[default]
    Literal,
    Hex,
}

/// A string object. Equality and hashing compare bytes only.
#[derive(Clone, Debug, Default)]
pub struct PdfString {
    pub bytes: Vec<u8>,
    pub format: StringFormat,
}

impl PdfString {
    pub fn literal(bytes: impl Into<Vec<u8>>) -> PdfString {
        PdfString {
            bytes: bytes.into(),
            format: StringFormat::Literal,
        }
    }

    pub fn hex(bytes: impl Into<Vec<u8>>) -> PdfString {
        PdfString {
            bytes: bytes.into(),
            format: StringFormat::Hex,
        }
    }

    /// A text string (ISO 32000-2 7.9.2.2): PDFDocEncoding when every
    /// character has a PDFDocEncoding byte, written literal; otherwise
    /// UTF-16BE with a byte order mark, written hex.
    pub fn from_text(value: &str) -> PdfString {
        let bytes = text::encode_text_string(value);
        if bytes.starts_with(&[0xFE, 0xFF]) {
            PdfString::hex(bytes)
        } else {
            PdfString::literal(bytes)
        }
    }

    /// Decode as a text string: UTF-16BE or UTF-8 with a byte order mark,
    /// otherwise PDFDocEncoding.
    pub fn to_text(&self) -> String {
        text::decode_text_string(&self.bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl PartialEq for PdfString {
    fn eq(&self, other: &PdfString) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for PdfString {}

impl std::hash::Hash for PdfString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

/// A dictionary that keeps keys in insertion order. Inserting an existing
/// key replaces its value in place.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict {
    entries: Vec<(Name, Object)>,
}

impl Dict {
    pub fn new() -> Dict {
        Dict {
            entries: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Dict {
        Dict {
            entries: Vec::with_capacity(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value for `key`, not following references.
    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.entries
            .iter()
            .find(|(k, _)| k.0 == key)
            .map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut Object> {
        self.entries
            .iter_mut()
            .find(|(k, _)| k.0 == key)
            .map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Set `key`, returning the previous value. A new key goes last.
    pub fn insert(&mut self, key: impl Into<Name>, value: impl Into<Object>) -> Option<Object> {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => Some(std::mem::replace(slot, value)),
            None => {
                self.entries.push((key, value));
                None
            }
        }
    }

    /// Remove `key`, keeping the order of the remaining keys.
    pub fn remove(&mut self, key: &[u8]) -> Option<Object> {
        let index = self.entries.iter().position(|(k, _)| k.0 == key)?;
        Some(self.entries.remove(index).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Name, &Object)> + '_ {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&Name, &mut Object)> + '_ {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &Name> + '_ {
        self.entries.iter().map(|(k, _)| k)
    }

    /// `/Key /Name` → the name's bytes.
    pub fn get_name(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key)?.as_name()
    }

    /// An integer value; a real is truncated toward zero.
    pub fn get_i64(&self, key: &[u8]) -> Option<i64> {
        self.get(key)?.as_i64()
    }

    /// An integer or real value as `f64`.
    pub fn get_f64(&self, key: &[u8]) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn get_bool(&self, key: &[u8]) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    pub fn get_string(&self, key: &[u8]) -> Option<&PdfString> {
        self.get(key)?.as_string()
    }

    pub fn get_array(&self, key: &[u8]) -> Option<&[Object]> {
        self.get(key)?.as_array()
    }

    pub fn get_dict(&self, key: &[u8]) -> Option<&Dict> {
        self.get(key)?.as_dict()
    }

    pub fn get_ref(&self, key: &[u8]) -> Option<ObjRef> {
        self.get(key)?.as_reference()
    }

    /// True when `/Type` is the name `type_name`.
    pub fn has_type(&self, type_name: &[u8]) -> bool {
        self.get_name(b"Type") == Some(type_name)
    }
}

impl FromIterator<(Name, Object)> for Dict {
    fn from_iter<I: IntoIterator<Item = (Name, Object)>>(iter: I) -> Dict {
        let mut dict = Dict::new();
        for (key, value) in iter {
            dict.insert(key, value);
        }
        dict
    }
}

impl IntoIterator for Dict {
    type Item = (Name, Object);
    type IntoIter = std::vec::IntoIter<(Name, Object)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

/// Stream bytes, shared with the file buffer when they were read unchanged.
#[derive(Clone)]
struct Bytes {
    buf: Arc<Vec<u8>>,
    start: usize,
    end: usize,
}

impl Bytes {
    fn owned(data: Vec<u8>) -> Bytes {
        let end = data.len();
        Bytes {
            buf: Arc::new(data),
            start: 0,
            end,
        }
    }

    fn as_slice(&self) -> &[u8] {
        self.buf.get(self.start..self.end).unwrap_or(&[])
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.end.saturating_sub(self.start))
    }
}

/// A stream: its dictionary and its raw (still filtered) data. Data read from
/// an encrypted file is already decrypted. The writer sets `/Length` from the
/// data, so the dictionary's `/Length` need not be kept in step.
#[derive(Clone, Debug)]
pub struct Stream {
    pub dict: Dict,
    data: Bytes,
}

impl Stream {
    pub fn new(dict: Dict, data: Vec<u8>) -> Stream {
        Stream {
            dict,
            data: Bytes::owned(data),
        }
    }

    /// A stream whose data is `buf[start..end]`, shared without copying.
    pub(crate) fn shared(dict: Dict, buf: Arc<Vec<u8>>, start: usize, end: usize) -> Stream {
        let end = end.min(buf.len());
        let start = start.min(end);
        Stream {
            dict,
            data: Bytes { buf, start, end },
        }
    }

    /// The raw data, still encoded by the stream's `/Filter`.
    pub fn raw(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Replace the raw data. The caller keeps `/Filter` and `/DecodeParms`
    /// consistent with it.
    pub fn set_raw(&mut self, data: Vec<u8>) {
        self.data = Bytes::owned(data);
    }

    /// Replace the data with unfiltered bytes: drops `/Filter`,
    /// `/DecodeParms`, and `/DL`.
    pub fn set_decoded(&mut self, data: Vec<u8>) {
        self.dict.remove(b"Filter");
        self.dict.remove(b"DecodeParms");
        self.dict.remove(b"DL");
        self.data = Bytes::owned(data);
    }
}

impl PartialEq for Stream {
    fn eq(&self, other: &Stream) -> bool {
        self.dict == other.dict && self.raw() == other.raw()
    }
}

/// A PDF object.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Object {
    #[default]
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    String(PdfString),
    Name(Name),
    Array(Vec<Object>),
    Dict(Dict),
    Stream(Stream),
    Reference(ObjRef),
}

impl Object {
    pub fn name(value: impl Into<Name>) -> Object {
        Object::Name(value.into())
    }

    /// A literal byte string.
    pub fn string(bytes: impl Into<Vec<u8>>) -> Object {
        Object::String(PdfString::literal(bytes))
    }

    /// A text string; see [`PdfString::from_text`].
    pub fn text(value: &str) -> Object {
        Object::String(PdfString::from_text(value))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Object::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Object::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// An integer; a finite real is truncated toward zero.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Object::Integer(value) => Some(*value),
            Object::Real(value) if value.is_finite() => Some(*value as i64),
            _ => None,
        }
    }

    /// An integer or real as `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Object::Integer(value) => Some(*value as f64),
            Object::Real(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Object::Name(name) => Some(name.as_bytes()),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&PdfString> {
        match self {
            Object::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Object::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Object>> {
        match self {
            Object::Array(items) => Some(items),
            _ => None,
        }
    }

    /// A dictionary (not a stream's dictionary; see [`Object::as_stream`]).
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(dict) => Some(dict),
            _ => None,
        }
    }

    pub fn as_dict_mut(&mut self) -> Option<&mut Dict> {
        match self {
            Object::Dict(dict) => Some(dict),
            _ => None,
        }
    }

    pub fn as_stream(&self) -> Option<&Stream> {
        match self {
            Object::Stream(stream) => Some(stream),
            _ => None,
        }
    }

    pub fn as_stream_mut(&mut self) -> Option<&mut Stream> {
        match self {
            Object::Stream(stream) => Some(stream),
            _ => None,
        }
    }

    pub fn as_reference(&self) -> Option<ObjRef> {
        match self {
            Object::Reference(id) => Some(*id),
            _ => None,
        }
    }

    /// The type as a word for messages: `null`, `boolean`, `integer`, `real`,
    /// `string`, `name`, `array`, `dictionary`, `stream`, `reference`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Object::Null => "null",
            Object::Bool(_) => "boolean",
            Object::Integer(_) => "integer",
            Object::Real(_) => "real",
            Object::String(_) => "string",
            Object::Name(_) => "name",
            Object::Array(_) => "array",
            Object::Dict(_) => "dictionary",
            Object::Stream(_) => "stream",
            Object::Reference(_) => "reference",
        }
    }

    /// Compact PDF syntax. A stream is written as its dictionary with the
    /// true `/Length`, then `stream`, the raw data, and `endstream`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        serialize::write_object(&mut out, self);
        out
    }

    /// Indented syntax in MuPDF's `xref_object` layout: one dictionary entry
    /// per line, arrays as `[ a b ]`. A stream shows its dictionary only.
    pub fn to_pretty_string(&self) -> String {
        serialize::pretty(self)
    }
}

impl From<bool> for Object {
    fn from(value: bool) -> Object {
        Object::Bool(value)
    }
}

impl From<i64> for Object {
    fn from(value: i64) -> Object {
        Object::Integer(value)
    }
}

impl From<i32> for Object {
    fn from(value: i32) -> Object {
        Object::Integer(i64::from(value))
    }
}

impl From<u32> for Object {
    fn from(value: u32) -> Object {
        Object::Integer(i64::from(value))
    }
}

impl From<usize> for Object {
    fn from(value: usize) -> Object {
        Object::Integer(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<f64> for Object {
    fn from(value: f64) -> Object {
        Object::Real(value)
    }
}

impl From<f32> for Object {
    fn from(value: f32) -> Object {
        Object::Real(f64::from(value))
    }
}

impl From<ObjRef> for Object {
    fn from(value: ObjRef) -> Object {
        Object::Reference(value)
    }
}

impl From<Name> for Object {
    fn from(value: Name) -> Object {
        Object::Name(value)
    }
}

impl From<PdfString> for Object {
    fn from(value: PdfString) -> Object {
        Object::String(value)
    }
}

impl From<Vec<Object>> for Object {
    fn from(value: Vec<Object>) -> Object {
        Object::Array(value)
    }
}

impl From<Dict> for Object {
    fn from(value: Dict) -> Object {
        Object::Dict(value)
    }
}

impl From<Stream> for Object {
    fn from(value: Stream) -> Object {
        Object::Stream(value)
    }
}
