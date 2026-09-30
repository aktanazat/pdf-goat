//! OCR font selection and page-local character codes. PDF font embedding is owned
//! by pdf-font; aliases never collapse into one Unicode value.

use std::collections::{BTreeMap, BTreeSet};

use pdf_core::{Dict, Document};
use pdf_font::{Font, FontKind, FontLocator, FontRequest, GlyphMapping, Script, embed_truetype};

#[derive(Default)]
pub(super) struct Fonts {
    programs: Vec<Font>,
    selected: BTreeMap<char, (usize, u16)>,
}

#[derive(Clone, Copy)]
pub(super) struct Glyph {
    pub font: usize,
    pub code: u16,
    pub advance: f64,
    pub ascent: f64,
    pub descent: f64,
}

pub(super) struct PageFonts {
    pub resources: Dict,
    pub names: Vec<String>,
    glyphs: BTreeMap<char, Glyph>,
}

impl PageFonts {
    pub fn glyph(&self, ch: char) -> Result<Glyph, String> {
        self.glyphs
            .get(&ch)
            .copied()
            .ok_or_else(|| format!("missing OCR glyph for U+{:04X}", u32::from(ch)))
    }
}

impl Fonts {
    pub fn embed(
        &mut self,
        doc: &mut Document,
        text: impl Iterator<Item = char>,
    ) -> Result<PageFonts, String> {
        let chars: BTreeSet<char> = text.chain([' ']).collect();
        self.select(&chars)?;
        let mut by_font: BTreeMap<usize, Vec<(char, u16)>> = BTreeMap::new();
        for ch in chars {
            let &(face, gid) = self
                .selected
                .get(&ch)
                .ok_or("OCR font selection lost a character")?;
            by_font.entry(face).or_default().push((ch, gid));
        }
        let mut page = PageFonts {
            resources: Dict::new(),
            names: Vec::new(),
            glyphs: BTreeMap::new(),
        };
        for (face, chars) in by_font {
            let font = &self.programs[face];
            let metrics = font.metrics();
            let em = f64::from(font.units_per_em());
            let index = page.names.len();
            let resource_name = format!("F{index}");
            let mut unicode = Vec::with_capacity(chars.len());
            for (offset, (ch, old_gid)) in chars.into_iter().enumerate() {
                let code =
                    u16::try_from(offset + 1).map_err(|_| "too many characters in an OCR font")?;
                let width = f64::from(
                    font.advance(old_gid)
                        .ok_or("the OCR glyph has no advance")?,
                ) / em;
                unicode.push((code, old_gid, ch.to_string()));
                page.glyphs.insert(
                    ch,
                    Glyph {
                        font: index,
                        code,
                        advance: width,
                        ascent: f64::from(metrics.ascent) / em,
                        descent: f64::from(metrics.descent) / em,
                    },
                );
            }
            let mappings: Vec<GlyphMapping<'_>> = unicode
                .iter()
                .map(|(code, gid, text)| GlyphMapping {
                    code: *code,
                    glyph_id: *gid,
                    unicode: text.as_str(),
                })
                .collect();
            let reference = embed_truetype(doc, font, &mappings).map_err(|e| e.to_string())?;
            page.resources.insert(resource_name.as_str(), reference);
            page.names.push(resource_name);
        }
        Ok(page)
    }

    fn select(&mut self, chars: &BTreeSet<char>) -> Result<(), String> {
        let mut missing: BTreeSet<char> = chars
            .iter()
            .filter(|ch| !self.selected.contains_key(ch))
            .copied()
            .collect();
        for (index, font) in self.programs.iter().enumerate() {
            missing.retain(
                |ch| match font.glyph_for_char(*ch).filter(|gid| *gid != 0) {
                    Some(gid) => {
                        self.selected.insert(*ch, (index, gid));
                        false
                    }
                    None => true,
                },
            );
        }
        if missing.is_empty() {
            return Ok(());
        }
        let locator = FontLocator::system();
        // A broad Unicode face avoids splitting common multilingual lines across fonts.
        let preferred = locator.find(&FontRequest {
            base_font: "ArialUnicodeMS",
            flags: 32,
            weight: Some(400),
            script: Script::Latin,
        });
        if let Some(candidate) = &preferred
            && let Ok(font) = candidate.load()
        {
            self.take_font(font, &mut missing);
        }
        if !missing.is_empty() {
            'files: for path in locator.files() {
                let Ok(mut data) = std::fs::read(path) else {
                    continue;
                };
                let count = Font::face_count(&data).unwrap_or(0).min(256);
                for index in 0..count {
                    if preferred
                        .as_ref()
                        .is_some_and(|p| p.path == *path && p.face_index == index)
                    {
                        continue;
                    }
                    let bytes = if index + 1 == count {
                        std::mem::take(&mut data)
                    } else {
                        data.clone()
                    };
                    if let Ok(font) = Font::parse_face(bytes, index) {
                        self.take_font(font, &mut missing);
                    }
                    if missing.is_empty() {
                        break 'files;
                    }
                }
            }
        }
        match missing.first() {
            Some(ch) => Err(format!(
                "no installed TrueType font contains OCR character U+{:04X}",
                u32::from(*ch)
            )),
            None => Ok(()),
        }
    }

    fn take_font(&mut self, font: Font, missing: &mut BTreeSet<char>) {
        if font.kind() != FontKind::TrueType {
            return;
        }
        let index = self.programs.len();
        let before = missing.len();
        missing.retain(
            |ch| match font.glyph_for_char(*ch).filter(|gid| *gid != 0) {
                Some(gid) => {
                    self.selected.insert(*ch, (index, gid));
                    false
                }
                None => true,
            },
        );
        if missing.len() != before {
            self.programs.push(font);
        }
    }
}
