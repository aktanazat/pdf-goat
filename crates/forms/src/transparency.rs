//! Opaque raster replacement for pages that actually paint with transparency.
//! The raster is 144 dpi; an invisible Unicode text layer keeps selection and
//! search. Page dictionaries, links, metadata, and opaque pages stay intact.

use std::collections::HashMap;
use std::fmt::Write;

use goat_common::GoatError;
use pdf_core::{Dict, Document, Matrix, Object, Page, Stream};
use pdf_font::{Font, FontKind, FontLocator, FontRequest, GlyphMapping, Script, embed_truetype};
use pdf_interp::{
    BlendMode, Brush, Device, FillEvent, GroupEvent, ImageEvent, ImageMaskEvent, Paint, Path,
    RunOptions, ShadingEvent, StrokeEvent, TextRun,
};
use pdf_text::{Block, TextPage};

use crate::error;

#[derive(Default)]
struct Probe {
    transparent: bool,
    depth: usize,
    failure: Option<GoatError>,
}

impl Probe {
    fn state(&mut self, alpha: f32, blend: BlendMode, soft_mask: bool) {
        self.transparent |= alpha < 1.0 || blend != BlendMode::Normal || soft_mask;
    }

    fn brush(&mut self, brush: &Brush<'_>) {
        self.state(brush.alpha, brush.blend, brush.soft_mask.is_some());
        if self.transparent || self.failure.is_some() {
            return;
        }
        if let Paint::Tiling(pattern) = brush.paint {
            if self.depth >= 64 {
                self.failure = Some(GoatError::message(
                    "transparency pattern nesting exceeds 64",
                ));
                return;
            }
            self.depth += 1;
            if let Err(failure) = pattern.run_cell(self) {
                self.failure = Some(GoatError::message(failure.to_string()));
            }
            self.depth -= 1;
        }
    }
}

impl Device for Probe {
    fn fill_path(&mut self, _: &Path, event: &FillEvent<'_>) {
        self.brush(&event.brush);
    }
    fn stroke_path(&mut self, _: &Path, event: &StrokeEvent<'_>) {
        self.brush(&event.brush);
    }
    fn text(&mut self, run: &TextRun<'_>) {
        if let Some(brush) = run.fill {
            self.brush(&brush);
        }
        if let Some(brush) = run.stroke {
            self.brush(&brush);
        }
    }
    fn fill_shading(&mut self, event: &ShadingEvent<'_>) {
        self.state(event.alpha, event.blend, event.soft_mask.is_some());
    }
    fn fill_image(&mut self, event: &ImageEvent<'_>) {
        self.state(event.alpha, event.blend, event.soft_mask.is_some());
        self.transparent |= event.image.has_alpha();
    }
    fn fill_image_mask(&mut self, event: &ImageMaskEvent<'_>) {
        self.brush(&event.brush);
    }
    fn begin_group(&mut self, _: &GroupEvent<'_>) {
        self.transparent = true;
    }
    fn wants_type3_procs(&self) -> bool {
        true
    }
}

fn uses_transparency(doc: &Document, page: &Page) -> Result<bool, GoatError> {
    let group = doc.resolve_key(&page.dict, b"Group").map_err(error)?;
    let mut probe = Probe {
        transparent: group
            .as_dict()
            .is_some_and(|group| group.get_name(b"S") == Some(b"Transparency")),
        ..Probe::default()
    };
    pdf_interp::run_page_contents(
        doc,
        page,
        &mut probe,
        &RunOptions {
            annotations: false,
            ..RunOptions::default()
        },
    )
    .map_err(|e| GoatError::message(e.to_string()))?;
    if let Some(failure) = probe.failure {
        return Err(failure);
    }
    Ok(probe.transparent)
}

fn search_font() -> Result<Font, GoatError> {
    let request = FontRequest {
        base_font: "Arial",
        flags: 0,
        weight: None,
        script: Script::Latin,
    };
    let substitute = FontLocator::system().find(&request).ok_or_else(|| {
        GoatError::message("a TrueType sans-serif font is required to preserve searchable text")
    })?;
    let font = substitute
        .load()
        .map_err(|e| GoatError::message(e.to_string()))?;
    if font.kind() != FontKind::TrueType {
        return Err(GoatError::message(
            "the searchable-text font must have TrueType outlines",
        ));
    }
    Ok(font)
}

fn searchable_layer(
    doc: &mut Document,
    text: &TextPage,
    inverse: &Matrix,
    selected_font: &mut Option<Font>,
    resources: &mut Dict,
    content: &mut String,
) -> Result<(), GoatError> {
    let mut codes = HashMap::new();
    let mut characters = Vec::new();
    for block in &text.blocks {
        if let Block::Text(block) = block {
            for line in &block.lines {
                for ch in &line.chars {
                    if let std::collections::hash_map::Entry::Vacant(entry) = codes.entry(ch.c) {
                        let code = u16::try_from(characters.len()).map_err(|_| {
                            GoatError::message(
                                "a searchable page exceeds 65536 distinct characters",
                            )
                        })?;
                        entry.insert(code);
                        characters.push(ch.c.to_string());
                    }
                }
            }
        }
    }
    if characters.is_empty() {
        return Ok(());
    }
    let font = match selected_font {
        Some(font) => font,
        None => selected_font.insert(search_font()?),
    };
    // Invisible glyphs need exact Unicode, not visible substitute outlines. A
    // missing outline may use .notdef without losing its explicit ToUnicode map.
    let mappings: Vec<_> = characters
        .iter()
        .enumerate()
        .map(|(code, value)| GlyphMapping {
            code: code as u16,
            glyph_id: font
                .glyph_for_char(value.chars().next().expect("one character"))
                .unwrap_or(0),
            unicode: value,
        })
        .collect();
    let id = embed_truetype(doc, font, &mappings).map_err(|e| GoatError::message(e.to_string()))?;
    let mut fonts = Dict::new();
    fonts.insert("SearchText", id);
    resources.insert("Font", fonts);
    let em = f64::from(font.units_per_em());
    content.push_str("BT\n3 Tr\n");
    for block in &text.blocks {
        if let Block::Text(block) = block {
            for line in &block.lines {
                for ch in &line.chars {
                    let code = codes[&ch.c];
                    let width = font
                        .advance(mappings[usize::from(code)].glyph_id)
                        .map(f64::from)
                        .unwrap_or(0.0)
                        / em;
                    let actual = (ch.quad.ur.x - ch.quad.ul.x).hypot(ch.quad.ur.y - ch.quad.ul.y);
                    let horizontal = if width > 0.0 && ch.size > 0.0 {
                        actual / (width * ch.size)
                    } else {
                        1.0
                    };
                    let matrix = Matrix::new(
                        line.dir.x * horizontal,
                        line.dir.y * horizontal,
                        line.dir.y,
                        -line.dir.x,
                        ch.origin.x,
                        ch.origin.y,
                    )
                    .concat(inverse);
                    writeln!(content, "/SearchText {:.8} Tf\n{:.8} {:.8} {:.8} {:.8} {:.8} {:.8} Tm\n<{code:04X}> Tj", ch.size, matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f).map_err(|e| GoatError::message(e.to_string()))?;
                }
            }
        }
    }
    content.push_str("ET\n");
    Ok(())
}

pub(crate) fn flatten_transparency(doc: &mut Document) -> Result<usize, GoatError> {
    let mut changed = 0;
    let mut font = None;
    for index in 0..doc.page_count().map_err(error)? {
        let mut page = doc.page(index).map_err(error)?;
        if !uses_transparency(doc, &page)? {
            continue;
        }
        let text = pdf_text::extract_page(doc, index, pdf_text::TextFlags::TEXT)?;
        let bounds = pdf_interp::page_bounds(&page);
        let inverse = pdf_interp::page_transform(&page)
            .invert()
            .ok_or_else(|| GoatError::message("invalid page transform"))?;
        let pixels = pdf_render::render_page(
            doc,
            &page,
            &pdf_render::RenderOptions {
                dpi: 144.0,
                alpha: false,
                annotations: false,
            },
        )
        .map_err(|e| GoatError::message(e.to_string()))?;
        let (width, height) = (pixels.width(), pixels.height());
        let mut rgb = pixels.into_data();
        // Compact in place: the renderer owns an RGBA buffer and a second
        // full-page allocation would be unnecessary.
        let count = rgb.len() / 4;
        for pixel in 0..count {
            let background = 255 - rgb[pixel * 4 + 3];
            for channel in 0..3 {
                rgb[pixel * 3 + channel] = rgb[pixel * 4 + channel].saturating_add(background);
            }
        }
        rgb.truncate(count * 3);
        let mut image = Dict::new();
        image.insert("Type", Object::name("XObject"));
        image.insert("Subtype", Object::name("Image"));
        image.insert("Width", i64::from(width));
        image.insert("Height", i64::from(height));
        image.insert("ColorSpace", Object::name("DeviceRGB"));
        image.insert("BitsPerComponent", 8_i64);
        let image = doc.add(Stream::new(image, rgb));
        let mut images = Dict::new();
        images.insert("OpaquePage", image);
        let mut resources = Dict::new();
        resources.insert("XObject", images);
        let matrix = Matrix::new(
            bounds.width(),
            0.0,
            0.0,
            -bounds.height(),
            bounds.x0,
            bounds.y1,
        )
        .concat(&inverse);
        let mut content = format!(
            "q\n{:.8} {:.8} {:.8} {:.8} {:.8} {:.8} cm\n/OpaquePage Do\nQ\n",
            matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f
        );
        let text_inverse = crate::page_transform(&mut page)
            .invert()
            .ok_or_else(|| GoatError::message("invalid text page transform"))?;
        searchable_layer(
            doc,
            &text,
            &text_inverse,
            &mut font,
            &mut resources,
            &mut content,
        )?;
        page.dict.insert(
            "Contents",
            doc.add(Stream::new(Dict::new(), content.into_bytes())),
        );
        page.dict.insert("Resources", resources);
        page.dict.remove(b"Group");
        doc.set(page.id, page.dict);
        changed += 1;
    }
    Ok(changed)
}
