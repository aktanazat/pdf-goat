//! TIFF image file directory reader shared by the TIFF decoder and the EXIF
//! segment parser of the JPEG scanner.

use crate::error::{CodecError, Result};

/// A parsed TIFF structure: byte order plus the entries of one IFD.
#[derive(Debug, Clone)]
pub(crate) struct Ifd {
    pub little_endian: bool,
    pub entries: Vec<IfdEntry>,
    /// Offset of the next IFD, 0 when this is the last one.
    pub next: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IfdEntry {
    pub tag: u16,
    pub kind: u16,
    pub count: u32,
    /// The raw four value bytes: the value itself when it fits, else an
    /// offset into the file.
    pub raw: [u8; 4],
}

pub(crate) const TYPE_BYTE: u16 = 1;
pub(crate) const TYPE_SHORT: u16 = 3;
pub(crate) const TYPE_LONG: u16 = 4;
pub(crate) const TYPE_RATIONAL: u16 = 5;

fn type_size(kind: u16) -> Option<usize> {
    Some(match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16..=18 => 8,
        _ => return None,
    })
}

/// Reads the TIFF header at the start of `data` and returns the byte order
/// and the offset of the first IFD.
pub(crate) fn read_header(codec: &'static str, data: &[u8]) -> Result<(bool, u32)> {
    let order = data
        .get(0..2)
        .ok_or_else(|| CodecError::malformed(codec, "short TIFF header"))?;
    let le = match order {
        b"II" => true,
        b"MM" => false,
        _ => return Err(CodecError::malformed(codec, "bad TIFF byte order")),
    };
    let magic =
        read_u16(data, 2, le).ok_or_else(|| CodecError::malformed(codec, "short TIFF header"))?;
    if magic != 42 {
        return Err(CodecError::malformed(codec, format!("TIFF magic {magic}")));
    }
    let first =
        read_u32(data, 4, le).ok_or_else(|| CodecError::malformed(codec, "short TIFF header"))?;
    Ok((le, first))
}

pub(crate) fn read_u16(data: &[u8], at: usize, le: bool) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(if le {
        u16::from_le_bytes([b[0], b[1]])
    } else {
        u16::from_be_bytes([b[0], b[1]])
    })
}

pub(crate) fn read_u32(data: &[u8], at: usize, le: bool) -> Option<u32> {
    let b = data.get(at..at + 4)?;
    let arr = [b[0], b[1], b[2], b[3]];
    Some(if le {
        u32::from_le_bytes(arr)
    } else {
        u32::from_be_bytes(arr)
    })
}

/// Reads the IFD at `offset`.
pub(crate) fn read_ifd(codec: &'static str, data: &[u8], offset: u32, le: bool) -> Result<Ifd> {
    let at = offset as usize;
    let count = read_u16(data, at, le)
        .ok_or_else(|| CodecError::malformed(codec, format!("IFD offset {offset} out of range")))?;
    let mut entries = Vec::with_capacity(usize::from(count.min(512)));
    for i in 0..usize::from(count) {
        let e = at + 2 + i * 12;
        let tag = read_u16(data, e, le);
        let kind = read_u16(data, e + 2, le);
        let cnt = read_u32(data, e + 4, le);
        let raw = data.get(e + 8..e + 12);
        match (tag, kind, cnt, raw) {
            (Some(tag), Some(kind), Some(count), Some(raw)) => {
                entries.push(IfdEntry {
                    tag,
                    kind,
                    count,
                    raw: [raw[0], raw[1], raw[2], raw[3]],
                });
            }
            _ => return Err(CodecError::malformed(codec, "truncated IFD")),
        }
    }
    let next = read_u32(data, at + 2 + usize::from(count) * 12, le).unwrap_or(0);
    Ok(Ifd {
        little_endian: le,
        entries,
        next,
    })
}

impl Ifd {
    pub(crate) fn entry(&self, tag: u16) -> Option<&IfdEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// The bytes holding an entry's values, whether inline or at an offset.
    fn value_bytes<'a>(&self, data: &'a [u8], e: &IfdEntry) -> Option<&'a [u8]> {
        let size = type_size(e.kind)?.checked_mul(e.count as usize)?;
        if size <= 4 {
            // Inline values live in the entry itself; the caller holds a copy.
            return None;
        }
        let off = read_u32(&e.raw, 0, self.little_endian)? as usize;
        data.get(off..off.checked_add(size)?)
    }

    /// All values of an integer-typed entry (BYTE, SHORT, LONG) as u32.
    pub(crate) fn ints(&self, data: &[u8], tag: u16) -> Option<Vec<u32>> {
        let e = self.entry(tag)?;
        let size = type_size(e.kind)?;
        if !matches!(e.kind, TYPE_BYTE | TYPE_SHORT | TYPE_LONG) {
            return None;
        }
        let count = e.count as usize;
        if count > 1 << 20 {
            return None;
        }
        let inline;
        let bytes: &[u8] = if size * count <= 4 {
            inline = e.raw;
            &inline[..size * count]
        } else {
            self.value_bytes(data, e)?
        };
        let le = self.little_endian;
        Some(
            bytes
                .chunks_exact(size)
                .map(|c| match size {
                    1 => u32::from(c[0]),
                    2 => u32::from(if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }),
                    _ => {
                        let a = [c[0], c[1], c[2], c[3]];
                        if le {
                            u32::from_le_bytes(a)
                        } else {
                            u32::from_be_bytes(a)
                        }
                    }
                })
                .collect(),
        )
    }

    /// The first value of an integer-typed entry.
    pub(crate) fn int(&self, data: &[u8], tag: u16) -> Option<u32> {
        self.ints(data, tag)?.first().copied()
    }

    /// The first value of a RATIONAL entry as a float, `None` for a zero
    /// denominator.
    pub(crate) fn rational(&self, data: &[u8], tag: u16) -> Option<f64> {
        let e = self.entry(tag)?;
        if e.kind != TYPE_RATIONAL {
            return self.int(data, tag).map(f64::from);
        }
        let bytes = self.value_bytes(data, e)?;
        let num = read_u32(bytes, 0, self.little_endian)?;
        let den = read_u32(bytes, 4, self.little_endian)?;
        if den == 0 {
            return None;
        }
        Some(f64::from(num) / f64::from(den))
    }

    /// The raw bytes of a BYTE/UNDEFINED entry (used for ICC profiles).
    pub(crate) fn bytes<'a>(&self, data: &'a [u8], tag: u16) -> Option<&'a [u8]> {
        let e = self.entry(tag)?;
        if !matches!(e.kind, 1 | 7) {
            return None;
        }
        if e.count <= 4 {
            return None;
        }
        self.value_bytes(data, e)
    }
}
