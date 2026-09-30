use std::collections::HashMap;

use pdf_core::{Document, Page};
use pdf_interp::{Brush, Device, FontSubtype, Paint, PdfFont, TextRun};

use crate::geom::{M, P, Q, R};
use crate::{Block, Char, FontInfo, GlyphSource, ImageBlock, Line, TextBlock, TextFlags, TextPage};

#[derive(Clone)]
pub(crate) struct Item {
    pub c: Option<char>,
    pub glyph: i64,
    pub adv: f32,
    pub trm: M,
    pub clip_bbox: R,
    pub cid: u32,
    pub bidi: u8,
    pub source: GlyphSource,
}

#[derive(Clone)]
pub(crate) struct Last {
    pub item: Item,
    pub font: usize,
    pub wmode: u8,
    pub flags: u32,
    pub clipped: bool,
    pub valid: bool,
}

pub(crate) struct RawChar {
    pub value: Char,
    pub raw: Q,
}

pub(crate) struct RawLine {
    pub dir: P,
    pub wmode: u8,
    pub joined: bool,
    pub chars: Vec<RawChar>,
}

pub(crate) enum RawBlock {
    Text(Vec<RawLine>),
    Image(ImageBlock),
}

pub(crate) struct StextDevice<'a> {
    pub doc: &'a Document,
    pub rect: R,
    pub options: TextFlags,
    pub blocks: Vec<RawBlock>,
    pub fonts: Vec<FontInfo>,
    font_ids: HashMap<u64, usize>,
    scissors: Vec<R>,
    text_clip: R,
    pen: P,
    lag_pen: P,
    start: P,
    last_char: char,
    last_bidi: u8,
    last_line: Option<(usize, usize)>,
    new_object: bool,
    maybe_bullet: bool,
    object: Option<u64>,
    argb: u32,
    char_flags: u32,
    pub last: Option<Last>,
    pub meta: Vec<crate::metatext::MetaText>,
    pub meta_counts: Vec<usize>,
    pub parents: Vec<i64>,
    pub parent_tree: Option<Vec<(i64, pdf_core::Object)>>,
    error: Option<crate::TextError>,
}

impl<'a> StextDevice<'a> {
    pub fn new(doc: &'a Document, page: &Page, rect: crate::Rect, options: TextFlags) -> Self {
        Self {
            doc,
            rect: R::from_rect(&rect),
            options,
            blocks: Vec::new(),
            fonts: Vec::new(),
            font_ids: HashMap::new(),
            scissors: Vec::new(),
            text_clip: R::EMPTY,
            pen: P::new(0.0, 0.0),
            lag_pen: P::new(0.0, 0.0),
            start: P::new(0.0, 0.0),
            last_char: ' ',
            last_bidi: 0,
            last_line: None,
            new_object: false,
            maybe_bullet: false,
            object: None,
            argb: 0,
            char_flags: 0,
            last: None,
            meta: Vec::new(),
            meta_counts: Vec::new(),
            parents: vec![page.dict.get_i64(b"StructParents").unwrap_or(-1)],
            parent_tree: None,
            error: None,
        }
    }

    fn font(&mut self, font: &PdfFont) -> usize {
        if let Some(&index) = self.font_ids.get(&font.id()) {
            return index;
        }
        let name = font.name();
        let flags = font.flags();
        let index = self.fonts.len();
        self.fonts.push(FontInfo {
            name: if name.as_bytes().get(6) == Some(&b'+') {
                name[7..].to_owned()
            } else {
                name.to_owned()
            },
            full_name: name.to_owned(),
            ascender: font.ascender(),
            descender: font.descender(),
            bbox: font.bbox(),
            bold: flags.bold,
            italic: flags.italic,
            serif: flags.serif,
            mono: flags.mono,
        });
        self.font_ids.insert(font.id(), index);
        index
    }

    pub fn clip(&self) -> R {
        self.scissors
            .last()
            .copied()
            .unwrap_or(R::INFINITE)
            .intersect(self.rect)
    }

    fn push_clip(&mut self, rect: R) {
        self.scissors.push(
            self.scissors
                .last()
                .map_or(rect, |prior| prior.intersect(rect)),
        );
    }

    fn glyph_box(&self, font: usize, trm: &M) -> R {
        let font = &self.fonts[font];
        let mut bbox = R::from_rect(&font.bbox);
        if bbox.is_empty() {
            bbox = R::new(0.0, font.descender as f32, 1.0, font.ascender as f32);
        }
        bbox.transform(trm)
    }

    pub fn extract_items(&mut self, items: &[Item], font: usize, wmode: u8, flags: &mut u32) {
        for (i, item) in items.iter().enumerate() {
            self.extract_item(item, font, wmode, flags, i == 0);
        }
    }

    pub fn extract_item(
        &mut self,
        item: &Item,
        font: usize,
        wmode: u8,
        flags: &mut u32,
        first: bool,
    ) {
        let clipped = if self.options.contains(TextFlags::MEDIABOX_CLIP) {
            let bbox = item.clip_bbox;
            let clip = self.clip();
            bbox.x1 <= clip.x0 || bbox.y1 <= clip.y0 || bbox.x0 >= clip.x1 || bbox.y0 >= clip.y1
        } else {
            false
        };
        if clipped {
            if let Some(last) = &mut self.last {
                last.clipped = true;
            }
            return;
        }
        self.last = Some(Last {
            item: item.clone(),
            font,
            wmode,
            flags: *flags,
            clipped: false,
            valid: true,
        });
        let mut item = item.clone();
        if item.c == Some('\u{fffd}') && self.options.contains(TextFlags::CID_FOR_UNKNOWN_UNICODE) {
            item.c = Some(char::from_u32(item.cid).unwrap_or('\u{fffd}'));
            *flags |= Char::UNICODE_IS_CID;
        }
        self.add(
            &item,
            font,
            wmode,
            *flags,
            first && self.options.contains(TextFlags::PRESERVE_SPANS),
        );
    }

    pub fn add(&mut self, item: &Item, font: usize, wmode: u8, flags: u32, force: bool) {
        let Some(c) = item.c else {
            return;
        };
        if !self.options.contains(TextFlags::PRESERVE_LIGATURES) {
            let expansion = match c {
                '\u{fb00}' => Some("ff"),
                '\u{fb01}' => Some("fi"),
                '\u{fb02}' => Some("fl"),
                '\u{fb03}' => Some("ffi"),
                '\u{fb04}' => Some("ffl"),
                '\u{fb05}' | '\u{fb06}' => Some("st"),
                _ => None,
            };
            if let Some(expansion) = expansion {
                for (i, c) in expansion.chars().enumerate() {
                    let mut part = item.clone();
                    part.c = Some(c);
                    if i > 0 {
                        part.glyph = -1;
                        part.adv = 0.0;
                    }
                    self.add_imp(&part, font, wmode, flags, i == 0 && force);
                }
                return;
            }
        }
        let mut item = item.clone();
        if !self.options.contains(TextFlags::PRESERVE_WHITESPACE)
            && matches!(
                c,
                '\t' | ' ' | '\u{a0}' | '\u{1680}' | '\u{180e}' | '\u{2000}'
                    ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            )
        {
            item.c = Some(' ');
        }
        self.add_imp(&item, font, wmode, flags, force);
    }

    fn current_line(&self) -> Option<&RawLine> {
        match self.blocks.last()? {
            RawBlock::Text(lines) => lines.last(),
            RawBlock::Image(_) => None,
        }
    }

    fn add_imp(&mut self, item: &Item, font: usize, wmode: u8, flags: u32, force: bool) {
        let Some(c) = item.c else {
            return;
        };
        let mut bidi = item.bidi & 1;
        let dir = if wmode == 0 {
            P::new(1.0, 0.0)
        } else {
            P::new(0.0, -1.0)
        }
        .transform_vector(&item.trm);
        let ndir = dir.normalize();
        let size = item.trm.expansion();
        let adv = if item.glyph < 0 && item.glyph != -2 {
            0.0
        } else {
            item.adv
        };
        let origin = P::new(item.trm.e, item.trm.f);
        let (p, q) = if wmode == 0 {
            (
                origin,
                P::new(adv.mul_add(dir.x, origin.x), adv.mul_add(dir.y, origin.y)),
            )
        } else {
            (
                P::new(
                    (-adv).mul_add(dir.x, origin.x),
                    (-adv).mul_add(dir.y, origin.y),
                ),
                origin,
            )
        };
        if self.current_line().is_some() && item.glyph == -1 {
            self.push_char(item, font, flags, bidi, (self.pen, self.pen), 0);
            self.last_bidi = bidi;
            self.last_char = c;
            return;
        }
        let mut new_para = false;
        let mut new_line = true;
        let mut add_space = 0;
        if let Some(line) = self.current_line() {
            if wmode != line.wmode || ndir.x * line.dir.x + ndir.y * line.dir.y < 0.999 {
                new_para = true;
            } else {
                let distance = (p.x - self.lag_pen.x).hypot(p.y - self.lag_pen.y);
                if distance / size < 0.1 && c == self.last_char && item.glyph >= 0 {
                    return;
                }
                let delta = P::new(p.x - self.pen.x, p.y - self.pen.y);
                let spacing = (ndir.x * delta.x + ndir.y * delta.y) / size;
                let base = (-ndir.y * delta.x + ndir.x * delta.y) / size;
                let can_space = wmode == 0 && may_add_space(self.last_char);
                if base.abs() < 0.8 {
                    if bidi != (self.last_bidi & 1) {
                        new_line = false;
                    } else if bidi & 1 != 0 {
                        let logical = (ndir.x * (p.x - self.lag_pen.x)
                            + ndir.y * (p.y - self.lag_pen.y))
                            / size
                            + adv;
                        if logical.abs() < 0.15 {
                            new_line = false;
                        } else if spacing.abs() < 0.15 {
                            bidi = 3;
                            new_line = false;
                        } else if logical > -0.8 && logical < 0.0 {
                            add_space = u8::from(can_space);
                            new_line = false;
                        } else if spacing > -0.8 && spacing < 0.0 {
                            new_line = false;
                        } else if spacing > 0.0 && spacing < 0.8 {
                            bidi = 3;
                            add_space = if can_space {
                                1 + u8::from(spacing > 0.3)
                            } else {
                                0
                            };
                            new_line = false;
                        }
                    } else if spacing.abs() < 0.15 || (spacing > -0.8 && spacing < 0.0) {
                        new_line = false;
                    } else if spacing > 0.0 && spacing < 0.8 {
                        add_space = if can_space {
                            1 + u8::from(spacing > 0.3)
                        } else {
                            0
                        };
                        new_line = false;
                    }
                } else if base.abs() <= 1.5 {
                    if wmode == 0
                        && self.new_object
                        && p.x - self.start.x > 0.5
                        && !self.maybe_bullet
                    {
                        new_para = true;
                    }
                } else {
                    new_para = true;
                }
            }
        } else {
            new_para = true;
        }
        if new_para {
            self.blocks.push(RawBlock::Text(Vec::new()));
        }
        if new_line && self.options.contains(TextFlags::DEHYPHENATE) && is_hyphen(self.last_char) {
            self.join_last_line();
        }
        if new_line || self.current_line().is_none() || force {
            if let Some(RawBlock::Text(lines)) = self.blocks.last_mut() {
                lines.push(RawLine {
                    dir: ndir,
                    wmode,
                    joined: false,
                    chars: Vec::new(),
                });
            }
            self.start = p;
            self.maybe_bullet = item.glyph == -2 || plausible_bullet(c);
        }
        if add_space != 0 && !self.options.contains(TextFlags::INHIBIT_SPACES) {
            let mut space = item.clone();
            space.c = Some(' ');
            self.push_char(&space, font, flags, bidi, (self.pen, p), add_space);
        }
        self.push_char(item, font, flags, bidi, (p, q), 0);
        self.last_char = c;
        self.last_bidi = bidi;
        self.lag_pen = p;
        self.pen = q;
        self.new_object = false;
    }

    fn push_char(
        &mut self,
        item: &Item,
        font: usize,
        mut flags: u32,
        bidi: u8,
        ends: (P, P),
        synthetic: u8,
    ) {
        let Some(c) = item.c else {
            return;
        };
        let index = self.blocks.len().saturating_sub(1);
        let Some(RawBlock::Text(lines)) = self.blocks.last_mut() else {
            return;
        };
        let line_index = lines.len().saturating_sub(1);
        let Some(line) = lines.last_mut() else {
            return;
        };
        let info = &self.fonts[font];
        let (a, d) = if line.wmode == 0 {
            (
                P::new(0.0, info.ascender as f32),
                P::new(0.0, info.descender as f32),
            )
        } else {
            (P::new(1.0, 0.0), P::new(0.0, 0.0))
        };
        let a = a.transform_vector(&item.trm);
        let d = d.transform_vector(&item.trm);
        let (p, q) = ends;
        let raw = Q {
            ul: P::new(p.x + a.x, p.y + a.y),
            ur: P::new(q.x + a.x, q.y + a.y),
            ll: P::new(p.x + d.x, p.y + d.y),
            lr: P::new(q.x + d.x, q.y + d.y),
        };
        if synthetic != 0 {
            flags |= Char::SYNTHETIC;
        }
        if synthetic > 1 {
            flags |= Char::SYNTHETIC_LARGE;
        }
        if info.bold {
            flags |= Char::BOLD;
        }
        let size = item.trm.expansion();
        let quad = crate::output::char_quad(raw, p, size, info, line.wmode, line.dir).quad();
        line.chars.push(RawChar {
            raw,
            value: Char {
                c,
                origin: p.point(),
                quad,
                size: f64::from(size),
                font,
                color: self.argb & 0xffffff,
                alpha: (self.argb >> 24) as u8,
                flags,
                bidi,
                source: item.source.clone(),
            },
        });
        self.last_line = Some((index, line_index));
    }

    fn join_last_line(&mut self) {
        if let Some((block, line)) = self.last_line
            && let Some(RawBlock::Text(lines)) = self.blocks.get_mut(block)
            && let Some(line) = lines.get_mut(line)
        {
            line.joined = true;
        }
    }

    pub fn flush_actual(&mut self, text: &str, advance: f32) {
        let Some(last) = self.last.clone() else {
            return;
        };
        if !last.valid || last.clipped || text.is_empty() {
            return;
        }
        let mut item = last.item;
        item.glyph = -2;
        item.adv = if advance == 0.0 {
            0.0
        } else {
            advance / text.chars().count() as f32
        };
        for (i, c) in text.chars().enumerate() {
            item.c = Some(c);
            item.trm.e = self.pen.x;
            item.trm.f = self.pen.y;
            self.add(
                &item,
                last.font,
                last.wmode,
                last.flags,
                i == 0 && self.options.contains(TextFlags::PRESERVE_SPANS),
            );
        }
    }

    pub fn finish(mut self) -> Result<TextPage, crate::TextError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        if self.options.contains(TextFlags::DEHYPHENATE) && is_hyphen(self.last_char) {
            self.join_last_line();
        }
        let mut blocks = Vec::with_capacity(self.blocks.len());
        for block in self.blocks {
            match block {
                RawBlock::Image(image) => blocks.push(Block::Image(image)),
                RawBlock::Text(raw_lines) => {
                    let mut bbox = R::EMPTY;
                    let mut lines = Vec::with_capacity(raw_lines.len());
                    for raw in raw_lines {
                        let line_box = raw
                            .chars
                            .iter()
                            .map(|c| c.raw.rect())
                            .reduce(R::union)
                            .unwrap_or(R::EMPTY);
                        bbox = bbox.union(line_box);
                        let mut chars: Vec<Char> = raw.chars.into_iter().map(|c| c.value).collect();
                        if chars.iter().any(|c| c.bidi == 3) {
                            for span in chars.split_mut(|c| c.bidi == 0) {
                                span.reverse();
                            }
                        }
                        lines.push(Line {
                            bbox: line_box.rect(),
                            dir: raw.dir.point(),
                            wmode: raw.wmode,
                            joined: raw.joined,
                            chars,
                        });
                    }
                    blocks.push(Block::Text(TextBlock {
                        bbox: bbox.rect(),
                        lines,
                    }));
                }
            }
        }
        Ok(TextPage {
            rect: self.rect.rect(),
            blocks,
            fonts: self.fonts,
        })
    }
}

impl Device for StextDevice<'_> {
    fn text(&mut self, run: &TextRun<'_>) {
        if run.glyphs.is_empty() {
            return;
        }
        let font = self.font(run.font);
        if self.object != Some(run.text_object) {
            self.object = Some(run.text_object);
            self.new_object = true;
            (self.argb, self.char_flags) = text_paint(run);
        }
        let mut items = Vec::with_capacity(run.glyphs.len());
        for glyph in run.glyphs {
            let trm = M::from_matrix(&glyph.trm);
            let coarse = self.glyph_box(font, &trm);
            let clip_bbox = if (self.options.contains(TextFlags::MEDIABOX_CLIP)
                && !self.clip().contains(&coarse))
                || run.clip
            {
                run.font
                    .glyph_bounds(glyph.gid)
                    .map_or(coarse, |bounds| R::from_rect(&bounds).transform(&trm))
            } else {
                coarse
            };
            if run.clip {
                self.text_clip = self.text_clip.union(clip_bbox);
            }
            for (i, &c) in glyph.unicode.as_slice().iter().enumerate() {
                items.push(Item {
                    c: Some(c),
                    glyph: if i == 0 { i64::from(glyph.gid) } else { -1 },
                    adv: if i == 0 { glyph.width as f32 } else { 0.0 },
                    trm,
                    clip_bbox,
                    cid: glyph.cid,
                    bidi: glyph.bidi,
                    source: run.glyph_source(glyph),
                });
            }
        }
        let mut flags = self.char_flags;
        if let Some(meta) = self.actual_index() {
            self.extract_actual(meta, &items, font, run.wmode, &mut flags);
        } else {
            self.extract_items(&items, font, run.wmode, &mut flags);
        }
    }

    fn clip_path(&mut self, path: &pdf_interp::Path, event: &pdf_interp::ClipEvent<'_>) {
        let rect = if event.text {
            std::mem::replace(&mut self.text_clip, R::EMPTY)
        } else {
            path.bounds(&event.ctm)
                .map_or(R::EMPTY, |r| R::from_rect(&r))
        };
        self.push_clip(rect);
    }

    fn pop_clip(&mut self) {
        self.scissors.pop();
    }
    fn begin_group(&mut self, event: &pdf_interp::GroupEvent<'_>) {
        self.push_clip(R::from_rect(&event.bbox));
    }
    fn end_group(&mut self) {
        self.scissors.pop();
    }

    fn fill_image(&mut self, event: &pdf_interp::ImageEvent<'_>) {
        self.image(event.image, event.ctm, event.alpha, event.source);
    }
    fn fill_image_mask(&mut self, event: &pdf_interp::ImageMaskEvent<'_>) {
        self.image(event.image, event.ctm, event.brush.alpha, event.source);
    }
    fn fill_shading(&mut self, event: &pdf_interp::ShadingEvent<'_>) {
        let bbox = event.shading.bbox().map_or(R::INFINITE, |r| {
            R::from_rect(&r).transform(&M::from_matrix(&event.matrix))
        });
        self.actual_bounds(bbox, event.source);
        if !self.options.contains(TextFlags::PRESERVE_IMAGES) || event.alpha < 0.5 {
            return;
        }
        let bbox = bbox.intersect(self.clip());
        if bbox.is_empty() {
            return;
        }
        let x0 = bbox.x0.floor();
        let y0 = bbox.y0.floor();
        let width = (bbox.x1.ceil() - x0) as u32;
        let height = (bbox.y1.ceil() - y0) as u32;
        let data_uri = match crate::html::shading_uri(event, x0, y0, width, height) {
            Ok(uri) => uri,
            Err(error) => {
                self.error.get_or_insert(error);
                return;
            }
        };
        self.blocks.push(RawBlock::Image(ImageBlock {
            bbox: bbox.rect(),
            transform: crate::Matrix::new(
                f64::from(width),
                0.0,
                0.0,
                f64::from(height),
                f64::from(x0),
                f64::from(y0),
            ),
            width,
            height,
            bpc: 8,
            colorspace: "DeviceRGB".to_owned(),
            source: event.source.clone(),
            data_uri,
        }));
    }

    fn begin_marked_content(&mut self, event: &pdf_interp::MarkedContentEvent<'_>) {
        self.begin_marked(event);
    }
    fn end_marked_content(&mut self) {
        self.end_marked();
    }
    fn begin_form(&mut self, event: &pdf_interp::FormEvent<'_>) {
        self.push_parent(event.form);
    }
    fn end_form(&mut self) {
        self.parents.pop();
    }
    fn begin_annotation(&mut self, event: &pdf_interp::AnnotationEvent<'_>) {
        self.push_parent(event.annot);
    }
    fn end_annotation(&mut self) {
        self.parents.pop();
    }
}

impl StextDevice<'_> {
    fn image(
        &mut self,
        image: &pdf_interp::PdfImage,
        ctm: crate::Matrix,
        alpha: f32,
        source: &GlyphSource,
    ) {
        let mut bbox = R::new(0.0, 0.0, 1.0, 1.0).transform(&M::from_matrix(&ctm));
        self.actual_bounds(bbox, source);
        if !self.options.contains(TextFlags::PRESERVE_IMAGES) || alpha < 0.5 {
            return;
        }
        if self.options.contains(TextFlags::MEDIABOX_CLIP) {
            bbox = bbox.intersect(self.clip());
        }
        let data_uri = match crate::html::image_uri(image) {
            Ok(uri) => uri,
            Err(error) => {
                self.error.get_or_insert(error);
                return;
            }
        };
        self.blocks.push(RawBlock::Image(ImageBlock {
            bbox: bbox.rect(),
            transform: ctm,
            width: image.width(),
            height: image.height(),
            bpc: image.bits_per_component(),
            colorspace: if image.is_mask() {
                "None".to_owned()
            } else {
                image.color_space_name()
            },
            source: source.clone(),
            data_uri,
        }));
    }
}

fn text_paint(run: &TextRun<'_>) -> (u32, u32) {
    let mode = if run.font.subtype() == FontSubtype::Type3 {
        if run.render_mode & 3 == 3 { 3 } else { 0 }
    } else {
        run.render_mode
    };
    if mode == 3 {
        return (0, 0);
    }
    if let Some(fill) = run.fill {
        return brush_paint(fill, Char::FILLED);
    }
    if let Some(stroke) = run.stroke {
        return brush_paint(stroke, Char::STROKED);
    }
    (0, Char::FILLED | Char::CLIPPED)
}

fn brush_paint(brush: Brush<'_>, flag: u32) -> (u32, u32) {
    match brush.paint {
        Paint::Color([r, g, b]) => {
            let byte = |v: f32| ((v * 255.0 + 0.5) as i32).clamp(0, 255) as u32;
            (
                byte(brush.alpha) << 24 | byte(r) << 16 | byte(g) << 8 | byte(b),
                flag,
            )
        }
        Paint::Shading(_) | Paint::Tiling(_) => (0, flag | Char::CLIPPED),
    }
}

fn may_add_space(c: char) -> bool {
    c != ' ' && (c < '\u{700}' || ('\u{2000}'..='\u{20cf}').contains(&c))
}
fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{ad}' | '\u{2010}' | '\u{2011}')
}
fn plausible_bullet(c: char) -> bool {
    matches!(c, '*' | '\u{b7}' | '\u{2022}' | '\u{2023}' | '\u{2043}' | '\u{204c}' | '\u{204d}' | '\u{2219}'
        | '\u{25c9}' | '\u{25cb}' | '\u{25cf}' | '\u{25d8}' | '\u{25e6}' | '\u{2619}'..='\u{261f}'
        | '\u{2765}' | '\u{2767}' | '\u{29be}' | '\u{29bf}' | '\u{2660}'..='\u{2667}'
        | '\u{1f446}'..='\u{1f449}' | '\u{1f597}'..='\u{1f5a3}' | '\u{1fbc1}'..='\u{1fbc3}' | '\u{fffd}')
}
