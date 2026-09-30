//! Shaping-free text layout: one glyph per character through the font's Unicode mapping.

use crate::font::Font;

/// One character laid out as one glyph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub ch: char,
    /// Glyph id; 0 when the font has no glyph for `ch`.
    pub gid: u16,
    /// Advance width in font units.
    pub advance: f32,
}

/// Maps each character of `text` to a glyph with [`Font::glyph_for_char`] and its advance.
/// No shaping, ligatures, or kerning.
pub fn layout_text(font: &Font, text: &str) -> Vec<PositionedGlyph> {
    text.chars()
        .map(|ch| {
            let gid = font.glyph_for_char(ch).unwrap_or(0);
            PositionedGlyph {
                ch,
                gid,
                advance: font.advance(gid).unwrap_or(0.0),
            }
        })
        .collect()
}

/// Width of `text` at `font_size`, in text space units, from [`layout_text`] advances.
pub fn text_advance(font: &Font, text: &str, font_size: f32) -> f32 {
    let units: f32 = layout_text(font, text).iter().map(|g| g.advance).sum();
    units * font_size / f32::from(font.units_per_em())
}
