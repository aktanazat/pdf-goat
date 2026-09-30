use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use goat_common::args::{optional, required};
use goat_common::paths::{default_out, ensure_parent, out_dir, resolve};
use goat_common::{Ctx, GoatError};
use pdf_core::{Document, SaveOptions};
use pdf_forms::WidgetType;
use pdf_text::{Block, TextFlags};
use serde_json::{Map, Value};

use crate::{EditError, edit_text, ooxml, plumber, pyfmt, redact};

fn source(matches: &ArgMatches) -> Result<PathBuf, GoatError> {
    resolve(required::<String>(matches, "file")?)
}

fn output(matches: &ArgMatches, src: &Path, suffix: &str, ext: &str) -> Result<PathBuf, GoatError> {
    let fallback;
    let path = match optional::<String>(matches, "output")?.filter(|s| !s.is_empty()) {
        Some(path) => path.as_str(),
        None => {
            fallback = default_out(&src.to_string_lossy(), suffix, ext)?;
            &fallback
        }
    };
    ensure_parent(path)
}

fn open(src: &Path) -> Result<Document, GoatError> {
    let doc = Document::open(src).map_err(EditError::from)?;
    if doc.needs_password() {
        return Err(GoatError::value_error("document closed or encrypted"));
    }
    Ok(doc)
}

fn receipt(verb: &str, src: &Path, outputs: &[PathBuf]) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("verb".into(), verb.into());
    result.insert("inputs".into(), serde_json::json!([src.to_string_lossy()]));
    result.insert(
        "outputs".into(),
        outputs
            .iter()
            .map(|p| Value::from(p.to_string_lossy().as_ref()))
            .collect(),
    );
    result
}

fn write(path: &Path, data: &[u8]) -> Result<(), GoatError> {
    fs::write(path, data).map_err(|e| GoatError::os(&e, path))
}

fn save(doc: &Document, path: &Path) -> Result<(), GoatError> {
    doc.save(
        path,
        &SaveOptions {
            compress_streams: true,
            object_streams: true,
            garbage_collect: true,
            ..SaveOptions::default()
        },
    )
    .map_err(EditError::from)?;
    Ok(())
}

pub(crate) fn redact(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = source(matches)?;
    let out = output(matches, &src, "redacted", "pdf")?;
    let find = required::<String>(matches, "find")?;
    let pattern = pdf_text::hit_pattern(find)?;
    let mut doc = open(&src)?;
    let mut widgets = Vec::new();
    let mut names = HashSet::new();
    for index in 0..doc.page_count().map_err(EditError::from)? {
        for widget in pdf_forms::page_widgets(&doc, index)? {
            let object = doc.get(widget).map_err(EditError::from)?;
            let Some(dict) = object.as_dict() else {
                continue;
            };
            if matches!(
                pdf_forms::widget_type(&doc, dict)?,
                WidgetType::Text | WidgetType::ComboBox
            ) {
                let name = pdf_forms::field_full_name(&doc, dict)?;
                if pattern.is_match(&pdf_forms::field_value_text(&doc, dict)?)? {
                    names.insert(name.clone());
                }
                widgets.push((widget, name));
            }
        }
    }
    let hits = (0..doc.page_count().map_err(EditError::from)?)
        .map(|index| pdf_text::page_hits(&pdf_text::page_words(&doc, index)?, &pattern))
        .collect::<Result<Vec<_>, GoatError>>()?;
    let fields = widgets
        .iter()
        .filter(|(_, name)| names.contains(name))
        .count();
    for (widget, name) in widgets {
        if names.remove(&name) {
            // Forms clears inherited values, reset/rich-text copies and every
            // matching widget appearance; a full rewrite removes stale objects.
            pdf_forms::clear_field_value(&mut doc, widget)?;
        }
    }
    let mut count = 0;
    for (index, rects) in hits.iter().enumerate() {
        if !rects.is_empty() {
            if count == 0 {
                redact::materialize_resources(&mut doc)?;
            }
            redact::apply(&mut doc, index, rects, [0.0; 3])?;
            count += rects.len();
        }
    }
    save(&doc, &out)?;
    let mut result = receipt("redact", &src, &[out]);
    result.insert("pattern".into(), find.clone().into());
    result.insert("redactions".into(), count.into());
    result.insert("field_redactions".into(), fields.into());
    Ok(result)
}

pub(crate) fn edit_text(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = source(matches)?;
    let out = output(matches, &src, "edited", "pdf")?;
    let find = required::<String>(matches, "find")?;
    let replace = required::<String>(matches, "replace")?;
    let mut doc = open(&src)?;
    let mut count = 0;
    for index in 0..doc.page_count().map_err(EditError::from)? {
        let text = pdf_text::extract_page(&doc, index, TextFlags::DICT)?;
        let mut edits = Vec::new();
        for block in &text.blocks {
            if let Block::Text(block) = block {
                for line in &block.lines {
                    for span in line.spans(&text) {
                        if span.text.contains(find) {
                            edits.push(edit_text::Replacement {
                                rect: span.bbox,
                                text: span.text.replace(find, replace),
                                size: span.size,
                                color: span.color,
                                origin: span.origin,
                            });
                        }
                    }
                }
            }
        }
        if !edits.is_empty() {
            let rects = edits.iter().map(|edit| edit.rect).collect::<Vec<_>>();
            if count == 0 {
                redact::materialize_resources(&mut doc)?;
            }
            redact::apply(&mut doc, index, &rects, [1.0; 3])?;
            for edit in &edits {
                edit_text::insert(&mut doc, index, edit)?;
            }
            count += edits.len();
        }
    }
    save(&doc, &out)?;
    let mut result = receipt("edit-text", &src, &[out]);
    result.insert("find".into(), find.clone().into());
    result.insert("replace".into(), replace.clone().into());
    result.insert("replacements".into(), count.into());
    result.insert(
        "note".into(),
        "Replaces simple text runs only. It does not reflow text or match embedded fonts.".into(),
    );
    Ok(result)
}

pub(crate) fn tables(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = source(matches)?;
    let dir = out_dir(
        optional::<String>(matches, "outdir")?.map(String::as_str),
        &src.to_string_lossy(),
        "tables",
    )?;
    let doc = open(&src)?;
    let mut outputs = Vec::new();
    for index in 0..doc.page_count().map_err(EditError::from)? {
        let page = doc.page(index).map_err(EditError::from)?;
        for (table_index, table) in plumber::plumber_page(&doc, &page)?
            .extract_tables()
            .iter()
            .enumerate()
        {
            let path = dir.join(format!("p{}_t{}.csv", index + 1, table_index + 1));
            let mut csv = String::new();
            for row in table {
                let fields = row.iter().map(Option::as_deref).collect::<Vec<_>>();
                csv.push_str(&pyfmt::csv_row(&fields));
            }
            write(&path, csv.as_bytes())?;
            outputs.push(path);
        }
    }
    let mut result = receipt("convert-tables", &src, &outputs);
    result.insert("tables".into(), outputs.len().into());
    Ok(result)
}

pub(crate) fn xlsx(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = source(matches)?;
    let out = output(matches, &src, "from-pdf", "xlsx")?;
    let doc = open(&src)?;
    let mut sheets = Vec::new();
    for index in 0..doc.page_count().map_err(EditError::from)? {
        let page = doc.page(index).map_err(EditError::from)?;
        let tables = plumber::plumber_page(&doc, &page)?.extract_tables();
        if !tables.is_empty() {
            let mut rows = Vec::new();
            for table in tables {
                for row in table {
                    let cells = row.into_iter().map(|cell| {
                        let mut value = cell.unwrap_or_default();
                        if let Some((offset, _)) = value.char_indices().nth(32767) { value.truncate(offset); }
                        if value.chars().any(|c| matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}')) {
                            return Err(GoatError::exception("IllegalCharacterError", format!("{value} cannot be used in worksheets.")));
                        }
                        Ok(value)
                    }).collect::<Result<Vec<_>, GoatError>>()?;
                    rows.push(Some(cells));
                }
                rows.push(None);
            }
            sheets.push((format!("page{}", index + 1), rows));
        }
    }
    let count = sheets.len();
    if sheets.is_empty() {
        sheets.push(("empty".into(), Vec::new()));
    }
    write(&out, &ooxml::xlsx(&sheets))?;
    let mut result = receipt("convert-xlsx", &src, &[out]);
    result.insert("sheets".into(), count.into());
    Ok(result)
}

pub(crate) fn docx(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = source(matches)?;
    let out = output(matches, &src, "from-pdf", "docx")?;
    let doc = open(&src)?;
    let mut pages = Vec::new();
    for index in 0..doc.page_count().map_err(EditError::from)? {
        pages.push(crate::word::page(&doc, index)?);
    }
    write(&out, &ooxml::docx(&pages))?;
    Ok(receipt("convert-docx", &src, &[out]))
}

pub(crate) fn pptx(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = source(matches)?;
    let out = output(matches, &src, "from-pdf", "pptx")?;
    let dpi = *required::<i64>(matches, "dpi")?;
    let doc = open(&src)?;
    let mut slides = Vec::new();
    for index in 0..doc.page_count().map_err(EditError::from)? {
        let page = doc.page(index).map_err(EditError::from)?;
        let bounds = pdf_interp::page_bounds(&page);
        let pixmap = pdf_render::render_page(
            &doc,
            &page,
            &pdf_render::RenderOptions {
                dpi: dpi as f64,
                alpha: false,
                annotations: true,
            },
        )
        .map_err(|e| GoatError::message(e.to_string()))?;
        let png = pdf_codec::encode_png(
            pixmap.data(),
            pixmap.width(),
            pixmap.height(),
            pdf_codec::PngColor::Rgba,
            Some((dpi as f64, dpi as f64)),
        )
        .map_err(|e| GoatError::message(e.to_string()))?;
        slides.push(ooxml::Slide {
            width_emu: (bounds.width() / 72.0 * 914400.0) as u64,
            height_emu: (bounds.height() / 72.0 * 914400.0) as u64,
            png,
        });
    }
    write(&out, &ooxml::pptx(&slides))?;
    let mut result = receipt("convert-pptx", &src, &[out]);
    result.insert("slides".into(), slides.len().into());
    Ok(result)
}
