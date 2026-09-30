//! Listing, replacing, and clearing the outline.

use clap::{Arg, ArgMatches, Command};
use goat_common::args::required;
use goat_common::paths;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, ObjRef, Object};
use serde_json::{Map, Value, json};

use crate::dest::to_i64;
use crate::open::{self, Lib, display, out_path, pdf_error, result, source};
use crate::outline;

type Output = Result<Map<String, Value>, GoatError>;

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("bookmarks")
                .about("list document outline entries")
                .arg(Arg::new("file").required(true)),
            get,
        ),
    );
    registry.family_verb(
        "bookmarks",
        Verb::new(
            Command::new("set")
                .about("set outline from JSON [{level,title,page}]")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("data").long("data").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            set,
        ),
    );
    registry.family_verb(
        "bookmarks",
        Verb::new(
            Command::new("clear")
                .about("remove the document outline")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            clear,
        ),
    );
}

fn get(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let doc = open::load(&src, Lib::PyMuPdf)?;
    if doc.needs_password() {
        return Err(open::still_encrypted());
    }
    let rows: Vec<Value> = outline::toc(&doc)?
        .into_iter()
        .map(|row| json!({"level": row.level, "title": row.title, "page": row.page}))
        .collect();
    let mut map = result("get-bookmarks", vec![display(&src)], vec![]);
    map.insert("count".into(), rows.len().into());
    map.insert("bookmarks".into(), rows.into());
    Ok(map)
}

fn clear(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "unbookmarked")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    if doc.needs_password() {
        return Err(open::still_encrypted());
    }
    let removed = clear_outline(&mut doc)?;
    open::save_mupdf(&doc, &out)?;
    let mut map = result("bookmarks-clear", vec![display(&src)], vec![display(&out)]);
    map.insert("removed".into(), removed.into());
    Ok(map)
}

fn clear_outline(doc: &mut Document) -> Result<usize, GoatError> {
    let old = outline::toc(doc)?;
    for item in &old {
        if let Some(id) = item.id {
            doc.delete(id);
        }
    }
    let mut catalog = doc.catalog().map_err(pdf_error)?;
    if let Some(Object::Reference(root)) = catalog.remove(b"Outlines") {
        doc.delete(root);
    }
    let root = doc.catalog_ref().map_err(pdf_error)?;
    doc.set(root, catalog);
    Ok(old.len())
}

struct Row {
    level: usize,
    title: String,
    page: i64,
}

fn key<'a>(row: &'a Value, key: &str) -> Result<&'a Value, GoatError> {
    row.get(key)
        .ok_or_else(|| GoatError::exception("KeyError", format!("'{key}'")))
}

fn set(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "outlined")?;
    let path = paths::expanduser(required::<String>(matches, "data")?);
    let text = std::fs::read_to_string(&path).map_err(|error| GoatError::os(&error, &path))?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|error| GoatError::exception("JSONDecodeError", error.to_string()))?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let items = data
        .as_array()
        .ok_or_else(|| GoatError::exception("TypeError", "'NoneType' object is not iterable"))?;
    let mut rows = Vec::with_capacity(items.len().min(100_000));
    if items.len() > 100_000 {
        return Err(GoatError::message("outline exceeds 100000 entries"));
    }
    for item in items {
        let level = key(item, "level")?.as_i64().unwrap_or(0);
        let title = key(item, "title")?
            .as_str()
            .ok_or_else(|| GoatError::exception("TypeError", "title must be a string"))?;
        let page = key(item, "page")?
            .as_i64()
            .ok_or_else(|| GoatError::value_error("bad page number"))?;
        rows.push(Row {
            level: usize::try_from(level).unwrap_or(0),
            title: title.to_owned(),
            page,
        });
    }
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let count = doc.page_count().map_err(pdf_error)?;
    if rows.first().is_some_and(|row| row.level != 1) {
        return Err(GoatError::value_error(
            "hierarchy level of item 0 must be 1",
        ));
    }
    // set_toc validates each row against its successor, not the final row.
    for (i, pair) in rows.windows(2).enumerate() {
        if pair[0].page < -1 || pair[0].page > to_i64(count) {
            return Err(GoatError::value_error(format!(
                "row {i}: page number out of range"
            )));
        }
        if pair[1].level < 1 || pair[1].level > pair[0].level + 1 {
            return Err(GoatError::value_error(format!(
                "bad hierarchy level in row {}",
                i + 1
            )));
        }
    }
    write_outline(&mut doc, &rows)?;
    open::save_mupdf(&doc, &out)?;
    let mut map = result("bookmarks-set", vec![display(&src)], vec![display(&out)]);
    map.insert("count".into(), rows.len().into());
    Ok(map)
}

fn write_outline(doc: &mut Document, rows: &[Row]) -> Result<(), GoatError> {
    clear_outline(doc)?;
    if rows.is_empty() {
        return Ok(());
    }
    let page_refs = doc.page_refs().map_err(pdf_error)?;
    let root = doc.add(Dict::new());
    let ids: Vec<ObjRef> = rows.iter().map(|_| doc.add(Dict::new())).collect();
    let mut dicts = vec![Dict::new(); rows.len() + 1];
    let mut stack = vec![0usize];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); dicts.len()];
    for (i, row) in rows.iter().enumerate() {
        while stack.len() > row.level {
            stack.pop();
        }
        let parent = *stack
            .last()
            .ok_or_else(|| GoatError::value_error("bad hierarchy level"))?;
        children[parent].push(i + 1);
        let mut dict = Dict::new();
        if row.page >= 0 && !page_refs.is_empty() {
            let target = usize::try_from(row.page.saturating_sub(1).max(0))
                .unwrap_or(0)
                .min(page_refs.len() - 1);
            let height = doc.page(target).map_err(pdf_error)?.crop_box().height();
            let mut action = Dict::new();
            action.insert("S", Object::name("GoTo"));
            action.insert(
                "D",
                Object::Array(vec![
                    Object::Reference(page_refs[target]),
                    Object::name("XYZ"),
                    Object::Integer(72),
                    Object::Real(height - 36.0),
                    Object::Integer(0),
                ]),
            );
            dict.insert("A", action);
        }
        dict.insert(
            "Parent",
            Object::Reference(if parent == 0 { root } else { ids[parent - 1] }),
        );
        dict.insert("Title", Object::text(&row.title));
        dicts[i + 1] = dict;
        stack.push(i + 1);
    }
    dicts[0].insert("Type", Object::name("Outlines"));
    for (parent, siblings) in children.iter().enumerate() {
        if let (Some(&first), Some(&last)) = (siblings.first(), siblings.last()) {
            dicts[parent].insert("First", Object::Reference(ids[first - 1]));
            dicts[parent].insert("Last", Object::Reference(ids[last - 1]));
            let count = to_i64(siblings.len());
            dicts[parent].insert(
                "Count",
                Object::Integer(if parent == 0 { count } else { -count }),
            );
        }
        for pair in siblings.windows(2) {
            dicts[pair[0]].insert("Next", Object::Reference(ids[pair[1] - 1]));
            dicts[pair[1]].insert("Prev", Object::Reference(ids[pair[0] - 1]));
        }
    }
    let mut values = dicts.into_iter();
    if let Some(dict) = values.next() {
        doc.set(root, dict);
    }
    for (id, dict) in ids.into_iter().zip(values) {
        doc.set(id, dict);
    }
    let mut catalog = doc.catalog().map_err(pdf_error)?;
    catalog.insert("Outlines", Object::Reference(root));
    doc.set(doc.catalog_ref().map_err(pdf_error)?, catalog);
    Ok(())
}
