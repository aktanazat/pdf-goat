//! `info`, `inspect`, and `preflight`: read-only document summaries.

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{int_value, required};
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::Document;
use serde_json::{Map, Value, json};

use crate::doc::{self, Lib, LinkKind};

pub(crate) fn register(registry: &mut Registry) {
    registry.command(Verb::new(
        Command::new("info")
            .about("show document metadata and page summary")
            .arg(Arg::new("file").required(true)),
        info,
    ));
    registry.command(Verb::new(
        Command::new("inspect")
            .about("list page sizes and content counts")
            .arg(Arg::new("file").required(true))
            .arg(
                Arg::new("start_page")
                    .long("start-page")
                    .value_parser(int_value)
                    .default_value("1"),
            )
            .arg(
                Arg::new("limit")
                    .long("limit")
                    .value_parser(int_value)
                    .default_value("25"),
            ),
        inspect,
    ));
    registry.command(Verb::new(
        Command::new("preflight")
            .about("inspect active content, links, forms, attachments, and basic accessibility signals")
            .arg(Arg::new("file").required(true)),
        preflight,
    ));
}

fn info(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open(required::<String>(matches, "file")?, Lib::PyMuPdf)?;
    let document = &opened.doc;
    let locked = document.needs_password();
    let pages = doc::pages(document)?;
    let mut has_text = false;
    let mut sizes: Vec<(f64, f64)> = Vec::with_capacity(pages.len());
    for page in &pages {
        let (width, height) = doc::page_size(page);
        sizes.push((doc::round1(width), doc::round1(height)));
        if !has_text && !doc::page_text(document, page.index)?.0.trim().is_empty() {
            has_text = true;
        }
    }
    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sizes.dedup();
    let file_size = std::fs::metadata(&opened.path)
        .map_err(|error| GoatError::os(&error, &opened.path))?
        .len();
    let field_count = doc::form_field_count(document);
    let mut result = doc::result("info", opened.inputs(), Vec::new());
    result.insert("pages".to_owned(), json!(pages.len()));
    result.insert("encrypted".to_owned(), Value::Bool(locked));
    result.insert("needs_password".to_owned(), Value::Bool(locked));
    result.insert(
        "permissions".to_owned(),
        Value::Object(doc::permissions(doc::permission_flags(document))),
    );
    result.insert("has_forms".to_owned(), Value::Bool(field_count > 0));
    result.insert("form_field_count".to_owned(), json!(field_count));
    result.insert("xfa".to_owned(), Value::Bool(doc::has_xfa(document)));
    result.insert("layer_count".to_owned(), json!(doc::layer_count(document)));
    result.insert("has_text".to_owned(), Value::Bool(has_text));
    result.insert(
        "page_sizes_pt".to_owned(),
        Value::Array(
            sizes
                .iter()
                .map(|(width, height)| json!({"width": width, "height": height}))
                .collect(),
        ),
    );
    result.insert("file_size_bytes".to_owned(), json!(file_size));
    result.insert(
        "metadata".to_owned(),
        Value::Object(doc::truthy(doc::metadata(document))),
    );
    Ok(result)
}

/// `_page_inventory(page, index)`.
fn inventory(
    document: &Document,
    page: &pdf_core::Page,
    rules: &[(i64, pdf_core::Dict)],
) -> Result<Value, GoatError> {
    let (text, words) = doc::page_text(document, page.index)?;
    let (width, height) = doc::page_size(page);
    let annots = doc::page_annots(document, page);
    Ok(json!({
        "page": page.index + 1,
        "label": doc::page_label(document, rules, page.index),
        "width_pt": doc::round1(width),
        "height_pt": doc::round1(height),
        "rotation": doc::rotation(page),
        "text_chars": text.chars().count(),
        "word_count": words,
        "image_count": doc::page_images(document, page).len(),
        "link_count": annots.links.len(),
        "annotation_count": annots.annotations.len(),
        "form_field_count": annots.widgets.len(),
    }))
}

fn inspect(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_unlocked(required::<String>(matches, "file")?, "inspect")?;
    let document = &opened.doc;
    let pages = doc::pages(document)?;
    let page_count = pages.len();
    let start_page = *required::<i64>(matches, "start_page")?;
    let limit = *required::<i64>(matches, "limit")?;
    if start_page < 1
        || usize::try_from(start_page).is_ok_and(|start| start > page_count)
        || page_count == 0
    {
        return Err(GoatError::message(format!(
            "--start-page must be between 1 and {page_count}"
        )));
    }
    if !(1..=100).contains(&limit) {
        return Err(GoatError::message("--limit must be between 1 and 100"));
    }
    let start = usize::try_from(start_page - 1).unwrap_or(0);
    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(100))
        .min(page_count);
    let rules = doc::page_label_rules(document);
    let inventories = pages[start..end]
        .iter()
        .map(|page| inventory(document, page, &rules))
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = doc::result("inspect", opened.inputs(), Vec::new());
    result.insert("total_pages".to_owned(), json!(page_count));
    result.insert("start_page".to_owned(), json!(start_page));
    result.insert("pages".to_owned(), Value::Array(inventories));
    result.insert(
        "next_page".to_owned(),
        if end < page_count {
            json!(end + 1)
        } else {
            Value::Null
        },
    );
    result.insert("truncated".to_owned(), Value::Bool(end < page_count));
    Ok(result)
}

/// The catalog is tagged: a structure tree, or `/MarkInfo /Marked true`.
pub(crate) fn is_tagged(document: &Document) -> bool {
    let Ok(catalog) = document.catalog() else {
        return false;
    };
    if catalog.contains_key(b"StructTreeRoot") {
        return true;
    }
    catalog
        .get(b"MarkInfo")
        .and_then(|info| document.resolve_dict(info).ok().flatten())
        .and_then(|info| {
            info.get(b"Marked")
                .and_then(|marked| document.resolve(marked).ok())
        })
        .and_then(|marked| marked.as_bool())
        .unwrap_or(false)
}

/// `str(Root.Lang)` when the catalog declares a language.
pub(crate) fn language(document: &Document) -> Option<String> {
    let catalog = document.catalog().ok()?;
    match document.resolve(catalog.get(b"Lang")?).ok()? {
        pdf_core::Object::String(text) => Some(text.to_text()),
        pdf_core::Object::Name(name) => Some(String::from_utf8_lossy(name.as_bytes()).into_owned()),
        pdf_core::Object::Null => None,
        other => Some(other.to_pretty_string()),
    }
}

fn finding(code: &str, severity: &str, message: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("code".to_owned(), Value::String(code.to_owned()));
    map.insert("severity".to_owned(), Value::String(severity.to_owned()));
    map.insert("message".to_owned(), Value::String(message.to_owned()));
    map
}

fn counted(code: &str, severity: &str, message: &str, count: u64) -> Value {
    let mut map = finding(code, severity, message);
    map.insert("count".to_owned(), json!(count));
    Value::Object(map)
}

fn preflight(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open(required::<String>(matches, "file")?, Lib::PyMuPdf)?;
    let document = &opened.doc;
    let mut result = doc::result("preflight", opened.inputs(), Vec::new());
    if document.needs_password() {
        result.insert("risk".to_owned(), Value::String("unknown".to_owned()));
        result.insert("encrypted".to_owned(), Value::Bool(true));
        result.insert("needs_password".to_owned(), Value::Bool(true));
        result.insert(
            "findings".to_owned(),
            Value::Array(vec![Value::Object(finding(
                "encrypted",
                "info",
                "A password is required before content checks can run.",
            ))]),
        );
        return Ok(result);
    }
    let pages = doc::pages(document)?;
    let (mut annotations, mut form_fields, mut external, mut unsafe_links) =
        (0u64, 0u64, 0u64, 0u64);
    let mut empty_pages: Vec<usize> = Vec::new();
    for page in &pages {
        let annots = doc::page_annots(document, page);
        annotations += annots.annotations.len() as u64;
        form_fields += annots.widgets.len() as u64;
        for link in annots.links.iter().filter(|link| link.is_external()) {
            external += 1;
            if LinkKind::is_unsafe(link) {
                unsafe_links += 1;
            }
        }
        if doc::page_fonts(document, page).is_empty() && doc::page_images(document, page).is_empty()
        {
            empty_pages.push(page.index + 1);
        }
    }
    let attachment_count = (doc::embedded_files(document)?.len()
        + doc::file_attachment_annotations(document, &pages).len())
        as u64;
    let structure = doc::structure(document);
    let title = doc::metadata(document)
        .into_iter()
        .find(|(key, _)| key == "title")
        .and_then(|(_, value)| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let active = structure.active_content();

    let mut findings = Vec::new();
    if active > 0 {
        findings.push(counted(
            "active_content",
            "warning",
            "The PDF contains document-open actions, JavaScript actions, or launch actions.",
            active,
        ));
    }
    if unsafe_links > 0 {
        findings.push(counted(
            "unsafe_links",
            "warning",
            "The PDF contains links with schemes other than HTTP, HTTPS, or mailto.",
            unsafe_links,
        ));
    }
    if attachment_count > 0 {
        findings.push(counted(
            "attachments",
            "info",
            "The PDF contains embedded or attached files.",
            attachment_count,
        ));
    }
    if structure.xfa {
        findings.push(Value::Object(finding(
            "xfa",
            "warning",
            "The PDF contains an unsupported XFA form.",
        )));
    }
    if !is_tagged(document) {
        findings.push(Value::Object(finding(
            "untagged",
            "info",
            "The PDF has no tag tree.",
        )));
    }
    if title.is_empty() {
        findings.push(Value::Object(finding(
            "missing_title",
            "info",
            "The PDF has no title metadata.",
        )));
    }
    if language(document).is_none() {
        findings.push(Value::Object(finding(
            "missing_language",
            "info",
            "The PDF has no document language.",
        )));
    }
    if !empty_pages.is_empty() {
        let mut map = finding(
            "empty_pages",
            "info",
            "Some pages declare no font and no image.",
        );
        map.insert(
            "pages".to_owned(),
            json!(empty_pages.iter().take(100).collect::<Vec<_>>()),
        );
        map.insert("count".to_owned(), json!(empty_pages.len()));
        findings.push(Value::Object(map));
    }
    let risk = if active > 0 || unsafe_links > 0 {
        "high"
    } else if attachment_count > 0 || structure.xfa {
        "medium"
    } else {
        "low"
    };
    result.insert("risk".to_owned(), Value::String(risk.to_owned()));
    result.insert("encrypted".to_owned(), Value::Bool(false));
    result.insert("needs_password".to_owned(), Value::Bool(false));
    result.insert("pages".to_owned(), json!(pages.len()));
    result.insert("annotations".to_owned(), json!(annotations));
    result.insert("form_fields".to_owned(), json!(form_fields));
    result.insert("signatures".to_owned(), json!(structure.signatures));
    result.insert("external_links".to_owned(), json!(external));
    result.insert("unsafe_links".to_owned(), json!(unsafe_links));
    result.insert("attachments".to_owned(), json!(attachment_count));
    result.insert("javascript_actions".to_owned(), json!(structure.javascript));
    result.insert("launch_actions".to_owned(), json!(structure.launch));
    result.insert("findings".to_owned(), Value::Array(findings));
    Ok(result)
}
