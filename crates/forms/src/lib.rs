//! AcroForm widgets, annotation appearances, and flattening.
//!
//! [`register`] adds the `form` and `annotate` families and `pages flatten`.
//! Field listing is widget-level; export is field-level, so widgets sharing a
//! parent retain separate page bounds but export one value. Parent resolution
//! rejects cycles and stops at 128 levels.
//!
//! Filling follows pypdf's value/state rules. The public widget-update helpers
//! follow MuPDF's rules, including retaining a text value on an empty-string
//! update. Both paths write normal appearance streams, not viewer-only values.
//!
//! [`flatten`] fits each selected normal appearance to its annotation rectangle,
//! respecting the appearance matrix, visibility flags, and page rotation. It
//! makes resources local, merges form defaults, and removes painted annotations.
//! `pages flatten` regenerates appearances when `NeedAppearances` is set.
//! Page content, including transparency, stays vector and unchanged, so
//! zooming keeps full detail.

mod annots;
mod appearance;
mod clear;
mod flatten;
mod forms;

pub use appearance::{set_field_value, update_widget_appearance};
pub use clear::clear_field_value;
pub use flatten::flatten;

use std::collections::HashSet;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::required;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, ObjRef, Object, Rect};
use serde_json::{Map, Value, json};

/// Widget classification exposed by PyMuPDF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WidgetType {
    Unknown,
    Button,
    CheckBox,
    RadioButton,
    Text,
    ListBox,
    ComboBox,
    Signature,
}

impl WidgetType {
    pub fn field_type_string(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Button => "Button",
            Self::CheckBox => "CheckBox",
            Self::RadioButton => "RadioButton",
            Self::Text => "Text",
            Self::ListBox => "ListBox",
            Self::ComboBox => "ComboBox",
            Self::Signature => "Signature",
        }
    }
}

pub(crate) fn error(error: pdf_core::Error) -> GoatError {
    GoatError::message(error.to_string())
}

/// Resolve a field property through its parent chain, rejecting cycles and excessive depth.
pub fn inherited(doc: &Document, field: &Dict, key: &[u8]) -> Result<Object, GoatError> {
    let mut current = field.clone();
    let mut seen = HashSet::new();
    for _ in 0..128 {
        if let Some(value) = current.get(key) {
            return doc.resolve(value).map_err(error);
        }
        let Some(parent) = current.get(b"Parent") else {
            return Ok(Object::Null);
        };
        if let Some(id) = parent.as_reference()
            && !seen.insert(id)
        {
            return Err(GoatError::message("cycle in form field parents"));
        }
        let Some(next) = doc.resolve_dict(parent).map_err(error)? else {
            return Ok(Object::Null);
        };
        current = next;
    }
    Err(GoatError::message("form field parent nesting exceeds 128"))
}

pub fn widget_type(doc: &Document, field: &Dict) -> Result<WidgetType, GoatError> {
    let flags = inherited(doc, field, b"Ff")?.as_i64().unwrap_or(0);
    Ok(match inherited(doc, field, b"FT")?.as_name() {
        Some(b"Tx") => WidgetType::Text,
        Some(b"Ch") if flags & 131072 != 0 => WidgetType::ComboBox,
        Some(b"Ch") => WidgetType::ListBox,
        Some(b"Sig") => WidgetType::Signature,
        Some(b"Btn") if flags & 65536 != 0 => WidgetType::Button,
        Some(b"Btn") if flags & 32768 != 0 => WidgetType::RadioButton,
        Some(b"Btn") => WidgetType::CheckBox,
        _ => WidgetType::Button,
    })
}

pub(crate) fn text(object: &Object) -> String {
    object
        .as_string()
        .map(|value| value.to_text())
        .unwrap_or_default()
}

pub fn field_full_name(doc: &Document, field: &Dict) -> Result<String, GoatError> {
    let mut parts = Vec::new();
    let mut current = field.clone();
    let mut seen = HashSet::new();
    for _ in 0..128 {
        let part = text(&doc.resolve_key(&current, b"T").map_err(error)?);
        if !part.is_empty() {
            parts.push(part);
        }
        let Some(parent) = current.get(b"Parent") else {
            parts.reverse();
            return Ok(parts.join("."));
        };
        if let Some(id) = parent.as_reference()
            && !seen.insert(id)
        {
            return Err(GoatError::message("cycle in form field parents"));
        }
        let Some(next) = doc.resolve_dict(parent).map_err(error)? else {
            parts.reverse();
            return Ok(parts.join("."));
        };
        current = next;
    }
    Err(GoatError::message("form field parent nesting exceeds 128"))
}

pub fn field_value_text(doc: &Document, field: &Dict) -> Result<String, GoatError> {
    if widget_type(doc, field)? == WidgetType::RadioButton
        && let Some(state) = field.get_name(b"AS")
    {
        return Ok(String::from_utf8_lossy(state).into_owned());
    }
    let value = inherited(doc, field, b"V")?;
    Ok(value
        .as_name()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_else(|| text(&value)))
}

/// Indirect widgets in page annotation order. Direct widgets are normalized on creation.
pub fn page_widgets(doc: &Document, page_index: usize) -> Result<Vec<ObjRef>, GoatError> {
    let page = doc.page(page_index).map_err(error)?;
    let annots = doc.resolve_key(&page.dict, b"Annots").map_err(error)?;
    let mut widgets = Vec::new();
    for annot in annots.as_array().unwrap_or(&[]) {
        if let Some(id) = annot.as_reference()
            && doc
                .resolve_dict(annot)
                .map_err(error)?
                .is_some_and(|d| d.get_name(b"Subtype") == Some(b"Widget"))
        {
            widgets.push(id);
        }
    }
    Ok(widgets)
}

pub(crate) fn on_state(doc: &Document, field: &Dict) -> Result<Value, GoatError> {
    let ap = doc.resolve_key(field, b"AP").map_err(error)?;
    if let Some(ap) = ap.as_dict() {
        for key in [b"N".as_slice(), b"D"] {
            let states = doc.resolve_key(ap, key).map_err(error)?;
            if let Some(states) = states.as_dict() {
                for name in states.keys() {
                    if name.as_bytes() != b"Off" {
                        return Ok(json!(String::from_utf8_lossy(name.as_bytes())));
                    }
                }
            }
        }
    }
    Ok(Value::Bool(true))
}

pub(crate) fn rounded(value: f64, places: usize) -> f64 {
    format!("{value:.places$}").parse().unwrap_or(value)
}

pub(crate) fn rect_json(rect: Rect, places: usize) -> Value {
    json!([rect.x0, rect.y0, rect.x1, rect.y1].map(|n| rounded(f64::from(n as f32), places)))
}

/// PyMuPDF's annotation APIs use unrotated page space. Change only the detached
/// page snapshot while asking the interpreter for the transform.
pub(crate) fn page_transform(page: &mut pdf_core::Page) -> pdf_core::Matrix {
    let rotation = page.dict.insert("Rotate", 0_i64);
    let matrix = pdf_interp::page_transform(page);
    if let Some(rotation) = rotation {
        page.dict.insert("Rotate", rotation);
    } else {
        page.dict.remove(b"Rotate");
    }
    matrix
}

pub(crate) fn fixed_annotation_rect(rect: Rect, flags: i64, rotation: u16) -> Rect {
    if flags & 16 == 0 {
        return rect;
    }
    let (w, h) = (rect.width(), rect.height());
    match rotation {
        90 => Rect::new(rect.x0, rect.y1, rect.x0 + h, rect.y1 + w),
        180 => Rect::new(rect.x0 - w, rect.y1, rect.x0, rect.y1 + h),
        270 => Rect::new(rect.x0 - h, rect.y1 - w, rect.x0, rect.y1),
        _ => rect,
    }
}

pub(crate) fn annotation_rect(field: &Dict, rotation: u16, matrix: &pdf_core::Matrix) -> Rect {
    let rect = field
        .get_array(b"Rect")
        .and_then(Rect::from_array)
        .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0))
        .normalized();
    fixed_annotation_rect(rect, field.get_i64(b"F").unwrap_or(0), rotation).transform(matrix)
}

pub(crate) fn result(verb: &str, input: &str, outputs: Vec<String>) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("verb".into(), json!(verb));
    result.insert("inputs".into(), json!([input]));
    result.insert("outputs".into(), json!(outputs));
    result
}

pub(crate) fn open(path: &str, command: &str) -> Result<(Document, String), GoatError> {
    let path = goat_common::paths::resolve(path)?;
    let bytes = std::fs::read(&path).map_err(|e| GoatError::os(&e, &path))?;
    let doc = Document::load(bytes).map_err(|e| match e {
        pdf_core::Error::Io(e) => GoatError::os(&e, &path),
        _ => GoatError::exception(
            "FileDataError",
            format!("Failed to open file '{}'.", path.display()),
        ),
    })?;
    if doc.needs_password() {
        return Err(match command {
            "form list" => GoatError::needs_password(command),
            "pypdf" => GoatError::exception("FileNotDecryptedError", "File has not been decrypted"),
            _ => GoatError::value_error("document closed or encrypted"),
        });
    }
    Ok((doc, path.to_string_lossy().into_owned()))
}

fn list(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let (doc, path) = open(required::<String>(matches, "file")?, "form list")?;
    let acro = doc
        .resolve_key(&doc.catalog().map_err(error)?, b"AcroForm")
        .map_err(error)?;
    let xfa = if let Some(acro) = acro.as_dict() {
        !doc.resolve_key(acro, b"XFA").map_err(error)?.is_null()
    } else {
        false
    };
    let mut fields = Vec::new();
    for index in 0..doc.page_count().map_err(error)? {
        let mut page = doc.page(index).map_err(error)?;
        let transform = page_transform(&mut page);
        let annots = doc.resolve_key(&page.dict, b"Annots").map_err(error)?;
        for annot in annots.as_array().unwrap_or(&[]) {
            let Some(field) = doc.resolve_dict(annot).map_err(error)? else {
                continue;
            };
            if field.get_name(b"Subtype") != Some(b"Widget") {
                continue;
            }
            let kind = widget_type(&doc, &field)?;
            let value = field_value_text(&doc, &field)?;
            let rect = annotation_rect(&field, page.rotation(), &transform);
            let mut item = Map::new();
            item.insert("name".into(), json!(field_full_name(&doc, &field)?));
            item.insert("type".into(), json!(kind.field_type_string()));
            item.insert("value".into(), json!(value));
            item.insert("page".into(), json!(index + 1));
            item.insert("rect".into(), rect_json(rect, 2));
            item.insert(
                "flags".into(),
                json!(inherited(&doc, &field, b"Ff")?.as_i64().unwrap_or(0)),
            );
            if matches!(kind, WidgetType::CheckBox | WidgetType::RadioButton) {
                let state = on_state(&doc, &field)?;
                item.insert("on_state".into(), state.clone());
                item.insert("checked".into(), json!(state == Value::String(value)));
            }
            if matches!(kind, WidgetType::ComboBox | WidgetType::ListBox) {
                let options = inherited(&doc, &field, b"Opt")?;
                let own = doc.resolve_key(&field, b"Opt").map_err(error)?;
                let mut values = Vec::new();
                for i in 0..options.as_array().map_or(0, |a| a.len()) {
                    let option = own
                        .as_array()
                        .and_then(|a| a.get(i))
                        .cloned()
                        .unwrap_or_default();
                    let option = doc.resolve(&option).map_err(error)?;
                    values.push(match option.as_array() {
                        Some(pair) if pair.len() == 2 => json!([
                            text(&doc.resolve(&pair[0]).map_err(error)?),
                            text(&doc.resolve(&pair[1]).map_err(error)?)
                        ]),
                        _ => json!(text(&option)),
                    });
                }
                item.insert(
                    "options".into(),
                    if values.is_empty() {
                        Value::Null
                    } else {
                        Value::Array(values)
                    },
                );
            }
            fields.push(Value::Object(item));
        }
    }
    let mut output = result("form-list", &path, Vec::new());
    output.insert("xfa".into(), json!(xfa));
    output.insert("field_count".into(), json!(fields.len()));
    output.insert("fields".into(), Value::Array(fields));
    Ok(output)
}

/// Register the form, annotation, and page-flattening commands.
pub fn register(registry: &mut Registry) {
    registry.family_verb(
        "form",
        Verb::new(
            Command::new("list")
                .about("list form widgets with page, bounds, value, and state")
                .arg(Arg::new("file").required(true)),
            list,
        ),
    );
    forms::register(registry);
    annots::register(registry);
    flatten::register(registry);
}
