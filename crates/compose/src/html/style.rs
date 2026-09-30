//! The cascade and computed values: a user-agent sheet after weasyprint's, author
//! `<style>` and `<link rel=stylesheet>` sheets, `style` attributes, inheritance, and the
//! `@page` size and margins. Lengths compute to CSS px.

use std::collections::HashMap;
use std::rc::Rc;
use url::Url;

use super::assets::Assets;
use super::css::{self, Cv, Declaration, PseudoElement, Rule, Selector, Token};
use super::dom::{Dom, NodeId};
use super::fonts::{FamilyId, Fonts};

/// The parts of weasyprint's `html5_ua.css` this renderer acts on.
const UA_CSS: &str = r#"
[hidden], area, base, basefont, command, datalist, head, input[type=hidden i], link, meta, noembed, noframes, param, rp, script, source, style, template, title, track { display: none }
[dir=rtl] { direction: rtl }
[dir=ltr] { direction: ltr }
address, article, aside, blockquote, body, center, dd, details, dir, div, dl, dt, frame, frameset, fieldset, figure, figcaption, footer, form, h1, h2, h3, h4, h5, h6, header, hgroup, hr, html, legend, listing, main, menu, nav, ol, p, plaintext, pre, section, summary, ul, xmp { display: block }
button, input, keygen, select, textarea { display: inline-block }
li { display: list-item }
table { display: table }
caption { display: table-caption }
colgroup { display: table-column-group }
col { display: table-column }
thead { display: table-header-group }
tbody { display: table-row-group }
tfoot { display: table-footer-group }
tr { display: table-row }
td, th { display: table-cell }
blockquote, dir, dl, figure, listing, menu, ol, p, plaintext, pre, ul, xmp { margin-top: 1em; margin-bottom: 1em }
:is(dir, dl, menu, ol, ul) :is(dir, dl, menu, ol, ul) { margin-top: 0; margin-bottom: 0 }
body { margin: 8px }
h1 { margin-top: .67em; margin-bottom: .67em }
h2 { margin-top: .83em; margin-bottom: .83em }
h3 { margin-top: 1em; margin-bottom: 1em }
h4 { margin-top: 1.33em; margin-bottom: 1.33em }
h5 { margin-top: 1.67em; margin-bottom: 1.67em }
h6 { margin-top: 2.33em; margin-bottom: 2.33em }
blockquote, figure { margin-left: 40px; margin-right: 40px }
dd { margin-left: 40px }
dir, menu, ol, ul { padding-left: 40px }
table { border-spacing: 2px; border-collapse: separate }
td, th { padding: 1px }
thead, tbody, tfoot, table > tr { vertical-align: middle }
tr, td, th { vertical-align: inherit }
sub { vertical-align: sub }
sup { vertical-align: super }
address, cite, dfn, em, i, var { font-style: italic }
b, strong, th { font-weight: bold }
code, kbd, listing, plaintext, pre, samp, tt, xmp { font-family: monospace }
h1 { font-size: 2em; font-weight: bold }
h2 { font-size: 1.5em; font-weight: bold }
h3 { font-size: 1.17em; font-weight: bold }
h4 { font-size: 1em; font-weight: bold }
h5 { font-size: .83em; font-weight: bold }
h6 { font-size: .67em; font-weight: bold }
big { font-size: larger }
small, sub, sup { font-size: smaller }
sub, sup { line-height: normal }
:link { color: blue }
mark { background: yellow; color: black }
table, td, th { border-color: gray }
thead, tbody, tfoot, tr { border-color: inherit }
:link, :visited, ins, u { text-decoration: underline }
abbr[title], acronym[title] { text-decoration: dotted underline }
del, s, strike { text-decoration: line-through }
q::before { content: open-quote }
q::after { content: close-quote }
nobr { white-space: nowrap }
hr { border-style: inset; border-width: 1px; color: gray; margin: .5em auto }
listing, plaintext, pre, xmp { white-space: pre }
textarea { white-space: pre-wrap }
ol { list-style-type: decimal }
dir, menu, ul { list-style-type: disc }
:is(dir, menu, ol, ul) ul { list-style-type: circle }
:is(dir, menu, ol, ul) :is(dir, menu, ol, ul) ul { list-style-type: square }
center { text-align: center }
table { box-sizing: border-box }
h1 { bookmark-level: 1 }
h2 { bookmark-level: 2 }
h3 { bookmark-level: 3 }
h4 { bookmark-level: 4 }
h5 { bookmark-level: 5 }
h6 { bookmark-level: 6 }
h1, h2, h3, h4, h5, h6 { break-after: avoid; break-inside: avoid }
ol, ul { break-before: avoid }
button, input, select, textarea { border: 1px solid black; font-size: .85em; padding: .2em; white-space: pre }
@page { margin: 75px }
"#;

/// `@import` and `<link>` nesting bound.
const MAX_IMPORT_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const BLACK: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    pub const TRANSPARENT: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    fn rgb8(r: u8, g: u8, b: u8) -> Color {
        Color {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: 1.0,
        }
    }

    pub fn visible(self) -> bool {
        self.a > 0.0
    }
}

/// A computed length: px, a percentage of the containing block, or `auto` (`none` for
/// the max- properties).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Len {
    Auto,
    Px(f32),
    Pct(f32),
}

impl Len {
    pub fn resolve(self, base: f32) -> Option<f32> {
        match self {
            Len::Auto => None,
            Len::Px(px) => Some(px),
            Len::Pct(pct) => Some(base * pct / 100.0),
        }
    }

    pub fn or_zero(self, base: f32) -> f32 {
        self.resolve(base).unwrap_or(0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    None,
    Block,
    Flex,
    Grid,
    Inline,
    InlineBlock,
    ListItem,
    Table,
    RowGroup,
    HeaderGroup,
    FooterGroup,
    Row,
    Cell,
    Caption,
    Column,
    ColumnGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhiteSpace {
    Normal,
    Pre,
    Nowrap,
    PreWrap,
    PreLine,
}

impl WhiteSpace {
    pub fn collapses(self) -> bool {
        matches!(
            self,
            WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
        )
    }

    pub fn keeps_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }

    pub fn wraps(self) -> bool {
        matches!(
            self,
            WhiteSpace::Normal | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineHeight {
    Normal,
    Number(f32),
    Px(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VAlign {
    Baseline,
    Sub,
    Super,
    TextTop,
    TextBottom,
    Middle,
    Top,
    Bottom,
    /// Raise by this many px.
    Px(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    None,
    Upper,
    Lower,
    Capitalize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ListType {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    LowerGreek,
    Str(Rc<str>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Break {
    Auto,
    Avoid,
    Page,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorderStyle {
    None,
    Solid,
    Dashed,
    Dotted,
    Double,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContentItem {
    Text(String),
    Attr(String),
    OpenQuote,
    CloseQuote,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Decoration {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
}

#[derive(Clone, Debug)]
pub struct Style {
    // Inherited.
    pub color: Color,
    pub family: FamilyId,
    pub font_size: f32,
    pub weight: u16,
    pub italic: bool,
    pub line_height: LineHeight,
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    pub text_indent: Len,
    pub letter_spacing: f32,
    pub word_spacing: f32,
    pub transform: Transform,
    pub list_type: ListType,
    pub list_inside: bool,
    pub border_collapse: bool,
    pub border_spacing: (f32, f32),
    pub visible: bool,
    pub orphans: u32,
    pub widows: u32,
    pub break_words: bool,
    pub rtl: bool,
    // Not inherited.
    pub display: Display,
    pub margin: [Len; 4],
    pub padding: [Len; 4],
    pub border_width: [f32; 4],
    pub border_style: [BorderStyle; 4],
    pub border_color: [Color; 4],
    pub radius: f32,
    pub width: Len,
    pub height: Len,
    pub min_width: Len,
    pub max_width: Len,
    pub min_height: Len,
    pub max_height: Len,
    pub border_box: bool,
    pub background: Color,
    pub decoration: Decoration,
    pub decoration_color: Option<Color>,
    pub vertical_align: VAlign,
    pub break_before: Break,
    pub break_after: Break,
    pub avoid_break_inside: bool,
    pub content: Option<Vec<ContentItem>>,
    pub bookmark_level: Option<u8>,
    pub position: Position,
    pub inset: [Len; 4],
    /// `Some(false)` is left, `Some(true)` is right.
    pub float: Option<bool>,
    /// Bit 1 clears left floats, bit 2 clears right floats.
    pub clear: u8,
    pub order: i32,
    pub formatting: Option<Rc<taffy::Style>>,
}

impl Style {
    fn initial(family: FamilyId) -> Style {
        Style {
            color: Color::BLACK,
            family,
            font_size: 16.0,
            weight: 400,
            italic: false,
            line_height: LineHeight::Normal,
            text_align: TextAlign::Start,
            white_space: WhiteSpace::Normal,
            text_indent: Len::Px(0.0),
            letter_spacing: 0.0,
            word_spacing: 0.0,
            transform: Transform::None,
            list_type: ListType::Disc,
            list_inside: false,
            border_collapse: false,
            border_spacing: (0.0, 0.0),
            visible: true,
            orphans: 2,
            widows: 2,
            break_words: false,
            rtl: false,
            display: Display::Inline,
            margin: [Len::Px(0.0); 4],
            padding: [Len::Px(0.0); 4],
            border_width: [0.0; 4],
            border_style: [BorderStyle::None; 4],
            border_color: [Color::BLACK; 4],
            radius: 0.0,
            width: Len::Auto,
            height: Len::Auto,
            min_width: Len::Auto,
            max_width: Len::Auto,
            min_height: Len::Auto,
            max_height: Len::Auto,
            border_box: false,
            background: Color::TRANSPARENT,
            decoration: Decoration::default(),
            decoration_color: None,
            vertical_align: VAlign::Baseline,
            break_before: Break::Auto,
            break_after: Break::Auto,
            avoid_break_inside: false,
            content: None,
            bookmark_level: None,
            position: Position::Static,
            inset: [Len::Auto; 4],
            float: None,
            clear: 0,
            order: 0,
            formatting: None,
        }
    }

    /// A child's starting point: the inherited values of `self`, initial values for the rest.
    pub fn inherit(&self) -> Style {
        let initial = Style::initial(self.family);
        Style {
            color: self.color,
            family: self.family,
            font_size: self.font_size,
            weight: self.weight,
            italic: self.italic,
            line_height: self.line_height,
            text_align: self.text_align,
            white_space: self.white_space,
            text_indent: self.text_indent,
            letter_spacing: self.letter_spacing,
            word_spacing: self.word_spacing,
            transform: self.transform,
            list_type: self.list_type.clone(),
            list_inside: self.list_inside,
            border_collapse: self.border_collapse,
            border_spacing: self.border_spacing,
            visible: self.visible,
            orphans: self.orphans,
            widows: self.widows,
            break_words: self.break_words,
            rtl: self.rtl,
            border_color: [self.color; 4],
            ..initial
        }
    }

    /// Anonymous boxes inherit everything inheritable and take initial values otherwise.
    pub fn anonymous(&self, display: Display) -> Style {
        Style {
            display,
            ..self.inherit()
        }
    }

    pub fn used_line_height(&self, text_height: f32) -> f32 {
        match self.line_height {
            LineHeight::Normal => text_height,
            LineHeight::Number(factor) => factor * self.font_size,
            LineHeight::Px(px) => px,
        }
    }

    /// Horizontal margin + border + padding on one side (3 = left, 1 = right).
    pub fn frame(&self, side: usize, base: f32) -> f32 {
        self.margin[side].or_zero(base) + self.border_width[side] + self.padding[side].or_zero(base)
    }
}

/// The page box: size and margins, in px.
#[derive(Clone, Copy, Debug)]
pub struct PageStyle {
    pub width: f32,
    pub height: f32,
    pub margin: [f32; 4],
}

struct StyleRule {
    selector: Selector,
    declarations: Rc<Vec<Declaration>>,
    author: bool,
    order: usize,
}

pub struct Stylist {
    rules: Vec<StyleRule>,
    page: Vec<(bool, Declaration)>,
    pub root_font_size: f32,
    /// Whether any author rule targets `::before` or `::after`.
    pub author_pseudo: bool,
}

fn collect_rules(
    stylist: &mut Stylist,
    text: &str,
    author: bool,
    assets: &Assets,
    base: &Url,
    depth: usize,
) {
    for rule in css::parse_stylesheet(text) {
        match rule {
            Rule::Style {
                selectors,
                declarations,
            } => {
                let declarations = Rc::new(declarations);
                for selector in selectors {
                    let order = stylist.rules.len();
                    stylist.author_pseudo |= author
                        && matches!(
                            selector.pseudo,
                            Some(PseudoElement::Before | PseudoElement::After)
                        );
                    stylist.rules.push(StyleRule {
                        selector,
                        declarations: Rc::clone(&declarations),
                        author,
                        order,
                    });
                }
            }
            Rule::Page(declarations) => stylist
                .page
                .extend(declarations.into_iter().map(|d| (author, d))),
            Rule::Import(url) => {
                if depth < MAX_IMPORT_DEPTH
                    && let Some((text, url)) = read_sheet(assets, base, &url)
                {
                    collect_rules(stylist, &text, author, assets, &url, depth + 1);
                }
            }
        }
    }
}

/// A linked stylesheet's final URL is the base for its imports after redirects.
fn read_sheet(assets: &Assets, base: &Url, reference: &str) -> Option<(String, Url)> {
    let asset = assets.fetch(base, reference)?;
    Some((
        String::from_utf8_lossy(&asset.bytes).into_owned(),
        asset.url.clone(),
    ))
}

pub fn percent_decode(text: &str) -> String {
    let bytes = percent_decode_bytes(text);
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

pub fn percent_decode_bytes(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            out.push(value);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn media_applies(media: Option<&str>) -> bool {
    let Some(media) = media else {
        return true;
    };
    media.split(',').any(|query| {
        let query = query.trim().to_ascii_lowercase();
        let first = query
            .split_whitespace()
            .find(|word| *word != "only")
            .unwrap_or("");
        query.is_empty() || matches!(first, "all" | "print") || first.starts_with('(')
    })
}

impl Stylist {
    /// The UA sheet plus the document's style elements and linked stylesheets.
    pub fn new(dom: &Dom, assets: &Assets, base: &Url) -> Stylist {
        let mut stylist = Stylist {
            rules: Vec::new(),
            page: Vec::new(),
            root_font_size: 16.0,
            author_pseudo: false,
        };
        collect_rules(&mut stylist, UA_CSS, false, assets, base, 0);
        let mut stack = vec![super::dom::DOCUMENT];
        let mut order = Vec::new();
        while let Some(node) = stack.pop() {
            order.push(node);
            stack.extend(dom.children(node).iter().rev());
        }
        for node in order {
            match dom.tag(node) {
                "style" if media_applies(dom.attr(node, "media")) => {
                    let text = dom.text_content(node);
                    collect_rules(&mut stylist, &text, true, assets, base, 0);
                }
                "link" => {
                    let rel = dom.attr(node, "rel").unwrap_or("").to_ascii_lowercase();
                    if rel
                        .split_ascii_whitespace()
                        .any(|word| word == "stylesheet")
                        && !rel.split_ascii_whitespace().any(|word| word == "alternate")
                        && media_applies(dom.attr(node, "media"))
                        && let Some(href) = dom.attr(node, "href")
                        && let Some((text, url)) = read_sheet(assets, base, href)
                    {
                        collect_rules(&mut stylist, &text, true, assets, &url, 1);
                    }
                }
                _ => {}
            }
        }
        stylist
    }

    /// The declarations that apply to `node` (or its pseudo-element), lowest precedence first.
    fn cascade<'s>(
        &'s self,
        dom: &Dom,
        node: NodeId,
        pseudo: Option<PseudoElement>,
        inline: &'s [Declaration],
    ) -> Vec<&'s Declaration> {
        let mut matched: Vec<(u8, u32, usize, &Declaration)> = Vec::new();
        for rule in &self.rules {
            if rule.selector.pseudo != pseudo || !css::matches(dom, &rule.selector, node) {
                continue;
            }
            for declaration in rule.declarations.iter() {
                let level = match (declaration.important, rule.author) {
                    (false, false) => 0,
                    (false, true) => 1,
                    (true, true) => 3,
                    (true, false) => 5,
                };
                matched.push((level, rule.selector.specificity, rule.order, declaration));
            }
        }
        for (index, declaration) in inline.iter().enumerate() {
            let level = if declaration.important { 4 } else { 2 };
            matched.push((level, u32::MAX, index, declaration));
        }
        matched.sort_by_key(|&(level, specificity, order, _)| (level, specificity, order));
        matched
            .into_iter()
            .map(|(_, _, _, declaration)| declaration)
            .collect()
    }

    /// The root's parent style: initial values with the default serif family.
    pub fn initial(fonts: &mut Fonts) -> Style {
        Style::initial(fonts.intern(vec!["serif".to_owned()]))
    }

    /// The computed style of an element, or of its `::before`/`::after`.
    pub fn compute(
        &self,
        dom: &Dom,
        node: NodeId,
        pseudo: Option<PseudoElement>,
        parent: &Style,
        fonts: &mut Fonts,
    ) -> Style {
        let inline = match (pseudo, dom.attr(node, "style")) {
            (None, Some(text)) => css::parse_style_attribute(text),
            _ => Vec::new(),
        };
        let declarations = self.cascade(dom, node, pseudo, &inline);
        let mut specified: HashMap<&'static str, &[Cv]> = HashMap::new();
        let mut longhands = Vec::new();
        for declaration in declarations {
            longhands.clear();
            expand(&declaration.name, &declaration.value, &mut longhands);
            for &(name, value) in &longhands {
                specified.insert(name, value);
            }
        }
        let is_root = pseudo.is_none() && dom.parent(node) == Some(super::dom::DOCUMENT);
        let root_font_size = if is_root { 16.0 } else { self.root_font_size };
        let mut style = compute(&specified, parent, root_font_size, fonts);
        if let Some(items) = style.content.as_mut() {
            resolve_attrs(items, dom, node);
        }
        if is_root && style.display == Display::Inline {
            style.display = Display::Block;
        }
        style
    }

    pub fn set_root_font_size(&mut self, size: f32) {
        self.root_font_size = size;
    }

    /// The `@page` box, UA margins overridden by author declarations.
    pub fn page(&self) -> PageStyle {
        let mut size = (793.7008, 1122.5197);
        let mut margin = [Len::Px(75.0); 4];
        let mut ordered: Vec<&(bool, Declaration)> = self.page.iter().collect();
        ordered.sort_by_key(|(author, declaration)| (declaration.important, *author));
        let mut longhands = Vec::new();
        for (_, declaration) in ordered {
            if declaration.name == "size" {
                if let Some(parsed) = page_size(&declaration.value) {
                    size = parsed;
                }
                continue;
            }
            longhands.clear();
            expand(&declaration.name, &declaration.value, &mut longhands);
            for &(name, value) in &longhands {
                let side = match name {
                    "margin-top" => 0,
                    "margin-right" => 1,
                    "margin-bottom" => 2,
                    "margin-left" => 3,
                    _ => continue,
                };
                if let Some(len) = single(value).and_then(|cv| length(cv, 16.0, 16.0)) {
                    margin[side] = len;
                }
            }
        }
        let (width, height) = size;
        let resolve = |len: Len, base: f32| len.resolve(base).unwrap_or(0.0);
        PageStyle {
            width,
            height,
            margin: [
                resolve(margin[0], height),
                resolve(margin[1], width),
                resolve(margin[2], height),
                resolve(margin[3], width),
            ],
        }
    }
}

fn resolve_attrs(items: &mut [ContentItem], dom: &Dom, node: NodeId) {
    for item in items {
        if let ContentItem::Attr(name) = item {
            *item = ContentItem::Text(dom.attr(node, name).unwrap_or("").to_owned());
        }
    }
}

const MM: f32 = 96.0 / 25.4;

fn page_size(value: &[Cv]) -> Option<(f32, f32)> {
    let parts: Vec<&Cv> = value.iter().filter(|cv| !cv.is_whitespace()).collect();
    let mut size: Option<(f32, f32)> = None;
    let mut landscape = None;
    let mut lengths = Vec::new();
    for part in parts {
        if let Some(word) = part.ident() {
            let word = word.to_ascii_lowercase();
            let named = match word.as_str() {
                "a5" => Some((148.0 * MM, 210.0 * MM)),
                "a4" => Some((210.0 * MM, 297.0 * MM)),
                "a3" => Some((297.0 * MM, 420.0 * MM)),
                "b5" => Some((176.0 * MM, 250.0 * MM)),
                "b4" => Some((250.0 * MM, 353.0 * MM)),
                "jis-b5" => Some((182.0 * MM, 257.0 * MM)),
                "jis-b4" => Some((257.0 * MM, 364.0 * MM)),
                "letter" => Some((8.5 * 96.0, 11.0 * 96.0)),
                "legal" => Some((8.5 * 96.0, 14.0 * 96.0)),
                "ledger" => Some((11.0 * 96.0, 17.0 * 96.0)),
                "auto" => Some((210.0 * MM, 297.0 * MM)),
                "landscape" => {
                    landscape = Some(true);
                    None
                }
                "portrait" => {
                    landscape = Some(false);
                    None
                }
                _ => return None,
            };
            if named.is_some() {
                size = named;
            }
        } else if let Some(Len::Px(px)) = length(part, 16.0, 16.0) {
            lengths.push(px);
        } else {
            return None;
        }
    }
    let (mut width, mut height) = match (lengths.as_slice(), size) {
        ([], Some(size)) => size,
        ([], None) => (210.0 * MM, 297.0 * MM),
        ([side], None) => (*side, *side),
        ([width, height], None) => (*width, *height),
        _ => return None,
    };
    if let Some(landscape) = landscape
        && landscape != (width > height)
    {
        std::mem::swap(&mut width, &mut height);
    }
    (width > 0.0 && height > 0.0).then_some((width, height))
}

/// The value as one component, ignoring surrounding whitespace.
pub(super) fn single(value: &[Cv]) -> Option<&Cv> {
    let mut parts = value.iter().filter(|cv| !cv.is_whitespace());
    let first = parts.next()?;
    parts.next().is_none().then_some(first)
}

/// Whitespace-separated components of a value.
pub(super) fn components(value: &[Cv]) -> Vec<&[Cv]> {
    value
        .split(Cv::is_whitespace)
        .filter(|part| !part.is_empty())
        .collect()
}

pub(super) fn keyword(value: &[Cv]) -> Option<String> {
    single(value)?.ident().map(str::to_ascii_lowercase)
}

const SIDES: [&str; 4] = ["top", "right", "bottom", "left"];

fn side_name(prefix: &str, side: usize, suffix: &str) -> &'static str {
    const NAMES: [[&str; 4]; 5] = [
        ["margin-top", "margin-right", "margin-bottom", "margin-left"],
        [
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ],
        [
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
        ],
        [
            "border-top-style",
            "border-right-style",
            "border-bottom-style",
            "border-left-style",
        ],
        [
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
        ],
    ];
    let row = match (prefix, suffix) {
        ("margin", _) => 0,
        ("padding", _) => 1,
        (_, "width") => 2,
        (_, "style") => 3,
        _ => 4,
    };
    NAMES[row][side]
}

fn four<'a>(parts: &[&'a [Cv]]) -> Option<[&'a [Cv]; 4]> {
    Some(match *parts {
        [all] => [all; 4],
        [vertical, horizontal] => [vertical, horizontal, vertical, horizontal],
        [top, horizontal, bottom] => [top, horizontal, bottom, horizontal],
        [top, right, bottom, left] => [top, right, bottom, left],
        _ => return None,
    })
}

fn is_wide_keyword(value: &[Cv]) -> bool {
    keyword(value).is_some_and(|word| matches!(word.as_str(), "inherit" | "initial" | "unset"))
}

fn is_border_style(word: &str) -> bool {
    matches!(
        word,
        "none"
            | "hidden"
            | "dotted"
            | "dashed"
            | "solid"
            | "double"
            | "groove"
            | "ridge"
            | "inset"
            | "outset"
    )
}

fn is_border_width(part: &[Cv]) -> bool {
    match single(part) {
        Some(Cv::T(Token::Ident(word))) => matches!(
            word.to_ascii_lowercase().as_str(),
            "thin" | "medium" | "thick"
        ),
        Some(cv) => length(cv, 16.0, 16.0).is_some(),
        None => false,
    }
}

/// Splits a declaration into longhands. An empty value means the initial value.
fn expand<'a>(name: &str, value: &'a [Cv], out: &mut Vec<(&'static str, &'a [Cv])>) {
    const INITIAL: &[Cv] = &[];
    let wide = is_wide_keyword(value);
    if super::format::expand(name, value, wide, out) {
        return;
    }
    match name {
        "margin" | "padding" => {
            let parts = if wide { vec![value] } else { components(value) };
            if let Some(sides) = four(&parts) {
                for (side, part) in sides.into_iter().enumerate() {
                    out.push((side_name(name, side, ""), part));
                }
            }
        }
        "border-width" | "border-style" | "border-color" => {
            let suffix = &name[7..];
            let parts = if wide { vec![value] } else { components(value) };
            if let Some(sides) = four(&parts) {
                for (side, part) in sides.into_iter().enumerate() {
                    out.push((side_name("border", side, suffix), part));
                }
            }
        }
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let sides: Vec<usize> = match name {
                "border" => vec![0, 1, 2, 3],
                _ => vec![
                    SIDES
                        .iter()
                        .position(|side| name.ends_with(side))
                        .unwrap_or(0),
                ],
            };
            let (mut width, mut style, mut color) = (INITIAL, INITIAL, INITIAL);
            if wide {
                (width, style, color) = (value, value, value);
            } else {
                for part in components(value) {
                    if keyword(part).is_some_and(|word| is_border_style(&word)) {
                        style = part;
                    } else if is_border_width(part) {
                        width = part;
                    } else {
                        color = part;
                    }
                }
            }
            for side in sides {
                out.push((side_name("border", side, "width"), width));
                out.push((side_name("border", side, "style"), style));
                out.push((side_name("border", side, "color"), color));
            }
        }
        "font" => expand_font(value, out),
        "background" => {
            let color = if wide {
                value
            } else {
                components(value)
                    .into_iter()
                    .rev()
                    .find(|part| single(part).and_then(parse_color_cv).is_some())
                    .unwrap_or(INITIAL)
            };
            out.push(("background-color", color));
        }
        "list-style" => {
            let (mut kind, mut position) = (INITIAL, INITIAL);
            if wide {
                (kind, position) = (value, value);
            } else {
                for part in components(value) {
                    match keyword(part).as_deref() {
                        Some("inside" | "outside") => position = part,
                        Some(_) | None
                            if single(part).is_some_and(|cv| matches!(cv, Cv::Func(..))) => {}
                        _ => kind = part,
                    }
                }
            }
            out.push(("list-style-type", kind));
            out.push(("list-style-position", position));
        }
        "text-decoration" => {
            if wide {
                out.push(("text-decoration-line", value));
                out.push(("text-decoration-color", value));
                return;
            }
            let mut color = INITIAL;
            let mut lines_start = None;
            let mut lines_end = 0;
            for (index, cv) in value.iter().enumerate() {
                if cv.is_whitespace() {
                    continue;
                }
                match cv.ident().map(str::to_ascii_lowercase).as_deref() {
                    Some("underline" | "overline" | "line-through" | "blink" | "none") => {
                        lines_start.get_or_insert(index);
                        lines_end = index + 1;
                    }
                    Some("solid" | "double" | "dotted" | "dashed" | "wavy") => {}
                    _ => color = std::slice::from_ref(cv),
                }
            }
            let lines = lines_start.map_or(INITIAL, |start| &value[start..lines_end]);
            out.push(("text-decoration-line", lines));
            out.push(("text-decoration-color", color));
        }
        "page-break-before" => out.push(("break-before", value)),
        "page-break-after" => out.push(("break-after", value)),
        "page-break-inside" => out.push(("break-inside", value)),
        "word-wrap" => out.push(("overflow-wrap", value)),
        "border-radius" => out.push((
            "border-radius",
            components(value).first().copied().unwrap_or(INITIAL),
        )),
        _ => {
            if let Some(&known) = LONGHANDS
                .iter()
                .chain(super::format::LONGHANDS)
                .find(|known| **known == name)
            {
                out.push((known, value));
            }
        }
    }
}

const LONGHANDS: &[&str] = &[
    "display",
    "color",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "line-height",
    "text-align",
    "white-space",
    "text-indent",
    "letter-spacing",
    "word-spacing",
    "text-transform",
    "list-style-type",
    "list-style-position",
    "border-collapse",
    "border-spacing",
    "visibility",
    "orphans",
    "widows",
    "overflow-wrap",
    "word-break",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "border-top-width",
    "border-right-width",
    "border-bottom-width",
    "border-left-width",
    "border-top-style",
    "border-right-style",
    "border-bottom-style",
    "border-left-style",
    "border-top-color",
    "border-right-color",
    "border-bottom-color",
    "border-left-color",
    "width",
    "height",
    "min-width",
    "max-width",
    "min-height",
    "max-height",
    "box-sizing",
    "background-color",
    "text-decoration-line",
    "text-decoration-color",
    "vertical-align",
    "break-before",
    "break-after",
    "break-inside",
    "content",
    "bookmark-level",
    "direction",
    "position",
    "top",
    "right",
    "bottom",
    "left",
    "float",
    "clear",
    "order",
];

fn expand_font<'a>(value: &'a [Cv], out: &mut Vec<(&'static str, &'a [Cv])>) {
    const INITIAL: &[Cv] = &[];
    if is_wide_keyword(value) {
        for name in [
            "font-style",
            "font-weight",
            "font-size",
            "line-height",
            "font-family",
        ] {
            out.push((name, value));
        }
        return;
    }
    let (mut style, mut weight, mut line_height) = (INITIAL, INITIAL, INITIAL);
    let mut index = 0;
    let mut size = None;
    while index < value.len() {
        let cv = &value[index];
        if cv.is_whitespace() {
            index += 1;
            continue;
        }
        let part = std::slice::from_ref(cv);
        match cv.ident().map(str::to_ascii_lowercase).as_deref() {
            Some("italic" | "oblique") => style = part,
            Some("bold" | "bolder" | "lighter") => weight = part,
            Some(
                "normal" | "small-caps" | "condensed" | "expanded" | "semi-condensed"
                | "semi-expanded",
            ) => {}
            _ => {
                if let Cv::T(Token::Number(n)) = cv
                    && *n >= 1.0
                    && *n <= 1000.0
                {
                    weight = part;
                } else {
                    size = Some(index);
                    break;
                }
            }
        }
        index += 1;
    }
    let Some(size_index) = size else {
        return;
    };
    let mut rest = size_index + 1;
    // An optional `/ line-height` follows the size.
    let mut look = rest;
    while value.get(look).is_some_and(Cv::is_whitespace) {
        look += 1;
    }
    if matches!(value.get(look), Some(Cv::T(Token::Delim('/')))) {
        look += 1;
        while value.get(look).is_some_and(Cv::is_whitespace) {
            look += 1;
        }
        if let Some(cv) = value.get(look) {
            line_height = std::slice::from_ref(cv);
            rest = look + 1;
        }
    }
    let family = &value[rest.min(value.len())..];
    if family.iter().all(Cv::is_whitespace) {
        return;
    }
    out.push(("font-style", style));
    out.push(("font-weight", weight));
    out.push(("font-size", std::slice::from_ref(&value[size_index])));
    out.push(("line-height", line_height));
    out.push(("font-family", family));
}

pub(super) enum Spec<'a> {
    Value(&'a [Cv]),
    Inherit,
    Initial,
}

pub(super) fn spec<'a>(
    specified: &HashMap<&'static str, &'a [Cv]>,
    name: &str,
    inherited: bool,
) -> Option<Spec<'a>> {
    let value = *specified.get(name)?;
    if value.is_empty() {
        return Some(Spec::Initial);
    }
    Some(match keyword(value).as_deref() {
        Some("inherit") => Spec::Inherit,
        Some("initial") => Spec::Initial,
        Some("unset") if inherited => Spec::Inherit,
        Some("unset") => Spec::Initial,
        _ => Spec::Value(value),
    })
}

/// Applies one property: `inherit` copies the parent's value, `initial` the initial
/// one, and a value that does not parse leaves the slot as it is.
pub(super) fn apply<T: Clone>(
    slot: &mut T,
    spec: Option<Spec<'_>>,
    parent: &T,
    initial: T,
    parse: impl FnOnce(&[Cv]) -> Option<T>,
) {
    match spec {
        None => {}
        Some(Spec::Inherit) => *slot = parent.clone(),
        Some(Spec::Initial) => *slot = initial,
        Some(Spec::Value(value)) => {
            if let Some(parsed) = parse(value) {
                *slot = parsed;
            }
        }
    }
}

/// A length component in px (`em` against `em`, `rem` against `rem`), or a percentage.
pub(super) fn length(cv: &Cv, em: f32, rem: f32) -> Option<Len> {
    match cv {
        Cv::T(Token::Number(n)) if *n == 0.0 => Some(Len::Px(0.0)),
        Cv::T(Token::Percentage(p)) => Some(Len::Pct(*p)),
        Cv::T(Token::Dimension(value, unit)) => {
            let scale = match unit.to_ascii_lowercase().as_str() {
                "px" => 1.0,
                "pt" => 96.0 / 72.0,
                "pc" => 16.0,
                "in" => 96.0,
                "cm" => 96.0 / 2.54,
                "mm" => MM,
                "q" => MM / 4.0,
                "em" => em,
                "rem" => rem,
                "ex" | "ch" => em * 0.5,
                _ => return None,
            };
            Some(Len::Px(value * scale))
        }
        _ => None,
    }
}

pub(super) fn length_value(value: &[Cv], em: f32, rem: f32, auto: bool) -> Option<Len> {
    let cv = single(value)?;
    if auto
        && cv.ident().is_some_and(|word| {
            word.eq_ignore_ascii_case("auto") || word.eq_ignore_ascii_case("none")
        })
    {
        return Some(Len::Auto);
    }
    length(cv, em, rem)
}

fn px_value(value: &[Cv], em: f32, rem: f32) -> Option<f32> {
    match length_value(value, em, rem, false)? {
        Len::Px(px) => Some(px),
        _ => None,
    }
}

/// Font-size keywords, smallest first, in px.
const FONT_SIZES: [(&str, f32); 8] = [
    ("xx-small", 9.6),
    ("x-small", 12.0),
    ("small", 13.333_333),
    ("medium", 16.0),
    ("large", 19.2),
    ("x-large", 24.0),
    ("xx-large", 32.0),
    ("xxx-large", 48.0),
];

fn font_size(value: &[Cv], parent: f32, rem: f32) -> Option<f32> {
    let cv = single(value)?;
    if let Some(word) = cv.ident().map(str::to_ascii_lowercase) {
        if let Some((_, size)) = FONT_SIZES.iter().find(|(name, _)| *name == word) {
            return Some(*size);
        }
        return match word.as_str() {
            "larger" => Some(
                FONT_SIZES
                    .iter()
                    .map(|(_, size)| *size)
                    .find(|size| *size > parent)
                    .unwrap_or(parent * 1.2),
            ),
            "smaller" => Some(
                FONT_SIZES
                    .iter()
                    .rev()
                    .map(|(_, size)| *size)
                    .find(|size| *size < parent)
                    .unwrap_or(parent * 0.8),
            ),
            _ => None,
        };
    }
    match length(cv, parent, rem)? {
        Len::Px(px) if px >= 0.0 => Some(px),
        Len::Pct(pct) if pct >= 0.0 => Some(parent * pct / 100.0),
        _ => None,
    }
}

fn font_weight(value: &[Cv], parent: u16) -> Option<u16> {
    match single(value)? {
        Cv::T(Token::Number(n)) if (1.0..=1000.0).contains(n) => Some(*n as u16),
        Cv::T(Token::Ident(word)) => match word.to_ascii_lowercase().as_str() {
            "normal" => Some(400),
            "bold" => Some(700),
            "bolder" => Some(match parent {
                0..350 => 400,
                350..550 => 700,
                _ => 900,
            }),
            "lighter" => Some(match parent {
                0..550 => 100,
                550..750 => 400,
                _ => 700,
            }),
            _ => None,
        },
        _ => None,
    }
}

fn font_family(value: &[Cv]) -> Option<Vec<String>> {
    let mut families = Vec::new();
    for part in value.split(|cv| matches!(cv, Cv::T(Token::Comma))) {
        let mut name = String::new();
        for cv in part {
            match cv {
                Cv::T(Token::Str(text)) => name.push_str(text),
                Cv::T(Token::Ident(word)) => {
                    if !name.is_empty() {
                        name.push(' ');
                    }
                    name.push_str(word);
                }
                Cv::T(Token::Whitespace) => {}
                _ => return None,
            }
        }
        if name.is_empty() {
            return None;
        }
        families.push(name.to_lowercase());
    }
    (!families.is_empty()).then_some(families)
}

fn hex_color(hex: &str) -> Option<Color> {
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).and_then(|d| u8::try_from(d).ok()))
        .collect::<Option<_>>()?;
    let pair = |i: usize| digits[i] * 16 + digits[i + 1];
    let (r, g, b, a) = match digits.len() {
        3 => (digits[0] * 17, digits[1] * 17, digits[2] * 17, 255),
        4 => (
            digits[0] * 17,
            digits[1] * 17,
            digits[2] * 17,
            digits[3] * 17,
        ),
        6 => (pair(0), pair(2), pair(4), 255),
        8 => (pair(0), pair(2), pair(4), pair(6)),
        _ => return None,
    };
    Some(Color {
        a: f32::from(a) / 255.0,
        ..Color::rgb8(r, g, b)
    })
}

fn channel(cv: &Cv) -> Option<f32> {
    match cv {
        Cv::T(Token::Number(n)) => Some((n / 255.0).clamp(0.0, 1.0)),
        Cv::T(Token::Percentage(p)) => Some((p / 100.0).clamp(0.0, 1.0)),
        _ => None,
    }
}

fn alpha(cv: &Cv) -> Option<f32> {
    match cv {
        Cv::T(Token::Number(n)) => Some(n.clamp(0.0, 1.0)),
        Cv::T(Token::Percentage(p)) => Some((p / 100.0).clamp(0.0, 1.0)),
        _ => None,
    }
}

fn hue_to_rgb(m1: f32, m2: f32, h: f32) -> f32 {
    let h = h.rem_euclid(1.0);
    if h * 6.0 < 1.0 {
        m1 + (m2 - m1) * h * 6.0
    } else if h * 2.0 < 1.0 {
        m2
    } else if h * 3.0 < 2.0 {
        m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
    } else {
        m1
    }
}

fn parse_color_cv(cv: &Cv) -> Option<Color> {
    match cv {
        Cv::T(Token::Hash(hex)) => hex_color(hex),
        Cv::T(Token::Ident(name)) => {
            let name = name.to_ascii_lowercase();
            if name == "transparent" {
                return Some(Color::TRANSPARENT);
            }
            NAMED_COLORS
                .binary_search_by(|(known, _)| known.cmp(&name.as_str()))
                .ok()
                .map(|index| {
                    let rgb = NAMED_COLORS[index].1;
                    Color::rgb8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
                })
        }
        Cv::Func(name, args) => {
            let args: Vec<&Cv> = args
                .iter()
                .filter(|cv| {
                    !cv.is_whitespace() && !matches!(cv, Cv::T(Token::Comma | Token::Delim('/')))
                })
                .collect();
            match name.to_ascii_lowercase().as_str() {
                "rgb" | "rgba" => {
                    let (r, g, b) = (
                        channel(args.first()?)?,
                        channel(args.get(1)?)?,
                        channel(args.get(2)?)?,
                    );
                    let a = args.get(3).map_or(Some(1.0), |cv| alpha(cv))?;
                    Some(Color { r, g, b, a })
                }
                "hsl" | "hsla" => {
                    let h = match args.first()? {
                        Cv::T(Token::Number(n)) => n / 360.0,
                        Cv::T(Token::Dimension(n, unit)) if unit.eq_ignore_ascii_case("deg") => {
                            n / 360.0
                        }
                        _ => return None,
                    };
                    let s = match args.get(1)? {
                        Cv::T(Token::Percentage(p) | Token::Number(p)) => {
                            (p / 100.0).clamp(0.0, 1.0)
                        }
                        _ => return None,
                    };
                    let l = match args.get(2)? {
                        Cv::T(Token::Percentage(p) | Token::Number(p)) => {
                            (p / 100.0).clamp(0.0, 1.0)
                        }
                        _ => return None,
                    };
                    let a = args.get(3).map_or(Some(1.0), |cv| alpha(cv))?;
                    let m2 = if l <= 0.5 {
                        l * (s + 1.0)
                    } else {
                        l + s - l * s
                    };
                    let m1 = l * 2.0 - m2;
                    Some(Color {
                        r: hue_to_rgb(m1, m2, h + 1.0 / 3.0),
                        g: hue_to_rgb(m1, m2, h),
                        b: hue_to_rgb(m1, m2, h - 1.0 / 3.0),
                        a,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// A color value; `currentcolor` is `current`.
fn color_value(value: &[Cv], current: Color) -> Option<Color> {
    let cv = single(value)?;
    if cv
        .ident()
        .is_some_and(|word| word.eq_ignore_ascii_case("currentcolor"))
    {
        return Some(current);
    }
    parse_color_cv(cv)
}

fn display(value: &[Cv]) -> Option<Display> {
    let words: Vec<String> = components(value)
        .iter()
        .filter_map(|part| keyword(part))
        .collect();
    let joined = words.join(" ");
    Some(match joined.as_str() {
        "none" => Display::None,
        "block" | "flow-root" | "block flow" | "block flow-root" | "run-in" => Display::Block,
        "flex" | "inline-flex" | "block flex" | "inline flex" => Display::Flex,
        "grid" | "inline-grid" | "block grid" | "inline grid" => Display::Grid,
        "inline" | "contents" | "inline flow" => Display::Inline,
        "inline-block" | "inline-table" | "inline flow-root" => Display::InlineBlock,
        "list-item" | "block list-item" | "list-item block" => Display::ListItem,
        "table" | "block table" => Display::Table,
        "table-row-group" => Display::RowGroup,
        "table-header-group" => Display::HeaderGroup,
        "table-footer-group" => Display::FooterGroup,
        "table-row" => Display::Row,
        "table-cell" => Display::Cell,
        "table-caption" => Display::Caption,
        "table-column" => Display::Column,
        "table-column-group" => Display::ColumnGroup,
        _ => return None,
    })
}

fn border_width(value: &[Cv], em: f32, rem: f32) -> Option<f32> {
    match keyword(value).as_deref() {
        Some("thin") => Some(1.0),
        Some("medium") => Some(3.0),
        Some("thick") => Some(5.0),
        _ => px_value(value, em, rem).filter(|px| *px >= 0.0),
    }
}

fn border_style(value: &[Cv]) -> Option<BorderStyle> {
    Some(match keyword(value)?.as_str() {
        "none" | "hidden" => BorderStyle::None,
        "dotted" => BorderStyle::Dotted,
        "dashed" => BorderStyle::Dashed,
        "double" => BorderStyle::Double,
        "solid" | "groove" | "ridge" | "inset" | "outset" => BorderStyle::Solid,
        _ => return None,
    })
}

fn list_type(value: &[Cv]) -> Option<ListType> {
    if let Some(Cv::T(Token::Str(text))) = single(value) {
        return Some(ListType::Str(Rc::from(text.as_str())));
    }
    Some(match keyword(value)?.as_str() {
        "none" => ListType::None,
        "disc" => ListType::Disc,
        "circle" => ListType::Circle,
        "square" => ListType::Square,
        "decimal" => ListType::Decimal,
        "decimal-leading-zero" => ListType::DecimalLeadingZero,
        "lower-alpha" | "lower-latin" => ListType::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListType::UpperAlpha,
        "lower-roman" => ListType::LowerRoman,
        "upper-roman" => ListType::UpperRoman,
        "lower-greek" => ListType::LowerGreek,
        _ => ListType::Decimal,
    })
}

fn break_value(value: &[Cv]) -> Option<Break> {
    Some(match keyword(value)?.as_str() {
        "auto" | "column" | "avoid-column" => Break::Auto,
        "avoid" | "avoid-page" => Break::Avoid,
        "page" | "always" | "left" | "right" | "recto" | "verso" => Break::Page,
        _ => return None,
    })
}

fn content(value: &[Cv]) -> Option<Option<Vec<ContentItem>>> {
    if keyword(value).is_some_and(|word| word == "none" || word == "normal") {
        return Some(None);
    }
    let mut items = Vec::new();
    for cv in value.iter().filter(|cv| !cv.is_whitespace()) {
        match cv {
            Cv::T(Token::Str(text)) => items.push(ContentItem::Text(text.clone())),
            Cv::T(Token::Ident(word)) => match word.to_ascii_lowercase().as_str() {
                "open-quote" => items.push(ContentItem::OpenQuote),
                "close-quote" => items.push(ContentItem::CloseQuote),
                "no-open-quote" | "no-close-quote" => {}
                _ => return None,
            },
            Cv::Func(name, args) if name.eq_ignore_ascii_case("attr") => {
                let attr = args.iter().find_map(Cv::ident)?;
                items.push(ContentItem::Attr(attr.to_ascii_lowercase()));
            }
            Cv::Func(..) => {}
            _ => return None,
        }
    }
    Some(Some(items))
}

fn compute(
    specified: &HashMap<&'static str, &[Cv]>,
    parent: &Style,
    rem: f32,
    fonts: &mut Fonts,
) -> Style {
    let mut s = parent.inherit();
    let get = |name: &str, inherited: bool| spec(specified, name, inherited);

    apply(
        &mut s.font_size,
        get("font-size", true),
        &parent.font_size,
        16.0,
        |v| font_size(v, parent.font_size, rem),
    );
    let em = s.font_size;
    apply(
        &mut s.weight,
        get("font-weight", true),
        &parent.weight,
        400,
        |v| font_weight(v, parent.weight),
    );
    apply(
        &mut s.italic,
        get("font-style", true),
        &parent.italic,
        false,
        |v| match keyword(v)?.as_str() {
            "italic" | "oblique" => Some(true),
            "normal" => Some(false),
            _ => None,
        },
    );
    let family = get("font-family", true);
    if let Some(Spec::Value(value)) = &family {
        if let Some(families) = font_family(value) {
            s.family = fonts.intern(families);
        }
    } else if let Some(Spec::Initial) = family {
        s.family = fonts.intern(vec!["serif".to_owned()]);
    }
    apply(
        &mut s.color,
        get("color", true),
        &parent.color,
        Color::BLACK,
        |v| color_value(v, parent.color),
    );
    apply(
        &mut s.line_height,
        get("line-height", true),
        &parent.line_height,
        LineHeight::Normal,
        |v| {
            let cv = single(v)?;
            match cv {
                Cv::T(Token::Ident(word)) if word.eq_ignore_ascii_case("normal") => {
                    Some(LineHeight::Normal)
                }
                Cv::T(Token::Number(n)) if *n >= 0.0 => Some(LineHeight::Number(*n)),
                _ => match length(cv, em, rem)? {
                    Len::Px(px) => Some(LineHeight::Px(px)),
                    Len::Pct(pct) => Some(LineHeight::Px(em * pct / 100.0)),
                    Len::Auto => None,
                },
            }
        },
    );
    apply(
        &mut s.text_align,
        get("text-align", true),
        &parent.text_align,
        TextAlign::Start,
        |v| {
            Some(match keyword(v)?.as_str() {
                "left" | "-webkit-left" => TextAlign::Left,
                "right" | "-webkit-right" => TextAlign::Right,
                "start" => TextAlign::Start,
                "end" => TextAlign::End,
                "center" | "-webkit-center" => TextAlign::Center,
                "justify" => TextAlign::Justify,
                _ => return None,
            })
        },
    );
    apply(
        &mut s.white_space,
        get("white-space", true),
        &parent.white_space,
        WhiteSpace::Normal,
        |v| {
            Some(match keyword(v)?.as_str() {
                "normal" => WhiteSpace::Normal,
                "pre" => WhiteSpace::Pre,
                "nowrap" => WhiteSpace::Nowrap,
                "pre-wrap" | "break-spaces" => WhiteSpace::PreWrap,
                "pre-line" => WhiteSpace::PreLine,
                _ => return None,
            })
        },
    );
    apply(
        &mut s.text_indent,
        get("text-indent", true),
        &parent.text_indent,
        Len::Px(0.0),
        |v| length_value(v, em, rem, false),
    );
    let spacing = |v: &[Cv]| {
        if keyword(v).as_deref() == Some("normal") {
            Some(0.0)
        } else {
            px_value(v, em, rem)
        }
    };
    apply(
        &mut s.letter_spacing,
        get("letter-spacing", true),
        &parent.letter_spacing,
        0.0,
        spacing,
    );
    apply(
        &mut s.word_spacing,
        get("word-spacing", true),
        &parent.word_spacing,
        0.0,
        spacing,
    );
    apply(
        &mut s.transform,
        get("text-transform", true),
        &parent.transform,
        Transform::None,
        |v| {
            Some(match keyword(v)?.as_str() {
                "none" => Transform::None,
                "uppercase" => Transform::Upper,
                "lowercase" => Transform::Lower,
                "capitalize" => Transform::Capitalize,
                _ => return None,
            })
        },
    );
    apply(
        &mut s.list_type,
        get("list-style-type", true),
        &parent.list_type,
        ListType::Disc,
        list_type,
    );
    apply(
        &mut s.list_inside,
        get("list-style-position", true),
        &parent.list_inside,
        false,
        |v| match keyword(v)?.as_str() {
            "inside" => Some(true),
            "outside" => Some(false),
            _ => None,
        },
    );
    apply(
        &mut s.border_collapse,
        get("border-collapse", true),
        &parent.border_collapse,
        false,
        |v| match keyword(v)?.as_str() {
            "collapse" => Some(true),
            "separate" => Some(false),
            _ => None,
        },
    );
    apply(
        &mut s.border_spacing,
        get("border-spacing", true),
        &parent.border_spacing,
        (0.0, 0.0),
        |v| {
            let parts: Vec<f32> = components(v)
                .iter()
                .map(|part| px_value(part, em, rem))
                .collect::<Option<_>>()?;
            match parts[..] {
                [both] => Some((both, both)),
                [horizontal, vertical] => Some((horizontal, vertical)),
                _ => None,
            }
        },
    );
    apply(
        &mut s.visible,
        get("visibility", true),
        &parent.visible,
        true,
        |v| match keyword(v)?.as_str() {
            "visible" => Some(true),
            "hidden" | "collapse" => Some(false),
            _ => None,
        },
    );
    let integer = |v: &[Cv]| match single(v)? {
        Cv::T(Token::Number(n)) if *n >= 1.0 => Some(*n as u32),
        _ => None,
    };
    apply(
        &mut s.orphans,
        get("orphans", true),
        &parent.orphans,
        2,
        integer,
    );
    apply(
        &mut s.widows,
        get("widows", true),
        &parent.widows,
        2,
        integer,
    );
    apply(
        &mut s.break_words,
        get("overflow-wrap", true),
        &parent.break_words,
        false,
        |v| match keyword(v)?.as_str() {
            "break-word" | "anywhere" => Some(true),
            "normal" => Some(false),
            _ => None,
        },
    );
    if let Some(Spec::Value(v)) = get("word-break", true)
        && keyword(v).as_deref() == Some("break-all")
    {
        s.break_words = true;
    }

    apply(
        &mut s.display,
        get("display", false),
        &parent.display,
        Display::Inline,
        display,
    );
    for side in 0..4 {
        let margin = get(side_name("margin", side, ""), false);
        apply(
            &mut s.margin[side],
            margin,
            &parent.margin[side],
            Len::Px(0.0),
            |v| length_value(v, em, rem, true),
        );
        let padding = get(side_name("padding", side, ""), false);
        apply(
            &mut s.padding[side],
            padding,
            &parent.padding[side],
            Len::Px(0.0),
            |v| {
                length_value(v, em, rem, false)
                    .filter(|len| !matches!(len, Len::Px(px) | Len::Pct(px) if *px < 0.0))
            },
        );
        let style = get(side_name("border", side, "style"), false);
        apply(
            &mut s.border_style[side],
            style,
            &parent.border_style[side],
            BorderStyle::None,
            border_style,
        );
        let width = get(side_name("border", side, "width"), false);
        let mut computed_width = 3.0;
        apply(
            &mut computed_width,
            width,
            &parent.border_width[side],
            3.0,
            |v| border_width(v, em, rem),
        );
        s.border_width[side] = if s.border_style[side] == BorderStyle::None {
            0.0
        } else {
            computed_width
        };
        let color = get(side_name("border", side, "color"), false);
        let current = s.color;
        apply(
            &mut s.border_color[side],
            color,
            &parent.border_color[side],
            current,
            |v| color_value(v, current),
        );
    }
    apply(
        &mut s.radius,
        get("border-radius", false),
        &parent.radius,
        0.0,
        |v| px_value(v, em, rem),
    );
    for (name, slot, parent_value, initial_value) in [
        ("width", &mut s.width, parent.width, Len::Auto),
        ("height", &mut s.height, parent.height, Len::Auto),
        ("min-width", &mut s.min_width, parent.min_width, Len::Auto),
        ("max-width", &mut s.max_width, parent.max_width, Len::Auto),
        (
            "min-height",
            &mut s.min_height,
            parent.min_height,
            Len::Auto,
        ),
        (
            "max-height",
            &mut s.max_height,
            parent.max_height,
            Len::Auto,
        ),
    ] {
        apply(slot, get(name, false), &parent_value, initial_value, |v| {
            length_value(v, em, rem, true)
        });
    }
    if matches!(s.min_width, Len::Auto) {
        s.min_width = Len::Px(0.0);
    }
    if matches!(s.min_height, Len::Auto) {
        s.min_height = Len::Px(0.0);
    }
    apply(
        &mut s.border_box,
        get("box-sizing", false),
        &parent.border_box,
        false,
        |v| match keyword(v)?.as_str() {
            "border-box" => Some(true),
            "content-box" => Some(false),
            _ => None,
        },
    );
    let current = s.color;
    apply(
        &mut s.background,
        get("background-color", false),
        &parent.background,
        Color::TRANSPARENT,
        |v| color_value(v, current),
    );
    apply(
        &mut s.decoration,
        get("text-decoration-line", false),
        &parent.decoration,
        Decoration::default(),
        |v| {
            let mut decoration = Decoration::default();
            for part in components(v) {
                match keyword(part)?.as_str() {
                    "underline" => decoration.underline = true,
                    "overline" => decoration.overline = true,
                    "line-through" => decoration.line_through = true,
                    "none" | "blink" => {}
                    _ => return None,
                }
            }
            Some(decoration)
        },
    );
    apply(
        &mut s.decoration_color,
        get("text-decoration-color", false),
        &parent.decoration_color,
        None,
        |v| {
            if keyword(v).as_deref() == Some("currentcolor") {
                Some(None)
            } else {
                color_value(v, current).map(Some)
            }
        },
    );
    let vertical_line_height = s.used_line_height(em);
    apply(
        &mut s.vertical_align,
        get("vertical-align", false),
        &parent.vertical_align,
        VAlign::Baseline,
        |v| {
            let cv = single(v)?;
            if let Some(word) = cv.ident() {
                return Some(match word.to_ascii_lowercase().as_str() {
                    "baseline" => VAlign::Baseline,
                    "sub" => VAlign::Sub,
                    "super" => VAlign::Super,
                    "text-top" => VAlign::TextTop,
                    "text-bottom" => VAlign::TextBottom,
                    "middle" => VAlign::Middle,
                    "top" => VAlign::Top,
                    "bottom" => VAlign::Bottom,
                    _ => return None,
                });
            }
            match length(cv, em, rem)? {
                Len::Px(px) => Some(VAlign::Px(px)),
                Len::Pct(pct) => Some(VAlign::Px(vertical_line_height * pct / 100.0)),
                Len::Auto => None,
            }
        },
    );
    apply(
        &mut s.break_before,
        get("break-before", false),
        &parent.break_before,
        Break::Auto,
        break_value,
    );
    apply(
        &mut s.break_after,
        get("break-after", false),
        &parent.break_after,
        Break::Auto,
        break_value,
    );
    apply(
        &mut s.avoid_break_inside,
        get("break-inside", false),
        &parent.avoid_break_inside,
        false,
        |v| Some(matches!(keyword(v)?.as_str(), "avoid" | "avoid-page")),
    );
    apply(
        &mut s.content,
        get("content", false),
        &parent.content,
        None,
        content,
    );
    apply(
        &mut s.bookmark_level,
        get("bookmark-level", false),
        &parent.bookmark_level,
        None,
        |v| match single(v)? {
            Cv::T(Token::Number(n)) if *n >= 1.0 => Some(Some((*n as u32).min(255) as u8)),
            Cv::T(Token::Ident(word)) if word.eq_ignore_ascii_case("none") => Some(None),
            _ => None,
        },
    );
    apply(
        &mut s.rtl,
        get("direction", true),
        &parent.rtl,
        false,
        |v| match keyword(v)?.as_str() {
            "rtl" => Some(true),
            "ltr" => Some(false),
            _ => None,
        },
    );
    apply(
        &mut s.position,
        get("position", false),
        &parent.position,
        Position::Static,
        |v| {
            Some(match keyword(v)?.as_str() {
                "static" => Position::Static,
                "relative" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                _ => return None,
            })
        },
    );
    for (index, name) in SIDES.iter().enumerate() {
        apply(
            &mut s.inset[index],
            get(name, false),
            &parent.inset[index],
            Len::Auto,
            |v| length_value(v, em, rem, true),
        );
    }
    apply(
        &mut s.float,
        get("float", false),
        &parent.float,
        None,
        |v| {
            Some(match keyword(v)?.as_str() {
                "left" => Some(false),
                "right" => Some(true),
                "none" => None,
                _ => return None,
            })
        },
    );
    apply(&mut s.clear, get("clear", false), &parent.clear, 0, |v| {
        Some(match keyword(v)?.as_str() {
            "left" => 1,
            "right" => 2,
            "both" => 3,
            "none" => 0,
            _ => return None,
        })
    });
    apply(
        &mut s.order,
        get("order", false),
        &parent.order,
        0,
        |v| match single(v)? {
            Cv::T(Token::Number(n)) if n.fract() == 0.0 => Some(*n as i32),
            _ => None,
        },
    );
    s.formatting = super::format::compute(specified, parent, em, rem);
    if (s.float.is_some() || matches!(s.position, Position::Absolute | Position::Fixed))
        && matches!(s.display, Display::Inline | Display::InlineBlock)
    {
        s.display = Display::Block;
    }
    s
}

/// CSS named colors, sorted by name, as 0xRRGGBB.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xF0F8FF),
    ("antiquewhite", 0xFAEBD7),
    ("aqua", 0x00FFFF),
    ("aquamarine", 0x7FFFD4),
    ("azure", 0xF0FFFF),
    ("beige", 0xF5F5DC),
    ("bisque", 0xFFE4C4),
    ("black", 0x000000),
    ("blanchedalmond", 0xFFEBCD),
    ("blue", 0x0000FF),
    ("blueviolet", 0x8A2BE2),
    ("brown", 0xA52A2A),
    ("burlywood", 0xDEB887),
    ("cadetblue", 0x5F9EA0),
    ("chartreuse", 0x7FFF00),
    ("chocolate", 0xD2691E),
    ("coral", 0xFF7F50),
    ("cornflowerblue", 0x6495ED),
    ("cornsilk", 0xFFF8DC),
    ("crimson", 0xDC143C),
    ("cyan", 0x00FFFF),
    ("darkblue", 0x00008B),
    ("darkcyan", 0x008B8B),
    ("darkgoldenrod", 0xB8860B),
    ("darkgray", 0xA9A9A9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xA9A9A9),
    ("darkkhaki", 0xBDB76B),
    ("darkmagenta", 0x8B008B),
    ("darkolivegreen", 0x556B2F),
    ("darkorange", 0xFF8C00),
    ("darkorchid", 0x9932CC),
    ("darkred", 0x8B0000),
    ("darksalmon", 0xE9967A),
    ("darkseagreen", 0x8FBC8F),
    ("darkslateblue", 0x483D8B),
    ("darkslategray", 0x2F4F4F),
    ("darkslategrey", 0x2F4F4F),
    ("darkturquoise", 0x00CED1),
    ("darkviolet", 0x9400D3),
    ("deeppink", 0xFF1493),
    ("deepskyblue", 0x00BFFF),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1E90FF),
    ("firebrick", 0xB22222),
    ("floralwhite", 0xFFFAF0),
    ("forestgreen", 0x228B22),
    ("fuchsia", 0xFF00FF),
    ("gainsboro", 0xDCDCDC),
    ("ghostwhite", 0xF8F8FF),
    ("gold", 0xFFD700),
    ("goldenrod", 0xDAA520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xADFF2F),
    ("grey", 0x808080),
    ("honeydew", 0xF0FFF0),
    ("hotpink", 0xFF69B4),
    ("indianred", 0xCD5C5C),
    ("indigo", 0x4B0082),
    ("ivory", 0xFFFFF0),
    ("khaki", 0xF0E68C),
    ("lavender", 0xE6E6FA),
    ("lavenderblush", 0xFFF0F5),
    ("lawngreen", 0x7CFC00),
    ("lemonchiffon", 0xFFFACD),
    ("lightblue", 0xADD8E6),
    ("lightcoral", 0xF08080),
    ("lightcyan", 0xE0FFFF),
    ("lightgoldenrodyellow", 0xFAFAD2),
    ("lightgray", 0xD3D3D3),
    ("lightgreen", 0x90EE90),
    ("lightgrey", 0xD3D3D3),
    ("lightpink", 0xFFB6C1),
    ("lightsalmon", 0xFFA07A),
    ("lightseagreen", 0x20B2AA),
    ("lightskyblue", 0x87CEFA),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xB0C4DE),
    ("lightyellow", 0xFFFFE0),
    ("lime", 0x00FF00),
    ("limegreen", 0x32CD32),
    ("linen", 0xFAF0E6),
    ("magenta", 0xFF00FF),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66CDAA),
    ("mediumblue", 0x0000CD),
    ("mediumorchid", 0xBA55D3),
    ("mediumpurple", 0x9370DB),
    ("mediumseagreen", 0x3CB371),
    ("mediumslateblue", 0x7B68EE),
    ("mediumspringgreen", 0x00FA9A),
    ("mediumturquoise", 0x48D1CC),
    ("mediumvioletred", 0xC71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xF5FFFA),
    ("mistyrose", 0xFFE4E1),
    ("moccasin", 0xFFE4B5),
    ("navajowhite", 0xFFDEAD),
    ("navy", 0x000080),
    ("oldlace", 0xFDF5E6),
    ("olive", 0x808000),
    ("olivedrab", 0x6B8E23),
    ("orange", 0xFFA500),
    ("orangered", 0xFF4500),
    ("orchid", 0xDA70D6),
    ("palegoldenrod", 0xEEE8AA),
    ("palegreen", 0x98FB98),
    ("paleturquoise", 0xAFEEEE),
    ("palevioletred", 0xDB7093),
    ("papayawhip", 0xFFEFD5),
    ("peachpuff", 0xFFDAB9),
    ("peru", 0xCD853F),
    ("pink", 0xFFC0CB),
    ("plum", 0xDDA0DD),
    ("powderblue", 0xB0E0E6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xFF0000),
    ("rosybrown", 0xBC8F8F),
    ("royalblue", 0x4169E1),
    ("saddlebrown", 0x8B4513),
    ("salmon", 0xFA8072),
    ("sandybrown", 0xF4A460),
    ("seagreen", 0x2E8B57),
    ("seashell", 0xFFF5EE),
    ("sienna", 0xA0522D),
    ("silver", 0xC0C0C0),
    ("skyblue", 0x87CEEB),
    ("slateblue", 0x6A5ACD),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xFFFAFA),
    ("springgreen", 0x00FF7F),
    ("steelblue", 0x4682B4),
    ("tan", 0xD2B48C),
    ("teal", 0x008080),
    ("thistle", 0xD8BFD8),
    ("tomato", 0xFF6347),
    ("turquoise", 0x40E0D0),
    ("violet", 0xEE82EE),
    ("wheat", 0xF5DEB3),
    ("white", 0xFFFFFF),
    ("whitesmoke", 0xF5F5F5),
    ("yellow", 0xFFFF00),
    ("yellowgreen", 0x9ACD32),
];
