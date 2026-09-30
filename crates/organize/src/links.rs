//! Link enumeration and editing, including PyMuPDF's distinction between an explicit
//! page destination, a named/view destination, and an external file or URI.

use std::collections::{HashMap, HashSet};

use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{flag, int_value, optional, required};
use goat_common::parse::{page_indices, selected_page};
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, ObjRef, Object, Rect};
use serde_json::{Map, Value, json};

use crate::dest::{self, Target};
use crate::geometry;
use crate::open::{self, Lib, display, out_path, pdf_error, result, source};

type Output = Result<Map<String, Value>, GoatError>;
type Names = HashMap<String, i64>;

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("links")
                .about("list links")
                .arg(Arg::new("file").required(true)),
            get,
        ),
    );
    registry.family_verb(
        "links",
        Verb::new(
            Command::new("add")
                .about("add a link")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("page")
                        .long("page")
                        .value_parser(int_value)
                        .default_value("1"),
                )
                .arg(Arg::new("rect").long("rect").required(true))
                .arg(Arg::new("uri").long("uri"))
                .arg(
                    Arg::new("goto")
                        .long("goto")
                        .value_parser(int_value)
                        .help("target page (1-based)"),
                )
                .arg(Arg::new("output").short('o').long("output")),
            add,
        ),
    );
    registry.family_verb(
        "links",
        Verb::new(
            Command::new("remove")
                .about("remove links")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("pages").long("pages"))
                .arg(
                    Arg::new("external_only")
                        .long("external-only")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("output").short('o').long("output")),
            remove,
        ),
    );
}

struct Destination {
    uri: Option<String>,
    page: Value,
    external: bool,
}

impl Destination {
    fn internal(page: Value) -> Self {
        Self {
            uri: None,
            page,
            external: false,
        }
    }
    fn file(page: Value) -> Self {
        Self {
            uri: None,
            page,
            external: true,
        }
    }
}

struct Link {
    id: Option<ObjRef>,
    rect: Rect,
    destination: Destination,
}

fn names(doc: &Document) -> Result<Names, GoatError> {
    let mut entries = Vec::new();
    if let Some(value) = doc.catalog().map_err(pdf_error)?.get(b"Dests")
        && let Some(dict) = doc.resolve_dict(value).map_err(pdf_error)?
    {
        for (name, value) in dict.iter() {
            entries.push((
                String::from_utf8_lossy(name.as_bytes()).into_owned(),
                value.clone(),
            ));
        }
    }
    for (name, value) in doc.names(b"Dests").map_err(pdf_error)? {
        entries.push((pdf_core::PdfString::literal(name).to_text(), value));
    }
    let mut out = Names::new();
    for (name, value) in entries {
        let value = doc.resolve(&value).map_err(pdf_error)?;
        let value = match value {
            Object::Dict(dict) => doc.resolve_key(&dict, b"D").map_err(pdf_error)?,
            value => value,
        };
        if let Target::Page(page) = dest::dest_target(doc, &value)? {
            out.insert(name, page);
        }
    }
    Ok(out)
}

fn named(name: &str, names: &Names) -> Destination {
    // linkDest percent-decodes UTF-8 bytes as Latin-1 before resolve_names lookup.
    let decoded: String = name
        .as_bytes()
        .iter()
        .map(|byte| char::from(*byte))
        .collect();
    Destination::internal(
        names
            .get(&decoded)
            .copied()
            .map_or(Value::Null, Value::from),
    )
}

fn explicit(
    doc: &Document,
    value: &Object,
    names: &Names,
) -> Result<Option<Destination>, GoatError> {
    let value = doc.resolve(value).map_err(pdf_error)?;
    let items = match value {
        Object::String(text) => return Ok(Some(named(&text.to_text(), names))),
        Object::Name(name) => {
            return Ok(Some(named(
                &String::from_utf8_lossy(name.as_bytes()),
                names,
            )));
        }
        Object::Array(items) => items,
        _ => return Ok(None),
    };
    let Some(first) = items.first() else {
        return Ok(None);
    };
    let page = match first {
        Object::Integer(page) => *page,
        Object::Reference(_) => dest::page_number(doc, Some(first))?,
        _ => 0,
    };
    if page < 0 || page >= dest::to_i64(doc.page_count().map_err(pdf_error)?) {
        return Ok(None);
    }
    let view = items.get(1).and_then(Object::as_name).unwrap_or_default();
    let has = |i: usize| items.get(i).is_some_and(|value| value.as_f64().is_some());
    let named_view = matches!(
        view,
        b"Fit" | b"FitB" | b"FitH" | b"FitV" | b"FitBH" | b"FitBV" | b"FitR"
    ) || view == b"XYZ" && has(2) != has(3);
    Ok(Some(Destination::internal(if named_view {
        (page + 1).to_string().into()
    } else {
        page.into()
    })))
}

fn uri(text: &str) -> Destination {
    let scheme = text.split_once(':').filter(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
    });
    if scheme.is_some_and(|(scheme, _)| scheme != "file") {
        return Destination {
            uri: Some(text.to_owned()),
            page: Value::Null,
            external: true,
        };
    }
    let page = text
        .split_once("#page=")
        .and_then(|(_, page)| page.split('&').next())
        .and_then(|page| page.parse::<i64>().ok());
    Destination::file(page.map_or(Value::Null, |page| (page - 1).into()))
}

fn action(
    doc: &Document,
    value: &Object,
    page: usize,
    names: &Names,
) -> Result<Option<Destination>, GoatError> {
    let Some(action) = doc.resolve_dict(value).map_err(pdf_error)? else {
        return Ok(None);
    };
    let kind = doc.resolve_key(&action, b"S").map_err(pdf_error)?;
    Ok(match kind.as_name().unwrap_or_default() {
        b"GoTo" => match action.get(b"D") {
            Some(value) => explicit(doc, value, names)?,
            None => None,
        },
        b"URI" => {
            let value = doc.resolve_key(&action, b"URI").map_err(pdf_error)?;
            Some(uri(&value
                .as_string()
                .map(|text| text.to_text())
                .unwrap_or_default()))
        }
        b"Named" => match dest::action_target(doc, value, Some(page))? {
            Target::Page(page) => Some(Destination::internal(page.into())),
            _ => None,
        },
        b"Launch" if action.contains_key(b"F") => Some(Destination::file(0.into())),
        b"GoToR" => {
            let value = doc.resolve_key(&action, b"D").map_err(pdf_error)?;
            let page = match value {
                Object::String(_) | Object::Name(_) => Value::Null,
                Object::Array(items) => items.first().and_then(Object::as_i64).unwrap_or(0).into(),
                _ => 0.into(),
            };
            Some(Destination::file(page))
        }
        _ => None,
    })
}

fn page_links(doc: &Document, index: usize, names: &Names) -> Result<Vec<Link>, GoatError> {
    let page = doc.page(index).map_err(pdf_error)?;
    let annots = match page.dict.get(b"Annots") {
        Some(value) => doc
            .resolve_array(value)
            .map_err(pdf_error)?
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut count = 0;
    let mut links = Vec::new();
    let matrix = geometry::ctm(&page);
    for annot in annots {
        let Some(dict) = doc.resolve_dict(&annot).map_err(pdf_error)? else {
            continue;
        };
        if dict.get_name(b"Subtype") != Some(b"Link".as_slice()) {
            continue;
        }
        count += 1;
        let rect = doc.resolve_key(&dict, b"Rect").map_err(pdf_error)?;
        let Some(rect) = rect.as_array().and_then(Rect::from_array) else {
            continue;
        };
        let destination = if let Some(value) = dict.get(b"Dest").filter(|value| !value.is_null()) {
            explicit(doc, value, names)?
        } else if let Some(value) = dict.get(b"A") {
            action(doc, value, index, names)?
        } else if let Some(aa) = dict.get(b"AA") {
            let aa = doc.resolve_dict(aa).map_err(pdf_error)?.unwrap_or_default();
            match aa.get(b"U") {
                Some(value) => action(doc, value, index, names)?,
                None => None,
            }
        } else {
            None
        };
        if let Some(destination) = destination {
            links.push(Link {
                id: annot.as_reference(),
                rect: rect.transform(&matrix),
                destination,
            });
        }
    }
    // PyMuPDF pairs MuPDF's loaded links with annotation xrefs only for equal counts.
    if count != links.len() {
        for link in &mut links {
            link.id = None;
        }
    }
    Ok(links)
}

fn get(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let doc = open::load(&src, Lib::PyMuPdf)?;
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let names = names(&doc)?;
    let mut rows = Vec::new();
    for page in 0..doc.page_count().map_err(pdf_error)? {
        for link in page_links(&doc, page, &names)? {
            let rect = geometry::values(link.rect)
                .map(|value| (f64::from(value as f32) * 10.0).round_ties_even() / 10.0);
            rows.push(json!({"page": page + 1, "rect": rect, "uri": link.destination.uri, "target_page": link.destination.page}));
        }
    }
    let mut map = result("get-links", vec![display(&src)], vec![]);
    map.insert("count".into(), rows.len().into());
    map.insert("links".into(), rows.into());
    Ok(map)
}

fn add(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "linked")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let number = *required::<i64>(matches, "page")?;
    let index = selected_page(number, doc.page_count().map_err(pdf_error)?)?;
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let mut page = doc.page(index).map_err(pdf_error)?;
    let rect = geometry::rect_arg(required::<String>(matches, "rect")?)?;
    let inverse = geometry::text_matrix(&page)
        .invert()
        .ok_or_else(|| GoatError::value_error("matrix not invertible"))?;
    let rect = rect.transform(&inverse);
    let mut action = Dict::new();
    if let Some(uri) = optional::<String>(matches, "uri")?.filter(|uri| !uri.is_empty()) {
        action.insert("S", Object::name("URI"));
        action.insert("URI", Object::text(uri));
    } else {
        let target = optional::<i64>(matches, "goto")?.copied().ok_or_else(|| {
            GoatError::exception(
                "TypeError",
                "unsupported operand type(s) for -: 'NoneType' and 'int'",
            )
        })?;
        if target < 1 {
            return Err(GoatError::exception("KeyError", "'to'"));
        }
        let target = usize::try_from(target - 1).unwrap_or(usize::MAX);
        if target >= doc.page_count().map_err(pdf_error)? {
            return Err(GoatError::exception("RuntimeError", "bad page number(s)"));
        }
        let to = doc.page(target).map_err(pdf_error)?;
        let point = pdf_core::Point::new(0.0, 0.0).transform(
            &geometry::text_matrix(&to)
                .invert()
                .ok_or_else(|| GoatError::value_error("matrix not invertible"))?,
        );
        action.insert("S", Object::name("GoTo"));
        action.insert(
            "D",
            Object::Array(vec![
                Object::Reference(to.id),
                Object::name("XYZ"),
                Object::Real(point.x),
                Object::Real(point.y),
                Object::Integer(0),
            ]),
        );
    }
    let mut annots = match page.dict.get(b"Annots") {
        Some(value) => doc
            .resolve_array(value)
            .map_err(pdf_error)?
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut used = HashSet::new();
    for value in &annots {
        if let Some(annot) = doc.resolve_dict(value).map_err(pdf_error)?
            && let Some(name) = annot.get(b"NM").and_then(Object::as_string)
        {
            used.insert(name.to_text());
        }
    }
    let mut suffix = 0;
    while used.contains(&format!("fitz-L{suffix}")) {
        suffix += 1;
    }
    let mut annot = Dict::new();
    annot.insert("A", action);
    annot.insert("Rect", rect.to_object());
    let mut border = Dict::new();
    border.insert("W", 0i64);
    annot.insert("BS", border);
    annot.insert("Subtype", Object::name("Link"));
    annot.insert("NM", Object::text(&format!("fitz-L{suffix}")));
    annots.push(Object::Reference(doc.add(annot)));
    page.dict.insert("Annots", annots);
    doc.set(page.id, page.dict);
    open::save_mupdf(&doc, &out)?;
    let mut map = result("links-add", vec![display(&src)], vec![display(&out)]);
    map.insert("page".into(), number.into());
    Ok(map)
}

fn remove(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "links-removed")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let indices = page_indices(
        optional::<String>(matches, "pages")?.map(String::as_str),
        doc.page_count().map_err(pdf_error)?,
    )?;
    let external_only = flag(matches, "external_only")?;
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let names = names(&doc)?;
    let mut removed = 0;
    for index in indices {
        let mut ids = HashSet::new();
        for link in page_links(&doc, index, &names)? {
            if external_only && !link.destination.external {
                continue;
            }
            removed += 1;
            if let Some(id) = link.id {
                ids.insert(id);
            }
        }
        if ids.is_empty() {
            continue;
        }
        let mut page = doc.page(index).map_err(pdf_error)?;
        if let Some(stored) = page.dict.get(b"Annots").cloned() {
            let mut annots = doc
                .resolve_array(&stored)
                .map_err(pdf_error)?
                .unwrap_or_default();
            annots.retain(|value| value.as_reference().is_none_or(|id| !ids.contains(&id)));
            match stored {
                Object::Reference(id) => doc.set(id, annots),
                _ => {
                    page.dict.insert("Annots", annots);
                    doc.set(page.id, page.dict);
                }
            }
        }
        for id in ids {
            doc.delete(id);
        }
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("links-remove", vec![display(&src)], vec![display(&out)]);
    map.insert("removed".into(), removed.into());
    map.insert("external_only".into(), external_only.into());
    Ok(map)
}
