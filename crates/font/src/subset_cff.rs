//! CFF subsets for embedding. Glyph ids stay as they are, so a name-keyed subset's CIDs
//! (its glyph ids) and a CID-keyed subset's charset select the same glyphs as the full
//! font. The charstrings of unused glyphs become a bare `endchar`, subroutines no kept
//! charstring calls become a bare `return`, and every offset structure is rebuilt around
//! the smaller INDEXes.

use std::collections::BTreeSet;

use crate::cff::{Index, parse_fd_select};
use crate::charstring::subr_bias;
use crate::error::{FontError, Result};
use crate::reader::read_u16;

const OP_CHARSET: u16 = 15;
const OP_ENCODING: u16 = 16;
const OP_CHARSTRINGS: u16 = 17;
const OP_PRIVATE: u16 = 18;
const OP_SUBRS: u16 = 19;
const OP_ROS: u16 = 0x0C1E;
const OP_FDARRAY: u16 = 0x0C24;
const OP_FDSELECT: u16 = 0x0C25;

const ENDCHAR: &[u8] = &[14];
const RETURN: &[u8] = &[11];
/// Type 2 subroutine nesting limit.
const MAX_SUBR_DEPTH: usize = 10;
/// Most charstring operations read while looking for subroutine calls.
const MAX_WORK: usize = 4_000_000;

/// A DICT entry: operator, the operands' bytes as stored, and their integer values
/// (`None` for a real).
struct Entry<'a> {
    op: u16,
    raw: &'a [u8],
    ints: Vec<Option<i64>>,
}

fn parse_dict(data: &[u8]) -> Result<Vec<Entry<'_>>> {
    let byte = |p: usize| data.get(p).copied().ok_or(FontError::Truncated("CFF DICT"));
    let mut entries = Vec::new();
    let mut ints = Vec::new();
    let (mut start, mut pos) = (0, 0);
    while pos < data.len() {
        let b0 = data[pos];
        match b0 {
            0..=21 => {
                let (op, len) = if b0 == 12 {
                    (0x0C00 | u16::from(byte(pos + 1)?), 2)
                } else {
                    (u16::from(b0), 1)
                };
                entries.push(Entry {
                    op,
                    raw: &data[start..pos],
                    ints: std::mem::take(&mut ints),
                });
                pos += len;
                start = pos;
            }
            28 => {
                ints.push(Some(i64::from(i16::from_be_bytes([
                    byte(pos + 1)?,
                    byte(pos + 2)?,
                ]))));
                pos += 3;
            }
            29 => {
                ints.push(Some(i64::from(i32::from_be_bytes([
                    byte(pos + 1)?,
                    byte(pos + 2)?,
                    byte(pos + 3)?,
                    byte(pos + 4)?,
                ]))));
                pos += 5;
            }
            30 => {
                pos += 1;
                loop {
                    let b = byte(pos)?;
                    pos += 1;
                    if b >> 4 == 0x0F || b & 0x0F == 0x0F {
                        break;
                    }
                }
                ints.push(None);
            }
            32..=246 => {
                ints.push(Some(i64::from(b0) - 139));
                pos += 1;
            }
            247..=250 => {
                ints.push(Some(
                    (i64::from(b0) - 247) * 256 + i64::from(byte(pos + 1)?) + 108,
                ));
                pos += 2;
            }
            251..=254 => {
                ints.push(Some(
                    -(i64::from(b0) - 251) * 256 - i64::from(byte(pos + 1)?) - 108,
                ));
                pos += 2;
            }
            _ => return Err(FontError::Malformed("CFF DICT operand")),
        }
    }
    Ok(entries)
}

fn entry<'e, 'a>(entries: &'e [Entry<'a>], op: u16) -> Option<&'e Entry<'a>> {
    entries.iter().rev().find(|e| e.op == op)
}

/// The non-negative integer operand `index` of `op`.
fn operand(entries: &[Entry<'_>], op: u16, index: usize) -> Option<usize> {
    let value = (*entry(entries, op)?.ints.get(index)?)?;
    usize::try_from(value).ok()
}

/// Serialises `entries`, writing the operands of each operator in `values` as five-byte
/// integers so the DICT's length does not depend on them, and dropping operators in `drop`.
fn write_dict(
    entries: &[Entry<'_>],
    values: &[(u16, Vec<usize>)],
    drop: &[u16],
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in entries.iter().filter(|e| !drop.contains(&e.op)) {
        match values.iter().find(|(op, _)| *op == entry.op) {
            Some((_, operands)) => {
                for &value in operands {
                    let value = i32::try_from(value)
                        .map_err(|_| FontError::LimitExceeded("CFF subset offset"))?;
                    out.push(29);
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
            None => out.extend_from_slice(entry.raw),
        }
        if entry.op >= 0x0C00 {
            out.extend_from_slice(&[12, (entry.op & 0xFF) as u8]);
        } else {
            out.push(entry.op as u8);
        }
    }
    Ok(out)
}

fn write_index(items: &[&[u8]]) -> Result<Vec<u8>> {
    let count =
        u16::try_from(items.len()).map_err(|_| FontError::LimitExceeded("CFF INDEX count"))?;
    let mut out = count.to_be_bytes().to_vec();
    if items.is_empty() {
        return Ok(out);
    }
    let last = items.iter().map(|item| item.len()).sum::<usize>() + 1;
    let off_size = match last {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    let last = u32::try_from(last).map_err(|_| FontError::LimitExceeded("CFF INDEX size"))?;
    out.push(off_size as u8);
    let mut offset = 1u32;
    out.extend_from_slice(&offset.to_be_bytes()[4 - off_size..]);
    for item in items {
        offset += item.len() as u32;
        out.extend_from_slice(&offset.to_be_bytes()[4 - off_size..]);
    }
    debug_assert_eq!(offset, last);
    for item in items {
        out.extend_from_slice(item);
    }
    Ok(out)
}

/// Bytes of a custom charset covering `glyphs` glyphs.
fn charset_len(cff: &[u8], offset: usize, glyphs: usize) -> Result<usize> {
    let byte = |p: usize| {
        cff.get(p)
            .copied()
            .ok_or(FontError::Truncated("CFF charset"))
    };
    let format = byte(offset)?;
    if format == 0 {
        return Ok(1 + 2 * glyphs.saturating_sub(1));
    }
    let record = match format {
        1 => 3,
        2 => 4,
        _ => return Err(FontError::Malformed("CFF charset format")),
    };
    let (mut pos, mut covered) = (offset + 1, 0usize);
    while covered < glyphs.saturating_sub(1) {
        let left = if format == 1 {
            usize::from(byte(pos + 2)?)
        } else {
            usize::from(read_u16(cff, pos + 2).ok_or(FontError::Truncated("CFF charset"))?)
        };
        covered += left + 1;
        pos += record;
    }
    Ok(pos - offset)
}

/// Bytes of a custom encoding, supplements included.
fn encoding_len(cff: &[u8], offset: usize) -> Result<usize> {
    let byte = |p: usize| {
        cff.get(p)
            .copied()
            .ok_or(FontError::Truncated("CFF Encoding"))
    };
    let format = byte(offset)?;
    let mut len = match format & 0x7F {
        0 => 2 + usize::from(byte(offset + 1)?),
        1 => 2 + 2 * usize::from(byte(offset + 1)?),
        _ => return Err(FontError::Unsupported("CFF encoding format")),
    };
    if format & 0x80 != 0 {
        len += 1 + 3 * usize::from(byte(offset + len)?);
    }
    Ok(len)
}

fn fd_select_len(cff: &[u8], offset: usize, glyphs: usize) -> Result<usize> {
    match cff.get(offset) {
        Some(0) => Ok(1 + glyphs),
        Some(3) => {
            let ranges = read_u16(cff, offset + 1).ok_or(FontError::Truncated("CFF FDSelect"))?;
            Ok(3 + 3 * usize::from(ranges) + 2)
        }
        Some(_) => Err(FontError::Malformed("CFF FDSelect format")),
        None => Err(FontError::Truncated("CFF FDSelect")),
    }
}

/// A Private DICT and its local subroutines.
struct Private<'a> {
    entries: Vec<Entry<'a>>,
    subrs: Option<Index>,
}

impl<'a> Private<'a> {
    /// The Private DICT a Top or Font DICT points to, if any.
    fn of(cff: &'a [u8], dict: &[Entry<'_>]) -> Result<Option<Private<'a>>> {
        let (Some(size), Some(offset)) =
            (operand(dict, OP_PRIVATE, 0), operand(dict, OP_PRIVATE, 1))
        else {
            return Ok(None);
        };
        let data = cff
            .get(offset..offset.saturating_add(size))
            .ok_or(FontError::Truncated("CFF Private DICT"))?;
        let entries = parse_dict(data)?;
        let subrs = match operand(&entries, OP_SUBRS, 0) {
            Some(rel) if rel > 0 => Some(Index::parse(cff, offset + rel)?),
            _ => None,
        };
        Ok(Some(Private { entries, subrs }))
    }
}

enum Flow {
    Continue,
    End,
}

/// Follows a Type 2 charstring's subroutine calls, recording each subroutine it reaches and
/// the StandardEncoding codes an `endchar` accent composition names.
struct Walker<'a, 'u> {
    cff: &'a [u8],
    global: &'a Index,
    local: Option<&'a Index>,
    used_global: &'u mut BTreeSet<u32>,
    used_local: &'u mut BTreeSet<u32>,
    seac: Vec<u8>,
    stack: Vec<f64>,
    stems: usize,
    work: usize,
}

impl Walker<'_, '_> {
    /// `None` when the charstring computes operands (arithmetic or storage operators) or
    /// cannot be read, so the calls it makes are unknown.
    fn run(&mut self, code: &[u8], depth: usize) -> Option<Flow> {
        if depth > MAX_SUBR_DEPTH {
            return None;
        }
        let mut pos = 0;
        while pos < code.len() {
            self.work += 1;
            if self.work > MAX_WORK {
                return None;
            }
            let b0 = code[pos];
            pos += 1;
            match b0 {
                28 => {
                    let v = i16::from_be_bytes([*code.get(pos)?, *code.get(pos + 1)?]);
                    pos += 2;
                    self.stack.push(f64::from(v));
                }
                32..=246 => self.stack.push(f64::from(b0) - 139.0),
                247..=250 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    self.stack
                        .push(f64::from(b0 - 247) * 256.0 + f64::from(b1) + 108.0);
                }
                251..=254 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    self.stack
                        .push(-f64::from(b0 - 251) * 256.0 - f64::from(b1) - 108.0);
                }
                255 => {
                    let v = i32::from_be_bytes(code.get(pos..pos + 4)?.try_into().ok()?);
                    pos += 4;
                    self.stack.push(f64::from(v) / 65536.0);
                }
                // hstem, vstem, hstemhm, vstemhm: a pair of operands per stem.
                1 | 3 | 18 | 23 => {
                    self.stems += self.stack.len() / 2;
                    self.stack.clear();
                }
                // hintmask, cntrmask: pending operands are vstems; one mask bit per stem.
                19 | 20 => {
                    self.stems += self.stack.len() / 2;
                    self.stack.clear();
                    pos += self.stems.div_ceil(8);
                }
                10 | 29 => {
                    let subrs = if b0 == 10 { self.local? } else { self.global };
                    let index = self.stack.pop()?;
                    let n = u32::try_from(index as i64 + subr_bias(subrs.count())).ok()?;
                    let body = subrs.get(self.cff, n)?;
                    if b0 == 10 {
                        self.used_local.insert(n);
                    } else {
                        self.used_global.insert(n);
                    }
                    if let Flow::End = self.run(body, depth + 1)? {
                        return Some(Flow::End);
                    }
                }
                11 => return Some(Flow::Continue),
                14 => {
                    if let [.., base, accent] = self.stack[..]
                        && self.stack.len() >= 4
                    {
                        self.seac.extend([base as u8, accent as u8]);
                    }
                    return Some(Flow::End);
                }
                12 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    match b1 {
                        // dotsection, hflex, flex, hflex1, flex1
                        0 | 34..=37 => self.stack.clear(),
                        _ => return None,
                    }
                }
                _ => self.stack.clear(),
            }
        }
        Some(Flow::Continue)
    }
}

/// Offsets in the subset of each section placed after the Global Subrs INDEX.
struct Layout {
    charset: Option<usize>,
    encoding: Option<usize>,
    fd_select: Option<usize>,
    charstrings: usize,
    fd_array: Option<usize>,
    privates: Vec<Option<usize>>,
}

/// A subset of the first font of the CFF data `cff` that keeps the outlines of `keep`, of
/// glyph 0, and of the glyphs they compose with `endchar` accents; `seac_glyph` maps a
/// StandardEncoding code to a glyph.
pub(crate) fn subset_cff(
    cff: &[u8],
    keep: &BTreeSet<u16>,
    seac_glyph: &dyn Fn(u8) -> Option<u16>,
) -> Result<Vec<u8>> {
    let header_size = usize::from(*cff.get(2).ok_or(FontError::Truncated("CFF header"))?);
    let names = Index::parse(cff, header_size)?;
    let top_index = Index::parse(cff, names.end())?;
    let strings = Index::parse(cff, top_index.end())?;
    let global = Index::parse(cff, strings.end())?;
    let top = parse_dict(
        top_index
            .get(cff, 0)
            .ok_or(FontError::Missing("CFF Top DICT"))?,
    )?;
    let charstrings = Index::parse(
        cff,
        operand(&top, OP_CHARSTRINGS, 0).ok_or(FontError::Missing("CFF CharStrings"))?,
    )?;
    let glyphs = charstrings.count() as usize;
    let cid = entry(&top, OP_ROS).is_some();
    let font_dicts = if cid {
        let fd_array = Index::parse(
            cff,
            operand(&top, OP_FDARRAY, 0).ok_or(FontError::Missing("CFF FDArray"))?,
        )?;
        (0..fd_array.count())
            .map(|i| {
                parse_dict(
                    fd_array
                        .get(cff, i)
                        .ok_or(FontError::Malformed("CFF FDArray entry"))?,
                )
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let privates = if cid {
        font_dicts
            .iter()
            .map(|fd| Private::of(cff, fd))
            .collect::<Result<Vec<_>>>()?
    } else {
        vec![Private::of(cff, &top)?]
    };
    let select_at = if cid {
        Some(operand(&top, OP_FDSELECT, 0).ok_or(FontError::Missing("CFF FDSelect"))?)
    } else {
        None
    };
    let select = match select_at {
        Some(at) => parse_fd_select(cff, at, glyphs)?,
        None => Vec::new(),
    };

    let mut kept: BTreeSet<u16> = keep
        .iter()
        .copied()
        .filter(|&g| usize::from(g) < glyphs)
        .collect();
    kept.insert(0);
    let mut used_global = BTreeSet::new();
    let mut used_local = vec![BTreeSet::new(); privates.len()];
    let mut known = true;
    let mut queue: Vec<u16> = kept.iter().copied().collect();
    while let Some(gid) = queue.pop() {
        let code = charstrings
            .get(cff, u32::from(gid))
            .ok_or(FontError::Missing("CFF glyph"))?;
        let fd = select
            .get(usize::from(gid))
            .map_or(0, |&fd| usize::from(fd));
        let private = privates
            .get(fd)
            .ok_or(FontError::Malformed("CFF FDSelect entry"))?;
        let mut walker = Walker {
            cff,
            global: &global,
            local: private.as_ref().and_then(|p| p.subrs.as_ref()),
            used_global: &mut used_global,
            used_local: &mut used_local[fd],
            seac: Vec::new(),
            stack: Vec::new(),
            stems: 0,
            work: 0,
        };
        known &= walker.run(code, 0).is_some();
        for code in walker.seac {
            if let Some(g) = seac_glyph(code)
                && usize::from(g) < glyphs
                && kept.insert(g)
            {
                queue.push(g);
            }
        }
    }

    let subrs = |index: &Index, used: &BTreeSet<u32>| -> Vec<&[u8]> {
        (0..index.count())
            .map(|i| match index.get(cff, i) {
                Some(body) if !known || used.contains(&i) => body,
                _ => RETURN,
            })
            .collect()
    };
    let name_index = write_index(&[names.get(cff, 0).ok_or(FontError::Missing("CFF name"))?])?;
    let string_index = &cff[top_index.end()..strings.end()];
    let global_index = write_index(&subrs(&global, &used_global))?;
    let custom = |op: u16, predefined: usize| operand(&top, op, 0).filter(|&at| at > predefined);
    let charset = match custom(OP_CHARSET, 2) {
        Some(at) => Some(&cff[at..at + charset_len(cff, at, glyphs)?]),
        None => None,
    };
    let encoding = match custom(OP_ENCODING, 1).filter(|_| !cid) {
        Some(at) => Some(&cff[at..at + encoding_len(cff, at)?]),
        None => None,
    };
    let fd_select = match select_at {
        Some(at) => Some(&cff[at..at + fd_select_len(cff, at, glyphs)?]),
        None => None,
    };
    let glyph_items: Vec<&[u8]> = (0..charstrings.count())
        .map(|g| match charstrings.get(cff, g) {
            Some(code) if kept.contains(&(g as u16)) => code,
            _ => ENDCHAR,
        })
        .collect();
    let charstrings_index = write_index(&glyph_items)?;
    // Each Private DICT is followed by its local subroutines, which its Subrs operand
    // addresses relative to the DICT's start.
    let mut blocks = Vec::new();
    for (private, used) in privates.iter().zip(&used_local) {
        let Some(private) = private else {
            blocks.push(None);
            continue;
        };
        let block = match &private.subrs {
            Some(index) => {
                let len = write_dict(&private.entries, &[(OP_SUBRS, vec![0])], &[])?.len();
                let mut block = write_dict(&private.entries, &[(OP_SUBRS, vec![len])], &[])?;
                block.extend(write_index(&subrs(index, used))?);
                (block, len)
            }
            None => {
                let block = write_dict(&private.entries, &[], &[OP_SUBRS])?;
                let len = block.len();
                (block, len)
            }
        };
        blocks.push(Some(block));
    }

    let top_values = |layout: &Layout| -> Vec<(u16, Vec<usize>)> {
        let mut values = vec![(OP_CHARSTRINGS, vec![layout.charstrings])];
        values.extend(layout.charset.map(|at| (OP_CHARSET, vec![at])));
        values.extend(layout.encoding.map(|at| (OP_ENCODING, vec![at])));
        values.extend(layout.fd_select.map(|at| (OP_FDSELECT, vec![at])));
        values.extend(layout.fd_array.map(|at| (OP_FDARRAY, vec![at])));
        if !cid
            && let (Some(Some(at)), Some(Some((_, len)))) =
                (layout.privates.first(), blocks.first())
        {
            values.push((OP_PRIVATE, vec![*len, *at]));
        }
        values
    };
    let font_dict_bytes = |layout: &Layout| -> Result<Vec<Vec<u8>>> {
        font_dicts
            .iter()
            .zip(&layout.privates)
            .zip(&blocks)
            .map(|((fd, at), block)| match (at, block) {
                (Some(at), Some((_, len))) => write_dict(fd, &[(OP_PRIVATE, vec![*len, *at])], &[]),
                _ => write_dict(fd, &[], &[OP_PRIVATE]),
            })
            .collect()
    };
    // Five-byte operands fix the DICTs' lengths, so a placeholder layout sizes them.
    let placeholder = Layout {
        charset: charset.map(|_| 0),
        encoding: encoding.map(|_| 0),
        fd_select: fd_select.map(|_| 0),
        charstrings: 0,
        fd_array: cid.then_some(0),
        privates: blocks.iter().map(|b| b.as_ref().map(|_| 0)).collect(),
    };
    // A CID-keyed font has no encoding; a stray Encoding offset would point into the new data.
    let top_drop: &[u16] = if cid { &[OP_ENCODING] } else { &[] };
    let top_len = write_index(&[&write_dict(&top, &top_values(&placeholder), top_drop)?])?.len();
    let fd_array_len = if cid {
        let dicts = font_dict_bytes(&placeholder)?;
        write_index(&dicts.iter().map(Vec::as_slice).collect::<Vec<_>>())?.len()
    } else {
        0
    };
    let mut pos = 4 + name_index.len() + top_len + string_index.len() + global_index.len();
    let mut place = |len: usize| {
        let at = pos;
        pos += len;
        at
    };
    let layout = Layout {
        charset: charset.map(|c| place(c.len())),
        encoding: encoding.map(|e| place(e.len())),
        fd_select: fd_select.map(|f| place(f.len())),
        charstrings: place(charstrings_index.len()),
        fd_array: cid.then(|| place(fd_array_len)),
        privates: blocks
            .iter()
            .map(|b| b.as_ref().map(|(block, _)| place(block.len())))
            .collect(),
    };

    let mut out = vec![1, 0, 4, 4];
    out.extend(name_index);
    out.extend(write_index(&[&write_dict(
        &top,
        &top_values(&layout),
        top_drop,
    )?])?);
    out.extend_from_slice(string_index);
    out.extend(global_index);
    for section in [charset, encoding, fd_select].into_iter().flatten() {
        out.extend_from_slice(section);
    }
    out.extend(charstrings_index);
    if cid {
        let dicts = font_dict_bytes(&layout)?;
        out.extend(write_index(
            &dicts.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )?);
    }
    for (block, _) in blocks.into_iter().flatten() {
        out.extend(block);
    }
    debug_assert_eq!(out.len(), pos);
    Ok(out)
}
