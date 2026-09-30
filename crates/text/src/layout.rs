use crate::hits::round;
use crate::{TextPage, Word};
use serde_json::{Value, json};

#[derive(Clone)]
struct Row {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    text: String,
}

impl Row {
    fn value(&self, column: Option<usize>) -> Value {
        let mut value =
            json!({"x0":self.x0,"y0":self.y0,"x1":self.x1,"y1":self.y1,"text":self.text});
        if let Some(column) = column {
            value["column"] = column.into();
        }
        value
    }
}

fn ordered<'a>(words: &[&'a Word]) -> Vec<&'a Word> {
    let mut result = words.to_vec();
    result.sort_by(|a, b| {
        a.rect
            .y0
            .total_cmp(&b.rect.y0)
            .then(a.rect.x0.total_cmp(&b.rect.x0))
    });
    result
}

fn rows(words: &[&Word]) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for word in ordered(words) {
        let text = goat_common::py::strip(&word.text);
        if text.is_empty() {
            continue;
        }
        if let Some(row) = rows
            .last_mut()
            .filter(|r| (word.rect.y0 - r.y0).abs() <= 3.0)
        {
            row.x0 = row.x0.min(word.rect.x0);
            row.x1 = row.x1.max(word.rect.x1);
            row.y1 = row.y1.max(word.rect.y1);
            row.text.push(' ');
            row.text.push_str(text);
        } else {
            rows.push(Row {
                x0: word.rect.x0,
                y0: word.rect.y0,
                x1: word.rect.x1,
                y1: word.rect.y1,
                text: text.to_owned(),
            });
        }
    }
    for row in &mut rows {
        row.x0 = round(row.x0, 2);
        row.y0 = round(row.y0, 2);
        row.x1 = round(row.x1, 2);
        row.y1 = round(row.y1, 2);
    }
    rows
}

// PyMuPDF's page-level sort preserves physical gaps, unlike the
// TextPage serializer's block sort. Word rectangles own the line height.
pub(crate) fn sorted_text(words: &[Word]) -> String {
    let mut sorted: Vec<&Word> = words.iter().collect();
    sorted.sort_by(|a, b| {
        a.rect
            .y1
            .total_cmp(&b.rect.y1)
            .then(a.rect.x0.total_cmp(&b.rect.x0))
    });
    let left = words
        .iter()
        .map(|w| w.rect.x0)
        .reduce(f64::min)
        .unwrap_or(0.0);
    let mut lines: Vec<Vec<&Word>> = Vec::new();
    for word in sorted {
        if let Some(line) = lines.last_mut().filter(|line| {
            let first = line[0];
            (word.rect.y0 - first.rect.y0).abs() <= 3.0
                || (word.rect.y1 - first.rect.y1).abs() <= 3.0
        }) {
            line.push(word);
        } else {
            lines.push(vec![word]);
        }
    }
    let mut text = String::new();
    let mut previous_bottom: Option<f64> = None;
    for mut line in lines {
        line.sort_by(|a, b| a.rect.x0.total_cmp(&b.rect.x0));
        let top = line
            .iter()
            .map(|w| w.rect.y0)
            .reduce(f64::min)
            .unwrap_or(0.0);
        let bottom = line
            .iter()
            .map(|w| w.rect.y1)
            .reduce(f64::max)
            .unwrap_or(top);
        if let Some(previous) = previous_bottom {
            let height = bottom - top;
            let count = if height > 0.0 {
                ((bottom - previous) / height).round_ties_even().max(0.0) as usize
            } else {
                0
            };
            text.extend(std::iter::repeat_n('\n', count.min(6)));
        }
        let mut right = left;
        for (index, word) in line.into_iter().enumerate() {
            let width = (word.rect.x1 - word.rect.x0) / word.text.chars().count() as f64;
            let spaces = if width > 0.0 {
                ((word.rect.x0 - right) / width).round_ties_even().max(0.0) as usize
            } else {
                0
            };
            text.extend(std::iter::repeat_n(
                ' ',
                spaces.max(usize::from(index != 0 && word.rect.x0 > right)),
            ));
            text.push_str(&word.text);
            right = word.rect.x1;
        }
        previous_bottom = Some(bottom);
    }
    text
}

pub(crate) fn extract_page_layout(
    doc: &pdf_core::Document,
    index: usize,
) -> Result<Value, crate::TextError> {
    let mut page = doc.page(index)?;
    let displayed = pdf_interp::page_bounds(&page);
    page.dict.insert("Rotate", 0_i64);
    let rect = pdf_interp::page_bounds(&page);
    let mut text = crate::extract(doc, &page, rect, crate::TextFlags::TEXT)?;
    // The reference uses displayed width for column detection, while
    // extraction rectangles remain in unrotated crop coordinates.
    text.rect = displayed;
    Ok(page_layout(&text))
}

/// `layout.extract_page_layout`: 24pt connected horizontal clusters,
/// 3pt line tolerance, and full-width lines inserted between body bands.
pub fn page_layout(page: &TextPage) -> Value {
    let all = page.words();
    let words: Vec<&Word> = all
        .iter()
        .filter(|w| !goat_common::py::strip(&w.text).is_empty())
        .collect();
    let mut physical: Vec<Vec<&Word>> = Vec::new();
    for word in ordered(&words) {
        if let Some(line) = physical
            .last_mut()
            .filter(|line| (word.rect.y0 - line[0].rect.y0).abs() <= 3.0)
        {
            line.push(word);
        } else {
            physical.push(vec![word]);
        }
    }
    let mut body = Vec::new();
    let mut spanning = Vec::new();
    for mut line in physical {
        line.sort_by(|a, b| a.rect.x0.total_cmp(&b.rect.x0));
        let width =
            line.last().map_or(0.0, |w| w.rect.x1) - line.first().map_or(0.0, |w| w.rect.x0);
        let gap = line
            .windows(2)
            .map(|w| w[1].rect.x0 - w[0].rect.x1)
            .reduce(f64::max)
            .unwrap_or(0.0);
        if width > (page.rect.x1 - page.rect.x0) * 0.55 && gap <= 24.0 {
            spanning.extend(line);
        } else {
            body.extend(line);
        }
    }
    if body.is_empty() {
        body = words.clone();
        spanning.clear();
    }
    let body_top = body
        .iter()
        .map(|w| w.rect.y0)
        .reduce(f64::min)
        .unwrap_or(0.0);
    let mut intervals: Vec<(f64, f64)> = body.iter().map(|w| (w.rect.x0, w.rect.x1)).collect();
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut groups: Vec<(f64, f64)> = Vec::new();
    for (x0, x1) in intervals {
        if let Some(last) = groups.last_mut().filter(|last| x0 - last.1 <= 24.0) {
            last.1 = last.1.max(x1);
        } else {
            groups.push((x0, x1));
        }
    }
    let mut columns = Vec::new();
    let mut column_rows = Vec::new();
    for (index, &(x0, x1)) in groups.iter().enumerate() {
        let mut selected: Vec<&Word> = body
            .iter()
            .copied()
            .filter(|w| w.rect.x0 >= x0 - 0.01 && w.rect.x0 <= x1 + 0.01)
            .collect();
        column_rows.push(rows(&selected));
        if index == 0 {
            selected.extend(spanning.iter().copied().filter(|w| w.rect.y0 < body_top));
        }
        if index + 1 == groups.len() {
            selected.extend(spanning.iter().copied().filter(|w| w.rect.y0 >= body_top));
        }
        let lines = rows(&selected);
        if lines.is_empty() {
            continue;
        }
        columns.push(json!({"x0":round(x0,2),"x1":round(x1,2),"text":lines.iter().map(|r|r.text.as_str()).collect::<Vec<_>>().join("\n"),
            "lines":lines.iter().map(|r|r.value(None)).collect::<Vec<_>>(),"word_count":selected.len(),"line_count":lines.len()}));
    }
    let mut reading = Vec::new();
    let text;
    if columns.len() <= 1 {
        text = sorted_text(&all);
        reading.extend(rows(&words).iter().map(|r| r.value(Some(1))));
        if let Some(column) = columns.first_mut() {
            column["text"] = text.clone().into();
        } else {
            columns.push(
                json!({"x0":0.0,"x1":0.0,"text":text,"lines":[],"word_count":0,"line_count":0}),
            );
        }
    } else {
        let mut lower = f64::NEG_INFINITY;
        for span in rows(&spanning) {
            for (i, rows) in column_rows.iter().enumerate() {
                reading.extend(
                    rows.iter()
                        .filter(|r| r.y0 >= lower && r.y0 < span.y0)
                        .map(|r| r.value(Some(i + 1))),
                );
            }
            lower = span.y0;
            reading.push(span.value(Some(0)));
        }
        for (i, rows) in column_rows.iter().enumerate() {
            reading.extend(
                rows.iter()
                    .filter(|r| r.y0 >= lower)
                    .map(|r| r.value(Some(i + 1))),
            );
        }
        text = reading
            .iter()
            .filter_map(|r| r["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    json!({"text":text,"column_count":columns.len(),"columns":columns,"reading_order":reading,"word_count":words.len()})
}
