//! Searchable OCR with embedded Unicode fonts. Forced OCR replaces the old page
//! content by its raster, so neither visible nor hidden old text survives twice.
//! A page that is one scanned image is read at the scan's own resolution, each
//! word's invisible text is fitted to its ink (see [`layout`]), and the result
//! is saved as a PDF/A-2b candidate the way `convert pdfa` saves one.

use pdf_core::{
    Dict, Document, Encryption, Matrix, Object, Operation, Page, Rect, SaveOptions, Stream,
    parse_content,
};
use pdf_interp::{Device, ImageEvent, ImageMaskEvent, RunOptions};
use pdf_ocr::{Bitmap, OcrOptions, PixelFormat};
use pdf_raster::Pixmap;
use pdf_render::{RenderOptions, render_page};

mod font;
mod layout;

/// Resolution for pages that are not one scanned image.
const DPI: f64 = 300.0;
/// Scans are read at their own resolution within this range and at its
/// nearer end outside it: Vision drops lines of small type in 100 dpi scans
/// read as they are and in 600 dpi scans resampled to 300 dpi.
const MIN_DPI: f64 = 150.0;
const MAX_DPI: f64 = 600.0;
/// Largest bitmap, in pixels, a scan's own resolution may ask for; a larger
/// page is read at [`DPI`] as before.
const MAX_SCAN_PIXELS: f64 = 67_108_864.0;
/// An image covering this share of the page is the page's scan.
const SCAN_COVER: f64 = 0.5;
const MAX_XOBJECT_DEPTH: usize = 8;
const STANDARD: &str = "PDF/A-2b";

/// The OCR result and the standard it was saved to.
pub struct OcrOutput {
    pub data: Vec<u8>,
    /// [`STANDARD`], or `None` when the PDF/A conversion failed and the
    /// plain OCR result was kept.
    pub standard: Option<&'static str>,
    pub warnings: Vec<String>,
}

pub fn ocr_document(data: Vec<u8>, force: bool) -> Result<OcrOutput, String> {
    let mut doc = Document::load(data).map_err(|e| e.to_string())?;
    if doc.needs_password() {
        return Err("the document is encrypted".to_owned());
    }
    let mut fonts = font::Fonts::default();
    let ocr = OcrOptions::default();
    for page in doc.pages().map_err(|e| e.to_string())? {
        if !force && page_has_text(&doc, &page) {
            continue;
        }
        let dpi = page_dpi(&doc, &page);
        let options = RenderOptions {
            dpi,
            alpha: false,
            annotations: false,
        };
        let pixmap = render_page(&doc, &page, &options).map_err(|e| e.to_string())?;
        let gray = pixmap.to_gray8(255);
        let bitmap = Bitmap {
            width: pixmap.width(),
            height: pixmap.height(),
            stride: pixmap.width() as usize,
            format: PixelFormat::Gray8,
            data: &gray,
        };
        let lines = pdf_ocr::recognize(&bitmap, &ocr).map_err(|e| format!("{e:?}"))?;
        let scan = Scan {
            pixmap: &pixmap,
            gray: &gray,
            dpi,
        };
        apply_page(&mut doc, &page, &scan, &lines, force, &mut fonts)?;
    }
    let plain = doc
        .save_to_bytes(&SaveOptions {
            compress_streams: true,
            garbage_collect: force,
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
    Ok(match pdfa(&plain) {
        Ok(data) => OcrOutput {
            data,
            standard: Some(STANDARD),
            warnings: Vec::new(),
        },
        Err(reason) => OcrOutput {
            data: plain,
            standard: None,
            warnings: vec![format!("saved without {STANDARD}: {reason}")],
        },
    })
}

/// The OCR result as a PDF/A-2b candidate. The conversion runs on a reloaded
/// copy, so a failure leaves the plain result intact.
fn pdfa(plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut doc = Document::load(plain.to_vec()).map_err(|e| e.to_string())?;
    pdf_inspect::make_pdfa(&mut doc).map_err(|e| e.to_string())?;
    doc.save_to_bytes(&SaveOptions {
        compress_streams: true,
        object_streams: true,
        garbage_collect: true,
        encryption: Encryption::Remove,
        version: Some((1, 7)),
        ..SaveOptions::default()
    })
    .map_err(|e| e.to_string())
}

/// The resolution to read `page` at: its scan's own, clamped to
/// [`MIN_DPI`]..=[`MAX_DPI`], when one image covers [`SCAN_COVER`] of it.
fn page_dpi(doc: &Document, page: &Page) -> f64 {
    let bounds = pdf_interp::page_bounds(page);
    let mut scan = ScanImage {
        page: bounds,
        best: None,
    };
    let options = RunOptions {
        annotations: false,
        ..RunOptions::default()
    };
    if pdf_interp::run_page_contents(doc, page, &mut scan, &options).is_err() {
        return DPI;
    }
    let Some((_, dpi)) = scan.best else {
        return DPI;
    };
    let square_inches = bounds.width() * bounds.height() / (72.0 * 72.0);
    let budget = (MAX_SCAN_PIXELS / square_inches).sqrt();
    dpi.clamp(MIN_DPI, MAX_DPI).min(budget.max(DPI))
}

/// Finds the image covering most of the page and its resolution.
struct ScanImage {
    page: Rect,
    /// (share of the page covered, dots per inch)
    best: Option<(f64, f64)>,
}

impl ScanImage {
    fn see(&mut self, width: u32, height: u32, ctm: &Matrix, bbox: &Rect) {
        let covered = bbox.normalized().intersect(&self.page);
        let area = self.page.width() * self.page.height();
        if covered.is_empty() || area <= 0.0 {
            return;
        }
        let cover = covered.width() * covered.height() / area;
        // Device space is 72 per inch; the unit square's sides carry the
        // image's columns and rows.
        let across = ctm.a.hypot(ctm.b);
        let down = ctm.c.hypot(ctm.d);
        let dpi = (72.0 * f64::from(width) / across).max(72.0 * f64::from(height) / down);
        if cover >= SCAN_COVER
            && dpi.is_finite()
            && dpi > 0.0
            && self.best.is_none_or(|(best, _)| cover > best)
        {
            self.best = Some((cover, dpi));
        }
    }
}

impl Device for ScanImage {
    fn fill_image(&mut self, event: &ImageEvent<'_>) {
        self.see(
            event.image.width(),
            event.image.height(),
            &event.ctm,
            &event.bbox,
        );
    }

    fn fill_image_mask(&mut self, event: &ImageMaskEvent<'_>) {
        self.see(
            event.image.width(),
            event.image.height(),
            &event.ctm,
            &event.bbox,
        );
    }
}

/// One page as Vision read it: the raster forced OCR keeps, the gray bitmap
/// behind recognition, and their resolution.
struct Scan<'a> {
    pixmap: &'a Pixmap,
    gray: &'a [u8],
    dpi: f64,
}

/// A word's text, whether a space follows it on its line, and its box.
struct Placed<'a> {
    text: &'a str,
    space: bool,
    place: layout::WordBox,
}

fn apply_page(
    doc: &mut Document,
    page: &Page,
    scan: &Scan<'_>,
    lines: &[pdf_ocr::Line],
    force: bool,
    fonts: &mut font::Fonts,
) -> Result<(), String> {
    let pixmap = scan.pixmap;
    let to_pixels =
        pdf_interp::page_transform(page).concat(&Matrix::scale(scan.dpi / 72.0, scan.dpi / 72.0));
    let to_page = to_pixels
        .invert()
        .ok_or("the OCR page transform is singular")?;
    let ink = layout::Ink {
        width: pixmap.width() as usize,
        height: pixmap.height() as usize,
        gray: scan.gray,
    };
    let words: Vec<Placed<'_>> = lines
        .iter()
        .flat_map(|line| {
            let count = line.words.len();
            line.words
                .iter()
                .zip(layout::place_words(&ink, line))
                .enumerate()
                .filter_map(move |(index, (word, place))| {
                    let place = place?;
                    (!word.text.is_empty()).then_some(Placed {
                        text: &word.text,
                        space: index + 1 < count,
                        place,
                    })
                })
        })
        .collect();
    if !force && words.is_empty() {
        return Ok(());
    }
    let mut resources = if force {
        Dict::new()
    } else {
        page.resources.clone()
    };
    let mut xobjects = doc
        .resolve_dict(resources.get(b"XObject").unwrap_or(&Object::Null))
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let mut ops = Vec::new();
    if force {
        let mut image = Dict::new();
        image.insert("Type", Object::name("XObject"));
        image.insert("Subtype", Object::name("Image"));
        image.insert("Width", i64::from(pixmap.width()));
        image.insert("Height", i64::from(pixmap.height()));
        image.insert("ColorSpace", Object::name("DeviceRGB"));
        image.insert("BitsPerComponent", 8);
        xobjects.insert(
            "GoatRaster",
            doc.add(Stream::new(image, pixmap.to_rgb8([255; 3]))),
        );
        let placement = Matrix::new(
            f64::from(pixmap.width()),
            0.0,
            0.0,
            -f64::from(pixmap.height()),
            0.0,
            f64::from(pixmap.height()),
        )
        .concat(&to_page);
        ops.push(operation(b"q", Vec::new()));
        ops.push(matrix_operation(b"cm", placement));
        ops.push(operation(b"Do", vec![Object::name("GoatRaster")]));
        ops.push(operation(b"Q", Vec::new()));
    }
    if !words.is_empty() {
        let embedded = fonts.embed(doc, words.iter().flat_map(|word| word.text.chars()))?;
        let layer = text_layer(&words, &embedded, to_page)?;
        let mut form_resources = Dict::new();
        form_resources.insert("Font", embedded.resources);
        let crop = page.crop_box();
        let mut form = Dict::new();
        form.insert("Type", Object::name("XObject"));
        form.insert("Subtype", Object::name("Form"));
        form.insert(
            "BBox",
            [crop.x0, crop.y0, crop.x1, crop.y1]
                .into_iter()
                .map(Object::Real)
                .collect::<Vec<_>>(),
        );
        form.insert("Resources", form_resources);
        let mut name = "GoatOCR".to_owned();
        let mut index = 1usize;
        while xobjects.get(name.as_bytes()).is_some() {
            name = format!("GoatOCR{index}");
            index += 1;
        }
        xobjects.insert(name.as_str(), doc.add(Stream::new(form, layer)));
        ops.push(operation(b"q", Vec::new()));
        ops.push(operation(b"Do", vec![Object::name(name)]));
        ops.push(operation(b"Q", Vec::new()));
    }
    resources.insert("XObject", xobjects);
    let mut contents = Vec::new();
    if !force {
        // Keep the old page's graphics state from leaking into the OCR form.
        contents.push(Object::Reference(
            doc.add(Stream::new(Dict::new(), b"q\n".to_vec())),
        ));
        contents.extend(page.content_refs().into_iter().map(Object::Reference));
        contents.push(Object::Reference(
            doc.add(Stream::new(Dict::new(), b"Q\n".to_vec())),
        ));
    }
    contents.push(Object::Reference(
        doc.add(Stream::new(Dict::new(), pdf_core::write_content(&ops))),
    ));
    let mut page_dict = page.dict.clone();
    page_dict.insert("Resources", resources);
    page_dict.insert("Contents", contents);
    doc.set(page.id, page_dict);
    Ok(())
}

/// Each word is fitted to its box in pixel space, turned with its line, then
/// mapped back through the renderer's exact crop/rotation/UserUnit transform.
/// CID codes preserve every Unicode scalar.
fn text_layer(
    words: &[Placed<'_>],
    fonts: &font::PageFonts,
    to_page: Matrix,
) -> Result<Vec<u8>, String> {
    let mut ops = vec![
        operation(b"BT", Vec::new()),
        operation(b"Tr", vec![Object::Integer(3)]),
    ];
    for operator in [b"Tc", b"Tw", b"Ts"] {
        ops.push(operation(operator, vec![Object::Integer(0)]));
    }
    for word in words {
        let (mut width, mut ascent, mut descent) = (0.0f64, 0.0f64, 0.0f64);
        for ch in word.text.chars() {
            let glyph = fonts.glyph(ch)?;
            width += glyph.advance;
            ascent = ascent.max(glyph.ascent);
            descent = descent.min(glyph.descent);
        }
        if width <= 0.0 || ascent <= descent {
            return Err("OCR word has invalid font metrics".to_owned());
        }
        let place = word.place;
        let size = place.height / (ascent - descent);
        let stretch = place.width / (width * size) * 100.0;
        let lift = ascent * size;
        ops.push(operation(b"Tz", vec![Object::Real(stretch)]));
        ops.push(matrix_operation(
            b"Tm",
            Matrix::new(
                place.along[0],
                place.along[1],
                -place.down[0],
                -place.down[1],
                place.origin.x + place.down[0] * lift,
                place.origin.y + place.down[1] * lift,
            )
            .concat(&to_page),
        ));
        let mut current = None;
        let mut encoded = Vec::with_capacity(word.text.len() * 2 + 2);
        for ch in word.text.chars().chain(word.space.then_some(' ')) {
            let glyph = fonts.glyph(ch)?;
            if current != Some(glyph.font) {
                if !encoded.is_empty() {
                    ops.push(operation(
                        b"Tj",
                        vec![Object::string(std::mem::take(&mut encoded))],
                    ));
                }
                ops.push(operation(
                    b"Tf",
                    vec![
                        Object::name(fonts.names[glyph.font].as_str()),
                        Object::Real(size),
                    ],
                ));
                current = Some(glyph.font);
            }
            encoded.extend_from_slice(&glyph.code.to_be_bytes());
        }
        ops.push(operation(b"Tj", vec![Object::string(encoded)]));
    }
    ops.push(operation(b"ET", Vec::new()));
    Ok(pdf_core::write_content(&ops))
}

fn operation(operator: &[u8], operands: Vec<Object>) -> Operation {
    Operation {
        operator: operator.to_vec(),
        operands,
    }
}

fn matrix_operation(operator: &[u8], matrix: Matrix) -> Operation {
    operation(
        operator,
        [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]
            .into_iter()
            .map(Object::Real)
            .collect(),
    )
}

fn page_has_text(doc: &Document, page: &Page) -> bool {
    let Ok(content) = doc.page_content(page) else {
        return false;
    };
    content_has_text(doc, &content, &page.resources, 0)
}

fn content_has_text(doc: &Document, content: &[u8], resources: &Dict, depth: usize) -> bool {
    let ops = parse_content(content).unwrap_or_default();
    if ops
        .iter()
        .any(|op| matches!(op.operator.as_slice(), b"Tj" | b"TJ" | b"'" | b"\""))
    {
        return true;
    }
    if depth >= MAX_XOBJECT_DEPTH {
        return false;
    }
    let xobjects = doc
        .resolve_dict(resources.get(b"XObject").unwrap_or(&Object::Null))
        .ok()
        .flatten()
        .unwrap_or_default();
    ops.iter()
        .filter(|op| op.operator.as_slice() == b"Do")
        .any(|op| {
            let Some(name) = op.operands.first().and_then(Object::as_name) else {
                return false;
            };
            let Some(stream) = xobjects
                .get(name)
                .and_then(|x| doc.resolve_stream(x).ok().flatten())
            else {
                return false;
            };
            if stream.dict.get_name(b"Subtype") != Some(b"Form") {
                return false;
            }
            let Ok(decoded) = doc.decode_stream(&stream) else {
                return false;
            };
            let inner = doc
                .resolve_dict(stream.dict.get(b"Resources").unwrap_or(&Object::Null))
                .ok()
                .flatten()
                .unwrap_or_default();
            content_has_text(doc, &decoded.data, &inner, depth + 1)
        })
}

#[cfg(test)]
mod tests;
