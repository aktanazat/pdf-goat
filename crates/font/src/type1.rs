//! Type 1 font programs: PFA, PFB, and PDF `FontFile` streams, with eexec and charstring decryption.

use std::collections::HashMap;

use crate::encoding::BaseEncoding;
use crate::error::{FontError, Result};
use crate::outline::{Outline, OutlineBuilder};
use crate::ps::{Lexer, Token};

const EEXEC_KEY: u16 = 55665;
const CHARSTRING_KEY: u16 = 4330;
const C1: u16 = 52845;
const C2: u16 = 22719;

/// Most subroutines accepted in `/Subrs`.
const MAX_SUBRS: usize = 65_536;
/// Most glyphs accepted in `/CharStrings`.
const MAX_GLYPHS: usize = 65_535;
const MAX_STACK: usize = 64;
const MAX_SUBR_DEPTH: u32 = 16;
const MAX_WORK: u32 = 1_000_000;

/// Decrypts eexec or charstring data with key `r`, dropping the first `skip` plain bytes.
pub(crate) fn decrypt(data: &[u8], mut r: u16, skip: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &c in data {
        out.push(c ^ (r >> 8) as u8);
        r = u16::from(c)
            .wrapping_add(r)
            .wrapping_mul(C1)
            .wrapping_add(C2);
    }
    out.drain(..skip.min(out.len()));
    out
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0C | 0)
}

/// Cleartext part and decrypted private part of a Type 1 program.
fn split_program(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    if data.first() == Some(&0x80) {
        return split_pfb(data);
    }
    split_pfa(data)
}

/// Cleartext followed by `eexec` and the encrypted part, in binary or hex.
fn split_pfa(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let pos = find(data, b"eexec").ok_or(FontError::Malformed("Type 1 program without eexec"))?;
    let clear = data[..pos].to_vec();
    let mut start = pos + 5;
    while data.get(start).is_some_and(|&b| is_space(b)) {
        start += 1;
    }
    let encrypted = &data[start..];
    let hex = encrypted.len() >= 4 && encrypted[..4].iter().all(|&b| is_hex(b));
    let binary = if hex {
        decode_hex(encrypted)
    } else {
        encrypted.to_vec()
    };
    Ok((clear, decrypt(&binary, EEXEC_KEY, 4)))
}

fn split_pfb(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut clear = Vec::new();
    let mut binary = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        if data[pos] != 0x80 {
            return Err(FontError::Malformed("PFB segment marker"));
        }
        let kind = *data
            .get(pos + 1)
            .ok_or(FontError::Truncated("PFB segment header"))?;
        if kind == 3 {
            break;
        }
        let len = data
            .get(pos + 2..pos + 6)
            .ok_or(FontError::Truncated("PFB segment header"))?;
        let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let body = data
            .get(pos + 6..(pos + 6).saturating_add(len))
            .ok_or(FontError::Truncated("PFB segment"))?;
        match kind {
            1 if binary.is_empty() => clear.extend_from_slice(body),
            1 => {}
            2 => binary.extend_from_slice(body),
            _ => return Err(FontError::Malformed("PFB segment type")),
        }
        pos += 6 + len;
    }
    if binary.is_empty() {
        // Some PFB files keep the eexec section inside an ASCII segment.
        return split_pfa(&clear);
    }
    let hex = binary.len() >= 4 && binary[..4].iter().all(|&b| is_hex(b));
    let binary = if hex { decode_hex(&binary) } else { binary };
    Ok((clear, decrypt(&binary, EEXEC_KEY, 4)))
}

fn decode_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut high: Option<u8> = None;
    for &b in data {
        let v = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ if is_space(b) => continue,
            _ => break,
        };
        match high.take() {
            Some(h) => out.push(h << 4 | v),
            None => high = Some(v),
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn number(tok: &Token<'_>) -> Option<f64> {
    match tok {
        Token::Int(v) => Some(*v as f64),
        Token::Real(v) => Some(*v),
        _ => None,
    }
}

/// The cleartext part of a Type 1 program: names, metrics, and the built-in encoding.
#[derive(Debug, Clone)]
pub(crate) struct Type1Header {
    pub(crate) font_name: Option<String>,
    pub(crate) family_name: Option<String>,
    pub(crate) full_name: Option<String>,
    pub(crate) weight: Option<String>,
    pub(crate) font_matrix: [f64; 6],
    pub(crate) bbox: [f64; 4],
    pub(crate) italic_angle: f64,
    pub(crate) is_fixed_pitch: bool,
    /// Built-in encoding: glyph name by code.
    pub(crate) encoding: Vec<Option<String>>,
}

/// A parsed Type 1 font.
#[derive(Debug, Clone)]
pub(crate) struct Type1 {
    pub(crate) header: Type1Header,
    glyphs: Vec<(String, Vec<u8>)>,
    by_name: HashMap<String, u16>,
    subrs: Vec<Option<Vec<u8>>>,
}

impl Type1Header {
    /// Parses the cleartext dictionary. A PFB segment header at the start is skipped, so a
    /// file prefix works too.
    pub(crate) fn parse(clear: &[u8]) -> Type1Header {
        let clear = if clear.first() == Some(&0x80) {
            clear.get(6..).unwrap_or(&[])
        } else {
            clear
        };
        let mut h = Type1Header {
            font_name: None,
            family_name: None,
            full_name: None,
            weight: None,
            font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
            bbox: [0.0; 4],
            italic_angle: 0.0,
            is_fixed_pitch: false,
            encoding: vec![None; 256],
        };
        let mut lx = Lexer::new(clear);
        while let Some(tok) = lx.next_token() {
            if tok == Token::Word(b"eexec") {
                break;
            }
            let Token::Name(key) = tok else { continue };
            match key {
                b"FontName" => {
                    if let Some(Token::Name(n)) = lx.next_token() {
                        h.font_name = Some(String::from_utf8_lossy(n).into_owned());
                    }
                }
                b"FamilyName" | b"FullName" | b"Weight" => {
                    if let Some(Token::Str(s)) = lx.next_token() {
                        let text = Some(String::from_utf8_lossy(&s).into_owned());
                        match key {
                            b"FamilyName" => h.family_name = text,
                            b"FullName" => h.full_name = text,
                            _ => h.weight = text,
                        }
                    }
                }
                b"ItalicAngle" => {
                    if let Some(v) = lx.next_token().as_ref().and_then(number) {
                        h.italic_angle = v;
                    }
                }
                b"isFixedPitch" => {
                    h.is_fixed_pitch = lx.next_token() == Some(Token::Word(b"true"));
                }
                b"FontMatrix" | b"FontBBox" => {
                    let values = read_number_array(&mut lx);
                    if key == b"FontMatrix" {
                        if let Ok(m) = <[f64; 6]>::try_from(values.as_slice())
                            && m[0] * m[3] - m[1] * m[2] != 0.0
                        {
                            h.font_matrix = m;
                        }
                    } else if let Ok(b) = <[f64; 4]>::try_from(values.as_slice()) {
                        h.bbox = b;
                    }
                }
                b"Encoding" => h.parse_encoding(&mut lx),
                _ => {}
            }
        }
        h
    }

    fn parse_encoding(&mut self, lx: &mut Lexer<'_>) {
        match lx.next_token() {
            Some(Token::Word(b"StandardEncoding")) => {
                for code in 0..=255u8 {
                    self.encoding[usize::from(code)] =
                        BaseEncoding::Standard.glyph_name(code).map(str::to_owned);
                }
            }
            Some(Token::Int(_)) => {
                let mut recent: Vec<Token<'_>> = Vec::new();
                while let Some(tok) = lx.next_token() {
                    if tok == Token::Word(b"def") {
                        break;
                    }
                    if tok == Token::Word(b"put")
                        && let [.., Token::Word(b"dup"), Token::Int(code), Token::Name(name)] =
                            recent.as_slice()
                        && let Ok(code) = u8::try_from(*code)
                    {
                        self.encoding[usize::from(code)] =
                            Some(String::from_utf8_lossy(name).into_owned());
                    }
                    recent.push(tok);
                    if recent.len() > 3 {
                        recent.remove(0);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Type1 {
    pub(crate) fn parse(data: &[u8]) -> Result<Type1> {
        let (clear, private) = split_program(data)?;
        let mut font = Type1 {
            header: Type1Header::parse(&clear),
            glyphs: Vec::new(),
            by_name: HashMap::new(),
            subrs: Vec::new(),
        };
        font.parse_private(&private)?;
        if font.glyphs.is_empty() {
            return Err(FontError::Missing("Type 1 CharStrings"));
        }
        Ok(font)
    }

    fn parse_private(&mut self, private: &[u8]) -> Result<()> {
        let mut lx = Lexer::new(private);
        let mut len_iv: i64 = 4;
        let mut rd_words: Vec<Vec<u8>> = vec![b"RD".to_vec(), b"-|".to_vec()];
        let mut raw_glyphs: Vec<(String, &[u8])> = Vec::new();
        let mut raw_subrs: Vec<(usize, &[u8])> = Vec::new();
        let mut recent: Vec<Token<'_>> = Vec::new();
        while let Some(tok) = lx.next_token() {
            match &tok {
                Token::Word(w) if rd_words.iter().any(|r| r.as_slice() == *w) => {
                    if let [.., prev2, Token::Int(len)] = recent.as_slice()
                        && let Ok(len) = usize::try_from(*len)
                    {
                        // One separator byte follows the RD word, then the charstring bytes.
                        lx.pos += 1;
                        let Some(bytes) = lx.raw(len) else { break };
                        match prev2 {
                            Token::Int(index) => {
                                if let Ok(i) = usize::try_from(*index)
                                    && i < MAX_SUBRS
                                {
                                    raw_subrs.push((i, bytes));
                                }
                            }
                            Token::Name(name) => {
                                if raw_glyphs.len() >= MAX_GLYPHS {
                                    return Err(FontError::LimitExceeded("Type 1 glyph count"));
                                }
                                raw_glyphs
                                    .push((String::from_utf8_lossy(name).into_owned(), bytes));
                            }
                            _ => {}
                        }
                        recent.clear();
                        continue;
                    }
                }
                Token::Word(b"string") => {
                    // `/name {string currentfile exch readstring pop} def` defines an RD alias.
                    if let [.., Token::Name(n), Token::OpenProc] = recent.as_slice() {
                        rd_words.push(n.to_vec());
                    }
                }
                Token::Int(v) => {
                    if let [.., Token::Name(b"lenIV")] = recent.as_slice() {
                        len_iv = *v;
                    }
                }
                _ => {}
            }
            recent.push(tok);
            if recent.len() > 4 {
                recent.remove(0);
            }
        }
        let skip = |b: &[u8]| -> Vec<u8> {
            if len_iv < 0 {
                b.to_vec()
            } else {
                decrypt(b, CHARSTRING_KEY, len_iv as usize)
            }
        };
        let max_subr = raw_subrs.iter().map(|(i, _)| *i + 1).max().unwrap_or(0);
        self.subrs = vec![None; max_subr];
        for (i, bytes) in raw_subrs {
            self.subrs[i] = Some(skip(bytes));
        }
        // .notdef goes first so that glyph 0 is the notdef glyph.
        if let Some(p) = raw_glyphs.iter().position(|(n, _)| n == ".notdef") {
            let notdef = raw_glyphs.remove(p);
            raw_glyphs.insert(0, notdef);
        }
        for (name, bytes) in raw_glyphs {
            if self.by_name.contains_key(&name) {
                continue;
            }
            self.by_name.insert(name.clone(), self.glyphs.len() as u16);
            self.glyphs.push((name, skip(bytes)));
        }
        Ok(())
    }

    pub(crate) fn num_glyphs(&self) -> u16 {
        self.glyphs.len() as u16
    }

    pub(crate) fn glyph_name(&self, gid: u16) -> Option<&str> {
        self.glyphs.get(usize::from(gid)).map(|(n, _)| n.as_str())
    }

    pub(crate) fn glyph_by_name(&self, name: &str) -> Option<u16> {
        self.by_name.get(name).copied()
    }

    /// Outline and advance width of a glyph in character space units.
    pub(crate) fn glyph(&self, gid: u16) -> Result<(Outline, f64)> {
        self.glyph_inner(gid, true)
    }

    fn glyph_inner(&self, gid: u16, allow_seac: bool) -> Result<(Outline, f64)> {
        let (_, code) = self
            .glyphs
            .get(usize::from(gid))
            .ok_or(FontError::Missing("Type 1 glyph"))?;
        let mut m = Machine {
            font: self,
            stack: Vec::new(),
            results: Vec::new(),
            b: OutlineBuilder::new(),
            x: 0.0,
            y: 0.0,
            lsb: 0.0,
            width: 0.0,
            flex: None,
            seac: None,
            work: 0,
        };
        m.exec(code, 0)?;
        let width = m.width;
        let mut outline = m.b.finish()?;
        if let Some(s) = m.seac
            && allow_seac
        {
            let gid_of = |code: f64| {
                let code = u8::try_from(code as i64).ok()?;
                self.glyph_by_name(BaseEncoding::Standard.glyph_name(code)?)
            };
            let base = gid_of(s.bchar).ok_or(FontError::Missing("seac base glyph"))?;
            let accent = gid_of(s.achar).ok_or(FontError::Missing("seac accent glyph"))?;
            let (base_outline, _) = self.glyph_inner(base, false)?;
            let (mut accent_outline, _) = self.glyph_inner(accent, false)?;
            // The accent origin sits at (adx - asb) from the composite's left sidebearing point.
            accent_outline.transform(&[1.0, 0.0, 0.0, 1.0, s.adx + m.lsb - s.asb, s.ady]);
            let mut combined = base_outline;
            combined.append(outline);
            combined.append(accent_outline);
            outline = combined;
        }
        Ok((outline, width))
    }
}

fn read_number_array(lx: &mut Lexer<'_>) -> Vec<f64> {
    let mut out = Vec::new();
    match lx.next_token() {
        Some(Token::OpenArray | Token::OpenProc) => {}
        _ => return out,
    }
    while let Some(tok) = lx.next_token() {
        match tok {
            Token::CloseArray | Token::CloseProc => break,
            t => match number(&t) {
                Some(v) if out.len() < 16 => out.push(v),
                _ => break,
            },
        }
    }
    out
}

struct SeacArgs {
    asb: f64,
    adx: f64,
    ady: f64,
    bchar: f64,
    achar: f64,
}

struct Machine<'a> {
    font: &'a Type1,
    stack: Vec<f64>,
    /// Values returned by the last `callothersubr`, taken by `pop` in order.
    results: Vec<f64>,
    b: OutlineBuilder,
    x: f64,
    y: f64,
    lsb: f64,
    width: f64,
    flex: Option<Vec<(f64, f64)>>,
    seac: Option<SeacArgs>,
    work: u32,
}

enum Flow {
    Continue,
    Return,
    End,
}

impl Machine<'_> {
    fn push(&mut self, v: f64) -> Result<()> {
        if self.stack.len() >= MAX_STACK {
            return Err(FontError::LimitExceeded("Type 1 operand stack"));
        }
        self.stack.push(v);
        Ok(())
    }

    fn pop(&mut self) -> Result<f64> {
        self.stack
            .pop()
            .ok_or(FontError::Malformed("Type 1 operand stack underflow"))
    }

    fn args<const N: usize>(&mut self) -> Result<[f64; N]> {
        if self.stack.len() < N {
            return Err(FontError::Malformed("Type 1 operand stack underflow"));
        }
        let start = self.stack.len() - N;
        let mut out = [0.0; N];
        out.copy_from_slice(&self.stack[start..]);
        self.stack.clear();
        Ok(out)
    }

    fn move_rel(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.x += dx;
        self.y += dy;
        if self.flex.is_some() {
            return Ok(());
        }
        self.b.move_to(self.x as f32, self.y as f32)
    }

    fn line_rel(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.x += dx;
        self.y += dy;
        self.b.line_to(self.x as f32, self.y as f32)
    }

    fn curve_rel(&mut self, d: [f64; 6]) -> Result<()> {
        let c1 = (self.x + d[0], self.y + d[1]);
        let c2 = (c1.0 + d[2], c1.1 + d[3]);
        self.x = c2.0 + d[4];
        self.y = c2.1 + d[5];
        self.b.curve_to(
            (c1.0 as f32, c1.1 as f32),
            (c2.0 as f32, c2.1 as f32),
            self.x as f32,
            self.y as f32,
        )
    }

    fn exec(&mut self, code: &[u8], depth: u32) -> Result<Flow> {
        let mut i = 0usize;
        while i < code.len() {
            self.work += 1;
            if self.work > MAX_WORK {
                return Err(FontError::LimitExceeded("Type 1 charstring work"));
            }
            let v = code[i];
            i += 1;
            let next = |i: &mut usize| -> Result<u8> {
                let b = *code
                    .get(*i)
                    .ok_or(FontError::Truncated("Type 1 charstring"))?;
                *i += 1;
                Ok(b)
            };
            match v {
                32..=246 => self.push(f64::from(v) - 139.0)?,
                247..=250 => {
                    let w = next(&mut i)?;
                    self.push(f64::from(v - 247) * 256.0 + f64::from(w) + 108.0)?;
                }
                251..=254 => {
                    let w = next(&mut i)?;
                    self.push(-f64::from(v - 251) * 256.0 - f64::from(w) - 108.0)?;
                }
                255 => {
                    let bytes = [next(&mut i)?, next(&mut i)?, next(&mut i)?, next(&mut i)?];
                    self.push(f64::from(i32::from_be_bytes(bytes)))?;
                }
                1 | 3 => self.stack.clear(),
                4 => {
                    let [dy] = self.args()?;
                    self.move_rel(0.0, dy)?;
                }
                5 => {
                    let [dx, dy] = self.args()?;
                    self.line_rel(dx, dy)?;
                }
                6 => {
                    let [dx] = self.args()?;
                    self.line_rel(dx, 0.0)?;
                }
                7 => {
                    let [dy] = self.args()?;
                    self.line_rel(0.0, dy)?;
                }
                8 => {
                    let d = self.args::<6>()?;
                    self.curve_rel(d)?;
                }
                9 => {
                    self.stack.clear();
                    self.b.close()?;
                }
                10 => {
                    if depth >= MAX_SUBR_DEPTH {
                        return Err(FontError::LimitExceeded("Type 1 subroutine depth"));
                    }
                    let n = self.pop()?;
                    let font = self.font;
                    let subr = usize::try_from(n as i64)
                        .ok()
                        .and_then(|n| font.subrs.get(n))
                        .and_then(|s| s.as_deref())
                        .ok_or(FontError::Malformed("Type 1 subroutine index"))?;
                    if let Flow::End = self.exec(subr, depth + 1)? {
                        return Ok(Flow::End);
                    }
                }
                11 => return Ok(Flow::Return),
                13 => {
                    let [sbx, wx] = self.args()?;
                    self.lsb = sbx;
                    self.width = wx;
                    self.x = sbx;
                    self.y = 0.0;
                }
                14 => {
                    self.stack.clear();
                    self.b.close()?;
                    return Ok(Flow::End);
                }
                21 => {
                    let [dx, dy] = self.args()?;
                    self.move_rel(dx, dy)?;
                }
                22 => {
                    let [dx] = self.args()?;
                    self.move_rel(dx, 0.0)?;
                }
                30 => {
                    let [dy1, dx2, dy2, dx3] = self.args()?;
                    self.curve_rel([0.0, dy1, dx2, dy2, dx3, 0.0])?;
                }
                31 => {
                    let [dx1, dx2, dy2, dy3] = self.args()?;
                    self.curve_rel([dx1, 0.0, dx2, dy2, 0.0, dy3])?;
                }
                12 => {
                    let op = next(&mut i)?;
                    if let Flow::End = self.escape(op)? {
                        return Ok(Flow::End);
                    }
                }
                _ => self.stack.clear(),
            }
        }
        Ok(Flow::Continue)
    }

    fn escape(&mut self, op: u8) -> Result<Flow> {
        match op {
            // dotsection, vstem3, hstem3
            0..=2 => self.stack.clear(),
            6 => {
                let [asb, adx, ady, bchar, achar] = self.args()?;
                self.seac = Some(SeacArgs {
                    asb,
                    adx,
                    ady,
                    bchar,
                    achar,
                });
                self.b.close()?;
                return Ok(Flow::End);
            }
            7 => {
                let [sbx, sby, wx, _wy] = self.args()?;
                self.lsb = sbx;
                self.width = wx;
                self.x = sbx;
                self.y = sby;
            }
            12 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(if b == 0.0 { 0.0 } else { a / b })?;
            }
            16 => self.call_other_subr()?,
            17 => {
                let v = if self.results.is_empty() {
                    0.0
                } else {
                    self.results.remove(0)
                };
                self.push(v)?;
            }
            33 => {
                let [x, y] = self.args()?;
                self.x = x;
                self.y = y;
            }
            _ => self.stack.clear(),
        }
        Ok(Flow::Continue)
    }

    fn call_other_subr(&mut self) -> Result<()> {
        let index = self.pop()? as i64;
        let n = self.pop()? as i64;
        let n = usize::try_from(n)
            .map_err(|_| FontError::Malformed("Type 1 othersubr argument count"))?;
        if n > self.stack.len() {
            return Err(FontError::Malformed("Type 1 othersubr argument count"));
        }
        let args = self.stack.split_off(self.stack.len() - n);
        self.results.clear();
        match index {
            0 => {
                let points = self.flex.take().unwrap_or_default();
                if let [_, p1, p2, p3, p4, p5, p6] = points.as_slice() {
                    let c = |p: &(f64, f64)| (p.0 as f32, p.1 as f32);
                    self.b.curve_to(c(p1), c(p2), p3.0 as f32, p3.1 as f32)?;
                    self.b.curve_to(c(p4), c(p5), p6.0 as f32, p6.1 as f32)?;
                    self.x = p6.0;
                    self.y = p6.1;
                } else {
                    return Err(FontError::Malformed("Type 1 flex point count"));
                }
                self.results = vec![self.x, self.y];
            }
            1 => {
                self.flex = Some(Vec::with_capacity(7));
            }
            2 => {
                if let Some(points) = self.flex.as_mut()
                    && points.len() < 16
                {
                    points.push((self.x, self.y));
                }
            }
            _ => self.results = args,
        }
        Ok(())
    }
}
