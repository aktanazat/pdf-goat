//! PDF content-stream interpreter.
//!
//! Runs a page's content streams (and, optionally, its annotation
//! appearances) the way MuPDF's `pdf_run_page` does and reports what they
//! draw to a [`Device`]: paths, text with per-glyph positions and Unicode,
//! images, shadings, clips, transparency groups, forms and marked content,
//! each event tagged with the [`Provenance`] of the operation that made it.
//!
//! Invalid operators, fonts, images and colour spaces are skipped where
//! recovery is possible. A run reports structural content/decode errors,
//! a locked document, or a work bound instead of inventing drawing data.

mod annot;
mod cmyk_table;
mod colorspace;
mod device;
mod font;
mod font_data;
mod function;
mod image;
mod interp;
mod ocg;
mod path;
mod pattern;
mod shading;

use std::fmt;

use pdf_core::{Document, Matrix, ObjRef, Page, Rect};

pub use device::{
    AnnotationEvent, BlendMode, Brush, ClipEvent, ContentSource, Device, FillEvent, FormCall,
    FormEvent, Glyph, GlyphPos, GlyphText, GroupEvent, ImageEvent, ImageMaskEvent,
    MarkedContentEvent, Paint, Provenance, Quad, Rgb, ShadingEvent, StrokeEvent, TextRun,
    Type3GlyphEvent,
};
pub use font::{FontFlags, FontSubtype, PdfFont};
pub use image::{AlphaChannel, ColorFamily, ImagePixels, ImageSamples, PdfImage, StencilPixels};
pub use path::{FillRule, LineCap, LineJoin, Path, PathEl, StrokeStyle};
pub use pattern::{ShadingPaint, SoftMask, TilingPaint};
pub use shading::{ShadeVertex, Shading};

/// Why a run stopped.
#[derive(Debug)]
pub enum InterpError {
    /// The document could not be read: [`pdf_core::Error::NeedsPassword`]
    /// for a locked document, a missing page content, a decode bound.
    Pdf(pdf_core::Error),
    /// [`RunOptions::max_operations`] or another work bound was exceeded.
    Limit(String),
}

impl fmt::Display for InterpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterpError::Pdf(error) => write!(f, "{error}"),
            InterpError::Limit(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for InterpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InterpError::Pdf(error) => Some(error),
            InterpError::Limit(_) => None,
        }
    }
}

impl From<pdf_core::Error> for InterpError {
    fn from(error: pdf_core::Error) -> InterpError {
        InterpError::Pdf(error)
    }
}

/// Which annotation /F flags decide visibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Intent {
    /// Screen display: Hidden and NoView annotations are skipped.
    #[default]
    View,
    /// Printing: Hidden annotations and those without the Print flag are
    /// skipped.
    Print,
}

/// How a page is run.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Applied after [`page_transform`] (MuPDF's `fz_run_page` ctm), e.g.
    /// `Matrix::scale(dpi / 72.0, dpi / 72.0)`.
    pub transform: Matrix,
    /// [`run_page`] also runs annotation appearances after the content
    /// (PyMuPDF `get_pixmap(annots=True)`).
    pub annotations: bool,
    pub intent: Intent,
    /// Emit optional content even when its visibility configuration hides
    /// it. Editors use this to remove hidden data; display devices leave
    /// it false (the default).
    pub include_hidden_content: bool,
    /// Bound on operations executed in one run, nested streams included.
    pub max_operations: u64,
}

impl Default for RunOptions {
    fn default() -> RunOptions {
        RunOptions {
            transform: Matrix::IDENTITY,
            annotations: true,
            intent: Intent::View,
            include_hidden_content: false,
            max_operations: 50_000_000,
        }
    }
}

/// MuPDF's `pdf_page_transform`: PDF user space → page space, with the
/// origin at the crop box's top-left, y growing down, /Rotate applied and
/// /UserUnit scaled.
pub fn page_transform(page: &Page) -> Matrix {
    page_box_and_transform(page).1
}

/// MuPDF's `fz_bound_page`: the crop box through [`page_transform`], so
/// `Rect(0, 0, width, height)` of the page as displayed.
pub fn page_bounds(page: &Page) -> Rect {
    let (crop, ctm) = page_box_and_transform(page);
    crop.transform(&ctm)
}

fn page_box_and_transform(page: &Page) -> (Rect, Matrix) {
    let mut crop = page.crop_box();
    if crop.x1 - crop.x0 < 1.0 || crop.y1 - crop.y0 < 1.0 {
        crop = Rect::new(0.0, 0.0, 1.0, 1.0);
    }
    let unit = page.user_unit();
    let rotated = Matrix::rotate(-f64::from(page.rotation())).concat(&Matrix::scale(unit, -unit));
    let placed = crop.transform(&rotated);
    (
        crop,
        rotated.concat(&Matrix::translate(-placed.x0, -placed.y0)),
    )
}

/// Runs the page's content, then its annotation appearances when
/// `options.annotations` is set.
pub fn run_page(
    doc: &Document,
    page: &Page,
    device: &mut dyn Device,
    options: &RunOptions,
) -> Result<(), InterpError> {
    Interpreter::new().run_page(doc, page, device, options)
}

/// Runs the page's content streams only (MuPDF `fz_run_page_contents`).
pub fn run_page_contents(
    doc: &Document,
    page: &Page,
    device: &mut dyn Device,
    options: &RunOptions,
) -> Result<(), InterpError> {
    Interpreter::new().run_page_contents(doc, page, device, options)
}

/// Runs displayable annotations in /Annots order, then widgets in /Annots order.
pub fn run_annotations(
    doc: &Document,
    page: &Page,
    device: &mut dyn Device,
    options: &RunOptions,
) -> Result<(), InterpError> {
    Interpreter::new().run_annotations(doc, page, device, options)
}

/// Runs one annotation's normal appearance; `Ok(false)` when MuPDF would
/// not draw it.
pub fn run_annotation_appearance(
    doc: &Document,
    page: &Page,
    annot: ObjRef,
    device: &mut dyn Device,
    options: &RunOptions,
) -> Result<bool, InterpError> {
    Interpreter::new().run_annotation_appearance(doc, page, annot, device, options)
}

/// The four runs with fonts, colour spaces, functions and images cached
/// across calls. Use one per document and thread.
#[derive(Default)]
pub struct Interpreter {
    cache: interp::Cache,
}

impl Interpreter {
    pub fn new() -> Interpreter {
        Interpreter::default()
    }

    /// Content, then annotations when `options.annotations`.
    pub fn run_page(
        &self,
        doc: &Document,
        page: &Page,
        device: &mut dyn Device,
        options: &RunOptions,
    ) -> Result<(), InterpError> {
        self.run_page_contents(doc, page, device, options)?;
        if options.annotations {
            self.run_annotations(doc, page, device, options)?;
        }
        Ok(())
    }

    pub fn run_page_contents(
        &self,
        doc: &Document,
        page: &Page,
        device: &mut dyn Device,
        options: &RunOptions,
    ) -> Result<(), InterpError> {
        let run = interp::Run::new(doc, &self.cache, options);
        run.page_contents(page, device)
    }

    pub fn run_annotations(
        &self,
        doc: &Document,
        page: &Page,
        device: &mut dyn Device,
        options: &RunOptions,
    ) -> Result<(), InterpError> {
        let run = interp::Run::new(doc, &self.cache, options);
        annot::run_all(&run, page, device)
    }

    pub fn run_annotation_appearance(
        &self,
        doc: &Document,
        page: &Page,
        annot: ObjRef,
        device: &mut dyn Device,
        options: &RunOptions,
    ) -> Result<bool, InterpError> {
        let run = interp::Run::new(doc, &self.cache, options);
        annot::run_one(&run, page, annot, device)
    }
}
