//! `compare structure`: what a text diff cannot see, for the document and each page.

use std::collections::BTreeMap;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::required;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Object};
use serde_json::{Map, Value, json};

use crate::doc;

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "compare",
        Verb::new(
            Command::new("structure")
                .about("compare annotations, images, form fields, and document objects")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("other").required(true)),
            compare_structure,
        ),
    );
}

/// `widget.field_name`: the fully qualified name through the `/Parent` chain.
fn field_name(document: &Document, widget: &Dict) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut current = widget.clone();
    for _ in 0..64 {
        if let Some(Object::String(text)) = current.get(b"T").and_then(|t| document.resolve(t).ok())
        {
            parts.push(text.to_text());
        }
        match current
            .get(b"Parent")
            .and_then(|parent| document.resolve_dict(parent).ok().flatten())
        {
            Some(parent) => current = parent,
            None => break,
        }
    }
    parts.reverse();
    parts.join(".")
}

/// The document summary and one entry per page.
type Structure = (Map<String, Value>, Vec<Map<String, Value>>);

/// `_structure(doc)`: the document summary and one entry per page.
fn structure(document: &Document) -> Result<Structure, GoatError> {
    let mut field_names: Vec<String> = Vec::new();
    let mut pages = Vec::new();
    for page in doc::pages(document)? {
        let annots = doc::page_annots(document, &page);
        for widget in &annots.widgets {
            let name = field_name(document, &widget.dict);
            if !field_names.contains(&name) {
                field_names.push(name);
            }
        }
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for annot in &annots.annotations {
            *counts.entry(annot.subtype()).or_insert(0) += 1;
        }
        let (width, height) = doc::page_size(&page);
        let mut entry = Map::new();
        entry.insert(
            "size".to_owned(),
            json!([doc::round1(width), doc::round1(height)]),
        );
        entry.insert("rotation".to_owned(), json!(doc::rotation(&page)));
        entry.insert("annotations".to_owned(), json!(counts));
        entry.insert("widgets".to_owned(), json!(annots.widgets.len()));
        entry.insert(
            "images".to_owned(),
            json!(doc::page_images(document, &page).len()),
        );
        pages.push(entry);
    }
    field_names.sort();
    let metadata: Map<String, Value> = doc::metadata(document).into_iter().collect();
    let pick = |key: &str| metadata.get(key).cloned().unwrap_or(Value::Null);
    let mut summary = Map::new();
    summary.insert("page_count".to_owned(), json!(pages.len()));
    summary.insert(
        "object_count".to_owned(),
        json!(document.xref_size().saturating_sub(1)),
    );
    summary.insert(
        "attachment_count".to_owned(),
        json!(doc::embedded_files(document)?.len()),
    );
    summary.insert("field_names".to_owned(), json!(field_names));
    summary.insert("encryption".to_owned(), pick("encryption"));
    summary.insert("producer".to_owned(), pick("producer"));
    summary.insert("creation_date".to_owned(), pick("creationDate"));
    summary.insert("modification_date".to_owned(), pick("modDate"));
    Ok((summary, pages))
}

/// `_structure_changes`: `{key: {"file": a, "other": b}}` for every differing key.
fn changes(
    file_side: Option<&Map<String, Value>>,
    other_side: Option<&Map<String, Value>>,
    keys: &[String],
) -> Map<String, Value> {
    let mut out = Map::new();
    for key in keys {
        let a = file_side
            .and_then(|side| side.get(key))
            .cloned()
            .unwrap_or(Value::Null);
        let b = other_side
            .and_then(|side| side.get(key))
            .cloned()
            .unwrap_or(Value::Null);
        if a == b {
            continue;
        }
        let (a, b) = if key == "field_names" {
            let names = |value: &Value| -> Vec<String> {
                value
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let (left, right) = (names(&a), names(&b));
            let only_left: Vec<&String> =
                left.iter().filter(|name| !right.contains(name)).collect();
            let only_right: Vec<&String> =
                right.iter().filter(|name| !left.contains(name)).collect();
            (json!(only_left), json!(only_right))
        } else {
            (a, b)
        };
        out.insert(key.clone(), json!({"file": a, "other": b}));
    }
    out
}

fn compare_structure(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let left = doc::open_unlocked(required::<String>(matches, "file")?, "compare structure")?;
    let right = doc::open_unlocked(required::<String>(matches, "other")?, "compare structure")?;
    let (left_document, left_pages) = structure(&left.doc)?;
    let (right_document, right_pages) = structure(&right.doc)?;
    let document_keys: Vec<String> = left_document.keys().cloned().collect();
    let document = changes(Some(&left_document), Some(&right_document), &document_keys);
    let mut pages = Vec::new();
    for index in 0..left_pages.len().max(right_pages.len()) {
        let file_page = left_pages.get(index);
        let other_page = right_pages.get(index);
        let keys: Vec<String> = file_page
            .or(other_page)
            .map(|page| page.keys().cloned().collect())
            .unwrap_or_default();
        let page_changes = changes(file_page, other_page, &keys);
        if !page_changes.is_empty() {
            pages.push(json!({"page": index + 1, "changes": page_changes}));
        }
    }
    let mut result = doc::result(
        "compare-structure",
        vec![left.display(), right.display()],
        Vec::new(),
    );
    result.insert(
        "identical".to_owned(),
        Value::Bool(document.is_empty() && pages.is_empty()),
    );
    result.insert("document".to_owned(), Value::Object(document));
    result.insert("pages".to_owned(), Value::Array(pages));
    Ok(result)
}
