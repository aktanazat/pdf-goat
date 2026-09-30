use crate::geom::{M, P, Q, R};
use crate::{Block, Char, FontInfo, Line, Point, Rect, TextPage};

#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub rect: Rect,
    pub text: String,
    pub block: usize,
    pub line: usize,
    pub word: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockEntry {
    pub rect: Rect,
    pub text: String,
    pub block: usize,
    /// 0 text, 1 image.
    pub kind: u8,
}

#[derive(Clone, Debug)]
pub struct Span<'a> {
    pub font: &'a FontInfo,
    pub size: f64,
    /// Superscript (1), italic (2), serif (4), monospace (8), bold (16).
    pub flags: u32,
    pub color: u32,
    pub origin: Point,
    pub bbox: Rect,
    pub text: String,
    pub chars: &'a [Char],
}

/// PyMuPDF's `JM_char_quad`. Low font metrics are normalized to one em;
/// the line/block bounds intentionally retain the unadjusted quads.
pub(crate) fn char_quad(
    mut quad: Q,
    origin: P,
    size: f32,
    font: &FontInfo,
    wmode: u8,
    dir: P,
) -> Q {
    let mut asc = font.ascender as f32;
    let mut dsc = font.descender as f32;
    let mut height = asc - dsc;
    if wmode != 0 || height + f32::EPSILON >= 1.0 {
        return quad;
    }
    if asc < 1e-3 {
        asc = 0.9;
        dsc = -0.1;
        height = 1.0;
    }
    if height < 1.0 {
        asc /= height;
        dsc /= height;
    }
    height = asc - dsc;
    asc = asc * size / height;
    dsc = dsc * size / height;
    let c = dir.x;
    let s = dir.y;
    quad = quad.transform(&M::new(1.0, 0.0, 0.0, 1.0, -origin.x, -origin.y));
    quad = quad.transform(&M::new(c, -s, s, if c == -1.0 { 1.0 } else { c }, 0.0, 0.0));
    if c == 1.0 && quad.ul.y > 0.0 {
        quad.ul.y = asc;
        quad.ur.y = asc;
        quad.ll.y = dsc;
        quad.lr.y = dsc;
    } else {
        quad.ul.y = -asc;
        quad.ur.y = -asc;
        quad.ll.y = -dsc;
        quad.lr.y = -dsc;
    }
    if quad.ll.x < 0.0 {
        quad.ll.x = 0.0;
        quad.ul.x = 0.0;
    }
    quad = quad.transform(&M::new(c, s, -s, if c == -1.0 { 1.0 } else { c }, 0.0, 0.0));
    quad.transform(&M::new(1.0, 0.0, 0.0, 1.0, origin.x, origin.y))
}

impl Char {
    /// PyMuPDF's `JM_char_bbox`, including its vertical-writing minimum
    /// height. Calculations stay f32, as in MuPDF.
    pub fn bbox(&self, line: &Line) -> Rect {
        let mut rect = Q::from_quad(&self.quad).rect();
        let size = self.size as f32;
        if line.wmode != 0 && rect.y1 < rect.y0 + size {
            rect.y0 = rect.y1 - size;
        }
        rect.rect()
    }
}

impl TextPage {
    pub(crate) fn includes(&self, rect: &Rect) -> bool {
        let page = R::from_rect(&self.rect);
        page.is_infinite() || page.overlaps(&R::from_rect(rect))
    }

    /// `TextPage.extractText(sort=False)`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            let Block::Text(block) = block else {
                continue;
            };
            let mut last = '\0';
            for line in &block.lines {
                for (i, ch) in line.chars.iter().enumerate() {
                    if line.joined && i + 1 == line.chars.len() && is_hyphen(ch.c) {
                        continue;
                    }
                    if self.includes(&ch.bbox(line)) {
                        out.push(ch.c);
                        last = ch.c;
                    }
                }
                if !line.joined && last != '\n' && last != '\0' {
                    out.push('\n');
                    last = '\n';
                }
            }
            if last != '\n' && last != '\0' {
                out.push('\n');
            }
        }
        out
    }

    /// `page.get_text("text", sort=True)`, preserving physical word gaps.
    pub fn text_sorted(&self) -> String {
        crate::layout::sorted_text(&self.words())
    }

    /// `TextPage.extractWORDS`: the same delimiters, directional splits,
    /// and per-line word indices as PyMuPDF.
    pub fn words(&self) -> Vec<Word> {
        let mut out = Vec::new();
        let text_blocks = self.blocks.iter().filter_map(|block| match block {
            Block::Text(b) => Some(b),
            Block::Image(_) => None,
        });
        for (block_index, block) in text_blocks.enumerate() {
            let mut last_rtl = false;
            for (line_index, line) in block.lines.iter().enumerate() {
                let mut text = String::new();
                let mut rect = R::EMPTY;
                let mut word_index = 0;
                for ch in &line.chars {
                    let bbox = ch.bbox(line);
                    if !self.includes(&bbox) || (ch.c == '\u{200d}' && text.is_empty()) {
                        continue;
                    }
                    let delimiter = ch.c <= ' '
                        || ch.c == '\u{a0}'
                        || ('\u{202a}'..='\u{202e}').contains(&ch.c);
                    let rtl = ('\u{590}'..='\u{900}').contains(&ch.c);
                    if delimiter || rtl != last_rtl {
                        if !text.is_empty() {
                            if !rect.is_empty() {
                                out.push(Word {
                                    rect: rect.rect(),
                                    text: std::mem::take(&mut text),
                                    block: block_index,
                                    line: line_index,
                                    word: word_index,
                                });
                                word_index += 1;
                            }
                            text.clear();
                            rect = R::EMPTY;
                        }
                        if delimiter {
                            continue;
                        }
                    }
                    text.push(ch.c);
                    last_rtl = rtl;
                    rect = rect.union(R::from_rect(&bbox));
                }
                if !text.is_empty() && !rect.is_empty() {
                    out.push(Word {
                        rect: rect.rect(),
                        text,
                        block: block_index,
                        line: line_index,
                        word: word_index,
                    });
                }
            }
        }
        out
    }

    /// `TextPage.extractBLOCKS`, optionally sorted by `(y1, x0)`.
    pub fn blocks(&self, sort: bool) -> Vec<BlockEntry> {
        let mut out = Vec::new();
        for block in &self.blocks {
            match block {
                Block::Text(block) => {
                    let mut text = String::new();
                    let mut rect = R::EMPTY;
                    let mut last = '\n';
                    for line in &block.lines {
                        let mut line_rect = R::EMPTY;
                        for ch in &line.chars {
                            let bbox = ch.bbox(line);
                            if self.includes(&bbox) {
                                text.push(ch.c);
                                last = ch.c;
                                line_rect = line_rect.union(R::from_rect(&bbox));
                            }
                        }
                        if last != '\n' && !line_rect.is_empty() {
                            text.push('\n');
                        }
                        rect = rect.union(line_rect);
                    }
                    out.push(BlockEntry {
                        rect: rect.rect(),
                        text,
                        block: out.len(),
                        kind: 0,
                    });
                }
                Block::Image(image) => {
                    let page = R::from_rect(&self.rect);
                    if page.is_infinite() || page.contains(&R::from_rect(&image.bbox)) {
                        out.push(BlockEntry {
                            rect: image.bbox,
                            block: out.len(),
                            kind: 1,
                            text: format!(
                                "<image: {}, width: {}, height: {}, bpc: {}>\n",
                                image.colorspace, image.width, image.height, image.bpc
                            ),
                        });
                    }
                }
            }
        }
        if sort {
            out.sort_by(|a, b| {
                a.rect
                    .y1
                    .total_cmp(&b.rect.y1)
                    .then(a.rect.x0.total_cmp(&b.rect.x0))
            });
        }
        out
    }
}

impl Line {
    /// PyMuPDF's style spans. Synthetic spaces do not split the style.
    pub fn spans<'a>(&'a self, page: &'a TextPage) -> Vec<Span<'a>> {
        let mut out = Vec::new();
        let mut start: Option<usize> = None;
        let mut text = String::new();
        let mut bbox = R::EMPTY;
        for (i, ch) in self.chars.iter().enumerate() {
            if !page.includes(&ch.bbox(self)) || ch.font >= page.fonts.len() {
                continue;
            }
            if let Some(first) = start
                && !self.same_style(&self.chars[first], ch, page)
            {
                out.push(self.span(page, first, i, std::mem::take(&mut text), bbox));
                start = None;
                bbox = R::EMPTY;
            }
            start.get_or_insert(i);
            text.push(ch.c);
            bbox = bbox.union(R::from_rect(&ch.bbox(self)));
        }
        if let Some(start) = start {
            out.push(self.span(page, start, self.chars.len(), text, bbox));
        }
        out
    }

    fn flags(&self, ch: &Char, font: &FontInfo) -> u32 {
        let superscript = self.wmode == 0
            && self.dir.x == 1.0
            && self.dir.y == 0.0
            && self
                .chars
                .first()
                .is_some_and(|first| ch.origin.y < first.origin.y - ch.size * 0.1);
        u32::from(superscript)
            | u32::from(font.italic) << 1
            | u32::from(font.serif) << 2
            | u32::from(font.mono) << 3
            | u32::from(font.bold) << 4
    }

    fn same_style(&self, a: &Char, b: &Char, page: &TextPage) -> bool {
        let af = &page.fonts[a.font];
        let bf = &page.fonts[b.font];
        a.size == b.size
            && self.flags(a, af) == self.flags(b, bf)
            && af.name == bf.name
            && a.flags & !Char::SYNTHETIC == b.flags & !Char::SYNTHETIC
            && a.color == b.color
            && a.alpha == b.alpha
            && a.bidi == b.bidi
    }

    fn span<'a>(
        &'a self,
        page: &'a TextPage,
        start: usize,
        end: usize,
        text: String,
        bbox: R,
    ) -> Span<'a> {
        let first = &self.chars[start];
        let font = &page.fonts[first.font];
        Span {
            font,
            size: first.size,
            flags: self.flags(first, font),
            color: first.color,
            origin: first.origin,
            bbox: bbox.rect(),
            text,
            chars: &self.chars[start..end],
        }
    }
}

fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{ad}' | '\u{2010}' | '\u{2011}')
}
