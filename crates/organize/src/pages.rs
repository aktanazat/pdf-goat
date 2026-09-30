//! In-place page insertion, duplication and page boxes.

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{int_value, optional, required};
use goat_common::parse::{page_indices, parse_pages, parse_rect};
use goat_common::paths;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Document, Object, Rect};
use serde_json::{Map, Value};

use crate::dest::{self, Target};
use crate::geometry;
use crate::open::{self, Lib, display, one_based, out_path, pdf_error, result, source};

type Output = Result<Map<String, Value>, GoatError>;

fn command(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(Arg::new("file").required(true))
}

pub(crate) fn register_before_layout(registry: &mut Registry) {
    registry.family_verb(
        "pages",
        Verb::new(
            command("blank", "insert blank pages matching the adjacent page")
                .arg(
                    Arg::new("at")
                        .long("at")
                        .value_parser(int_value)
                        .help("1-based insertion position; default: end"),
                )
                .arg(
                    Arg::new("count")
                        .long("count")
                        .value_parser(int_value)
                        .default_value("1"),
                )
                .arg(Arg::new("output").short('o').long("output")),
            blank,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("duplicate", "duplicate selected pages in place")
                .arg(Arg::new("pages").long("pages").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            duplicate,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("crop", "set crop box")
                .arg(
                    Arg::new("box")
                        .long("box")
                        .required(true)
                        .help("x0,y0,x1,y1"),
                )
                .arg(Arg::new("pages").long("pages"))
                .arg(Arg::new("output").short('o').long("output")),
            crop,
        ),
    );
}

pub(crate) fn register_after_layout(registry: &mut Registry) {
    registry.family_verb(
        "pages",
        Verb::new(
            command("boxes", "set the media, crop, trim, or bleed box")
                .arg(
                    Arg::new("box")
                        .long("box")
                        .required(true)
                        .value_parser(["media", "crop", "trim", "bleed"]),
                )
                .arg(Arg::new("rect").long("rect").required(true))
                .arg(Arg::new("pages").long("pages"))
                .arg(Arg::new("output").short('o').long("output")),
            boxes,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("insert", "insert pages from another PDF")
                .arg(Arg::new("source").long("source").required(true))
                .arg(
                    Arg::new("at")
                        .long("at")
                        .value_parser(int_value)
                        .help("1-based position (default: end)"),
                )
                .arg(Arg::new("output").short('o').long("output")),
            insert,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("replace", "replace pages with another PDF")
                .arg(Arg::new("source").long("source").required(true))
                .arg(Arg::new("pages").long("pages").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            replace,
        ),
    );
}

fn blank(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let count = *required::<i64>(matches, "count")?;
    if !(1..=100).contains(&count) {
        return Err(GoatError::message("--count must be between 1 and 100"));
    }
    let src = source(matches)?;
    let out = out_path(matches, &src, "blank-pages")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let total = doc.page_count().map_err(pdf_error)?;
    let max = dest::to_i64(total + 1);
    let at = optional::<i64>(matches, "at")?.copied().unwrap_or(max);
    if !(1..=max).contains(&at) {
        return Err(GoatError::message(format!(
            "--at must be between 1 and {max}"
        )));
    }
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let index = usize::try_from(at - 1).unwrap_or(0);
    let template = doc
        .page(index.saturating_sub(1).min(total.saturating_sub(1)))
        .map_err(pdf_error)?;
    let bounds = geometry::page_rect(&template);
    for offset in 0..usize::try_from(count).unwrap_or(0) {
        geometry::new_page(&mut doc, bounds.width(), bounds.height())?;
        let last = doc.page_count().map_err(pdf_error)? - 1;
        doc.move_page(last, index + offset).map_err(pdf_error)?;
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-blank", vec![display(&src)], vec![display(&out)]);
    map.insert("inserted_at".into(), at.into());
    map.insert("inserted_pages".into(), count.into());
    map.insert(
        "total_pages".into(),
        doc.page_count().map_err(pdf_error)?.into(),
    );
    Ok(map)
}

fn duplicate(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "duplicated")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let donor = open::load(&src, Lib::PyMuPdf)?;
    let mut selected = parse_pages(
        required::<String>(matches, "pages")?,
        doc.page_count().map_err(pdf_error)?,
    )?;
    selected.sort_unstable();
    selected.dedup();
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    for (offset, &index) in selected.iter().enumerate() {
        let original = donor.page(index).map_err(pdf_error)?;
        let originals = match original.dict.get(b"Annots") {
            Some(annots) => donor
                .resolve_array(annots)
                .map_err(pdf_error)?
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut keep = Vec::with_capacity(originals.len());
        for annot in &originals {
            let target = if let Some(dict) = donor.resolve_dict(annot).map_err(pdf_error)? {
                if dict.get_name(b"Subtype") == Some(b"Link".as_slice()) {
                    match (dict.get(b"Dest"), dict.get(b"A")) {
                        (Some(target), _) => dest::dest_target(&donor, target)?,
                        (_, Some(action)) => dest::action_target(&donor, action, Some(index))?,
                        _ => Target::Nothing,
                    }
                } else {
                    Target::External
                }
            } else {
                Target::Nothing
            };
            keep.push(match target {
                Target::Page(page) => page == dest::to_i64(index),
                Target::External => true,
                Target::Nothing => false,
            });
        }
        let copied = doc
            .import_pages(&donor, &[index], index + offset + 1)
            .map_err(pdf_error)?;
        for id in copied {
            let mut page = doc
                .get(id)
                .map_err(pdf_error)?
                .as_dict()
                .cloned()
                .ok_or_else(|| GoatError::message("invalid page"))?;
            if let Some(annots) = page.get(b"Annots") {
                let annots = doc
                    .resolve_array(annots)
                    .map_err(pdf_error)?
                    .unwrap_or_default();
                let retained: Vec<Object> = annots
                    .into_iter()
                    .zip(&keep)
                    .filter_map(|(annot, keep)| keep.then_some(annot))
                    .collect();
                if retained.is_empty() {
                    page.remove(b"Annots");
                } else {
                    page.insert("Annots", retained);
                }
                doc.set(id, page);
            }
        }
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-duplicate", vec![display(&src)], vec![display(&out)]);
    map.insert("duplicated_pages".into(), one_based(&selected));
    map.insert(
        "total_pages".into(),
        doc.page_count().map_err(pdf_error)?.into(),
    );
    Ok(map)
}

fn crop(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "cropped")?;
    let requested = geometry::rect_arg(required::<String>(matches, "box")?)?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let indices = page_indices(
        optional::<String>(matches, "pages")?.map(String::as_str),
        doc.page_count().map_err(pdf_error)?,
    )?;
    for index in indices {
        if doc.needs_password() {
            return Err(open::closed_or_encrypted());
        }
        let mut page = doc.page(index).map_err(pdf_error)?;
        let media = page.media_box();
        let stored = Rect::new(
            requested.x0,
            media.y1 - requested.y1,
            requested.x1,
            media.y1 - requested.y0,
        );
        if stored.is_empty()
            || stored.x0 < media.x0
            || stored.y0 < media.y0
            || stored.x1 > media.x1
            || stored.y1 > media.y1
        {
            return Err(GoatError::value_error("CropBox not in MediaBox"));
        }
        page.dict.insert("CropBox", stored.to_object());
        doc.set(page.id, page.dict);
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-crop", vec![display(&src)], vec![display(&out)]);
    map.insert("box".into(), geometry::values(requested).into());
    Ok(map)
}

fn boxes(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "boxes")?;
    let name = required::<String>(matches, "box")?;
    let key = match name.as_str() {
        "media" => "MediaBox",
        "crop" => "CropBox",
        "trim" => "TrimBox",
        _ => "BleedBox",
    };
    let rect = parse_rect(required::<String>(matches, "rect")?)?;
    let mut doc = open::load(&src, Lib::PikePdf)?;
    for index in page_indices(
        optional::<String>(matches, "pages")?.map(String::as_str),
        doc.page_count().map_err(pdf_error)?,
    )? {
        let mut page = doc.page(index).map_err(pdf_error)?;
        page.dict.insert(
            key,
            Object::Array(rect.iter().copied().map(Object::Real).collect()),
        );
        doc.set(page.id, page.dict);
    }
    open::save_qpdf(&doc, &out)?;
    let mut map = result("pages-boxes", vec![display(&src)], vec![display(&out)]);
    map.insert("box".into(), name.clone().into());
    map.insert("rect".into(), rect.into());
    Ok(map)
}

fn insert(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let other = paths::resolve(required::<String>(matches, "source")?)?;
    let out = out_path(matches, &src, "inserted")?;
    let mut doc = open::load(&src, Lib::PikePdf)?;
    let donor = open::load(&other, Lib::PikePdf)?;
    let at = optional::<i64>(matches, "at")?.copied();
    let count = doc.page_count().map_err(pdf_error)?;
    let position = at
        .filter(|at| *at != 0)
        .map_or(dest::to_i64(count), |at| at.saturating_sub(1));
    insert_donor(&mut doc, &donor, position)?;
    open::save_qpdf(&doc, &out)?;
    let mut map = result(
        "pages-insert",
        vec![display(&src), display(&other)],
        vec![display(&out)],
    );
    map.insert("at".into(), at.into());
    Ok(map)
}

fn replace(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let other = paths::resolve(required::<String>(matches, "source")?)?;
    let out = out_path(matches, &src, "replaced")?;
    let mut doc = open::load(&src, Lib::PikePdf)?;
    let donor = open::load(&other, Lib::PikePdf)?;
    let mut targets = parse_pages(
        required::<String>(matches, "pages")?,
        doc.page_count().map_err(pdf_error)?,
    )?;
    targets.sort_unstable();
    targets.dedup();
    let at = targets
        .first()
        .copied()
        .ok_or_else(|| GoatError::exception("IndexError", "list index out of range"))?;
    for &index in targets.iter().rev() {
        doc.remove_page(index).map_err(pdf_error)?;
    }
    insert_donor(&mut doc, &donor, dest::to_i64(at))?;
    open::save_qpdf(&doc, &out)?;
    let mut map = result(
        "pages-replace",
        vec![display(&src), display(&other)],
        vec![display(&out)],
    );
    map.insert("replaced".into(), one_based(&targets));
    Ok(map)
}

/// Import once to preserve links between donor pages; then place each in the order
/// pikepdf's repeated `pages.insert(at + offset, page)` uses, including negative indices.
fn insert_donor(doc: &mut Document, donor: &Document, at: i64) -> Result<(), GoatError> {
    let count = doc.page_count().map_err(pdf_error)?;
    let pages: Vec<usize> = (0..donor.page_count().map_err(pdf_error)?).collect();
    if !pages.is_empty() && (at < -dest::to_i64(count) || at > dest::to_i64(count)) {
        return Err(GoatError::exception(
            "IndexError",
            "Accessing nonexistent PDF page number",
        ));
    }
    let imported = doc.import_pages(donor, &pages, count).map_err(pdf_error)?;
    for (offset, id) in imported.into_iter().enumerate() {
        let signed = at.saturating_add(dest::to_i64(offset));
        let target = if signed < 0 {
            dest::to_i64(count + offset).saturating_add(signed)
        } else {
            signed
        };
        let target = usize::try_from(target).map_err(|_| {
            GoatError::exception("IndexError", "Accessing nonexistent PDF page number")
        })?;
        let current = doc
            .page_index(id)
            .map_err(pdf_error)?
            .ok_or_else(|| GoatError::message("imported page not found"))?;
        doc.move_page(current, target).map_err(pdf_error)?;
    }
    Ok(())
}
