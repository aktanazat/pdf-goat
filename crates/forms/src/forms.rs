use std::collections::HashSet;
use std::path::PathBuf;

use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{flag, int_value, optional, required};
use goat_common::{Ctx, GoatError, Registry, Verb, paths};
use pdf_core::{Dict, Document, Encryption, ObjRef, Object, Rect, SaveOptions};
use serde_json::{Map, Value, json};

use crate::appearance::{child_dict, dict, numbers, pypdf_text, store_child};
use crate::{error, field_full_name, open, result, text, update_widget_appearance};

pub(crate) fn output(
    matches: &ArgMatches,
    source: &str,
    suffix: &str,
    extension: &str,
) -> Result<PathBuf, GoatError> {
    let path = optional::<String>(matches, "output")?
        .filter(|s| !s.is_empty())
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| paths::default_out(source, suffix, extension))?;
    paths::ensure_parent(&path)
}

pub(crate) fn save(doc: &Document, path: &PathBuf, compressed: bool) -> Result<(), GoatError> {
    doc.save(
        path,
        &SaveOptions {
            compress_streams: compressed,
            object_streams: compressed,
            garbage_collect: true,
            encryption: Encryption::Remove,
            ..SaveOptions::default()
        },
    )
    .map_err(error)
}

pub(crate) fn acroform(doc: &mut Document, create: bool) -> Result<ObjRef, GoatError> {
    let catalog_id = doc.catalog_ref().map_err(error)?;
    let mut catalog = doc.catalog().map_err(error)?;
    if let Some(id) = catalog.get_ref(b"AcroForm")
        && doc.get(id).map_err(error)?.as_dict().is_some()
    {
        return Ok(id);
    }
    let existing = doc.resolve_key(&catalog, b"AcroForm").map_err(error)?;
    let acro = match existing {
        Object::Dict(d) => d,
        _ if create => {
            let mut d = Dict::new();
            d.insert("Fields", Object::Array(Vec::new()));
            d
        }
        _ => {
            return Err(GoatError::exception(
                "PyPdfError",
                "No /AcroForm dictionary in PDF of PdfWriter Object",
            ));
        }
    };
    let id = doc.add(acro);
    catalog.insert("AcroForm", Object::Reference(id));
    doc.set(catalog_id, catalog);
    Ok(id)
}

pub(crate) fn append_annot(
    doc: &mut Document,
    page_index: usize,
    annotation: Dict,
) -> Result<ObjRef, GoatError> {
    let page = doc.page(page_index).map_err(error)?;
    let mut page_dict = page.dict;
    let mut annots = doc
        .resolve_key(&page_dict, b"Annots")
        .map_err(error)?
        .as_array()
        .unwrap_or(&[])
        .to_vec();
    let id = doc.add(annotation);
    annots.push(Object::Reference(id));
    store_child(doc, &mut page_dict, "Annots", Object::Array(annots));
    doc.set(page.id, page_dict);
    Ok(id)
}

pub(crate) fn next_name(
    doc: &Document,
    page_index: usize,
    prefix: &str,
) -> Result<String, GoatError> {
    let page = doc.page(page_index).map_err(error)?;
    let annotations = doc.resolve_key(&page.dict, b"Annots").map_err(error)?;
    let mut used = HashSet::new();
    for annotation in annotations.as_array().unwrap_or(&[]) {
        let dictionary = dict(doc, annotation)?;
        used.insert(text(&doc.resolve_key(&dictionary, b"NM").map_err(error)?));
    }
    for index in 0..=used.len() {
        let name = format!("{prefix}{index}");
        if !used.contains(&name) {
            return Ok(name);
        }
    }
    Err(GoatError::message("annotation name allocation failed"))
}

fn data(matches: &ArgMatches, xml: bool) -> Result<Map<String, Value>, GoatError> {
    let path = paths::expanduser(required::<String>(matches, "data")?);
    let source = std::fs::read_to_string(&path).map_err(|e| GoatError::os(&e, &path))?;
    if xml
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("xfdf"))
    {
        let document = roxmltree::Document::parse(&source)
            .map_err(|e| GoatError::exception("ParseError", e.to_string()))?;
        let mut values = Map::new();
        for node in document
            .descendants()
            .filter(|n| n.has_tag_name(("http://ns.adobe.com/xfdf/", "field")))
        {
            let name = node
                .attribute("name")
                .ok_or_else(|| GoatError::value_error("XFDF field is missing its name"))?;
            let value = node
                .children()
                .find(|n| n.has_tag_name(("http://ns.adobe.com/xfdf/", "value")))
                .and_then(|n| n.text())
                .unwrap_or("");
            values.insert(name.to_owned(), json!(value));
        }
        Ok(values)
    } else {
        let value: Value = serde_json::from_str(&source)
            .map_err(|e| GoatError::exception("JSONDecodeError", e.to_string()))?;
        match value {
            Value::Object(values) => Ok(values),
            other => Err(GoatError::exception(
                "AttributeError",
                format!(
                    "'{}' object has no attribute 'items'",
                    match other {
                        Value::Array(_) => "list",
                        Value::String(_) => "str",
                        Value::Number(_) => "int",
                        Value::Bool(_) => "bool",
                        _ => "NoneType",
                    }
                ),
            )),
        }
    }
}

/// A check box or radio field and the widgets and value one input name gives it.
struct ButtonFill<'a> {
    field: ObjRef,
    name: &'a str,
    value: &'a Value,
    widgets: Vec<ObjRef>,
}

/// Fills every field an input name matches and returns the names that matched.
pub(crate) fn fill_values<'a>(
    doc: &mut Document,
    values: &'a Map<String, Value>,
) -> Result<HashSet<&'a str>, GoatError> {
    let acro_id = acroform(doc, false)?;
    let mut acro = dict(doc, &Object::Reference(acro_id))?;
    if !acro.contains_key(b"Fields") {
        acro.insert("Fields", Object::Array(Vec::new()));
    }
    acro.insert("NeedAppearances", false);
    doc.set(acro_id, acro);
    let mut matched = HashSet::new();
    let mut buttons: Vec<ButtonFill<'a>> = Vec::new();
    for page_index in 0..doc.page_count().map_err(error)? {
        for widget in crate::page_widgets(doc, page_index)? {
            let annotation = dict(doc, &Object::Reference(widget))?;
            let parent_id = if annotation.contains_key(b"FT") && annotation.contains_key(b"T") {
                widget
            } else {
                annotation.get_ref(b"Parent").unwrap_or(widget)
            };
            let mut parent = dict(doc, &Object::Reference(parent_id))?;
            let qualified = qualified_name(doc, &parent)?;
            let local = text(&doc.resolve_key(&parent, b"T").map_err(error)?);
            let button = crate::inherited(doc, &parent, b"FT")?.as_name() == Some(b"Btn");
            for (name, value) in values {
                if name != &qualified && name != &local {
                    continue;
                }
                matched.insert(name.as_str());
                if button {
                    // A button's state is decided once per field, across all its widgets.
                    match buttons
                        .iter_mut()
                        .find(|fill| fill.field == parent_id && fill.name == name)
                    {
                        Some(fill) => fill.widgets.push(widget),
                        None => buttons.push(ButtonFill {
                            field: parent_id,
                            name,
                            value,
                            widgets: vec![widget],
                        }),
                    }
                    continue;
                }
                if parent.get_name(b"FT") == Some(b"Ch") {
                    parent.remove(b"I");
                }
                let field_value = if let Value::Array(values) = value {
                    Object::Array(
                        values
                            .iter()
                            .map(|v| Object::text(&goat_common::py::str_value(v)))
                            .collect(),
                    )
                } else {
                    Object::text(&goat_common::py::str_value(value))
                };
                parent.insert("V", field_value);
                doc.set(parent_id, parent.clone());
                if matches!(parent.get_name(b"FT"), Some(b"Tx" | b"Ch")) {
                    pypdf_text(doc, widget, &parent, acro_id)?;
                }
            }
        }
    }
    for fill in &buttons {
        fill_button(doc, fill)?;
    }
    Ok(matched)
}

/// Sets each widget's `/AS` and the field's `/V` to the state `fill` asks for.
fn fill_button(doc: &mut Document, fill: &ButtonFill) -> Result<(), GoatError> {
    let mut widget_states = Vec::with_capacity(fill.widgets.len());
    let mut states: Vec<String> = Vec::new();
    for &widget in &fill.widgets {
        let annotation = dict(doc, &Object::Reference(widget))?;
        let ap = child_dict(doc, &annotation, b"AP")?;
        let normal = child_dict(doc, &ap, b"N")?;
        let own: Vec<String> = normal
            .keys()
            .map(|key| String::from_utf8_lossy(key.as_bytes()).into_owned())
            .filter(|state| state != "Off")
            .collect();
        for state in &own {
            if !states.contains(state) {
                states.push(state.clone());
            }
        }
        widget_states.push((widget, annotation, own));
    }
    let field = dict(doc, &Object::Reference(fill.field))?;
    let chosen = button_state(fill, crate::widget_type(doc, &field)?, &states)?;
    for (widget, mut annotation, own) in widget_states {
        let state = if own.contains(&chosen) {
            chosen.as_str()
        } else {
            "Off"
        };
        annotation.insert("AS", Object::name(state));
        doc.set(widget, annotation);
    }
    // Re-read: the field may be one of the widgets just updated.
    let mut field = dict(doc, &Object::Reference(fill.field))?;
    field.insert("V", Object::name(chosen.as_str()));
    doc.set(fill.field, field);
    Ok(())
}

/// The state a check box or radio value selects: a state name with or without its
/// leading slash, `true` for a check box's only on state, or `false`, `"Off"` and
/// `""` for off. Anything else names no state of the field and is refused.
fn button_state(
    fill: &ButtonFill,
    kind: crate::WidgetType,
    states: &[String],
) -> Result<String, GoatError> {
    let field = fill.name;
    let listed = || {
        states
            .iter()
            .map(String::as_str)
            .chain(["Off"])
            .collect::<Vec<_>>()
            .join(", ")
    };
    if kind == crate::WidgetType::Button {
        return Err(GoatError::message(format!(
            "form field '{field}' is a push button and holds no value"
        )));
    }
    let requested = match fill.value {
        Value::Bool(false) => return Ok("Off".to_owned()),
        Value::Bool(true) => {
            return match states {
                [on] if kind == crate::WidgetType::CheckBox => Ok(on.clone()),
                _ => Err(GoatError::message(format!(
                    "form field '{field}' {}, so true is ambiguous; give one of its states: {}",
                    if kind == crate::WidgetType::RadioButton {
                        "is a radio group"
                    } else {
                        "has no single on state"
                    },
                    listed()
                ))),
            };
        }
        Value::String(text) => text.strip_prefix('/').unwrap_or(text).to_owned(),
        other => goat_common::py::str_value(other),
    };
    if requested.is_empty() || requested == "Off" {
        return Ok("Off".to_owned());
    }
    if states.contains(&requested) {
        return Ok(requested);
    }
    Err(GoatError::message(format!(
        "form field '{field}' has no state '{requested}'; its states are: {}",
        listed()
    )))
}

fn fill(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    fill_or_import(matches, false)
}
fn import(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    fill_or_import(matches, true)
}
fn fill_or_import(matches: &ArgMatches, importing: bool) -> Result<Map<String, Value>, GoatError> {
    let source = paths::resolve(required::<String>(matches, "file")?)?
        .to_string_lossy()
        .into_owned();
    let values = data(matches, importing)?;
    let output = output(matches, &source, "filled", "pdf")?;
    let (mut doc, _) = open(&source, "pypdf")?;
    let matched = fill_values(&mut doc, &values)?;
    let flattened = flag(matches, "flatten")?;
    if flattened {
        crate::flatten::flatten(&mut doc, false)?;
    }
    save(&doc, &output, false)?;
    let mut result = result(
        if importing {
            "form-import"
        } else {
            "form-fill"
        },
        &source,
        vec![output.to_string_lossy().into_owned()],
    );
    let (set, unknown): (Vec<&String>, Vec<&String>) = values
        .keys()
        .partition(|name| matched.contains(name.as_str()));
    result.insert("fields_set".into(), json!(set));
    result.insert("unknown_fields".into(), json!(unknown));
    result.insert("flattened".into(), json!(flattened));
    Ok(result)
}

fn qualified_name(doc: &Document, field: &Dict) -> Result<String, GoatError> {
    if let Some(mapping) = field.get(b"TM") {
        return Ok(text(&doc.resolve(mapping).map_err(error)?));
    }
    field_full_name(doc, field)
}

fn pdf_value(doc: &Document, object: &Object, depth: usize) -> Result<String, GoatError> {
    if depth > 128 {
        return Err(GoatError::message("form value nesting exceeds 128"));
    }
    let object = doc.resolve(object).map_err(error)?;
    Ok(match object {
        Object::Null => String::new(),
        Object::String(s) => s.to_text(),
        Object::Name(n) => format!("/{}", String::from_utf8_lossy(n.as_bytes())),
        Object::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|v| pdf_value(doc, v, depth + 1).map(|s| goat_common::py::repr_str(&s)))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        Object::Integer(n) => n.to_string(),
        Object::Real(n) => goat_common::py::float_repr(n),
        Object::Bool(b) => if b { "True" } else { "False" }.to_owned(),
        other => String::from_utf8_lossy(&other.to_bytes()).into_owned(),
    })
}

fn export_values(doc: &Document) -> Result<Map<String, Value>, GoatError> {
    let acro = child_dict(doc, &doc.catalog().map_err(error)?, b"AcroForm")?;
    let fields = doc.resolve_key(&acro, b"Fields").map_err(error)?;
    let mut stack: Vec<_> = fields
        .as_array()
        .unwrap_or(&[])
        .iter()
        .rev()
        .map(|o| (o.clone(), 0_usize))
        .collect();
    let mut seen = HashSet::new();
    let mut values = Map::new();
    while let Some((object, depth)) = stack.pop() {
        if depth >= 128 {
            return Err(GoatError::message("form field nesting exceeds 128"));
        }
        if let Some(id) = object.as_reference()
            && !seen.insert(id)
        {
            continue;
        }
        let field = dict(doc, &object)?;
        if !field.contains_key(b"T") && !field.contains_key(b"TM") {
            continue;
        }
        values.insert(
            qualified_name(doc, &field)?,
            json!(pdf_value(doc, field.get(b"V").unwrap_or(&Object::Null), 0)?),
        );
        let children = doc.resolve_key(&field, b"Kids").map_err(error)?;
        stack.extend(
            children
                .as_array()
                .unwrap_or(&[])
                .iter()
                .rev()
                .map(|o| (o.clone(), depth + 1)),
        );
    }
    Ok(values)
}

fn export(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let (doc, source) = open(required::<String>(matches, "file")?, "pypdf")?;
    let values = export_values(&doc)?;
    let format = required::<String>(matches, "format")?;
    let output = output(matches, &source, "formdata", format)?;
    let contents = match format.as_str() {
        "xfdf" => {
            let mut xml="<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xfdf xmlns=\"http://ns.adobe.com/xfdf/\"><fields>".to_owned();
            for (name, value) in &values {
                xml.push_str(&format!(
                    "<field name=\"{}\"><value>{}</value></field>",
                    escape_xml(name),
                    escape_xml(value.as_str().unwrap_or(""))
                ));
            }
            xml.push_str("</fields></xfdf>");
            xml
        }
        "fdf" => {
            let mut fdf = "%FDF-1.2\n1 0 obj\n<< /FDF << /Fields [\n".to_owned();
            for (name, value) in &values {
                fdf.push_str(&format!(
                    "<< /T ({name}) /V ({}) >>\n",
                    value.as_str().unwrap_or("")
                ));
            }
            fdf.push_str("] >> >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n");
            fdf
        }
        _ => goat_common::py::json_pretty(&Value::Object(values.clone())),
    };
    std::fs::write(&output, contents).map_err(|e| GoatError::os(&e, &output))?;
    let mut result = result(
        "form-export",
        &source,
        vec![output.to_string_lossy().into_owned()],
    );
    result.insert("format".into(), json!(format));
    result.insert("field_count".into(), json!(values.len()));
    Ok(result)
}
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn create_text(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    create(matches, false)
}
fn create_checkbox(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    create(matches, true)
}
fn create(matches: &ArgMatches, checkbox: bool) -> Result<Map<String, Value>, GoatError> {
    let (mut doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
    let index = goat_common::parse::selected_page(
        *required::<i64>(matches, "page")?,
        doc.page_count().map_err(error)?,
    )?;
    let coordinates = goat_common::parse::parse_rect(required::<String>(matches, "rect")?)?;
    let rect = Rect::new(
        coordinates[0],
        coordinates[1],
        coordinates[2],
        coordinates[3],
    );
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return Err(GoatError::value_error("bad rect"));
    }
    let mut page = doc.page(index).map_err(error)?;
    let inverse = crate::page_transform(&mut page)
        .invert()
        .ok_or_else(|| GoatError::value_error("invalid page transform"))?;
    let name = required::<String>(matches, "name")?;
    let mut field = Dict::new();
    field.insert("Type", Object::name("Annot"));
    field.insert("Subtype", Object::name("Widget"));
    field.insert("FT", Object::name(if checkbox { "Btn" } else { "Tx" }));
    field.insert("T", Object::text(name));
    field.insert("NM", Object::text(&next_name(&doc, index, "fitz-W")?));
    field.insert("Rect", rect.transform(&inverse).to_object());
    let mut mk = Dict::new();
    mk.insert("BG", numbers(&[0.96, 0.96, 0.96]));
    mk.insert("BC", numbers(&[0.4, 0.4, 0.4]));
    field.insert("MK", mk);
    field.insert("F", 4_i64);
    let mut bs = Dict::new();
    bs.insert("S", Object::name("S"));
    bs.insert("W", 1_i64);
    field.insert("BS", bs);
    field.insert("DA", Object::text("0 0 0 rg /Helv 0 Tf"));
    field.insert("Ff", 0_i64);
    if checkbox {
        field.insert("AS", Object::name("Off"));
        field.insert("V", Object::name("Off"));
    }
    let id = append_annot(&mut doc, index, field)?;
    let acro_id = acroform(&mut doc, true)?;
    let mut acro = dict(&doc, &Object::Reference(acro_id))?;
    let mut fields = doc
        .resolve_key(&acro, b"Fields")
        .map_err(error)?
        .as_array()
        .unwrap_or(&[])
        .to_vec();
    fields.push(Object::Reference(id));
    store_child(&mut doc, &mut acro, "Fields", Object::Array(fields));
    doc.set(acro_id, acro);
    update_widget_appearance(&mut doc, id)?;
    let output = output(
        matches,
        &source,
        if checkbox { "checkbox" } else { "field" },
        "pdf",
    )?;
    save(&doc, &output, true)?;
    let mut result = result(
        if checkbox {
            "form-create-checkbox"
        } else {
            "form-create-text"
        },
        &source,
        vec![output.to_string_lossy().into_owned()],
    );
    result.insert("field".into(), json!(name));
    Ok(result)
}

pub(crate) fn base(name: &'static str, help: &'static str) -> Command {
    Command::new(name)
        .about(help)
        .arg(Arg::new("file").required(true))
}
pub(crate) fn out(command: Command) -> Command {
    command.arg(Arg::new("output").short('o').long("output"))
}
pub(crate) fn page(command: Command) -> Command {
    command.arg(
        Arg::new("page")
            .long("page")
            .value_parser(int_value)
            .default_value("1"),
    )
}

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "form",
        Verb::new(
            out(base("fill", "fill a form from JSON")
                .arg(
                    Arg::new("data")
                        .long("data")
                        .required(true)
                        .help("JSON file of field:value"),
                )
                .arg(
                    Arg::new("flatten")
                        .long("flatten")
                        .action(ArgAction::SetTrue),
                )),
            fill,
        ),
    );
    for (name, help, handler) in [
        (
            "create-text",
            "add a text field",
            create_text as goat_common::Handler,
        ),
        ("create-checkbox", "add a checkbox field", create_checkbox),
    ] {
        registry.family_verb(
            "form",
            Verb::new(
                out(
                    page(base(name, help).arg(Arg::new("name").long("name").required(true))).arg(
                        Arg::new("rect")
                            .long("rect")
                            .required(true)
                            .help("x0,y0,x1,y1"),
                    ),
                ),
                handler,
            ),
        );
    }
    registry.family_verb(
        "form",
        Verb::new(
            out(base("export", "export field data").arg(
                Arg::new("format")
                    .long("format")
                    .value_parser(["json", "xfdf", "fdf"])
                    .default_value("json"),
            )),
            export,
        ),
    );
    registry.family_verb(
        "form",
        Verb::new(
            out(base("import", "import JSON or XFDF field data")
                .arg(Arg::new("data").long("data").required(true))
                .arg(
                    Arg::new("flatten")
                        .long("flatten")
                        .action(ArgAction::SetTrue),
                )),
            import,
        ),
    );
}
