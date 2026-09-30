//! Positioned PDF text, Python-compatible text operations, and the CLI's text verbs.
//!
//! [`extract_page`] returns blocks, lines, characters, fonts, and content provenance.
//! Page indices are zero-based. Text coordinates are points in the unrotated crop
//! frame: origin at the upper left, y increasing downward. Reading text does not
//! alter the document. Annotation appearances participate in extraction.
//!
//! Use [`TextFlags::TEXT`] for plain text and words, [`TextFlags::HTML`] to retain
//! images, and [`TextFlags::SEARCH_FOR`] for dehyphenated page-search quads.
//! [`TextFlags::SEARCH`] is the separate PyMuPDF `TEXTFLAGS_SEARCH` preset.
//! [`TextPage::text_sorted`] preserves physical line gaps; [`layout::page_layout`]
//! adds columns and a reading-order view. [`Line::spans`] borrows font metadata
//! from the same text page.
//!
//! [`page_words`], [`hit_pattern`], [`page_hits`], and [`mask_text`] are shared with
//! other verb crates. Regex compilation and execution are fallible; callers must
//! propagate errors rather than treating an invalid mask as an empty match.
//!
//! [`register`] adds text, count, literal/meaning search, bounded text blocks,
//! page-mapped text comparison, local-model setup/status, HTML/audio conversion,
//! and transcript reading/resolution. Meaning search never installs a model
//! implicitly. Audio conversion uses the platform `say` command.

mod compare;
mod convert;
mod device;
mod difflib;
mod geom;
mod hits;
mod html;
pub mod layout;
mod metatext;
mod output;
pub mod pyre;
mod search;
mod search_verb;
pub mod transcript;
mod transcript_io;
mod verbs;

use std::fmt;

use goat_common::GoatError;
use goat_common::textcache::WordColumns;
use pdf_core::{Document, Page};
use pdf_interp::{InterpError, RunOptions};

pub use hits::{HitPattern, hit_pattern, mask_text, page_hits};
pub use output::{BlockEntry, Span, Word};
pub use pdf_core::{Matrix, Point, Rect};
pub use verbs::register;

/// The content operation a character or image came from.
pub type GlyphSource = pdf_interp::Provenance;

/// MuPDF's `FZ_STEXT_*` option bits (PyMuPDF's `TEXT_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextFlags(pub u32);

impl TextFlags {
    pub const PRESERVE_LIGATURES: u32 = 1;
    pub const PRESERVE_WHITESPACE: u32 = 2;
    pub const PRESERVE_IMAGES: u32 = 4;
    pub const INHIBIT_SPACES: u32 = 8;
    pub const DEHYPHENATE: u32 = 16;
    pub const PRESERVE_SPANS: u32 = 32;
    pub const MEDIABOX_CLIP: u32 = 64;
    pub const CID_FOR_UNKNOWN_UNICODE: u32 = 128;

    /// PyMuPDF `TEXTFLAGS_TEXT`.
    pub const TEXT: TextFlags = TextFlags(195);
    /// PyMuPDF `TEXTFLAGS_WORDS`.
    pub const WORDS: TextFlags = TextFlags(195);
    /// PyMuPDF `TEXTFLAGS_BLOCKS`.
    pub const BLOCKS: TextFlags = TextFlags(195);
    /// PyMuPDF `TEXTFLAGS_DICT`.
    pub const DICT: TextFlags = TextFlags(199);
    /// PyMuPDF `TEXTFLAGS_HTML`.
    pub const HTML: TextFlags = TextFlags(199);
    /// PyMuPDF `TEXTFLAGS_SEARCH`.
    pub const SEARCH: TextFlags = TextFlags(210);
    /// The flags PyMuPDF's `Page.search_for` builds its text page with:
    /// dehyphenate, preserve whitespace, preserve ligatures, mediabox clip.
    pub const SEARCH_FOR: TextFlags = TextFlags(83);

    pub fn contains(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    pub fn without(self, bit: u32) -> TextFlags {
        TextFlags(self.0 & !bit)
    }
}

/// A character's four corners in page space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quad {
    pub ul: Point,
    pub ur: Point,
    pub ll: Point,
    pub lr: Point,
}

impl Quad {
    /// The smallest rectangle holding all four corners (`fz_rect_from_quad`).
    pub fn rect(&self) -> Rect {
        geom::Q::from_quad(self).rect().rect()
    }
}

/// MuPDF's `fz_stext_page`: the page's text in blocks, lines and
/// characters, in the order the content drew them.
#[derive(Clone, Debug)]
pub struct TextPage {
    /// The extraction area in unrotated crop coordinates; HTML can request a
    /// different crop extent.
    pub rect: Rect,
    pub blocks: Vec<Block>,
    /// The fonts [`Char::font`] indexes.
    pub fonts: Vec<FontInfo>,
}

#[derive(Clone, Debug)]
pub enum Block {
    Text(TextBlock),
    Image(ImageBlock),
}

#[derive(Clone, Debug)]
pub struct TextBlock {
    pub bbox: Rect,
    pub lines: Vec<Line>,
}

/// An image the page draws, kept with `PRESERVE_IMAGES`.
#[derive(Clone, Debug)]
pub struct ImageBlock {
    pub bbox: Rect,
    /// Normalized image coordinates (top-left origin, y down) → page space.
    pub transform: Matrix,
    pub width: u32,
    pub height: u32,
    pub bpc: u8,
    pub colorspace: String,
    pub source: GlyphSource,
    /// Encoded display image for the positioned HTML serializer.
    pub data_uri: String,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub bbox: Rect,
    /// 0 horizontal, 1 vertical.
    pub wmode: u8,
    /// Unit vector along the baseline.
    pub dir: Point,
    /// `FZ_STEXT_LINE_FLAGS_JOINED`: the line ends in a hyphen that
    /// dehyphenation joins to the next line.
    pub joined: bool,
    pub chars: Vec<Char>,
}

#[derive(Clone, Debug)]
pub struct Char {
    pub c: char,
    /// The glyph origin on the baseline.
    pub origin: Point,
    /// PyMuPDF's `JM_char_quad`: MuPDF's quad with the font's ascender and
    /// descender normalised as PyMuPDF reports it.
    pub quad: Quad,
    pub size: f64,
    /// Index into [`TextPage::fonts`].
    pub font: usize,
    /// 0xRRGGBB.
    pub color: u32,
    /// 0..=255.
    pub alpha: u8,
    /// MuPDF's `FZ_STEXT_*` character flags ([`Char::FILLED`] ...).
    pub flags: u32,
    /// 0 left-to-right, 1 right-to-left, 3 reordered from visual order.
    pub bidi: u8,
    pub source: GlyphSource,
}

impl Char {
    pub const STRIKEOUT: u32 = 1;
    pub const UNDERLINE: u32 = 2;
    pub const SYNTHETIC: u32 = 4;
    pub const BOLD: u32 = 8;
    pub const FILLED: u32 = 16;
    pub const STROKED: u32 = 32;
    pub const CLIPPED: u32 = 64;
    pub const UNICODE_IS_CID: u32 = 128;
    pub const UNICODE_IS_GID: u32 = 256;
    pub const SYNTHETIC_LARGE: u32 = 512;
}

/// A font the page's characters use.
#[derive(Clone, Debug, PartialEq)]
pub struct FontInfo {
    /// PyMuPDF's `JM_font_name`: a six-letter subset tag stripped.
    pub name: String,
    /// MuPDF's `fz_font_name`, subset tag kept.
    pub full_name: String,
    /// In em.
    pub ascender: f64,
    /// In em, negative below the baseline.
    pub descender: f64,
    pub bbox: Rect,
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub mono: bool,
}

/// Why a page's text could not be read.
#[derive(Debug)]
pub enum TextError {
    Pdf(pdf_core::Error),
    /// The interpreter stopped at a work bound.
    Interp(String),
    /// The document needs a password.
    Locked,
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextError::Pdf(error) => write!(f, "{error}"),
            TextError::Interp(message) => write!(f, "{message}"),
            TextError::Locked => write!(f, "document closed or encrypted"),
        }
    }
}

impl std::error::Error for TextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TextError::Pdf(error) => Some(error),
            TextError::Interp(_) | TextError::Locked => None,
        }
    }
}

impl From<pdf_core::Error> for TextError {
    fn from(error: pdf_core::Error) -> Self {
        TextError::Pdf(error)
    }
}

impl From<InterpError> for TextError {
    fn from(error: InterpError) -> TextError {
        match error {
            InterpError::Pdf(error) => TextError::Pdf(error),
            InterpError::Limit(message) => TextError::Interp(message),
        }
    }
}

impl From<TextError> for GoatError {
    /// A locked document is PyMuPDF's `ValueError: document closed or
    /// encrypted`; anything else carries the reader's message.
    fn from(error: TextError) -> GoatError {
        match error {
            TextError::Locked => GoatError::value_error("document closed or encrypted"),
            other => GoatError::message(other.to_string()),
        }
    }
}

/// The text page PyMuPDF's `page.get_textpage(flags=...)` builds: the page's
/// content and annotations run through the stext device, kept within the
/// page bounds.
pub fn extract_page(
    doc: &Document,
    page_index: usize,
    flags: TextFlags,
) -> Result<TextPage, TextError> {
    let page = load_page(doc, page_index)?;
    extract(doc, &page, pdf_interp::page_bounds(&page), flags)
}

fn load_page(doc: &Document, page_index: usize) -> Result<Page, TextError> {
    if doc.needs_password() {
        return Err(TextError::Locked);
    }
    let mut page = doc.page(page_index).map_err(TextError::Pdf)?;
    // PyMuPDF temporarily clears /Rotate while creating a text page.
    // Page is a detached dictionary; the document itself stays unchanged.
    page.dict.insert("Rotate", 0_i64);
    Ok(page)
}

/// Runs `page` through the stext device with `rect` as the text page's
/// area.
fn extract(
    doc: &Document,
    page: &Page,
    rect: Rect,
    flags: TextFlags,
) -> Result<TextPage, TextError> {
    let mut device = device::StextDevice::new(doc, page, rect, flags);
    pdf_interp::run_page(doc, page, &mut device, &RunOptions::default())?;
    device.finish()
}

/// `cli._page_text_and_words`: one `TEXTFLAGS_TEXT` text page read for its
/// plain text and its words.
pub fn page_text_and_words(doc: &Document, index: usize) -> Result<(String, Vec<Word>), TextError> {
    let page = extract_page(doc, index, TextFlags::TEXT)?;
    Ok((page.text(), page.words()))
}

/// `cli._page_words`: the page's words (ligatures split, flags 194) as the
/// text cache's columns.
pub fn page_words(doc: &Document, index: usize) -> Result<WordColumns, TextError> {
    let page = extract_page(
        doc,
        index,
        TextFlags::WORDS.without(TextFlags::PRESERVE_LIGATURES),
    )?;
    Ok(word_columns(&page.words()))
}

/// `cli._word_columns`: word texts joined by newlines, four rect floats and
/// two line keys (block, line) per word.
pub fn word_columns(words: &[Word]) -> WordColumns {
    let mut text = String::new();
    let mut rects = Vec::with_capacity(words.len() * 4);
    let mut lines = Vec::with_capacity(words.len() * 2);
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        text.push_str(&word.text);
        rects.extend([word.rect.x0, word.rect.y0, word.rect.x1, word.rect.y1]);
        lines.extend([saturating_i32(word.block), saturating_i32(word.line)]);
    }
    WordColumns { text, rects, lines }
}

fn saturating_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
