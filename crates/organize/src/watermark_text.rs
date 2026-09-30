//! Unicode watermark text uses the shared font subset writer. Font selection and
//! character codes belong to this text run; PDF embedding belongs to pdf-font.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use goat_common::GoatError;
use pdf_core::{Dict, Document, ObjRef, Point};
use pdf_font::{Font, FontKind, FontLocator, FontRequest, GlyphMapping, Script, embed_truetype};

use crate::open::pdf_error;

pub(crate) struct UnicodeText {
    fonts: Vec<ObjRef>,
    glyphs: Vec<(usize, u16)>,
}

struct Face {
    font: Font,
    chars: Vec<(char, u16)>,
    codes: HashMap<char, u16>,
}

impl UnicodeText {
    pub(crate) fn prepare(doc: &mut Document, text: &str) -> Result<Self, GoatError> {
        let locator = FontLocator::system();
        let mut tried = HashSet::new();
        let mut faces: Vec<Face> = Vec::new();
        let mut glyphs = Vec::with_capacity(text.len());
        for ch in text.chars() {
            let mut found = faces
                .iter()
                .position(|face| face.font.glyph_for_char(ch).is_some_and(|gid| gid != 0));
            if found.is_none() {
                for (name, script) in [
                    ("Arial Unicode MS", Script::Latin),
                    ("Arial", Script::Latin),
                    ("Noto Sans", Script::Latin),
                    ("Noto Sans CJK SC", Script::SimplifiedChinese),
                    ("Noto Sans CJK JP", Script::Japanese),
                    ("Noto Sans CJK KR", Script::Korean),
                    ("Apple Symbols", Script::Latin),
                ] {
                    let Some(substitute) = locator.find(&FontRequest {
                        base_font: name,
                        flags: 32,
                        weight: None,
                        script,
                    }) else {
                        continue;
                    };
                    if !tried.insert((substitute.path.clone(), substitute.face_index)) {
                        continue;
                    }
                    let font = substitute
                        .load()
                        .map_err(|error| GoatError::message(error.to_string()))?;
                    if font.kind() != FontKind::TrueType {
                        continue;
                    }
                    let supports = font.glyph_for_char(ch).is_some_and(|gid| gid != 0);
                    faces.push(Face {
                        font,
                        chars: Vec::new(),
                        codes: HashMap::new(),
                    });
                    if supports {
                        found = Some(faces.len() - 1);
                        break;
                    }
                }
            }
            let index = found.ok_or_else(|| {
                GoatError::message(format!(
                    "no installed TrueType font covers U+{:04X}",
                    u32::from(ch)
                ))
            })?;
            let face = &mut faces[index];
            let code = if let Some(&code) = face.codes.get(&ch) {
                code
            } else {
                let code = u16::try_from(face.chars.len() + 1).map_err(|_| {
                    GoatError::message(
                        "watermark uses more than 65535 distinct characters in one font",
                    )
                })?;
                let gid = face.font.glyph_for_char(ch).ok_or_else(|| {
                    GoatError::message("selected font lost its character mapping")
                })?;
                face.chars.push((ch, gid));
                face.codes.insert(ch, code);
                code
            };
            glyphs.push((index, code));
        }
        let mut fonts = Vec::with_capacity(faces.len());
        let mut remap = HashMap::new();
        for (index, face) in faces.into_iter().enumerate() {
            if face.chars.is_empty() {
                continue;
            }
            let unicode: Vec<String> = face.chars.iter().map(|(ch, _)| ch.to_string()).collect();
            let mappings: Vec<GlyphMapping<'_>> = face
                .chars
                .iter()
                .zip(&unicode)
                .enumerate()
                .map(|(index, ((_, gid), unicode))| GlyphMapping {
                    code: (index + 1) as u16,
                    glyph_id: *gid,
                    unicode,
                })
                .collect();
            remap.insert(index, fonts.len());
            fonts.push(
                embed_truetype(doc, &face.font, &mappings)
                    .map_err(|error| GoatError::message(error.to_string()))?,
            );
        }
        for (index, _) in &mut glyphs {
            *index = *remap
                .get(index)
                .ok_or_else(|| GoatError::message("watermark font mapping missing"))?;
        }
        Ok(Self { fonts, glyphs })
    }

    pub(crate) fn draw(
        &self,
        doc: &mut Document,
        index: usize,
        point: Point,
        size: f64,
    ) -> Result<String, GoatError> {
        let mut page = doc.page(index).map_err(pdf_error)?;
        let mut fonts = match page.resources.get(b"Font") {
            Some(value) => doc
                .resolve_dict(value)
                .map_err(pdf_error)?
                .unwrap_or_default(),
            None => Dict::new(),
        };
        let mut names = Vec::with_capacity(self.fonts.len());
        for id in &self.fonts {
            let mut suffix = 0;
            while fonts.contains_key(format!("GoatWM{suffix}").as_bytes()) {
                suffix += 1;
            }
            let name = format!("GoatWM{suffix}");
            fonts.insert(name.as_bytes(), *id);
            names.push(name);
        }
        page.resources.insert("Font", fonts);
        page.dict.insert("Resources", page.resources);
        doc.set(page.id, page.dict);
        let mut content = format!("1 0 0 1 {} {} Tm\n", point.x as f32, point.y as f32);
        let mut active = None;
        for &(font, code) in &self.glyphs {
            if active != Some(font) {
                if active.is_some() {
                    content.push_str(">]TJ\n");
                }
                let _ = write!(content, "/{} {} Tf\n[<", names[font], size as f32);
                active = Some(font);
            }
            let _ = write!(content, "{code:04x}");
        }
        if active.is_some() {
            content.push_str(">]TJ\n");
        }
        Ok(content)
    }
}
