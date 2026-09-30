//! Whole-document page structure: `merge`, `split`, `extract`, `delete`, `reorder`, and
//! `rotate`.

use std::collections::HashSet;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{int_value, many, optional, required};
use goat_common::parse::parse_pages;
use goat_common::paths;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, ObjRef, Object};
use serde_json::{Map, Value};

use crate::dest;
use crate::open::{
    self, Lib, display, one_based, out_path, pdf_error, result, save_atomic, source,
};
use crate::outline;

/// Deepest page tree walked for an inherited `/Rotate`.
const MAX_TREE_DEPTH: usize = 64;

type Output = Result<Map<String, Value>, GoatError>;

pub(crate) fn register(registry: &mut Registry) {
    registry.command(Verb::new(
        Command::new("merge")
            .about("concatenate PDFs")
            .arg(Arg::new("files").required(true).num_args(1..))
            .arg(Arg::new("output").short('o').long("output")),
        merge,
    ));
    registry.command(Verb::new(
        Command::new("split")
            .about("split a PDF into chunks")
            .arg(Arg::new("file").required(true))
            .arg(
                Arg::new("every")
                    .long("every")
                    .value_parser(int_value)
                    .default_value("1")
                    .help("pages per chunk"),
            )
            .arg(Arg::new("outdir").short('o').long("outdir")),
        split,
    ));
    registry.command(Verb::new(
        Command::new("extract")
            .about("extract pages, e.g. 2-5,9")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("pages").long("pages").required(true))
            .arg(Arg::new("output").short('o').long("output")),
        extract,
    ));
    registry.command(Verb::new(
        Command::new("delete")
            .about("delete pages")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("pages").long("pages").required(true))
            .arg(Arg::new("output").short('o').long("output")),
        delete,
    ));
    registry.command(Verb::new(
        Command::new("reorder")
            .about(
                "reorder pages, e.g. --order 3,1,2; a partial list moves those pages first and the rest \
                 follow in their original order",
            )
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("order").long("order").required(true).help(
                "pages in the wanted order, e.g. 3,1,2; a partial list moves the named pages first and every \
                 unnamed page follows once, in its original relative order",
            ))
            .arg(Arg::new("output").short('o').long("output")),
        reorder,
    ));
    registry.command(Verb::new(
        Command::new("rotate")
            .about("rotate pages by a multiple of 90")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("pages").long("pages").help("default: all pages"))
            .arg(
                Arg::new("deg")
                    .long("deg")
                    .value_parser(int_value)
                    .required(true),
            )
            .arg(Arg::new("output").short('o').long("output")),
        rotate,
    ));
}

/// `merge`: every page of every input, in order, into a new document (pikepdf
/// `pages.extend`): the inputs' outlines and forms stay behind, links between copied
/// pages follow them.
fn merge(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let sources = many::<String>(matches, "files")?
        .into_iter()
        .map(|file| paths::resolve(file))
        .collect::<Result<Vec<_>, _>>()?;
    let requested = optional::<String>(matches, "output")?.filter(|path| !path.is_empty());
    let out = paths::ensure_parent(requested.map_or("merged.pdf", String::as_str))?;
    let mut merged = Document::new();
    let mut total = 0;
    for src in &sources {
        let pdf = open::load(src, Lib::PikePdf)?;
        let pages: Vec<usize> = (0..pdf.page_count().map_err(pdf_error)?).collect();
        let at = merged.page_count().map_err(pdf_error)?;
        merged.import_pages(&pdf, &pages, at).map_err(pdf_error)?;
        total += pages.len();
    }
    open::save_qpdf_new(&merged, &out)?;
    let inputs = sources.iter().map(|src| display(src)).collect();
    let mut map = result("merge", inputs, vec![display(&out)]);
    map.insert("merged_pages".to_owned(), Value::from(total));
    Ok(map)
}

/// `split`: chunks of `--every` pages (at least one) as `{stem}_{NNN}.pdf` in
/// `--outdir`, `{stem}_split` under the working directory by default.
fn split(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let every = (*required::<i64>(matches, "every")?).max(1);
    let requested = optional::<String>(matches, "outdir")?.map(String::as_str);
    let outdir = paths::out_dir(requested, &display(&src), "split")?;
    let pdf = open::load(&src, Lib::PikePdf)?;
    let count = pdf.page_count().map_err(pdf_error)?;
    let step = usize::try_from(every).unwrap_or(usize::MAX);
    let stem = paths::stem(&display(&src)).to_owned();
    let mut outputs = Vec::new();
    for (part, start) in (0..count).step_by(step).enumerate() {
        let pages: Vec<usize> = (start..count.min(start.saturating_add(step))).collect();
        let mut chunk = Document::new();
        chunk.import_pages(&pdf, &pages, 0).map_err(pdf_error)?;
        let out = outdir.join(format!("{stem}_{:03}.pdf", part + 1));
        save_atomic(&out, |partial| open::save_qpdf_new(&chunk, partial))?;
        outputs.push(display(&out));
    }
    let parts = outputs.len();
    let mut map = result("split", vec![display(&src)], outputs);
    map.insert("every".to_owned(), Value::from(every));
    map.insert("parts".to_owned(), Value::from(parts));
    Ok(map)
}

/// `extract`: the `--pages` selection, in the order and with the repeats given, as a
/// new document.
fn extract(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "extract")?;
    let pdf = open::load(&src, Lib::PikePdf)?;
    let count = pdf.page_count().map_err(pdf_error)?;
    let pages = parse_pages(required::<String>(matches, "pages")?, count)?;
    let mut extracted = Document::new();
    extracted.import_pages(&pdf, &pages, 0).map_err(pdf_error)?;
    save_atomic(&out, |partial| open::save_qpdf_new(&extracted, partial))?;
    let mut map = result("extract", vec![display(&src)], vec![display(&out)]);
    map.insert("pages".to_owned(), one_based(&pages));
    Ok(map)
}

/// `delete`: remove the `--pages` selection; removing every page is refused.
fn delete(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "deleted")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let count = doc.page_count().map_err(pdf_error)?;
    let mut dropped = parse_pages(required::<String>(matches, "pages")?, count)?;
    dropped.sort_unstable();
    dropped.dedup();
    if dropped.len() == count {
        return Err(GoatError::message("--pages would remove every page"));
    }
    if !dropped.is_empty() {
        if doc.needs_password() {
            return Err(open::still_encrypted());
        }
        delete_pages(&mut doc, &dropped)?;
    }
    save_atomic(&out, |partial| open::save_mupdf(&doc, partial))?;
    let mut map = result("delete", vec![display(&src)], vec![display(&out)]);
    map.insert("deleted_pages".to_owned(), one_based(&dropped));
    map.insert(
        "remaining_pages".to_owned(),
        Value::from(count - dropped.len()),
    );
    Ok(map)
}

/// PyMuPDF `delete_pages`: outline items that lead to a dropped page point nowhere,
/// links on the kept pages that lead to one go, then the pages go, last first.
fn delete_pages(doc: &mut Document, dropped: &[usize]) -> Result<(), GoatError> {
    let targets: HashSet<i64> = dropped.iter().map(|&index| dest::to_i64(index)).collect();
    for item in outline::toc(doc)? {
        if let Some(id) = item.id
            && targets.contains(&(item.page - 1))
        {
            outline::disable_item(doc, id)?;
        }
    }
    remove_links_to(doc, &targets)?;
    for &index in dropped.iter().rev() {
        doc.remove_page(index).map_err(pdf_error)?;
    }
    Ok(())
}

/// `_remove_links_to`: link annotations on the kept pages whose target is a dropped page
/// leave `/Annots`; the garbage-collecting save drops their objects.
fn remove_links_to(doc: &mut Document, targets: &HashSet<i64>) -> Result<(), GoatError> {
    let pages = doc.page_refs().map_err(pdf_error)?;
    for (index, page_ref) in pages.into_iter().enumerate() {
        if targets.contains(&dest::to_i64(index)) {
            continue;
        }
        let Some(mut page) = doc.get(page_ref).map_err(pdf_error)?.as_dict().cloned() else {
            continue;
        };
        let Some(value) = page.get(b"Annots").cloned() else {
            continue;
        };
        let Some(mut annots) = doc.resolve_array(&value).map_err(pdf_error)? else {
            continue;
        };
        let before = annots.len();
        for position in (0..annots.len()).rev() {
            if link_page(doc, &annots[position])?.is_some_and(|target| targets.contains(&target)) {
                annots.remove(position);
            }
        }
        if annots.len() == before {
            continue;
        }
        match value {
            Object::Reference(id) => doc.set(id, Object::Array(annots)),
            _ => {
                page.insert("Annots", Object::Array(annots));
                doc.set(page_ref, Object::Dict(page));
            }
        }
    }
    Ok(())
}

/// The page `_remove_dest_range` reads from a link annotation: a GoTo action's `/D` (any
/// other action keeps the link), else `/Dest`. An array names a page object; a string is
/// resolved as a link URI.
fn link_page(doc: &Document, annot: &Object) -> Result<Option<i64>, GoatError> {
    let Some(annot) = doc.resolve_dict(annot).map_err(pdf_error)? else {
        return Ok(None);
    };
    let subtype = doc.resolve_key(&annot, b"Subtype").map_err(pdf_error)?;
    if subtype.as_name() != Some(b"Link".as_slice()) {
        return Ok(None);
    }
    let dest = match annot.get(b"A") {
        Some(action) => {
            let Some(action) = doc.resolve_dict(action).map_err(pdf_error)? else {
                return Ok(None);
            };
            let kind = doc.resolve_key(&action, b"S").map_err(pdf_error)?;
            if kind.as_name() != Some(b"GoTo".as_slice()) {
                return Ok(None);
            }
            doc.resolve_key(&action, b"D").map_err(pdf_error)?
        }
        None => doc.resolve_key(&annot, b"Dest").map_err(pdf_error)?,
    };
    let page = match &dest {
        Object::Array(items) => dest::page_number(doc, items.first())?,
        Object::String(text) => dest::uri_page(doc, &text.to_text())?,
        _ => -1,
    };
    Ok((page >= 0).then_some(page))
}

/// `reorder`: the `--order` pages first, then every other page in its original order.
fn reorder(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "reordered")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let count = doc.page_count().map_err(pdf_error)?;
    let named = parse_pages(required::<String>(matches, "order")?, count)?;
    if named.is_empty() {
        return Err(GoatError::message("--order must name at least one page"));
    }
    let unique: HashSet<usize> = named.iter().copied().collect();
    if unique.len() != named.len() {
        return Err(GoatError::message(
            "--order must not name the same page twice",
        ));
    }
    let mut order = named;
    order.extend((0..count).filter(|index| !unique.contains(index)));
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    permute(&mut doc, &order)?;
    crate::select::cleanup(&mut doc)?;
    save_atomic(&out, |partial| open::save_mupdf(&doc, partial))?;
    let mut map = result("reorder", vec![display(&src)], vec![display(&out)]);
    map.insert("order".to_owned(), one_based(&order));
    Ok(map)
}

/// Moves the pages into `order`, a permutation of every page index. Outline items and
/// links keep pointing at the same page objects.
fn permute(doc: &mut Document, order: &[usize]) -> Result<(), GoatError> {
    let pages = doc.page_refs().map_err(pdf_error)?;
    for (position, &index) in order.iter().enumerate() {
        let Some(&wanted) = pages.get(index) else {
            continue;
        };
        if let Some(current) = doc.page_index(wanted).map_err(pdf_error)?
            && current != position
        {
            doc.move_page(current, position).map_err(pdf_error)?;
        }
    }
    Ok(())
}

/// `rotate`: turn the `--pages` selection (every page when absent or empty) by `--deg`
/// relative to its current rotation.
fn rotate(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let deg = *required::<i64>(matches, "deg")?;
    if deg % 90 != 0 {
        return Err(GoatError::message("--deg must be a multiple of 90"));
    }
    let src = source(matches)?;
    let out = out_path(matches, &src, "rotated")?;
    let mut pdf = open::load(&src, Lib::PikePdf)?;
    let pages = pdf.page_refs().map_err(pdf_error)?;
    let mut selected = match optional::<String>(matches, "pages")?.filter(|spec| !spec.is_empty()) {
        Some(spec) => parse_pages(spec, pages.len())?,
        None => (0..pages.len()).collect(),
    };
    selected.sort_unstable();
    selected.dedup();
    for &index in &selected {
        if let Some(&id) = pages.get(index) {
            rotate_page(&mut pdf, id, deg)?;
        }
    }
    open::save_qpdf(&pdf, &out)?;
    let mut map = result("rotate", vec![display(&src)], vec![display(&out)]);
    map.insert("rotated_pages".to_owned(), one_based(&selected));
    map.insert("deg".to_owned(), Value::from(deg));
    Ok(map)
}

/// QPDF `rotatePage(angle, relative=true)`: add the angle to the page's own or inherited
/// integer `/Rotate` (a value that is not a multiple of 90 counts as 0) and store
/// `(sum + 360) % 360` on the page. The C remainder keeps its sign, so -450 on an
/// unrotated page stores -90.
fn rotate_page(doc: &mut Document, id: ObjRef, angle: i64) -> Result<(), GoatError> {
    let Some(mut page) = doc.get(id).map_err(pdf_error)?.as_dict().cloned() else {
        return Ok(());
    };
    let old = inherited_rotate(doc, &page)?;
    let old = if old % 90 == 0 { old } else { 0 };
    page.insert(
        "Rotate",
        Object::Integer(angle.saturating_add(old).saturating_add(360) % 360),
    );
    doc.set(id, Object::Dict(page));
    Ok(())
}

/// The first integer `/Rotate` on the page or up its `/Parent` chain, read as a C `int`
/// (a value outside its range clamps to one that is not a multiple of 90).
fn inherited_rotate(doc: &Document, page: &Dict) -> Result<i64, GoatError> {
    let mut node = page.clone();
    for _ in 0..MAX_TREE_DEPTH {
        if let Object::Integer(value) = doc.resolve_key(&node, b"Rotate").map_err(pdf_error)? {
            return Ok(i32::try_from(value).map_or(0, i64::from));
        }
        match doc.resolve_key(&node, b"Parent").map_err(pdf_error)? {
            Object::Dict(parent) => node = parent,
            _ => break,
        }
    }
    Ok(0)
}
