//! Searchable OCR with embedded Unicode fonts. Forced OCR replaces the old page
//! content by its raster, so neither visible nor hidden old text survives twice.

use pdf_core::{Dict, Document, Matrix, Object, Operation, Page, Stream, parse_content};
use pdf_ocr::{Bitmap, OcrOptions, PixelFormat};
use pdf_raster::Pixmap;
use pdf_render::{RenderOptions, render_page};

mod font;

const DPI: f64 = 300.0;
const MAX_XOBJECT_DEPTH: usize = 8;

pub fn ocr_document(data: Vec<u8>, force: bool) -> Result<Vec<u8>, String> {
    let mut doc = Document::load(data).map_err(|e| e.to_string())?;
    if doc.needs_password() {
        return Err("the document is encrypted".to_owned());
    }
    let mut fonts = font::Fonts::default();
    let options = RenderOptions {
        dpi: DPI,
        alpha: false,
        annotations: false,
    };
    let ocr = OcrOptions::default();
    for page in doc.pages().map_err(|e| e.to_string())? {
        if !force && page_has_text(&doc, &page) {
            continue;
        }
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
        apply_page(&mut doc, &page, &pixmap, &lines, force, &mut fonts)?;
    }
    doc.save_to_bytes(&pdf_core::SaveOptions {
        compress_streams: true,
        garbage_collect: force,
        ..Default::default()
    })
    .map_err(|e| e.to_string())
}

fn apply_page(
    doc: &mut Document,
    page: &Page,
    pixmap: &Pixmap,
    lines: &[pdf_ocr::Line],
    force: bool,
    fonts: &mut font::Fonts,
) -> Result<(), String> {
    let to_pixels = pdf_interp::page_transform(page).concat(&Matrix::scale(DPI / 72.0, DPI / 72.0));
    let to_page = to_pixels
        .invert()
        .ok_or("the OCR page transform is singular")?;
    let words: Vec<(&pdf_ocr::Word, bool)> = lines
        .iter()
        .flat_map(|line| {
            line.words.iter().enumerate().filter_map(|(index, word)| {
                let rect = word.bbox?;
                (!word.text.is_empty() && rect.width > 0.0 && rect.height > 0.0)
                    .then_some((word, index + 1 < line.words.len()))
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
        let embedded = fonts.embed(doc, words.iter().flat_map(|(word, _)| word.text.chars()))?;
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

/// Each word is fitted in pixel space, then mapped back through the renderer's
/// exact crop/rotation/UserUnit transform. CID codes preserve every Unicode scalar.
fn text_layer(
    words: &[(&pdf_ocr::Word, bool)],
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
    for &(word, space) in words {
        let rect = word.bbox.ok_or("OCR word has no box")?;
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
        let size = rect.height / (ascent - descent);
        let stretch = rect.width / (width * size) * 100.0;
        ops.push(operation(b"Tz", vec![Object::Real(stretch)]));
        ops.push(matrix_operation(
            b"Tm",
            Matrix::new(1.0, 0.0, 0.0, -1.0, rect.x, rect.y + ascent * size).concat(&to_page),
        ));
        let mut current = None;
        let mut encoded = Vec::with_capacity(word.text.len() * 2 + 2);
        for ch in word.text.chars().chain(space.then_some(' ')) {
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
