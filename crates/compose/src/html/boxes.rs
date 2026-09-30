//! CSS box generation, including anonymous blocks around mixed block/inline children.

use std::rc::Rc;
use url::Url;

use super::assets::Assets;
use super::css::PseudoElement;
use super::dom::{Dom, NodeData, NodeId};
use super::fonts::Fonts;
use super::image::{ImageId, Images};
use super::style::{Color, ContentItem, Display, ListType, Style, Stylist, percent_decode};

pub type SharedStyle = Rc<Style>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Uri(String),
    Anchor(String),
}

#[derive(Clone, Default)]
pub struct Marks {
    pub anchor: Option<String>,
    pub bookmark: Option<(u8, String)>,
    pub link: Option<Link>,
}

pub struct Block {
    pub style: SharedStyle,
    pub kind: Kind,
    pub marks: Marks,
    pub marker: Option<String>,
}

pub enum Kind {
    Flow(Vec<Block>),
    Inline(Vec<Inline>),
    Table(Table),
    Image(ImageId),
}

pub enum Inline {
    Text(String, SharedStyle),
    Open(SharedStyle, Marks),
    Close,
    Image(ImageId, SharedStyle),
    Block(Box<Block>),
    Break(SharedStyle),
}

pub struct Table {
    pub captions: Vec<Block>,
    pub rows: Vec<Row>,
    pub columns: usize,
    pub headers: usize,
}

pub struct Row {
    pub style: SharedStyle,
    pub cells: Vec<Cell>,
}

pub struct Cell {
    pub block: Block,
    pub column: usize,
    pub colspan: usize,
    pub rowspan: usize,
}

struct Flow {
    style: SharedStyle,
    blocks: Vec<Block>,
    inline: Vec<Inline>,
    open: Vec<(SharedStyle, Marks)>,
}

impl Flow {
    fn new(style: SharedStyle) -> Self {
        Self {
            style,
            blocks: Vec::new(),
            inline: Vec::new(),
            open: Vec::new(),
        }
    }

    fn open(&mut self, style: SharedStyle, marks: Marks) {
        self.open.push((Rc::clone(&style), marks.clone()));
        self.inline.push(Inline::Open(style, marks));
    }

    fn close(&mut self) {
        self.open.pop();
        self.inline.push(Inline::Close);
    }

    fn flush(&mut self) {
        if meaningful(&self.inline) {
            self.inline.extend(self.open.iter().map(|_| Inline::Close));
            self.blocks.push(Block {
                style: Rc::new(self.style.anonymous(Display::Block)),
                kind: Kind::Inline(std::mem::take(&mut self.inline)),
                marks: Marks::default(),
                marker: None,
            });
        } else {
            self.inline.clear();
        }
        self.inline.extend(
            self.open
                .iter()
                .map(|(style, marks)| Inline::Open(Rc::clone(style), marks.clone())),
        );
    }

    fn finish(mut self) -> Kind {
        if self.blocks.is_empty() && !matches!(self.style.display, Display::Flex | Display::Grid) {
            Kind::Inline(self.inline)
        } else {
            self.flush();
            Kind::Flow(self.blocks)
        }
    }
}

fn meaningful(inline: &[Inline]) -> bool {
    inline.iter().any(|item| match item {
        Inline::Text(text, style) => {
            !style.white_space.collapses() || text.chars().any(|c| !c.is_ascii_whitespace())
        }
        Inline::Open(_, marks) => marks.anchor.is_some(),
        Inline::Close => false,
        _ => true,
    })
}

pub fn build(
    dom: &Dom,
    stylist: &mut Stylist,
    fonts: &mut Fonts,
    images: &mut Images,
    assets: &Assets,
    base: &Url,
) -> (Block, Color) {
    let initial = Stylist::initial(fonts);
    let Some(root) = dom.root_element() else {
        return (
            Block {
                style: Rc::new(initial),
                kind: Kind::Inline(Vec::new()),
                marks: Marks::default(),
                marker: None,
            },
            Color::TRANSPARENT,
        );
    };
    let style = stylist.compute(dom, root, None, &initial, fonts);
    stylist.set_root_font_size(style.font_size);
    let background = style.background;
    let mut builder = Builder {
        dom,
        stylist,
        fonts,
        images,
        assets,
        base,
        quotes: 0,
        background,
    };
    let block = builder.block(root, Rc::new(style), 0);
    (block, builder.background)
}

struct Builder<'a> {
    dom: &'a Dom,
    stylist: &'a Stylist,
    fonts: &'a mut Fonts,
    images: &'a mut Images,
    assets: &'a Assets,
    base: &'a Url,
    quotes: usize,
    background: Color,
}

impl Builder<'_> {
    fn marks(&self, id: NodeId, style: &Style) -> Marks {
        let anchor = self
            .dom
            .attr(id, "id")
            .or_else(|| {
                (self.dom.tag(id) == "a")
                    .then(|| self.dom.attr(id, "name"))
                    .flatten()
            })
            .map(str::to_owned);
        let bookmark = style.bookmark_level.map(|level| {
            (
                level,
                self.dom
                    .text_content(id)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        });
        let link = self
            .dom
            .attr(id, "href")
            .filter(|_| self.dom.tag(id) == "a")
            .and_then(|href| {
                if let Some(id) = href.strip_prefix('#') {
                    Some(Link::Anchor(percent_decode(id)))
                } else if href.contains(':') {
                    Some(Link::Uri(href.to_owned()))
                } else {
                    self.base.join(href).ok().map(|url| Link::Uri(url.into()))
                }
            });
        Marks {
            anchor,
            bookmark,
            link,
        }
    }

    fn image(&mut self, id: NodeId) -> Option<ImageId> {
        if self.dom.tag(id) == "svg" {
            return self
                .images
                .load_svg(self.assets, self.base, &self.dom.svg(id), self.fonts);
        }
        let attribute = if self.dom.tag(id) == "object" {
            "data"
        } else {
            "src"
        };
        let source = self.dom.attr(id, attribute)?;
        self.images.load(self.assets, self.base, source, self.fonts)
    }

    fn block(&mut self, id: NodeId, style: SharedStyle, depth: usize) -> Block {
        let marks = self.marks(id, &style);
        if self.dom.tag(id) == "body" && !self.background.visible() {
            self.background = style.background;
        }
        let marker = (style.display == Display::ListItem)
            .then(|| marker(&style.list_type, self.ordinal(id)));
        let kind = if style.display == Display::None {
            Kind::Inline(Vec::new())
        } else if matches!(self.dom.tag(id), "img" | "svg" | "object" | "embed")
            && let Some(image) = self.image(id)
        {
            Kind::Image(image)
        } else if style.display == Display::Table {
            Kind::Table(self.table(id, &style, depth + 1))
        } else {
            let mut flow = Flow::new(Rc::clone(&style));
            if style.list_inside
                && let Some(marker) = &marker
            {
                flow.inline
                    .push(Inline::Text(marker.clone(), Rc::clone(&style)));
            }
            self.pseudo(id, PseudoElement::Before, &style, &mut flow);
            self.children(id, &style, &mut flow, depth + 1);
            self.pseudo(id, PseudoElement::After, &style, &mut flow);
            flow.finish()
        };
        let marker = marker.filter(|_| !style.list_inside);
        Block {
            style,
            kind,
            marks,
            marker,
        }
    }

    fn children(&mut self, id: NodeId, parent: &SharedStyle, flow: &mut Flow, depth: usize) {
        if depth > 128 {
            flow.inline
                .push(Inline::Text(self.dom.text_content(id), Rc::clone(parent)));
            return;
        }
        let dom = self.dom;
        for &child in dom.children(id) {
            match &dom.nodes[child].data {
                NodeData::Text(text) => flow
                    .inline
                    .push(Inline::Text(text.clone(), Rc::clone(parent))),
                NodeData::Element { .. } => self.element(child, parent, flow, depth),
                _ => {}
            }
        }
    }

    fn element(&mut self, id: NodeId, parent: &SharedStyle, flow: &mut Flow, depth: usize) {
        let mut computed = self.stylist.compute(self.dom, id, None, parent, self.fonts);
        if matches!(parent.display, Display::Flex | Display::Grid)
            && matches!(computed.display, Display::Inline | Display::InlineBlock)
        {
            computed.display = Display::Block;
        }
        let style = Rc::new(computed);
        match style.display {
            Display::None | Display::Column | Display::ColumnGroup => {}
            Display::Inline => {
                let marks = self.marks(id, &style);
                flow.open(Rc::clone(&style), marks);
                match self.dom.tag(id) {
                    "br" => flow.inline.push(Inline::Break(Rc::clone(&style))),
                    "wbr" => flow
                        .inline
                        .push(Inline::Text("\u{200b}".to_owned(), Rc::clone(&style))),
                    "img" | "svg" | "object" | "embed" => {
                        if let Some(image) = self.image(id) {
                            flow.inline.push(Inline::Image(image, Rc::clone(&style)));
                        } else if let Some(alt) = self.dom.attr(id, "alt") {
                            flow.inline
                                .push(Inline::Text(alt.to_owned(), Rc::clone(&style)));
                        }
                    }
                    _ => {
                        self.pseudo(id, PseudoElement::Before, &style, flow);
                        self.children(id, &style, flow, depth + 1);
                        self.pseudo(id, PseudoElement::After, &style, flow);
                    }
                }
                flow.close();
            }
            Display::InlineBlock => {
                if matches!(self.dom.tag(id), "img" | "svg" | "object" | "embed")
                    && let Some(image) = self.image(id)
                {
                    flow.inline.push(Inline::Image(image, style));
                } else {
                    flow.inline
                        .push(Inline::Block(Box::new(self.block(id, style, depth + 1))));
                }
            }
            _ => {
                flow.flush();
                flow.blocks.push(self.block(id, style, depth + 1));
            }
        }
    }

    fn pseudo(&mut self, id: NodeId, pseudo: PseudoElement, parent: &SharedStyle, flow: &mut Flow) {
        if !self.stylist.author_pseudo && self.dom.tag(id) != "q" {
            return;
        }
        let style = self
            .stylist
            .compute(self.dom, id, Some(pseudo), parent, self.fonts);
        if style.display == Display::None {
            return;
        }
        let Some(content) = &style.content else {
            return;
        };
        let mut text = String::new();
        for item in content {
            match item {
                ContentItem::Text(s) => text.push_str(s),
                ContentItem::OpenQuote => {
                    text.push(if self.quotes.is_multiple_of(2) {
                        '“'
                    } else {
                        '‘'
                    });
                    self.quotes += 1;
                }
                ContentItem::CloseQuote => {
                    self.quotes = self.quotes.saturating_sub(1);
                    text.push(if self.quotes.is_multiple_of(2) {
                        '”'
                    } else {
                        '’'
                    });
                }
                ContentItem::Attr(_) => {}
            }
        }
        let style = Rc::new(style);
        flow.open(Rc::clone(&style), Marks::default());
        flow.inline.push(Inline::Text(text, style));
        flow.close();
    }

    fn ordinal(&self, id: NodeId) -> i64 {
        let Some(parent) = self.dom.parent(id) else {
            return 1;
        };
        // Weasyprint does not enable presentational hints for these commands.
        let mut value = 1i64;
        for sibling in self.dom.element_children(parent) {
            if self.dom.tag(sibling) != "li" {
                continue;
            }
            if sibling == id {
                break;
            }
            value = value.saturating_add(1);
        }
        value
    }

    fn row(&mut self, id: NodeId, style: SharedStyle, depth: usize) -> Row {
        let mut cells = Vec::new();
        for child in self.dom.element_children(id) {
            let cell_style = Rc::new(
                self.stylist
                    .compute(self.dom, child, None, &style, self.fonts),
            );
            if cell_style.display != Display::Cell {
                continue;
            }
            let span = |name, limit| {
                self.dom
                    .attr(child, name)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, limit)
            };
            let (colspan, rowspan) = (span("colspan", 1000), span("rowspan", 65534));
            cells.push(Cell {
                block: self.block(child, cell_style, depth + 1),
                column: 0,
                colspan,
                rowspan,
            });
        }
        Row { style, cells }
    }

    fn table(&mut self, id: NodeId, style: &SharedStyle, depth: usize) -> Table {
        let (mut captions, mut headers, mut bodies, mut footers) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for child in self.dom.element_children(id) {
            let child_style = Rc::new(
                self.stylist
                    .compute(self.dom, child, None, style, self.fonts),
            );
            match child_style.display {
                Display::Caption => captions.push(self.block(child, child_style, depth + 1)),
                Display::Row => bodies.push(self.row(child, child_style, depth + 1)),
                Display::RowGroup | Display::HeaderGroup | Display::FooterGroup => {
                    let rows = match child_style.display {
                        Display::HeaderGroup => &mut headers,
                        Display::FooterGroup => &mut footers,
                        _ => &mut bodies,
                    };
                    for row in self.dom.element_children(child) {
                        let mut computed =
                            self.stylist
                                .compute(self.dom, row, None, &child_style, self.fonts);
                        if !computed.background.visible() {
                            computed.background = child_style.background;
                        }
                        let row_style = Rc::new(computed);
                        if row_style.display == Display::Row {
                            rows.push(self.row(row, row_style, depth + 1));
                        }
                    }
                }
                _ => {}
            }
        }
        let header_count = headers.len();
        headers.extend(bodies);
        headers.extend(footers);
        let mut busy_until: Vec<usize> = Vec::new();
        let row_count = headers.len();
        for (r, row) in headers.iter_mut().enumerate() {
            let mut column = 0;
            for cell in &mut row.cells {
                while busy_until.get(column).is_some_and(|until| *until > r) {
                    column += 1;
                }
                cell.column = column;
                cell.rowspan = cell.rowspan.min(row_count - r);
                let end = (column + cell.colspan).min(4096);
                cell.colspan = end.saturating_sub(column);
                if cell.colspan == 0 {
                    continue;
                }
                busy_until.resize(busy_until.len().max(end), 0);
                busy_until[column..end].fill(r + cell.rowspan);
                column = end;
            }
        }
        Table {
            captions,
            rows: headers,
            columns: busy_until.len(),
            headers: header_count,
        }
    }
}

fn marker(kind: &ListType, n: i64) -> String {
    let number = match kind {
        ListType::None => return String::new(),
        ListType::Disc => return "• ".to_owned(),
        ListType::Circle => return "◦ ".to_owned(),
        ListType::Square => return "▪ ".to_owned(),
        ListType::Str(s) => return s.to_string(),
        ListType::Decimal => n.to_string(),
        ListType::DecimalLeadingZero => format!("{n:02}"),
        ListType::LowerAlpha => alphabetic(n, "abcdefghijklmnopqrstuvwxyz"),
        ListType::UpperAlpha => alphabetic(n, "ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        ListType::LowerGreek => alphabetic(n, "αβγδεζηθικλμνξοπρστυφχψω"),
        ListType::LowerRoman => roman(n).to_lowercase(),
        ListType::UpperRoman => roman(n),
    };
    format!("{number}. ")
}

fn alphabetic(mut n: i64, alphabet: &str) -> String {
    if n < 1 {
        return n.to_string();
    }
    let chars: Vec<char> = alphabet.chars().collect();
    let base = chars.len() as i64;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(chars[(n % base) as usize]);
        n /= base;
    }
    out.into_iter().rev().collect()
}

fn roman(mut n: i64) -> String {
    if !(1..4000).contains(&n) {
        return n.to_string();
    }
    let mut out = String::new();
    for (value, numeral) in [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}
