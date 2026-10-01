//! Type 1 subsets for embedding. The cleartext part is kept as it is. The private part
//! keeps every subroutine but only the CharStrings of `.notdef`, the requested glyphs and
//! the base and accent glyphs their `seac` composites name, and is encrypted again.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::BaseEncoding;
use crate::error::{FontError, Result};
use crate::type1::{C1, C2, CHARSTRING_KEY, EEXEC_KEY, decrypt, find, is_space, split_program};

/// A Type 1 program laid out as a PDF `FontFile` stream holds it.
pub(crate) struct Type1Program {
    pub(crate) data: Vec<u8>,
    /// `Length1` to `Length3`: the cleartext part, the binary eexec part and the trailer.
    pub(crate) lengths: [usize; 3],
}

/// One `/name len RD <charstring> ND` entry of the CharStrings dictionary.
struct Entry<'a> {
    name: &'a [u8],
    /// The entry in the decrypted private part.
    span: Range<usize>,
    charstring: &'a [u8],
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn skip_space(&mut self) {
        while self.data.get(self.pos).is_some_and(|&b| is_space(b)) {
            self.pos += 1;
        }
    }

    /// The next whitespace-delimited word; empty at the end of the data.
    fn word(&mut self) -> &'a [u8] {
        self.skip_space();
        let start = self.pos;
        while self.data.get(self.pos).is_some_and(|&b| !is_space(b)) {
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    fn number(&mut self) -> Option<i64> {
        std::str::from_utf8(self.word()).ok()?.parse().ok()
    }
}

/// The subset of the PFA or PFB `program` that draws the glyphs `names`.
pub(crate) fn subset_type1(program: &[u8], names: &[&str]) -> Result<Type1Program> {
    let (mut clear, private) = split_program(program)?;
    // PFA splitting stops before `eexec` and PFB keeps it: end the part with it either way.
    while clear.last().is_some_and(|&b| is_space(b)) {
        clear.pop();
    }
    if !clear.ends_with(b"eexec") {
        clear.extend_from_slice(b" eexec");
    }
    clear.push(b'\n');
    // Decryption runs on past the encrypted part into the trailer's zeros.
    let private = match rfind(&private, b"closefile") {
        Some(at) => &private[..at + b"closefile".len()],
        None => &private[..],
    };
    let len_iv = find(private, b"/lenIV")
        .and_then(|at| {
            Cursor {
                data: private,
                pos: at + b"/lenIV".len(),
            }
            .number()
        })
        .unwrap_or(4);
    let start = find(private, b"/CharStrings").ok_or(FontError::Missing("Type 1 CharStrings"))?;
    let mut cursor = Cursor {
        data: private,
        pos: start + b"/CharStrings".len(),
    };
    cursor.skip_space();
    let count_at = cursor.pos;
    cursor
        .number()
        .ok_or(FontError::Malformed("Type 1 CharStrings count"))?;
    let count = count_at..cursor.pos;
    loop {
        match cursor.word() {
            b"begin" => break,
            b"" => return Err(FontError::Malformed("Type 1 CharStrings dictionary")),
            _ => {}
        }
    }
    let first = cursor.pos;
    let mut entries = Vec::new();
    let end = loop {
        cursor.skip_space();
        let at = cursor.pos;
        // `end` closes the dictionary.
        let Some(name) = cursor.word().strip_prefix(b"/") else {
            break at;
        };
        let len = cursor
            .number()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(FontError::Malformed("Type 1 charstring length"))?;
        // The RD word, then one separator byte before the charstring.
        cursor.word();
        let body = cursor.pos + 1;
        let charstring = private
            .get(body..)
            .and_then(|rest| rest.get(..len))
            .ok_or(FontError::Truncated("Type 1 charstring"))?;
        cursor.pos = body + len;
        if cursor.word() == b"noaccess" {
            cursor.word();
        }
        entries.push(Entry {
            name,
            span: at..cursor.pos,
            charstring,
        });
    };

    let mut keep: BTreeSet<&[u8]> = names.iter().map(|name| name.as_bytes()).collect();
    keep.insert(b".notdef");
    for entry in entries
        .iter()
        .filter(|entry| names.iter().any(|name| name.as_bytes() == entry.name))
    {
        let code = if len_iv < 0 {
            entry.charstring.to_vec()
        } else {
            decrypt(entry.charstring, CHARSTRING_KEY, len_iv as usize)
        };
        for component in seac(&code).into_iter().flatten() {
            if let Some(name) = BaseEncoding::Standard.glyph_name(component) {
                keep.insert(name.as_bytes());
            }
        }
    }
    let mut seen = BTreeSet::new();
    let kept: Vec<&Entry<'_>> = entries
        .iter()
        .filter(|entry| keep.contains(entry.name) && seen.insert(entry.name))
        .collect();

    let mut plain = Vec::with_capacity(private.len());
    plain.extend_from_slice(&private[..count.start]);
    plain.extend_from_slice(kept.len().to_string().as_bytes());
    plain.extend_from_slice(&private[count.end..first]);
    for entry in &kept {
        plain.push(b'\n');
        plain.extend_from_slice(&private[entry.span.clone()]);
    }
    plain.push(b'\n');
    plain.extend_from_slice(&private[end..]);
    plain.push(b'\n');
    let encrypted = eexec(&plain);
    let mut trailer = Vec::with_capacity(8 * 65 + 12);
    for _ in 0..8 {
        trailer.extend_from_slice(&[b'0'; 64]);
        trailer.push(b'\n');
    }
    trailer.extend_from_slice(b"cleartomark\n");
    let lengths = [clear.len(), encrypted.len(), trailer.len()];
    let mut data = clear;
    data.extend(encrypted);
    data.extend(trailer);
    Ok(Type1Program { data, lengths })
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .rposition(|window| window == needle)
}

/// The StandardEncoding codes of the base and accent glyphs a decrypted charstring's
/// `seac` names.
fn seac(code: &[u8]) -> Option<[u8; 2]> {
    let mut stack: Vec<f64> = Vec::new();
    let mut i = 0;
    while let Some(&v) = code.get(i) {
        match v {
            32..=246 => {
                stack.push(f64::from(v) - 139.0);
                i += 1;
            }
            247..=250 => {
                stack.push((f64::from(v) - 247.0) * 256.0 + f64::from(*code.get(i + 1)?) + 108.0);
                i += 2;
            }
            251..=254 => {
                stack.push(-(f64::from(v) - 251.0) * 256.0 - f64::from(*code.get(i + 1)?) - 108.0);
                i += 2;
            }
            255 => {
                let b = code.get(i + 1..i + 5)?;
                stack.push(f64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]])));
                i += 5;
            }
            12 if code.get(i + 1) == Some(&6) => {
                let n = stack.len();
                return (n >= 2).then(|| [stack[n - 2] as u8, stack[n - 1] as u8]);
            }
            12 => {
                stack.clear();
                i += 2;
            }
            _ => {
                stack.clear();
                i += 1;
            }
        }
    }
    None
}

/// eexec encryption with four lead bytes whose cipher is not all hex digits, so every
/// reader takes the part as binary.
fn eexec(plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for lead in 0..=u8::MAX {
        out = encrypt(&[&[lead; 4][..], plain].concat(), EEXEC_KEY);
        if !out[..4].iter().all(u8::is_ascii_hexdigit) {
            break;
        }
    }
    out
}

fn encrypt(plain: &[u8], mut r: u16) -> Vec<u8> {
    plain
        .iter()
        .map(|&p| {
            let c = p ^ (r >> 8) as u8;
            r = u16::from(c)
                .wrapping_add(r)
                .wrapping_mul(C1)
                .wrapping_add(C2);
            c
        })
        .collect()
}
