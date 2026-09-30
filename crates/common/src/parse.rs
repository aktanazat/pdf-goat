//! Parsing page selections, colors, rectangles, and points.

use crate::error::GoatError;
use crate::py::{PyInt, parse_float, parse_hex, parse_int, repr_str, strip};

/// `parse_pages(spec, n)`: `'2-5,9'` (1-based, inclusive, ranges may run downward) into
/// 0-based indices in spec order, duplicates kept.
///
/// A part that is not a number or range fails at once; otherwise the first value outside
/// `1..=page_count`, in spec order, fails.
pub fn parse_pages(spec: &str, page_count: usize) -> Result<Vec<usize>, GoatError> {
    let mut spans = Vec::new();
    for part in spec.split(',') {
        let part = strip(part);
        if part.is_empty() {
            continue;
        }
        let number = |text: &str| {
            parse_int(text).map_err(|_| {
                GoatError::message(format!("{} is not a page number or range", repr_str(part)))
            })
        };
        spans.push(match part.split_once('-') {
            Some((first, last)) => (number(first)?, number(last)?),
            None => {
                let page = number(part)?;
                (page.clone(), page)
            }
        });
    }
    // Python materializes every range before it checks any value; checking each span's
    // endpoints finds the same first offender without building a huge range.
    let count = i64::try_from(page_count).unwrap_or(i64::MAX);
    let in_range = |page: &PyInt| {
        page.as_i64()
            .is_some_and(|value| (1..=count).contains(&value))
    };
    let mut pages = Vec::new();
    for (first, last) in &spans {
        if !in_range(first) {
            return Err(outside(first, page_count));
        }
        let (Some(start), last_value) = (first.as_i64(), last.as_i64()) else {
            return Err(outside(first, page_count));
        };
        let descending = last.is_negative() && last_value.is_none()
            || last_value.is_some_and(|value| value < start);
        if descending {
            if !in_range(last) {
                return Err(outside(&PyInt::Small(0), page_count));
            }
            pages.extend((last_value.unwrap_or(1)..=start).rev());
        } else {
            if !in_range(last) {
                return Err(outside(&PyInt::Small(count + 1), page_count));
            }
            pages.extend(start..=last_value.unwrap_or(count));
        }
    }
    Ok(pages
        .into_iter()
        .filter_map(|page| usize::try_from(page - 1).ok())
        .collect())
}

fn outside(page: &PyInt, page_count: usize) -> GoatError {
    GoatError::message(format!(
        "page {page} is outside the range 1 to {page_count}"
    ))
}

/// `page_indices(a, n)`: the parsed `--pages` spec, or every page when it is absent or empty.
pub fn page_indices(spec: Option<&str>, page_count: usize) -> Result<Vec<usize>, GoatError> {
    match spec {
        Some(spec) if !spec.is_empty() => parse_pages(spec, page_count),
        _ => Ok((0..page_count).collect()),
    }
}

/// `_selected_page(doc, page)`: the 0-based index of a 1-based `--page`.
pub fn selected_page(page: i64, page_count: usize) -> Result<usize, GoatError> {
    usize::try_from(page)
        .ok()
        .filter(|page| (1..=page_count).contains(page))
        .map(|page| page - 1)
        .ok_or_else(|| GoatError::message(format!("--page must be between 1 and {page_count}")))
}

/// `parse_color(spec, default)`: `None` for an absent or empty spec (the caller supplies the
/// default); `#rrggbb` into three channels over 255; otherwise up to three comma-separated
/// floats, every one converted first.
pub fn parse_color(spec: Option<&str>) -> Result<Option<Vec<f64>>, GoatError> {
    let Some(spec) = spec.filter(|spec| !spec.is_empty()) else {
        return Ok(None);
    };
    let spec = strip(spec);
    if spec.starts_with('#') {
        let hex: Vec<char> = spec.trim_start_matches('#').chars().collect();
        let mut channels = Vec::with_capacity(3);
        for start in [0, 2, 4] {
            let pair: String = hex.iter().skip(start).take(2).collect();
            channels.push(parse_hex(&pair)? as f64 / 255.0);
        }
        return Ok(Some(channels));
    }
    let mut channels = floats(spec)?;
    channels.truncate(3);
    Ok(Some(channels))
}

/// `parse_rect(spec)`: every comma-separated float converted, the first four kept.
pub fn parse_rect(spec: &str) -> Result<Vec<f64>, GoatError> {
    let mut values = floats(spec)?;
    values.truncate(4);
    Ok(values)
}

/// `parse_point(spec)`: exactly two comma-separated floats, unpacked lazily as Python does.
pub fn parse_point(spec: &str) -> Result<(f64, f64), GoatError> {
    let mut parts = spec.split(',');
    let mut next = || parts.next().map(parse_float).transpose();
    let x = next()?;
    let y = next()?;
    let (Some(x), Some(y)) = (x, y) else {
        return Err(GoatError::value_error(
            "not enough values to unpack (expected 2, got 1)",
        ));
    };
    if next()?.is_some() {
        return Err(GoatError::value_error(
            "too many values to unpack (expected 2)",
        ));
    }
    Ok((x, y))
}

fn floats(spec: &str) -> Result<Vec<f64>, GoatError> {
    spec.split(',').map(parse_float).collect()
}
