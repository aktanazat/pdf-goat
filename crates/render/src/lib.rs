//! Page rasterisation: runs a page through pdf-interp onto a pdf-raster
//! canvas, and the `render` and `compare visual` verbs.

mod command;
mod device;
mod glyph;

use std::fmt;

use pdf_core::{Document, Matrix, Page, Rect};
use pdf_interp::{InterpError, Interpreter, RunOptions};
use pdf_raster::{Pixmap, RasterError};

/// How a page is rasterised.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderOptions {
    /// Pixels per inch; 72 renders one pixel per point.
    pub dpi: f64,
    /// Transparent background instead of white.
    pub alpha: bool,
    /// Draw annotation appearances after the content.
    pub annotations: bool,
}

impl Default for RenderOptions {
    fn default() -> RenderOptions {
        RenderOptions {
            dpi: 72.0,
            alpha: false,
            annotations: true,
        }
    }
}

/// Why a page did not render.
#[derive(Debug)]
pub enum RenderError {
    Interp(InterpError),
    Raster(RasterError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Interp(error) => write!(f, "{error}"),
            RenderError::Raster(error) => write!(f, "{error:?}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<InterpError> for RenderError {
    fn from(error: InterpError) -> RenderError {
        RenderError::Interp(error)
    }
}

impl From<RasterError> for RenderError {
    fn from(error: RasterError) -> RenderError {
        RenderError::Raster(error)
    }
}

/// The page as a premultiplied RGBA pixmap at `options.dpi`, crop box and
/// rotation applied, on white unless `options.alpha`.
pub fn render_page(
    doc: &Document,
    page: &Page,
    options: &RenderOptions,
) -> Result<Pixmap, RenderError> {
    render_area(doc, &Interpreter::new(), page, options, None, true)
}

/// Renders page graphics without painting editable body text. Text clipping
/// still applies. Text inside patterns and soft masks remains part of their
/// paint. Use a transparent background to place this behind editable content.
pub fn render_page_graphics(
    doc: &Document,
    page: &Page,
    options: &RenderOptions,
) -> Result<Pixmap, RenderError> {
    render_area(doc, &Interpreter::new(), page, options, None, false)
}

fn render_area(
    doc: &Document,
    interpreter: &Interpreter,
    page: &Page,
    options: &RenderOptions,
    clip: Option<Rect>,
    draw_text: bool,
) -> Result<Pixmap, RenderError> {
    if !options.dpi.is_finite() || options.dpi <= 0.0 {
        return Err(InterpError::Limit("dpi must be positive and finite".to_owned()).into());
    }
    let page_bounds = pdf_interp::page_bounds(page);
    let bounds = clip.map_or(page_bounds, |clip| page_bounds.intersect(&clip));
    if bounds.is_empty() {
        return Err(InterpError::Limit("render region has no area".to_owned()).into());
    }
    // MuPDF constructs its dpi matrix in float precision before composing
    // it with the page transform; later rounding shifts exact subsamples.
    let scale = f64::from(options.dpi as f32 / 72.0_f32);
    let x0 = (bounds.x0 * scale + 0.001).floor();
    let y0 = (bounds.y0 * scale + 0.001).floor();
    let x1 = (bounds.x1 * scale - 0.001).ceil();
    let y1 = (bounds.y1 * scale - 0.001).ceil();
    let mut device =
        device::RasterDevice::new((x1 - x0) as u32, (y1 - y0) as u32, options.alpha, draw_text)?;
    interpreter.run_page(
        doc,
        page,
        &mut device,
        &RunOptions {
            transform: Matrix::scale(scale, scale).concat(&Matrix::translate(-x0, -y0)),
            annotations: options.annotations,
            ..RunOptions::default()
        },
    )?;
    device.finish()
}

/// Registers `render` and `compare visual`.
pub fn register(registry: &mut goat_common::Registry) {
    command::register(registry);
}
