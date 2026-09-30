//! Built-in single-byte encodings and font encodings with /Differences.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use crate::data::encodings::{
    MAC_EXPERT, MAC_ROMAN, PDF_DOC, STANDARD, SYMBOL, WIN_ANSI, ZAPF_DINGBATS,
};
use crate::glyphnames::{dingbats_name_to_unicode, glyph_name_to_unicode};

/// The predefined single-byte encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseEncoding {
    Standard,
    MacRoman,
    WinAnsi,
    PdfDoc,
    MacExpert,
    /// Built-in encoding of the Symbol font.
    Symbol,
    /// Built-in encoding of the ZapfDingbats font.
    ZapfDingbats,
}

impl BaseEncoding {
    pub const ALL: [BaseEncoding; 7] = [
        BaseEncoding::Standard,
        BaseEncoding::MacRoman,
        BaseEncoding::WinAnsi,
        BaseEncoding::PdfDoc,
        BaseEncoding::MacExpert,
        BaseEncoding::Symbol,
        BaseEncoding::ZapfDingbats,
    ];

    /// The encoding a PDF /Encoding or /BaseEncoding name selects.
    pub fn from_pdf_name(name: &str) -> Option<BaseEncoding> {
        match name {
            "StandardEncoding" => Some(BaseEncoding::Standard),
            "MacRomanEncoding" => Some(BaseEncoding::MacRoman),
            "WinAnsiEncoding" => Some(BaseEncoding::WinAnsi),
            "PDFDocEncoding" => Some(BaseEncoding::PdfDoc),
            "MacExpertEncoding" => Some(BaseEncoding::MacExpert),
            _ => None,
        }
    }

    fn table(self) -> &'static [&'static str; 256] {
        match self {
            BaseEncoding::Standard => &STANDARD,
            BaseEncoding::MacRoman => &MAC_ROMAN,
            BaseEncoding::WinAnsi => &WIN_ANSI,
            BaseEncoding::PdfDoc => &PDF_DOC,
            BaseEncoding::MacExpert => &MAC_EXPERT,
            BaseEncoding::Symbol => &SYMBOL,
            BaseEncoding::ZapfDingbats => &ZAPF_DINGBATS,
        }
    }

    /// Glyph name at `code`, `None` where the encoding leaves the code undefined.
    pub fn glyph_name(self, code: u8) -> Option<&'static str> {
        let name = self.table()[usize::from(code)];
        (!name.is_empty()).then_some(name)
    }

    /// Lowest code that maps to `glyph_name`.
    pub fn code_for_name(self, glyph_name: &str) -> Option<u8> {
        self.table()
            .iter()
            .position(|n| *n == glyph_name)
            .and_then(|i| u8::try_from(i).ok())
    }

    /// Unicode for `code`: the glyph name through the Adobe Glyph List (the dingbats table for
    /// ZapfDingbats). WinAnsi and MacRoman map their second space code (0xA0, 0xCA) to U+0020.
    pub fn to_unicode(self, code: u8) -> Option<char> {
        let name = self.glyph_name(code)?;
        if self == BaseEncoding::ZapfDingbats {
            return dingbats_name_to_unicode(name);
        }
        let text = glyph_name_to_unicode(name)?;
        let mut chars = text.chars();
        let ch = chars.next()?;
        chars.next().is_none().then_some(ch)
    }

    /// Code for a Unicode character. WinAnsi follows Windows code page 1252 (so U+00A0 is
    /// 0xA0); the other encodings reverse [`BaseEncoding::to_unicode`], lowest code first.
    pub fn from_unicode(self, ch: char) -> Option<u8> {
        if self == BaseEncoding::WinAnsi {
            return cp1252_from_unicode(ch);
        }
        reverse_table(self).get(&ch).copied()
    }
}

static REVERSE: LazyLock<[HashMap<char, u8>; 7]> =
    LazyLock::new(|| BaseEncoding::ALL.map(build_reverse));

fn build_reverse(enc: BaseEncoding) -> HashMap<char, u8> {
    let mut map = HashMap::new();
    for code in 0..=255u8 {
        if let Some(ch) = enc.to_unicode(code) {
            map.entry(ch).or_insert(code);
        }
    }
    map
}

fn reverse_table(enc: BaseEncoding) -> &'static HashMap<char, u8> {
    let idx = BaseEncoding::ALL
        .iter()
        .position(|e| *e == enc)
        .unwrap_or(0);
    &REVERSE[idx]
}

/// Windows-1252 code for `ch`; `None` for characters the code page lacks.
pub fn cp1252_from_unicode(ch: char) -> Option<u8> {
    let cp = u32::from(ch);
    if cp < 0x80 || (0xA0..=0xFF).contains(&cp) {
        return u8::try_from(cp).ok();
    }
    let code = match ch {
        '\u{20AC}' => 0x80,
        '\u{201A}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201E}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02C6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8A,
        '\u{2039}' => 0x8B,
        '\u{0152}' => 0x8C,
        '\u{017D}' => 0x8E,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201C}' => 0x93,
        '\u{201D}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02DC}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9A,
        '\u{203A}' => 0x9B,
        '\u{0153}' => 0x9C,
        '\u{017E}' => 0x9E,
        '\u{0178}' => 0x9F,
        _ => return None,
    };
    Some(code)
}

/// A simple font's code-to-glyph-name table: a base encoding with /Differences applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoding {
    names: Vec<Option<Cow<'static, str>>>,
}

impl Encoding {
    /// Starts from `base`, or from an all-undefined table when `base` is `None`.
    pub fn new(base: Option<BaseEncoding>) -> Encoding {
        let names = (0..=255u8)
            .map(|code| base.and_then(|b| b.glyph_name(code)).map(Cow::Borrowed))
            .collect();
        Encoding { names }
    }

    /// Assigns `glyph_name` to `code`, as one /Differences entry does.
    pub fn set(&mut self, code: u8, glyph_name: &str) {
        self.names[usize::from(code)] = Some(Cow::Owned(glyph_name.to_owned()));
    }

    /// Applies a /Differences array given as `(first code, names...)` runs.
    pub fn apply_differences<'a, I>(&mut self, runs: I)
    where
        I: IntoIterator<Item = (u8, &'a [&'a str])>,
    {
        for (first, names) in runs {
            for (offset, name) in names.iter().enumerate() {
                match u8::try_from(usize::from(first) + offset) {
                    Ok(code) => self.set(code, name),
                    Err(_) => break,
                }
            }
        }
    }

    pub fn glyph_name(&self, code: u8) -> Option<&str> {
        self.names[usize::from(code)].as_deref()
    }

    /// Unicode text for `code` through [`glyph_name_to_unicode`].
    pub fn to_unicode(&self, code: u8) -> Option<String> {
        glyph_name_to_unicode(self.glyph_name(code)?)
    }
}
