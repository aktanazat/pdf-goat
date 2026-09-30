//! Metrics of the 14 standard PDF fonts and a width helper for laying out stamped text.

use crate::data::std14::STD14;
use crate::encoding::{BaseEncoding, cp1252_from_unicode};

/// Font-wide metrics and glyph widths (1/1000 em) of one standard font.
#[derive(Debug)]
pub struct Std14Metrics {
    pub name: &'static str,
    pub family: &'static str,
    pub weight: &'static str,
    /// PDF font descriptor flags: FixedPitch 1, Serif 2, Symbolic 4, Nonsymbolic 32, Italic 64.
    pub flags: u32,
    pub italic_angle: f32,
    /// FontBBox as `[x_min, y_min, x_max, y_max]`.
    pub bbox: [f32; 4],
    /// Absent for Symbol and ZapfDingbats.
    pub ascent: Option<f32>,
    pub descent: Option<f32>,
    pub cap_height: Option<f32>,
    pub x_height: Option<f32>,
    /// `(glyph name, width)` sorted by name bytes.
    pub widths: &'static [(&'static str, u16)],
}

/// The standard 14 fonts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Standard14 {
    Courier,
    CourierBold,
    CourierBoldOblique,
    CourierOblique,
    Helvetica,
    HelveticaBold,
    HelveticaBoldOblique,
    HelveticaOblique,
    Symbol,
    TimesBold,
    TimesBoldItalic,
    TimesItalic,
    TimesRoman,
    ZapfDingbats,
}

impl Standard14 {
    pub const ALL: [Standard14; 14] = [
        Standard14::Courier,
        Standard14::CourierBold,
        Standard14::CourierBoldOblique,
        Standard14::CourierOblique,
        Standard14::Helvetica,
        Standard14::HelveticaBold,
        Standard14::HelveticaBoldOblique,
        Standard14::HelveticaOblique,
        Standard14::Symbol,
        Standard14::TimesBold,
        Standard14::TimesBoldItalic,
        Standard14::TimesItalic,
        Standard14::TimesRoman,
        Standard14::ZapfDingbats,
    ];

    /// The standard font a /BaseFont name refers to: the 14 PostScript names, the aliases of
    /// PDF 1.7 Table H.3 (Arial, TimesNewRoman, CourierNew with `,Bold` / `,Italic` /
    /// `,BoldItalic`) and their common PostScript spellings (`ArialMT`, `Arial-BoldMT`,
    /// `TimesNewRomanPS-ItalicMT`, `CourierNewPSMT`, ...). A six-letter subset tag
    /// (`ABCDEF+`) is ignored.
    pub fn from_base_font(name: &str) -> Option<Standard14> {
        let name = strip_subset_tag(name);
        if let Some(font) = Standard14::ALL.into_iter().find(|f| f.name() == name) {
            return Some(font);
        }
        let (family, style) = match name.find([',', '-']) {
            Some(i) => (&name[..i], &name[i + 1..]),
            None => (name, ""),
        };
        let family = family.trim_end_matches("MT").trim_end_matches("PS");
        let style = style.trim_end_matches("MT").trim_end_matches("PS");
        let (bold, italic) = match style {
            "" | "Roman" | "Regular" | "Normal" | "Book" | "Medium" => (false, false),
            "Bold" => (true, false),
            "Italic" | "Oblique" => (false, true),
            "BoldItalic" | "BoldOblique" => (true, true),
            _ => return None,
        };
        use Standard14::*;
        let pick = |r, b, i, bi| {
            Some(match (bold, italic) {
                (false, false) => r,
                (true, false) => b,
                (false, true) => i,
                (true, true) => bi,
            })
        };
        match family {
            "Helvetica" | "Arial" => pick(
                Helvetica,
                HelveticaBold,
                HelveticaOblique,
                HelveticaBoldOblique,
            ),
            "Times" | "TimesNewRoman" | "TimesRoman" => {
                pick(TimesRoman, TimesBold, TimesItalic, TimesBoldItalic)
            }
            "Courier" | "CourierNew" => {
                pick(Courier, CourierBold, CourierOblique, CourierBoldOblique)
            }
            "Symbol" if style.is_empty() => Some(Symbol),
            "ZapfDingbats" | "Dingbats" if style.is_empty() => Some(ZapfDingbats),
            _ => None,
        }
    }

    /// The PostScript name, e.g. `Helvetica-BoldOblique`.
    pub fn name(self) -> &'static str {
        self.metrics().name
    }

    pub fn metrics(self) -> &'static Std14Metrics {
        &STD14[self as usize]
    }

    /// Width of the named glyph in 1/1000 em.
    pub fn glyph_width(self, glyph_name: &str) -> Option<u16> {
        let widths = self.metrics().widths;
        widths
            .binary_search_by(|(n, _)| n.as_bytes().cmp(glyph_name.as_bytes()))
            .ok()
            .map(|i| widths[i].1)
    }

    /// The font's built-in encoding: Standard for the Latin fonts, the font's own for Symbol and
    /// ZapfDingbats.
    pub fn builtin_encoding(self) -> BaseEncoding {
        match self {
            Standard14::Symbol => BaseEncoding::Symbol,
            Standard14::ZapfDingbats => BaseEncoding::ZapfDingbats,
            _ => BaseEncoding::Standard,
        }
    }

    /// Bytes to show `text` with this font: Windows-1252 codes for the Latin fonts (a
    /// WinAnsiEncoding font), raw character values for Symbol and ZapfDingbats (built-in
    /// encoding). Characters without a code become 0xB7.
    pub fn encode_text(self, text: &str) -> Vec<u8> {
        text.chars().map(|ch| self.code_for_char(ch)).collect()
    }

    fn code_for_char(self, ch: char) -> u8 {
        match self {
            Standard14::Symbol | Standard14::ZapfDingbats => {
                u8::try_from(u32::from(ch)).unwrap_or(0xB7)
            }
            _ => cp1252_from_unicode(ch).unwrap_or(0xB7),
        }
    }

    /// Width in 1/1000 em of `code` under the encoding [`Standard14::encode_text`] uses;
    /// 0 for codes without a glyph.
    pub fn code_width(self, code: u8) -> u16 {
        let encoding = match self {
            Standard14::Symbol | Standard14::ZapfDingbats => self.builtin_encoding(),
            _ => BaseEncoding::WinAnsi,
        };
        encoding
            .glyph_name(code)
            .and_then(|name| self.glyph_width(name))
            .unwrap_or(0)
    }

    /// Advance width of `text` at `font_size`, in the same units as `font_size`, for the bytes
    /// [`Standard14::encode_text`] produces.
    pub fn text_width(self, text: &str, font_size: f32) -> f32 {
        let units: f64 = text
            .chars()
            .map(|ch| f64::from(self.code_width(self.code_for_char(ch))))
            .sum();
        (units * f64::from(font_size) / 1000.0) as f32
    }
}

/// `name` without a subset tag (six uppercase letters and `+`), e.g. `ABCDEF+Arial` → `Arial`.
pub fn strip_subset_tag(name: &str) -> &str {
    match name.split_once('+') {
        Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) => rest,
        _ => name,
    }
}
