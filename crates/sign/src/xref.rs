//! The cross-reference chain of a file read section by section, the way pyhanko's
//! `XRefCache` sees it: which revision defined each object, where each section starts and
//! ends, and what each revision's trailer declares. `pdf_core::Document` merges the chain,
//! which is what a reader wants and what signature coverage and difference analysis
//! cannot use.

use std::collections::HashSet;

use pdf_core::filters::{decode, filter_chain};
use pdf_core::{Dict, ObjRef, Object, parse_indirect_at, parse_object_at};

const MAX_SECTIONS: usize = 512;
const MAX_ENTRIES: i64 = 50_000_000;
const DECODE_LIMIT: usize = 256 * 1024 * 1024;
const EOF_WINDOW: usize = 1024;

/// One cross-reference section and its trailer.
#[derive(Clone, Debug)]
pub struct Section {
    /// The offset `startxref` or `/Prev` named for this section.
    pub declared_offset: usize,
    /// Just past the last table entry, or past the stream's `endobj`.
    pub end: usize,
    pub trailer: Dict,
    /// In-use and compressed entries other than object 0, in table order.
    pub defined: Vec<ObjRef>,
    /// Free entries for objects other than 0.
    pub freed: Vec<u32>,
    /// The cross-reference stream object, for a stream section.
    pub stream_ref: Option<ObjRef>,
    /// A classic table with an `/XRefStm` hybrid stream.
    pub hybrid: bool,
}

/// The sections in chronological order: index 0 is the original file.
#[derive(Clone, Debug)]
pub struct Chain {
    pub sections: Vec<Section>,
}

impl Chain {
    pub fn read(data: &[u8]) -> Result<Chain, String> {
        let mut offset = Some(startxref_at_eof(data)?);
        let mut seen = HashSet::new();
        let mut sections = Vec::new();
        while let Some(at) = offset {
            if !seen.insert(at) {
                return Err("Circular reference in cross-reference chain".to_owned());
            }
            if sections.len() >= MAX_SECTIONS {
                return Err("Too many cross-reference sections".to_owned());
            }
            let section = read_section(data, at)?;
            offset = match section.trailer.get(b"Prev") {
                Some(Object::Integer(prev)) => {
                    Some(usize::try_from(*prev).map_err(|_| "Negative /Prev offset")?)
                }
                Some(_) => return Err("/Prev is not an integer".to_owned()),
                None => None,
            };
            sections.push(section);
        }
        sections.reverse();
        Ok(Chain { sections })
    }

    /// The index of the newest section that defines object `num`.
    pub fn last_definer(&self, num: u32) -> Option<usize> {
        self.sections
            .iter()
            .rposition(|section| section.defined.iter().any(|id| id.num == num))
    }

    /// The length of the file prefix that ends with revision `index`: just past its
    /// `%%EOF` marker and the line ending after it.
    pub fn revision_end(&self, data: &[u8], index: usize) -> Option<usize> {
        let section = self.sections.get(index)?;
        let marker = find(data, b"%%EOF", section.end)?;
        let mut end = marker + 5;
        while end < data.len() && matches!(data[end], b'\r' | b'\n') && end < marker + 7 {
            end += 1;
        }
        Some(end)
    }
}

/// pyhanko's `process_data_at_eof`: the `startxref` value announced by the last `%%EOF`
/// in the final kilobyte of `data`.
pub fn startxref_at_eof(data: &[u8]) -> Result<usize, String> {
    let window_start = data.len().saturating_sub(EOF_WINDOW);
    let eof = rfind(data, b"%%EOF", data.len())
        .filter(|&at| at >= window_start)
        .ok_or("EOF marker not found")?;
    let keyword = rfind(data, b"startxref", eof).ok_or("startxref not found")?;
    let mut cursor = Cursor {
        data,
        pos: keyword + 9,
    };
    let value = cursor.int().ok_or("startxref value missing")?;
    usize::try_from(value).map_err(|_| "startxref value is negative".to_owned())
}

fn read_section(data: &[u8], declared_offset: usize) -> Result<Section, String> {
    let mut cursor = Cursor {
        data,
        pos: declared_offset,
    };
    cursor.skip_ws();
    if cursor.starts_with(b"xref") {
        read_table(data, declared_offset, cursor.pos)
    } else {
        read_stream_section(data, declared_offset, cursor.pos)
    }
}

fn read_table(data: &[u8], declared_offset: usize, start: usize) -> Result<Section, String> {
    let mut cursor = Cursor {
        data,
        pos: start + 4,
    };
    let mut defined = Vec::new();
    let mut freed = Vec::new();
    let mut end = cursor.pos;
    let trailer = loop {
        cursor.skip_ws();
        if cursor.starts_with(b"trailer") {
            cursor.pos += 7;
            match parse_object_at(data, cursor.pos)
                .map_err(|e| e.to_string())?
                .0
            {
                Object::Dict(dict) => break dict,
                _ => return Err("trailer is not a dictionary".to_owned()),
            }
        }
        let first = cursor
            .int()
            .ok_or("malformed cross-reference subsection header")?;
        let count = cursor
            .int()
            .ok_or("malformed cross-reference subsection header")?;
        if first < 0 || !(0..=MAX_ENTRIES).contains(&count) {
            return Err("malformed cross-reference subsection header".to_owned());
        }
        for index in 0..count {
            let offset = cursor.int().ok_or("malformed cross-reference entry")?;
            let generation = cursor.int().ok_or("malformed cross-reference entry")?;
            cursor.skip_ws();
            let kind = cursor.byte().ok_or("malformed cross-reference entry")?;
            cursor.pos += 1;
            end = cursor.pos;
            let Ok(num) = u32::try_from(first + index) else {
                continue;
            };
            if num == 0 {
                continue;
            }
            match kind {
                b'n' if offset >= 0 => defined.push(ObjRef::new(
                    num,
                    u16::try_from(generation).unwrap_or(u16::MAX),
                )),
                b'f' => freed.push(num),
                _ => return Err("malformed cross-reference entry".to_owned()),
            }
        }
    };
    let mut section = Section {
        declared_offset,
        end,
        trailer,
        defined,
        freed,
        stream_ref: None,
        hybrid: false,
    };
    if let Some(Object::Integer(at)) = section.trailer.get(b"XRefStm") {
        let at = usize::try_from(*at).map_err(|_| "negative /XRefStm offset")?;
        let hybrid = read_stream_section(data, at, at)?;
        section.defined.extend(hybrid.defined);
        section.freed.extend(hybrid.freed);
        section.hybrid = true;
    }
    Ok(section)
}

fn read_stream_section(
    data: &[u8],
    declared_offset: usize,
    start: usize,
) -> Result<Section, String> {
    let (id, object, end) = parse_indirect_at(data, start).map_err(|e| e.to_string())?;
    let Object::Stream(stream) = object else {
        return Err("cross-reference stream is not a stream".to_owned());
    };
    let filters = filter_chain(stream.dict.get(b"Filter"), stream.dict.get(b"DecodeParms"));
    let decoded = decode(stream.raw(), &filters, DECODE_LIMIT)
        .map_err(|e| format!("cross-reference stream: {e}"))?
        .data;
    let dict = stream.dict;
    let widths: Vec<usize> = dict
        .get_array(b"W")
        .ok_or("cross-reference stream without /W")?
        .iter()
        .map(|w| {
            w.as_i64()
                .and_then(|w| usize::try_from(w).ok())
                .filter(|w| *w <= 8)
        })
        .collect::<Option<Vec<usize>>>()
        .ok_or("malformed /W")?;
    if widths.len() != 3 {
        return Err("malformed /W".to_owned());
    }
    let row_len: usize = widths.iter().sum();
    let size = dict.get_i64(b"Size").unwrap_or(0);
    let index: Vec<i64> = match dict.get_array(b"Index") {
        Some(items) => items
            .iter()
            .map(Object::as_i64)
            .collect::<Option<Vec<i64>>>()
            .ok_or("malformed /Index")?,
        None => vec![0, size],
    };
    let mut defined = Vec::new();
    let mut freed = Vec::new();
    let mut rows = decoded.chunks_exact(row_len.max(1));
    for &[first, count] in index.as_chunks::<2>().0 {
        if first < 0 || !(0..=MAX_ENTRIES).contains(&count) {
            return Err("malformed /Index".to_owned());
        }
        for offset in 0..count {
            let Some(row) = rows.next() else { break };
            let mut fields = [1u64, 0, 0];
            let mut at = 0;
            for (slot, width) in fields.iter_mut().zip(&widths) {
                if *width > 0 {
                    *slot = row[at..at + width]
                        .iter()
                        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
                    at += width;
                }
            }
            let Ok(num) = u32::try_from(first + offset) else {
                continue;
            };
            if num == 0 {
                continue;
            }
            match fields[0] {
                1 => defined.push(ObjRef::new(
                    num,
                    u16::try_from(fields[2]).unwrap_or(u16::MAX),
                )),
                2 => defined.push(ObjRef::new(num, 0)),
                0 => freed.push(num),
                _ => {}
            }
        }
    }
    Ok(Section {
        declared_offset,
        end,
        trailer: dict,
        defined,
        freed,
        stream_ref: Some(id),
        hybrid: false,
    })
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

fn is_ws(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

impl Cursor<'_> {
    fn byte(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn starts_with(&self, needle: &[u8]) -> bool {
        self.data
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(needle))
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.byte() {
            if is_ws(byte) {
                self.pos += 1;
            } else if byte == b'%' {
                while let Some(byte) = self.byte() {
                    if byte == b'\n' || byte == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> &[u8] {
        let start = self.pos;
        while self.byte().is_some_and(|b| !is_ws(b) && !is_delim(b)) {
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    fn int(&mut self) -> Option<i64> {
        self.skip_ws();
        let save = self.pos;
        let token = self.token();
        match std::str::from_utf8(token)
            .ok()
            .and_then(|t| t.parse::<i64>().ok())
        {
            Some(value) => Some(value),
            None => {
                self.pos = save;
                None
            }
        }
    }
}

/// First index at or after `from` where `needle` occurs.
pub fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

/// Last index before `before` where `needle` starts.
pub fn rfind(haystack: &[u8], needle: &[u8], before: usize) -> Option<usize> {
    let end = before.min(haystack.len());
    haystack[..end]
        .windows(needle.len())
        .rposition(|window| window == needle)
}
