//! CMaps: code-to-CID mappings (embedded and predefined), ToUnicode maps, and CID-to-Unicode tables.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use crate::error::{FontError, Result};
use crate::glyphnames::glyph_name_to_unicode;
use crate::predefined::{self, UnicodeRuns};
use crate::ps::{Lexer, Token};
use crate::sfnt::decode_utf16be;

/// Deepest chain of predefined `usecmap` references followed.
const MAX_USECMAP_DEPTH: u32 = 8;

/// A codespace range: codes of `len` bytes whose every byte lies between the matching bytes of
/// `low` and `high`. Bytes past `len` are zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodespaceRange {
    pub len: u8,
    pub low: [u8; 4],
    pub high: [u8; 4],
}

impl CodespaceRange {
    /// True when `bytes` has this range's length and every byte is within bounds.
    pub fn contains(&self, bytes: &[u8]) -> bool {
        bytes.len() == usize::from(self.len)
            && bytes
                .iter()
                .enumerate()
                .all(|(i, &b)| self.low[i] <= b && b <= self.high[i])
    }

    fn full(len: u8) -> CodespaceRange {
        let mut high = [0u8; 4];
        for h in high.iter_mut().take(usize::from(len)) {
            *h = 0xFF;
        }
        CodespaceRange {
            len,
            low: [0; 4],
            high,
        }
    }
}

/// The `/CIDSystemInfo` of an embedded CMap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CidSystemInfo {
    pub registry: String,
    pub ordering: String,
    pub supplement: i32,
}

/// One character code read from a string: its value (big-endian bytes) and byte length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CharCode {
    pub code: u32,
    pub len: u8,
}

/// Adobe character collections with predefined CMaps and CID-to-Unicode tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CidCollection {
    Japan1,
    Gb1,
    Cns1,
    Korea1,
}

static UNICODE_TABLES: [LazyLock<Option<UnicodeRuns>>; 4] = [
    LazyLock::new(|| predefined::load_unicode_runs(0).ok()),
    LazyLock::new(|| predefined::load_unicode_runs(1).ok()),
    LazyLock::new(|| predefined::load_unicode_runs(2).ok()),
    LazyLock::new(|| predefined::load_unicode_runs(3).ok()),
];

fn find_run(runs: &[(u32, u32, u32)], cid: u32) -> Option<char> {
    let i = runs.partition_point(|r| r.0 <= cid).checked_sub(1)?;
    let (start, count, first) = runs[i];
    if cid - start >= count {
        return None;
    }
    char::from_u32(first + (cid - start))
}

impl CidCollection {
    pub const ALL: [CidCollection; 4] = [
        CidCollection::Japan1,
        CidCollection::Gb1,
        CidCollection::Cns1,
        CidCollection::Korea1,
    ];

    /// Collection for a `/CIDSystemInfo` ordering such as `Japan1` or `GB1`.
    pub fn from_ordering(ordering: &str) -> Option<CidCollection> {
        match ordering {
            "Japan1" => Some(CidCollection::Japan1),
            "GB1" => Some(CidCollection::Gb1),
            "CNS1" => Some(CidCollection::Cns1),
            "Korea1" => Some(CidCollection::Korea1),
            _ => None,
        }
    }

    pub fn ordering(self) -> &'static str {
        match self {
            CidCollection::Japan1 => "Japan1",
            CidCollection::Gb1 => "GB1",
            CidCollection::Cns1 => "CNS1",
            CidCollection::Korea1 => "Korea1",
        }
    }

    fn index(self) -> usize {
        match self {
            CidCollection::Japan1 => 0,
            CidCollection::Gb1 => 1,
            CidCollection::Cns1 => 2,
            CidCollection::Korea1 => 3,
        }
    }

    /// Unicode for a CID of this collection; `vertical` selects vertical-form code points.
    pub fn cid_to_unicode(self, cid: u32, vertical: bool) -> Option<char> {
        let table = UNICODE_TABLES[self.index()].as_ref()?;
        if vertical && let Some(c) = find_run(&table.vertical, cid) {
            return Some(c);
        }
        find_run(&table.horizontal, cid)
    }
}

/// Values stored per code range; `shift` gives the value for a code `by` past the range start.
trait Shift: Clone {
    fn shift(&self, by: u32) -> Self;
}

impl Shift for u32 {
    fn shift(&self, by: u32) -> u32 {
        self.saturating_add(by)
    }
}

/// Non-overlapping code ranges keyed by (code length, first code) with (last code, value).
type RangeMap<V> = BTreeMap<(u8, u32), (u32, V)>;

/// Inserts `lo..=hi`, replacing whatever part of earlier ranges it overlaps.
fn range_insert<V: Shift>(map: &mut RangeMap<V>, len: u8, lo: u32, hi: u32, value: V) {
    let mut overlapping = Vec::new();
    if let Some((&k, &(h, _))) = map.range(..(len, lo)).next_back()
        && k.0 == len
        && h >= lo
    {
        overlapping.push(k);
    }
    overlapping.extend(map.range((len, lo)..=(len, hi)).map(|(&k, _)| k));
    for k in overlapping {
        let Some((h, old)) = map.remove(&k) else {
            continue;
        };
        if k.1 < lo {
            map.insert(k, (lo - 1, old.clone()));
        }
        if h > hi {
            map.insert((len, hi + 1), (h, old.shift(hi + 1 - k.1)));
        }
    }
    map.insert((len, lo), (hi, value));
}

fn range_get<V>(map: &RangeMap<V>, len: u8, code: u32) -> Option<(&V, u32)> {
    let (&(l, lo), (hi, v)) = map.range(..=(len, code)).next_back()?;
    (l == len && code <= *hi).then_some((v, code - lo))
}

fn code_value(bytes: &[u8]) -> Option<(u8, u32)> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    let v = bytes.iter().fold(0u32, |acc, &b| acc << 8 | u32::from(b));
    Some((bytes.len() as u8, v))
}

/// A code-to-CID CMap.
#[derive(Debug, Clone, Default)]
pub struct CMap {
    name: Option<String>,
    vertical: bool,
    codespaces: Vec<CodespaceRange>,
    cids: RangeMap<u32>,
    notdefs: RangeMap<u32>,
    system_info: Option<CidSystemInfo>,
    collection: Option<CidCollection>,
    use_cmap: Option<String>,
}

impl CMap {
    /// Identity-H (or Identity-V): two-byte codes equal to their CIDs.
    pub fn identity(vertical: bool) -> CMap {
        let mut cids = RangeMap::new();
        cids.insert((2, 0), (0xFFFF, 0));
        CMap {
            name: Some(if vertical { "Identity-V" } else { "Identity-H" }.to_owned()),
            vertical,
            codespaces: vec![CodespaceRange::full(2)],
            cids,
            ..CMap::default()
        }
    }

    /// A predefined CMap by name: Identity-H/V, OneByteIdentityH/V, or one of the Adobe CJK CMaps.
    pub fn predefined(name: &str) -> Result<CMap> {
        match name {
            "Identity-H" => return Ok(CMap::identity(false)),
            "Identity-V" => return Ok(CMap::identity(true)),
            "OneByteIdentityH" | "OneByteIdentityV" => {
                let mut cids = RangeMap::new();
                cids.insert((1, 0), (0xFF, 0));
                return Ok(CMap {
                    name: Some(name.to_owned()),
                    vertical: name.ends_with('V'),
                    codespaces: vec![CodespaceRange::full(1)],
                    cids,
                    ..CMap::default()
                });
            }
            _ => {}
        }
        let index =
            predefined::find_code_cmap(name).ok_or(FontError::Missing("predefined CMap"))?;
        load_predefined(index, 0)
    }

    /// Parses an embedded CMap stream. A `usecmap` naming a predefined CMap is applied; any other
    /// name is left in [`CMap::use_cmap`] for the caller to resolve with [`CMap::with_base`].
    pub fn parse(data: &[u8]) -> Result<CMap> {
        let mut own = CMap::default();
        let mut base: Option<CMap> = None;
        let mut lx = Lexer::new(data);
        let mut key: Option<Vec<u8>> = None;
        let mut last_name: Option<Vec<u8>> = None;
        while let Some(tok) = lx.next_token() {
            match tok {
                Token::Word(b"begincodespacerange") => {
                    for [lo, hi] in read_block(&mut lx, b"endcodespacerange").as_chunks::<2>().0 {
                        if let (Token::Hex(lo), Token::Hex(hi)) = (lo, hi)
                            && lo.len() == hi.len()
                            && (1..=4).contains(&lo.len())
                        {
                            let mut range = CodespaceRange {
                                len: lo.len() as u8,
                                low: [0; 4],
                                high: [0; 4],
                            };
                            range.low[..lo.len()].copy_from_slice(lo);
                            range.high[..hi.len()].copy_from_slice(hi);
                            own.codespaces.push(range);
                        }
                    }
                }
                Token::Word(w @ (b"begincidrange" | b"beginnotdefrange")) => {
                    let (end, notdef): (&[u8], bool) = if w == b"begincidrange" {
                        (b"endcidrange", false)
                    } else {
                        (b"endnotdefrange", true)
                    };
                    for [lo, hi, cid] in read_block(&mut lx, end).as_chunks::<3>().0 {
                        if let (Token::Hex(lo), Token::Hex(hi), Token::Int(cid)) = (lo, hi, cid)
                            && let (Some((len, lo)), Some((hlen, hi))) =
                                (code_value(lo), code_value(hi))
                            && len == hlen
                            && lo <= hi
                            && let Ok(cid) = u32::try_from(*cid)
                        {
                            let map = if notdef {
                                &mut own.notdefs
                            } else {
                                &mut own.cids
                            };
                            range_insert(map, len, lo, hi, cid);
                        }
                    }
                }
                Token::Word(w @ (b"begincidchar" | b"beginnotdefchar")) => {
                    let (end, notdef): (&[u8], bool) = if w == b"begincidchar" {
                        (b"endcidchar", false)
                    } else {
                        (b"endnotdefchar", true)
                    };
                    for [code, cid] in read_block(&mut lx, end).as_chunks::<2>().0 {
                        if let (Token::Hex(code), Token::Int(cid)) = (code, cid)
                            && let Some((len, code)) = code_value(code)
                            && let Ok(cid) = u32::try_from(*cid)
                        {
                            let map = if notdef {
                                &mut own.notdefs
                            } else {
                                &mut own.cids
                            };
                            range_insert(map, len, code, code, cid);
                        }
                    }
                }
                Token::Word(b"beginbfchar") => {
                    read_block(&mut lx, b"endbfchar");
                }
                Token::Word(b"beginbfrange") => {
                    read_block(&mut lx, b"endbfrange");
                }
                Token::Word(b"usecmap") => {
                    if let Some(name) = last_name.take() {
                        let name = String::from_utf8_lossy(&name).into_owned();
                        match predefined_by_name(&name, 1) {
                            Some(b) => base = Some(b),
                            None => own.use_cmap = Some(name),
                        }
                    }
                }
                Token::Name(n) => {
                    if key.as_deref() == Some(b"CMapName") {
                        own.name = Some(String::from_utf8_lossy(n).into_owned());
                        key = None;
                    } else {
                        key = Some(n.to_vec());
                    }
                    last_name = Some(n.to_vec());
                    continue;
                }
                Token::Int(v) => match key.as_deref() {
                    Some(b"WMode") => own.vertical = v == 1,
                    Some(b"Supplement") => {
                        own.system_info.get_or_insert_default().supplement =
                            i32::try_from(v).unwrap_or(0);
                    }
                    _ => {}
                },
                Token::Str(s) => match key.as_deref() {
                    Some(b"Registry") => {
                        own.system_info.get_or_insert_default().registry =
                            String::from_utf8_lossy(&s).into_owned();
                    }
                    Some(b"Ordering") => {
                        own.system_info.get_or_insert_default().ordering =
                            String::from_utf8_lossy(&s).into_owned();
                    }
                    _ => {}
                },
                _ => {}
            }
            key = None;
        }
        if let Some(info) = &own.system_info
            && info.registry == "Adobe"
        {
            own.collection = CidCollection::from_ordering(&info.ordering);
        }
        let mut out = match base {
            Some(b) => own.with_base(&b),
            None => own,
        };
        if out.codespaces.is_empty() {
            let mut lens: Vec<u8> = out.cids.keys().map(|k| k.0).collect();
            lens.dedup();
            if lens.is_empty() {
                lens.push(2);
            }
            out.codespaces = lens.into_iter().map(CodespaceRange::full).collect();
        }
        Ok(out)
    }

    /// This CMap layered over `base`: mappings here take precedence, codespaces are combined.
    pub fn with_base(&self, base: &CMap) -> CMap {
        let mut out = base.clone();
        for (&(len, lo), &(hi, cid)) in &self.cids {
            range_insert(&mut out.cids, len, lo, hi, cid);
        }
        for (&(len, lo), &(hi, cid)) in &self.notdefs {
            range_insert(&mut out.notdefs, len, lo, hi, cid);
        }
        for cs in &self.codespaces {
            if !out.codespaces.contains(cs) {
                out.codespaces.push(*cs);
            }
        }
        out.name = self.name.clone().or(out.name);
        out.vertical = self.vertical;
        out.system_info = self.system_info.clone().or(out.system_info);
        out.collection = self.collection.or(out.collection);
        out.use_cmap = None;
        out
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn is_vertical(&self) -> bool {
        self.vertical
    }

    pub fn codespaces(&self) -> &[CodespaceRange] {
        &self.codespaces
    }

    pub fn cid_system_info(&self) -> Option<&CidSystemInfo> {
        self.system_info.as_ref()
    }

    /// The Adobe collection of a predefined CMap, or of an embedded one from its CIDSystemInfo.
    pub fn collection(&self) -> Option<CidCollection> {
        self.collection
    }

    /// A `usecmap` name that was not a predefined CMap.
    pub fn use_cmap(&self) -> Option<&str> {
        self.use_cmap.as_deref()
    }

    /// Reads the next code from `bytes` following the codespace ranges. A byte sequence outside
    /// every range takes the length of the shortest range whose first byte matches, else of the
    /// shortest range. `None` only for empty input.
    pub fn next_code(&self, bytes: &[u8]) -> Option<CharCode> {
        let first = *bytes.first()?;
        let mut code = 0u32;
        for n in 1..=bytes.len().min(4) {
            code = code << 8 | u32::from(bytes[n - 1]);
            if self.codespaces.iter().any(|r| r.contains(&bytes[..n])) {
                return Some(CharCode { code, len: n as u8 });
            }
        }
        let partial = self
            .codespaces
            .iter()
            .filter(|r| r.low[0] <= first && first <= r.high[0])
            .map(|r| r.len)
            .min();
        let n = partial
            .or_else(|| self.codespaces.iter().map(|r| r.len).min())
            .unwrap_or(1);
        let n = usize::from(n).clamp(1, bytes.len());
        let code = bytes[..n]
            .iter()
            .fold(0u32, |acc, &b| acc << 8 | u32::from(b));
        Some(CharCode { code, len: n as u8 })
    }

    /// The CID a code maps to, without the notdef fallback.
    pub fn lookup(&self, code: CharCode) -> Option<u32> {
        range_get(&self.cids, code.len, code.code).map(|(cid, off)| cid.saturating_add(off))
    }

    /// The CID for a code: its mapping, else its notdef mapping, else 0.
    pub fn cid(&self, code: CharCode) -> u32 {
        self.lookup(code)
            .or_else(|| range_get(&self.notdefs, code.len, code.code).map(|(cid, _)| *cid))
            .unwrap_or(0)
    }

    /// Splits `bytes` into codes and maps each to its CID.
    pub fn decode(&self, bytes: &[u8]) -> Vec<(CharCode, u32)> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(code) = self.next_code(rest) {
            out.push((code, self.cid(code)));
            rest = &rest[usize::from(code.len)..];
        }
        out
    }
}

fn predefined_by_name(name: &str, depth: u32) -> Option<CMap> {
    if depth > MAX_USECMAP_DEPTH {
        return None;
    }
    match name {
        "Identity-H" | "Identity-V" | "OneByteIdentityH" | "OneByteIdentityV" => {
            CMap::predefined(name).ok()
        }
        _ => load_predefined(predefined::find_code_cmap(name)?, depth).ok(),
    }
}

fn load_predefined(index: usize, depth: u32) -> Result<CMap> {
    if depth > MAX_USECMAP_DEPTH {
        return Err(FontError::LimitExceeded("usecmap depth"));
    }
    let data = predefined::load_code_cmap(index)?;
    let mut cmap = match data.base {
        Some(b) => load_predefined(b, depth + 1)?,
        None => CMap::default(),
    };
    cmap.name = predefined::entry_name(index).map(str::to_owned);
    cmap.vertical = data.vertical;
    cmap.codespaces = data
        .codespaces
        .iter()
        .map(|&(len, low, high)| CodespaceRange { len, low, high })
        .collect();
    cmap.collection = Some(CidCollection::ALL[usize::from(data.collection.min(3))]);
    for (len, lo, hi, cid) in data.runs {
        range_insert(&mut cmap.cids, len, lo, hi, cid);
    }
    Ok(cmap)
}

/// Tokens up to (not including) the `end` keyword; nested arrays are kept as their elements
/// between `OpenArray` and `CloseArray` markers.
fn read_block<'a>(lx: &mut Lexer<'a>, end: &[u8]) -> Vec<Token<'a>> {
    let mut out = Vec::new();
    while let Some(tok) = lx.next_token() {
        if let Token::Word(w) = tok
            && w == end
        {
            break;
        }
        out.push(tok);
    }
    out
}

/// Destination of a ToUnicode range.
#[derive(Debug, Clone)]
enum UniDest {
    /// bfrange base string: the last up to four bytes count up, the prefix stays.
    Counter {
        prefix: Arc<[u8]>,
        base: u32,
        width: u8,
    },
    /// bfrange array form: one string per code from `start`.
    List {
        items: Arc<[String]>,
        start: u32,
    },
    /// cidrange in a ToUnicode map: consecutive code points.
    Scalar(u32),
    Text(String),
}

impl Shift for UniDest {
    fn shift(&self, by: u32) -> UniDest {
        match self {
            UniDest::Counter {
                prefix,
                base,
                width,
            } => UniDest::Counter {
                prefix: prefix.clone(),
                base: base.wrapping_add(by),
                width: *width,
            },
            UniDest::List { items, start } => UniDest::List {
                items: items.clone(),
                start: start.saturating_add(by),
            },
            UniDest::Scalar(v) => UniDest::Scalar(v.saturating_add(by)),
            UniDest::Text(t) => UniDest::Text(t.clone()),
        }
    }
}

impl UniDest {
    fn text(&self, offset: u32) -> Option<String> {
        let s = match self {
            UniDest::Counter {
                prefix,
                base,
                width,
            } => {
                let v = base.wrapping_add(offset).to_be_bytes();
                let mut bytes = prefix.to_vec();
                bytes.extend_from_slice(&v[4 - usize::from(*width)..]);
                decode_utf16be(&bytes)
            }
            UniDest::List { items, start } => {
                items.get(start.checked_add(offset)? as usize)?.clone()
            }
            UniDest::Scalar(v) => char::from_u32(v.checked_add(offset)?)?.to_string(),
            UniDest::Text(t) => t.clone(),
        };
        (!s.is_empty()).then_some(s)
    }
}

/// A ToUnicode CMap: character codes to Unicode strings, independent of code length.
#[derive(Debug, Clone, Default)]
pub struct ToUnicodeMap {
    map: RangeMap<UniDest>,
}

fn dest_text(tok: &Token<'_>) -> Option<String> {
    match tok {
        Token::Hex(b) | Token::Str(b) => Some(decode_utf16be(b)),
        Token::Name(n) => glyph_name_to_unicode(std::str::from_utf8(n).ok()?),
        Token::Int(v) => char::from_u32(u32::try_from(*v).ok()?).map(String::from),
        _ => None,
    }
}

impl ToUnicodeMap {
    pub fn parse(data: &[u8]) -> Result<ToUnicodeMap> {
        let mut out = ToUnicodeMap::default();
        let mut lx = Lexer::new(data);
        while let Some(tok) = lx.next_token() {
            match tok {
                Token::Word(b"beginbfchar") => {
                    for [src, dest] in read_block(&mut lx, b"endbfchar").as_chunks::<2>().0 {
                        if let Token::Hex(src) = src
                            && let Some((_, code)) = code_value(src)
                            && let Some(text) = dest_text(dest)
                        {
                            out.insert(code, code, UniDest::Text(text));
                        }
                    }
                }
                Token::Word(b"beginbfrange") => {
                    let toks = read_block(&mut lx, b"endbfrange");
                    out.bfrange(&toks);
                }
                Token::Word(b"begincidchar") => {
                    for [src, v] in read_block(&mut lx, b"endcidchar").as_chunks::<2>().0 {
                        if let (Token::Hex(src), Token::Int(v)) = (src, v)
                            && let Some((_, code)) = code_value(src)
                            && let Ok(v) = u32::try_from(*v)
                        {
                            out.insert(code, code, UniDest::Scalar(v));
                        }
                    }
                }
                Token::Word(b"begincidrange") => {
                    for [lo, hi, v] in read_block(&mut lx, b"endcidrange").as_chunks::<3>().0 {
                        if let (Token::Hex(lo), Token::Hex(hi), Token::Int(v)) = (lo, hi, v)
                            && let (Some((l1, lo)), Some((l2, hi))) =
                                (code_value(lo), code_value(hi))
                            && l1 == l2
                            && lo <= hi
                            && let Ok(v) = u32::try_from(*v)
                        {
                            out.insert(lo, hi, UniDest::Scalar(v));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn bfrange(&mut self, toks: &[Token<'_>]) {
        let mut i = 0usize;
        while i + 2 < toks.len() {
            let (Token::Hex(lo), Token::Hex(hi)) = (&toks[i], &toks[i + 1]) else {
                i += 1;
                continue;
            };
            let range = match (code_value(lo), code_value(hi)) {
                (Some((l1, lo)), Some((l2, hi))) if l1 == l2 && lo <= hi => Some((lo, hi)),
                _ => None,
            };
            i += 2;
            match &toks[i] {
                Token::OpenArray => {
                    let mut items = Vec::new();
                    i += 1;
                    while i < toks.len() && toks[i] != Token::CloseArray {
                        items.push(dest_text(&toks[i]).unwrap_or_default());
                        i += 1;
                    }
                    i += 1;
                    if let Some((lo, hi)) = range
                        && !items.is_empty()
                    {
                        let count = (items.len() as u64).min(u64::from(hi - lo) + 1) as u32;
                        self.insert(
                            lo,
                            lo + (count - 1),
                            UniDest::List {
                                items: items.into(),
                                start: 0,
                            },
                        );
                    }
                }
                Token::Hex(dst) => {
                    i += 1;
                    if let Some((lo, hi)) = range
                        && !dst.is_empty()
                    {
                        let split = dst.len().saturating_sub(4);
                        let var = &dst[split..];
                        let base = var.iter().fold(0u32, |acc, &b| acc << 8 | u32::from(b));
                        let dest = UniDest::Counter {
                            prefix: dst[..split].into(),
                            base,
                            width: var.len() as u8,
                        };
                        self.insert(lo, hi, dest);
                    }
                }
                _ => i += 1,
            }
        }
    }

    /// Inserts a mapping, except that a code already mapped to a space keeps it over U+00A0.
    fn insert(&mut self, lo: u32, hi: u32, dest: UniDest) {
        let mut keep: Vec<u32> = Vec::new();
        let nbsp_offsets: Vec<u32> = match &dest {
            UniDest::Counter {
                prefix,
                base,
                width: 2,
            } if prefix.is_empty() => 0xA0u32
                .checked_sub(*base)
                .filter(|&k| k <= hi - lo)
                .into_iter()
                .collect(),
            UniDest::Scalar(v) => 0xA0u32
                .checked_sub(*v)
                .filter(|&k| k <= hi - lo)
                .into_iter()
                .collect(),
            UniDest::Text(t) if t == "\u{a0}" => vec![0],
            UniDest::List { items, start } => items
                .iter()
                .enumerate()
                .skip(*start as usize)
                .filter(|(_, s)| s.as_str() == "\u{a0}")
                .map(|(k, _)| k as u32 - start)
                .filter(|&k| k <= hi - lo)
                .collect(),
            _ => Vec::new(),
        };
        for k in nbsp_offsets {
            if self.lookup(lo + k).as_deref() == Some(" ") {
                keep.push(lo + k);
            }
        }
        let mut start = u64::from(lo);
        for code in keep {
            if u64::from(code) > start {
                let s = start as u32;
                range_insert(&mut self.map, 0, s, code - 1, dest.shift(s - lo));
            }
            start = u64::from(code) + 1;
        }
        if start <= u64::from(hi) {
            let s = start as u32;
            range_insert(&mut self.map, 0, s, hi, dest.shift(s - lo));
        }
    }

    /// Unicode text for a character code.
    pub fn lookup(&self, code: u32) -> Option<String> {
        let (dest, offset) = range_get(&self.map, 0, code)?;
        dest.text(offset)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Most entries per `beginbfchar` block (the PDF limit).
const BFCHAR_BLOCK: usize = 100;

/// A ToUnicode CMap stream for two-byte codes (Identity-H): one bfchar entry per (code, text),
/// sorted by code; later duplicates of a code and empty texts are dropped.
pub fn write_to_unicode_cmap(entries: &[(u16, &str)]) -> Vec<u8> {
    let mut entries: Vec<(u16, &str)> = entries
        .iter()
        .copied()
        .filter(|(_, t)| !t.is_empty())
        .collect();
    entries.sort_by_key(|(code, _)| *code);
    entries.dedup_by_key(|(code, _)| *code);
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    for block in entries.chunks(BFCHAR_BLOCK) {
        out.push_str(&format!("{} beginbfchar\n", block.len()));
        for (code, text) in block {
            out.push_str(&format!("<{code:04X}> <"));
            for unit in text.encode_utf16() {
                out.push_str(&format!("{unit:04X}"));
            }
            out.push_str(">\n");
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out.into_bytes()
}
