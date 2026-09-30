//! PDF page placement: overlay, scale, n-up, and booklet imposition.

use std::collections::HashMap;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{IntChoices, float_value, required};
use goat_common::paths;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Matrix, ObjRef, Object, Rect, Stream};
use serde_json::{Map, Value};

use crate::geometry::{self, page_rect, text_matrix};
use crate::open::{self, Lib, display, one_based, out_path, pdf_error, result, source};

type Output = Result<Map<String, Value>, GoatError>;

fn command(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(Arg::new("file").required(true))
}

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "pages",
        Verb::new(
            command("scale", "scale page size")
                .arg(
                    Arg::new("factor")
                        .long("factor")
                        .value_parser(float_value)
                        .required(true),
                )
                .arg(Arg::new("output").short('o').long("output")),
            scale,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("nup", "n-up imposition (2 or 4)")
                .arg(
                    Arg::new("n")
                        .long("n")
                        .value_parser(IntChoices(&["2", "4"]))
                        .required(true),
                )
                .arg(Arg::new("output").short('o').long("output")),
            nup,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("booklet", "saddle-stitch booklet imposition")
                .arg(Arg::new("output").short('o').long("output")),
            booklet,
        ),
    );
    registry.command(Verb::new(
        command("overlay", "stamp one PDF over another")
            .arg(Arg::new("stamp").required(true))
            .arg(Arg::new("output").short('o').long("output")),
        overlay,
    ));
}

fn readable(doc: &Document) -> Result<(), GoatError> {
    if doc.needs_password() {
        Err(open::closed_or_encrypted())
    } else {
        Ok(())
    }
}

fn scale(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "scaled")?;
    let donor = open::load(&src, Lib::PyMuPdf)?;
    readable(&donor)?;
    let factor = *required::<f64>(matches, "factor")?;
    let mut doc = Document::new();
    let mut forms = HashMap::new();
    for index in 0..donor.page_count().map_err(pdf_error)? {
        let rect = page_rect(&donor.page(index).map_err(pdf_error)?);
        geometry::new_page(&mut doc, rect.width() * factor, rect.height() * factor)?;
        let target = page_rect(&doc.page(index).map_err(pdf_error)?);
        place(&mut doc, index, target, &donor, index, &mut forms)?;
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-scale", vec![display(&src)], vec![display(&out)]);
    map.insert("factor".into(), factor.into());
    Ok(map)
}

fn nup(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let n = *required::<i64>(matches, "n")?;
    let out = out_path(matches, &src, &format!("{n}up"))?;
    let donor = open::load(&src, Lib::PyMuPdf)?;
    readable(&donor)?;
    let first = page_rect(&donor.page(0).map_err(pdf_error)?);
    let (w, h) = (first.width(), first.height());
    let rows = if n == 2 { 1 } else { 2 };
    let per = rows * 2;
    let mut doc = Document::new();
    let mut forms = HashMap::new();
    let count = donor.page_count().map_err(pdf_error)?;
    for start in (0..count).step_by(per) {
        let sheet = doc.page_count().map_err(pdf_error)?;
        geometry::new_page(&mut doc, 2.0 * w, rows as f64 * h)?;
        for slot in 0..per.min(count - start) {
            let (x, y) = ((slot % 2) as f64 * w, (slot / 2) as f64 * h);
            place(
                &mut doc,
                sheet,
                Rect::new(x, y, x + w, y + h),
                &donor,
                start + slot,
                &mut forms,
            )?;
        }
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-nup", vec![display(&src)], vec![display(&out)]);
    map.insert("n".into(), n.into());
    map.insert("sheets".into(), doc.page_count().map_err(pdf_error)?.into());
    Ok(map)
}

fn booklet(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "booklet")?;
    let donor = open::load(&src, Lib::PyMuPdf)?;
    readable(&donor)?;
    let count = donor.page_count().map_err(pdf_error)?;
    let padded = count.div_ceil(4) * 4;
    let mut order = Vec::with_capacity(padded);
    let mut low = 0;
    let mut high = padded.saturating_sub(1);
    while low < high {
        order.extend([high, low, low + 1, high - 1]);
        low += 2;
        high -= 2;
    }
    let bounds = page_rect(&donor.page(0).map_err(pdf_error)?);
    let (w, h) = (bounds.width(), bounds.height());
    let mut doc = Document::new();
    let mut forms = HashMap::new();
    for (sheet, pair) in order.as_chunks::<2>().0.iter().enumerate() {
        geometry::new_page(&mut doc, w * 2.0, h)?;
        for (slot, &page) in pair.iter().enumerate() {
            if page < count {
                let x = slot as f64 * w;
                place(
                    &mut doc,
                    sheet,
                    Rect::new(x, 0.0, x + w, h),
                    &donor,
                    page,
                    &mut forms,
                )?;
            }
        }
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-booklet", vec![display(&src)], vec![display(&out)]);
    map.insert("sheets".into(), doc.page_count().map_err(pdf_error)?.into());
    map.insert("page_order".into(), one_based(&order));
    Ok(map)
}

fn overlay(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let stamp = paths::resolve(required::<String>(matches, "stamp")?)?;
    let out = out_path(matches, &src, "overlay")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let donor = open::load(&stamp, Lib::PyMuPdf)?;
    readable(&doc)?;
    readable(&donor)?;
    let mut forms = HashMap::new();
    let last = donor.page_count().map_err(pdf_error)?.saturating_sub(1);
    for index in 0..doc.page_count().map_err(pdf_error)? {
        let target = page_rect(&doc.page(index).map_err(pdf_error)?);
        wrap_contents(&mut doc, index)?;
        place(&mut doc, index, target, &donor, index.min(last), &mut forms)?;
    }
    open::save_mupdf(&doc, &out)?;
    Ok(result(
        "overlay",
        vec![display(&src), display(&stamp)],
        vec![display(&out)],
    ))
}

fn form_dict(bounds: Rect) -> Dict {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Form"));
    dict.insert("BBox", bounds.to_object());
    dict.insert("Matrix", Matrix::IDENTITY.to_object());
    dict
}

fn inverse(matrix: Matrix) -> Result<Matrix, GoatError> {
    matrix
        .invert()
        .ok_or_else(|| GoatError::value_error("matrix not invertible"))
}

/// Two Form XObjects, as show_pdf_page uses: one shared full page and one placement
/// that clips in source user space and maps it proportionally into the target.
fn place(
    doc: &mut Document,
    target_index: usize,
    target: Rect,
    donor: &Document,
    source_index: usize,
    forms: &mut HashMap<usize, ObjRef>,
) -> Result<(), GoatError> {
    let source_page = donor.page(source_index).map_err(pdf_error)?;
    let content = donor.page_content(&source_page).map_err(pdf_error)?;
    if content.is_empty() {
        return Err(GoatError::value_error(
            "nothing to show - source page empty",
        ));
    }
    let source_bounds = page_rect(&source_page).transform(&inverse(text_matrix(&source_page))?);
    let target_page = doc.page(target_index).map_err(pdf_error)?;
    let target_bounds = target.transform(&inverse(text_matrix(&target_page))?);
    if target_bounds.is_empty() {
        return Err(GoatError::value_error("rect must be finite and not empty"));
    }
    let scale = (target_bounds.width() / source_bounds.width())
        .min(target_bounds.height() / source_bounds.height());
    let matrix = Matrix::translate(
        -(source_bounds.x0 + source_bounds.x1) / 2.0,
        -(source_bounds.y0 + source_bounds.y1) / 2.0,
    )
    .concat(&Matrix::scale(scale, scale))
    .concat(&Matrix::translate(
        (target_bounds.x0 + target_bounds.x1) / 2.0,
        (target_bounds.y0 + target_bounds.y1) / 2.0,
    ));
    let full = match forms.get(&source_index) {
        Some(id) => *id,
        None => {
            let at = doc.page_count().map_err(pdf_error)?;
            doc.import_pages(donor, &[source_index], at)
                .map_err(pdf_error)?;
            let copied = doc.page(at).map_err(pdf_error)?;
            doc.remove_page(at).map_err(pdf_error)?;
            let mut dict = form_dict(source_page.media_box());
            dict.insert("Resources", copied.resources);
            let full = doc.add(Stream::new(dict, content));
            forms.insert(source_index, full);
            full
        }
    };
    let mut placement = form_dict(source_bounds);
    placement.insert("Matrix", matrix.to_object());
    let mut xobjects = Dict::new();
    xobjects.insert("fullpage", full);
    let mut resources = Dict::new();
    resources.insert("XObject", xobjects);
    placement.insert("Resources", resources);
    let form = doc.add(Stream::new(placement, b"/fullpage Do".to_vec()));
    let mut page = doc.page(target_index).map_err(pdf_error)?;
    let mut xobjects = match page.resources.get(b"XObject") {
        Some(value) => doc
            .resolve_dict(value)
            .map_err(pdf_error)?
            .unwrap_or_default(),
        None => Dict::new(),
    };
    let mut suffix = 0;
    while xobjects.contains_key(format!("fzFrm{suffix}").as_bytes()) {
        suffix += 1;
    }
    let name = format!("fzFrm{suffix}");
    xobjects.insert(name.as_bytes(), form);
    page.resources.insert("XObject", xobjects);
    page.dict.insert("Resources", page.resources);
    doc.set(page.id, page.dict);
    append_contents(doc, target_index, format!(" q /{name} Do Q ").into_bytes())
}

pub(crate) fn append_contents(
    doc: &mut Document,
    index: usize,
    bytes: Vec<u8>,
) -> Result<(), GoatError> {
    let mut page = doc.page(index).map_err(pdf_error)?;
    let mut contents = match page.dict.get(b"Contents") {
        Some(value) => match doc.resolve(value).map_err(pdf_error)? {
            Object::Array(items) => items,
            Object::Stream(_) => vec![value.clone()],
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    contents.push(Object::Reference(doc.add(Stream::new(Dict::new(), bytes))));
    page.dict.insert("Contents", contents);
    doc.set(page.id, page.dict);
    Ok(())
}

/// Isolate the old page graphics state before appending new drawing operations.
pub(crate) fn wrap_contents(doc: &mut Document, index: usize) -> Result<(), GoatError> {
    let mut page = doc.page(index).map_err(pdf_error)?;
    let mut contents = match page.dict.get(b"Contents") {
        Some(value) => match doc.resolve(value).map_err(pdf_error)? {
            Object::Array(items) => items,
            Object::Stream(_) => vec![value.clone()],
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    if !contents.is_empty() {
        contents.insert(
            0,
            Object::Reference(doc.add(Stream::new(Dict::new(), b"q\n".to_vec()))),
        );
        contents.push(Object::Reference(
            doc.add(Stream::new(Dict::new(), b"\nQ\n".to_vec())),
        ));
        page.dict.insert("Contents", contents);
        doc.set(page.id, page.dict);
    }
    Ok(())
}
