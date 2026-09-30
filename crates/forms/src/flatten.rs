use std::collections::HashSet;

use clap::ArgMatches;
use goat_common::args::required;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Matrix, Object, Rect, Stream};
use serde_json::{Map, Value, json};

use crate::appearance::{child_dict, dict, install, literal, store_child};
use crate::{error, inherited, open, result, text};

fn merge_resources(doc: &Document, target: &mut Dict, source: &Dict) -> Result<(), GoatError> {
    for (key, value) in source.iter() {
        let value = doc.resolve(value).map_err(error)?;
        let old = doc.resolve_key(target, key.as_bytes()).map_err(error)?;
        match (old, value) {
            (Object::Dict(mut old), Object::Dict(value)) => {
                for (name, item) in value.iter() {
                    if !old.contains_key(name.as_bytes()) {
                        old.insert(name.clone(), item.clone());
                    }
                }
                target.insert(key.clone(), old);
            }
            (Object::Array(mut old), Object::Array(value)) => {
                for item in value {
                    if !old.contains(&item) {
                        old.push(item);
                    }
                }
                target.insert(key.clone(), Object::Array(old));
            }
            (Object::Null, value) => {
                target.insert(key.clone(), value);
            }
            _ => {}
        }
    }
    Ok(())
}

fn normal(doc: &Document, annotation: &Dict) -> Result<Object, GoatError> {
    let ap = child_dict(doc, annotation, b"AP")?;
    let object = ap.get(b"N").cloned().unwrap_or_default();
    let resolved = doc.resolve(&object).map_err(error)?;
    if resolved.as_stream().is_some() {
        return Ok(object);
    }
    if let Some(states) = resolved.as_dict()
        && let Some(state) = annotation.get_name(b"AS")
    {
        return Ok(states.get(state).cloned().unwrap_or_default());
    }
    Ok(Object::Null)
}

fn qnum(value: f64) -> String {
    if value.abs() < 0.00001 {
        return "0".into();
    }
    let result = format!("{value:.5}");
    result
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn placement(annotation: &Dict, appearance: &Dict, rotate: u16) -> Option<Matrix> {
    if annotation.get_i64(b"F").unwrap_or(0) & 3 != 0 {
        return None;
    }
    let mut rect = annotation
        .get_array(b"Rect")
        .and_then(Rect::from_array)?
        .normalized();
    let bbox = appearance
        .get_array(b"BBox")
        .and_then(Rect::from_array)?
        .normalized();
    let mut matrix = appearance
        .get_array(b"Matrix")
        .and_then(Matrix::from_array)
        .unwrap_or(Matrix::IDENTITY);
    let no_rotate = rotate != 0 && annotation.get_i64(b"F").unwrap_or(0) & 16 != 0;
    let rotation = Matrix::rotate(f64::from(rotate));
    if no_rotate {
        matrix = matrix.concat(&rotation);
        rect = crate::fixed_annotation_rect(rect, annotation.get_i64(b"F").unwrap_or(0), rotate);
    }
    let target = bbox.transform(&matrix);
    if target.width() == 0.0 || target.height() == 0.0 {
        return None;
    }
    let matrix = Matrix::translate(-target.x0, -target.y0)
        .concat(&Matrix::scale(
            rect.width() / target.width(),
            rect.height() / target.height(),
        ))
        .concat(&Matrix::translate(rect.x0, rect.y0));
    Some(if no_rotate {
        rotation.concat(&matrix)
    } else {
        matrix
    })
}

fn generate(doc: &mut Document, acro: &Dict) -> Result<(), GoatError> {
    let dr = child_dict(doc, acro, b"DR")?;
    for page_index in 0..doc.page_count().map_err(error)? {
        for widget in crate::page_widgets(doc, page_index)? {
            let mut field = dict(doc, &Object::Reference(widget))?;
            match inherited(doc, &field, b"FT")?.as_name() {
                Some(b"Btn") => {
                    let value = inherited(doc, &field, b"V")?;
                    let state = value.as_name().unwrap_or(b"Off");
                    let ap = child_dict(doc, &field, b"AP")?;
                    let states = child_dict(doc, &ap, b"N")?;
                    field.insert(
                        "AS",
                        Object::name(if states.contains_key(state) {
                            state
                        } else {
                            b"Off"
                        }),
                    );
                    doc.set(widget, field);
                }
                Some(b"Tx" | b"Ch") => {
                    let rect = field
                        .get_array(b"Rect")
                        .and_then(Rect::from_array)
                        .ok_or_else(|| GoatError::message("widget has no rectangle"))?
                        .normalized();
                    let old = normal(doc, &field)?;
                    let old = doc.resolve(&old).map_err(error)?;
                    let mut dictionary =
                        old.as_stream().map(|s| s.dict.clone()).unwrap_or_default();
                    let bbox = dictionary
                        .get_array(b"BBox")
                        .and_then(Rect::from_array)
                        .unwrap_or(Rect::new(0.0, 0.0, rect.width(), rect.height()));
                    let mut resources = child_dict(doc, &dictionary, b"Resources")?;
                    merge_resources(doc, &mut resources, &dr)?;
                    let da = inherited(doc, &field, b"DA")?;
                    let da = if da.is_null() {
                        text(&doc.resolve_key(acro, b"DA").map_err(error)?)
                    } else {
                        text(&da)
                    };
                    let tokens: Vec<_> = da.split_whitespace().collect();
                    let tf = tokens.iter().position(|t| *t == "Tf");
                    let size = tf
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| tokens[i].parse::<f64>().ok())
                        .filter(|v| *v > 1.0 && *v < 1000.0)
                        .unwrap_or(11.0);
                    let value = text(&inherited(doc, &field, b"V")?);
                    let flags = inherited(doc, &field, b"Ff")?.as_i64().unwrap_or(0);
                    let listbox = inherited(doc, &field, b"FT")?.as_name() == Some(b"Ch")
                        && flags & 131072 == 0;
                    let options = inherited(doc, &field, b"Opt")?;
                    let lines = if listbox {
                        options
                            .as_array()
                            .unwrap_or(&[])
                            .iter()
                            .map(|o| {
                                o.as_array()
                                    .and_then(|p| p.get(1))
                                    .map(text)
                                    .unwrap_or_else(|| text(o))
                            })
                            .collect::<Vec<_>>()
                    } else {
                        vec![value.clone()]
                    };
                    let leading = 1.2 * size;
                    let mut y = bbox.y1 - (bbox.height() - lines.len() as f64 * leading) / 2.0;
                    let mut body = Vec::new();
                    if listbox {
                        for (i, line) in lines.iter().enumerate() {
                            if line == &value {
                                body.extend_from_slice(
                                    format!(
                                        "q\n0.85 0.85 0.85 rg\n{} {} {} {} re f\nQ\n",
                                        qnum(bbox.x0),
                                        qnum(bbox.y0 + y - leading * (i + 1) as f64),
                                        qnum(bbox.width()),
                                        qnum(leading)
                                    )
                                    .as_bytes(),
                                );
                            }
                        }
                    }
                    y -= size;
                    body.extend_from_slice(format!("q\nBT\n{da}\n").as_bytes());
                    for (i, line) in lines.iter().enumerate() {
                        body.extend_from_slice(
                            if i == 0 {
                                format!("{} {} Td\n", qnum(bbox.x0 + 1.0), qnum(bbox.y0 + y))
                            } else {
                                format!("0 {} Td\n", qnum(-leading))
                            }
                            .as_bytes(),
                        );
                        body.extend(literal(line));
                        body.extend_from_slice(b" Tj\n");
                    }
                    body.extend_from_slice(b"ET\nQ\nEMC");
                    let original = if let Some(old) = old.as_stream() {
                        doc.decode_stream(old).map_err(error)?.data
                    } else {
                        b"/Tx BMC\nEMC\n".to_vec()
                    };
                    let start = original.windows(3).position(|w| w == b"BMC");
                    let mut contents = Vec::new();
                    if let Some(start) = start {
                        let end = original[start + 3..]
                            .windows(3)
                            .position(|w| w == b"EMC")
                            .map(|i| i + start + 3);
                        contents.extend_from_slice(&original[..start + 3]);
                        contents.push(b'\n');
                        contents.extend(body);
                        if let Some(end) = end {
                            contents.extend_from_slice(&original[end + 3..]);
                        }
                    } else {
                        contents.extend(original);
                        contents.extend_from_slice(b"\n/Tx BMC\n");
                        contents.extend(body);
                    }
                    dictionary.remove(b"Filter");
                    dictionary.remove(b"DecodeParms");
                    dictionary.insert("Type", Object::name("XObject"));
                    dictionary.insert("Subtype", Object::name("Form"));
                    dictionary.insert("BBox", bbox.to_object());
                    dictionary.insert("Resources", resources);
                    let id = doc.add(Stream::new(dictionary, contents));
                    install(doc, widget, Object::Reference(id))?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Paint normal appearances into page content, removing the corresponding annotations.
pub fn flatten(doc: &mut Document, generate_if_needed: bool) -> Result<(), GoatError> {
    flatten_appearances(doc, generate_if_needed, false)
}

fn flatten_appearances(
    doc: &mut Document,
    generate_if_needed: bool,
    preserve_links: bool,
) -> Result<(), GoatError> {
    let mut catalog = doc.catalog().map_err(error)?;
    let mut acro = child_dict(doc, &catalog, b"AcroForm")?;
    let mut need = acro.get_bool(b"NeedAppearances").unwrap_or(false);
    if need && generate_if_needed {
        generate(doc, &acro)?;
        acro.remove(b"NeedAppearances");
        store_child(doc, &mut catalog, "AcroForm", Object::Dict(acro.clone()));
        need = false;
    }
    let defaults = child_dict(doc, &acro, b"DR")?;
    for index in 0..doc.page_count().map_err(error)? {
        let page = doc.page(index).map_err(error)?;
        let rotation = page.rotation();
        let mut page_dict = page.dict;
        let annotations = doc.resolve_key(&page_dict, b"Annots").map_err(error)?;
        let annotations = annotations.as_array().unwrap_or(&[]);
        let mut resources = page.resources;
        let mut xobjects = child_dict(doc, &resources, b"XObject")?;
        let mut names = HashSet::new();
        for (_, value) in resources.iter() {
            if let Some(d) = doc.resolve(value).map_err(error)?.as_dict() {
                names.extend(d.keys().map(|n| n.as_bytes().to_vec()));
            }
        }
        let mut kept = Vec::new();
        let mut content = Vec::new();
        let mut suffix = 1;
        for object in annotations {
            let annot = dict(doc, object)?;
            let widget = annot.get_name(b"Subtype") == Some(b"Widget");
            let link = preserve_links && annot.get_name(b"Subtype") == Some(b"Link");
            if link || (need && widget) || doc.resolve_key(&annot, b"AP").map_err(error)?.is_null()
            {
                kept.push(object.clone());
                continue;
            }
            let appearance = normal(doc, &annot)?;
            let Some(mut appearance_stream) = doc.resolve_stream(&appearance).map_err(error)?
            else {
                continue;
            };
            if widget {
                let mut res = child_dict(doc, &appearance_stream.dict, b"Resources")?;
                merge_resources(doc, &mut res, &defaults)?;
                appearance_stream.dict.insert("Resources", res);
            }
            let Some(matrix) = placement(&annot, &appearance_stream.dict, rotation) else {
                continue;
            };
            appearance_stream
                .dict
                .insert("Subtype", Object::name("Form"));
            let appearance_id = if let Some(id) = appearance.as_reference() {
                doc.set(id, appearance_stream);
                id
            } else {
                doc.add(appearance_stream)
            };
            let name = loop {
                let name = format!("Fxo{suffix}");
                suffix += 1;
                if names.insert(name.as_bytes().to_vec()) {
                    break name;
                }
            };
            xobjects.insert(name.as_str(), Object::Reference(appearance_id));
            content.extend_from_slice(
                format!(
                    "q\n{} cm\n/{name} Do\nQ\n",
                    [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]
                        .map(qnum)
                        .join(" ")
                )
                .as_bytes(),
            );
        }
        if kept.len() != annotations.len() {
            if kept.is_empty() {
                page_dict.remove(b"Annots");
            } else {
                store_child(doc, &mut page_dict, "Annots", Object::Array(kept));
            }
            let before = doc.add(Stream::new(Dict::new(), b"q\n".to_vec()));
            let mut contents = vec![Object::Reference(before)];
            if let Some(original) = page_dict.get(b"Contents") {
                let resolved = doc.resolve(original).map_err(error)?;
                if let Some(array) = resolved.as_array() {
                    contents.extend_from_slice(array);
                } else if !resolved.is_null() {
                    contents.push(original.clone());
                }
            }
            let mut after = b"\nQ\n".to_vec();
            after.extend(content);
            contents.push(Object::Reference(doc.add(Stream::new(Dict::new(), after))));
            page_dict.insert("Contents", Object::Array(contents));
        }
        if !xobjects.is_empty() {
            resources.insert("XObject", xobjects);
        }
        page_dict.insert("Resources", resources);
        doc.set(page.id, page_dict);
    }
    if !need {
        catalog.remove(b"AcroForm");
    }
    let id = doc.catalog_ref().map_err(error)?;
    doc.set(id, catalog);
    Ok(())
}

fn pages(matches: &ArgMatches, _: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let (mut doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
    let output = crate::forms::output(matches, &source, "flattened", "pdf")?;
    flatten_appearances(&mut doc, true, true)?;
    crate::forms::save(&doc, &output, true)?;
    let mut output = result(
        "pages-flatten",
        &source,
        vec![output.to_string_lossy().into_owned()],
    );
    output.insert("flattened".into(), json!(["annotations", "form_fields"]));
    Ok(output)
}
pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "pages",
        Verb::new(
            crate::forms::out(crate::forms::base(
                "flatten",
                "flatten annotations and form appearances into page content",
            )),
            pages,
        ),
    );
}
