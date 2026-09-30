//! Flex/grid computed values and intrinsic-content layout with Taffy.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;
use std::str::FromStr;

use super::css::{Cv, Token};
use super::style::{Len, Style, apply, components, keyword, length, single, spec};

pub const LONGHANDS: &[&str] = &[
    "flex-direction",
    "flex-wrap",
    "flex-grow",
    "flex-shrink",
    "flex-basis",
    "align-items",
    "align-self",
    "align-content",
    "justify-items",
    "justify-self",
    "justify-content",
    "row-gap",
    "column-gap",
    "grid-template-columns",
    "grid-template-rows",
    "grid-template-areas",
    "grid-auto-columns",
    "grid-auto-rows",
    "grid-auto-flow",
    "grid-column-start",
    "grid-column-end",
    "grid-row-start",
    "grid-row-end",
];

pub fn expand<'a>(
    name: &str,
    value: &'a [Cv],
    wide: bool,
    out: &mut Vec<(&'static str, &'a [Cv])>,
) -> bool {
    const INITIAL: &[Cv] = &[];
    const ONE: &[Cv] = &[Cv::T(Token::Number(1.0))];
    const ZERO: &[Cv] = &[Cv::T(Token::Number(0.0))];
    const BASIS: &[Cv] = &[Cv::T(Token::Percentage(0.0))];
    match name {
        "flex" => {
            let (mut grow, mut shrink, mut basis) = (ONE, ONE, BASIS);
            if wide {
                (grow, shrink, basis) = (value, value, value);
            } else {
                match keyword(value).as_deref() {
                    Some("none") => {
                        grow = ZERO;
                        shrink = ZERO;
                        basis = INITIAL;
                    }
                    Some("auto") => basis = INITIAL,
                    _ => {
                        let parts = components(value);
                        let mut numbers = 0;
                        for part in parts {
                            if matches!(single(part), Some(Cv::T(Token::Number(n))) if *n >= 0.0)
                                && numbers < 2
                            {
                                if numbers == 0 {
                                    grow = part;
                                } else {
                                    shrink = part;
                                }
                                numbers += 1;
                            } else {
                                basis = part;
                            }
                        }
                    }
                }
            }
            out.extend([
                ("flex-grow", grow),
                ("flex-shrink", shrink),
                ("flex-basis", basis),
            ]);
        }
        "flex-flow" => {
            let (mut direction, mut wrap) = (INITIAL, INITIAL);
            if wide {
                (direction, wrap) = (value, value);
            } else {
                for part in components(value) {
                    match keyword(part).as_deref() {
                        Some("row" | "row-reverse" | "column" | "column-reverse") => {
                            direction = part
                        }
                        Some("nowrap" | "wrap" | "wrap-reverse") => wrap = part,
                        _ => return true,
                    }
                }
            }
            out.extend([("flex-direction", direction), ("flex-wrap", wrap)]);
        }
        "gap" | "grid-gap" | "place-items" | "place-self" | "place-content" => {
            let names = match name {
                "place-items" => ["align-items", "justify-items"],
                "place-self" => ["align-self", "justify-self"],
                "place-content" => ["align-content", "justify-content"],
                _ => ["row-gap", "column-gap"],
            };
            let parts = if wide { vec![value] } else { components(value) };
            if let Some(&first) = parts.first() {
                out.extend([
                    (names[0], first),
                    (names[1], parts.get(1).copied().unwrap_or(first)),
                ]);
            }
        }
        "grid-row" | "grid-column" => {
            let parts: Vec<_> = value
                .split(|cv| matches!(cv, Cv::T(Token::Delim('/'))))
                .collect();
            let first = parts.first().copied().unwrap_or(INITIAL);
            let end = parts.get(1).copied().unwrap_or_else(|| {
                if keyword(first).is_some_and(|s| s != "auto") {
                    first
                } else {
                    INITIAL
                }
            });
            let names = if name == "grid-row" {
                ["grid-row-start", "grid-row-end"]
            } else {
                ["grid-column-start", "grid-column-end"]
            };
            out.extend([
                (names[0], first),
                (names[1], if wide { value } else { end }),
            ]);
        }
        "grid-area" => {
            let parts: Vec<_> = value
                .split(|cv| matches!(cv, Cv::T(Token::Delim('/'))))
                .collect();
            let first = parts.first().copied().unwrap_or(INITIAL);
            for (index, key) in [
                "grid-row-start",
                "grid-column-start",
                "grid-row-end",
                "grid-column-end",
            ]
            .into_iter()
            .enumerate()
            {
                let part = parts.get(index).copied().unwrap_or_else(|| {
                    if keyword(first).is_some_and(|s| s != "auto") {
                        first
                    } else {
                        INITIAL
                    }
                });
                out.push((key, if wide { value } else { part }));
            }
        }
        _ => return false,
    }
    true
}

fn css_text(value: &[Cv], em: f32, rem: f32) -> String {
    fn append(out: &mut String, value: &[Cv], em: f32, rem: f32) {
        for cv in value {
            match cv {
                Cv::T(Token::Ident(s)) => out.push_str(s),
                Cv::T(Token::Number(n)) => {
                    let _ = write!(out, "{n}");
                }
                Cv::T(Token::Percentage(n)) => {
                    let _ = write!(out, "{n}%");
                }
                Cv::T(Token::Dimension(n, unit)) => {
                    if let Some(Len::Px(px)) = length(cv, em, rem) {
                        let _ = write!(out, "{px}px");
                    } else {
                        let _ = write!(out, "{n}{unit}");
                    }
                }
                Cv::T(Token::Whitespace) => out.push(' '),
                Cv::T(Token::Comma) => out.push(','),
                Cv::T(Token::Delim(ch)) => out.push(*ch),
                Cv::Func(name, inner) => {
                    out.push_str(name);
                    out.push('(');
                    append(out, inner, em, rem);
                    out.push(')');
                }
                Cv::Block('[', inner) => {
                    out.push('[');
                    append(out, inner, em, rem);
                    out.push(']');
                }
                _ => out.push('\0'),
            }
        }
    }
    let mut text = String::new();
    append(&mut text, value, em, rem);
    text
}

fn parsed<T: FromStr>(value: &[Cv], em: f32, rem: f32) -> Option<T> {
    css_text(value, em, rem).parse().ok()
}

fn areas(value: &[Cv]) -> Option<Option<taffy::GridTemplateAreas<String>>> {
    if keyword(value).as_deref() == Some("none") {
        return Some(None);
    }
    let mut rows = Vec::new();
    for cv in value.iter().filter(|cv| !cv.is_whitespace()) {
        let Cv::T(Token::Str(row)) = cv else {
            return None;
        };
        rows.push(row.split_whitespace().collect::<Vec<_>>());
    }
    let width = rows.first()?.len();
    if width == 0 || width * rows.len() > 4096 || rows.iter().any(|row| row.len() != width) {
        return None;
    }
    let mut named: HashMap<&str, [usize; 4]> = HashMap::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, &name) in row.iter().enumerate() {
            if name.chars().all(|ch| ch == '.') {
                continue;
            }
            let area = named.entry(name).or_insert([r, c, r + 1, c + 1]);
            area[2] = r + 1;
            area[3] = area[3].max(c + 1);
        }
    }
    let mut areas = Vec::with_capacity(named.len());
    for (name, [r0, c0, r1, c1]) in named {
        if rows[r0..r1]
            .iter()
            .any(|row| row[c0..c1].iter().any(|&cell| cell != name))
        {
            return None;
        }
        areas.push(taffy::GridTemplateArea {
            name: name.to_owned(),
            row_start: (r0 + 1) as u16,
            row_end: (r1 + 1) as u16,
            column_start: (c0 + 1) as u16,
            column_end: (c1 + 1) as u16,
        });
    }
    Some(Some(taffy::GridTemplateAreas {
        areas,
        row_count: rows.len() as u16,
        column_count: width as u16,
    }))
}

pub fn compute(
    specified: &HashMap<&'static str, &[Cv]>,
    parent: &Style,
    em: f32,
    rem: f32,
) -> Option<Rc<taffy::Style>> {
    if !LONGHANDS.iter().any(|key| specified.contains_key(key)) {
        return None;
    }
    let mut out = taffy::Style::default();
    let default = taffy::Style::default();
    let parent = parent.formatting.as_deref().unwrap_or(&default);
    let get = |key| spec(specified, key, false);
    apply(
        &mut out.flex_direction,
        get("flex-direction"),
        &parent.flex_direction,
        default.flex_direction,
        |v| {
            Some(match keyword(v)?.as_str() {
                "row" => taffy::FlexDirection::Row,
                "row-reverse" => taffy::FlexDirection::RowReverse,
                "column" => taffy::FlexDirection::Column,
                "column-reverse" => taffy::FlexDirection::ColumnReverse,
                _ => return None,
            })
        },
    );
    apply(
        &mut out.flex_wrap,
        get("flex-wrap"),
        &parent.flex_wrap,
        default.flex_wrap,
        |v| {
            Some(match keyword(v)?.as_str() {
                "nowrap" => taffy::FlexWrap::NoWrap,
                "wrap" => taffy::FlexWrap::Wrap,
                "wrap-reverse" => taffy::FlexWrap::WrapReverse,
                _ => return None,
            })
        },
    );
    let number = |v: &[Cv]| match single(v)? {
        Cv::T(Token::Number(n)) if *n >= 0.0 => Some(*n),
        _ => None,
    };
    apply(
        &mut out.flex_grow,
        get("flex-grow"),
        &parent.flex_grow,
        0.0,
        number,
    );
    apply(
        &mut out.flex_shrink,
        get("flex-shrink"),
        &parent.flex_shrink,
        1.0,
        number,
    );
    apply(
        &mut out.flex_basis,
        get("flex-basis"),
        &parent.flex_basis,
        default.flex_basis,
        |v| parsed(v, em, rem),
    );
    apply(
        &mut out.align_items,
        get("align-items"),
        &parent.align_items,
        None,
        |v| parsed(v, em, rem).map(Some),
    );
    apply(
        &mut out.align_self,
        get("align-self"),
        &parent.align_self,
        None,
        |v| {
            if keyword(v).as_deref() == Some("auto") {
                Some(None)
            } else {
                parsed(v, em, rem).map(Some)
            }
        },
    );
    apply(
        &mut out.align_content,
        get("align-content"),
        &parent.align_content,
        None,
        |v| parsed(v, em, rem).map(Some),
    );
    apply(
        &mut out.justify_items,
        get("justify-items"),
        &parent.justify_items,
        None,
        |v| parsed(v, em, rem).map(Some),
    );
    apply(
        &mut out.justify_self,
        get("justify-self"),
        &parent.justify_self,
        None,
        |v| {
            if keyword(v).as_deref() == Some("auto") {
                Some(None)
            } else {
                parsed(v, em, rem).map(Some)
            }
        },
    );
    apply(
        &mut out.justify_content,
        get("justify-content"),
        &parent.justify_content,
        None,
        |v| parsed(v, em, rem).map(Some),
    );
    apply(
        &mut out.gap.width,
        get("column-gap"),
        &parent.gap.width,
        default.gap.width,
        |v| parsed(v, em, rem),
    );
    apply(
        &mut out.gap.height,
        get("row-gap"),
        &parent.gap.height,
        default.gap.height,
        |v| parsed(v, em, rem),
    );
    type Tracks = taffy::GridTemplateTracks<String, taffy::GridTemplateComponent<String>>;
    for (key, tracks, names, old_tracks, old_names) in [
        (
            "grid-template-columns",
            &mut out.grid_template_columns,
            &mut out.grid_template_column_names,
            &parent.grid_template_columns,
            &parent.grid_template_column_names,
        ),
        (
            "grid-template-rows",
            &mut out.grid_template_rows,
            &mut out.grid_template_row_names,
            &parent.grid_template_rows,
            &parent.grid_template_row_names,
        ),
    ] {
        let mut result = (tracks.clone(), names.clone());
        apply(
            &mut result,
            get(key),
            &(old_tracks.clone(), old_names.clone()),
            (Vec::new(), Vec::new()),
            |v| {
                if keyword(v).as_deref() == Some("none") {
                    Some((Vec::new(), Vec::new()))
                } else {
                    parsed::<Tracks>(v, em, rem).map(|t| (t.tracks, t.line_names))
                }
            },
        );
        (*tracks, *names) = result;
    }
    apply(
        &mut out.grid_template_areas,
        get("grid-template-areas"),
        &parent.grid_template_areas,
        None,
        areas,
    );
    apply(
        &mut out.grid_auto_columns,
        get("grid-auto-columns"),
        &parent.grid_auto_columns,
        Vec::new(),
        |v| parsed::<taffy::GridAutoTracks>(v, em, rem).map(|t| t.0),
    );
    apply(
        &mut out.grid_auto_rows,
        get("grid-auto-rows"),
        &parent.grid_auto_rows,
        Vec::new(),
        |v| parsed::<taffy::GridAutoTracks>(v, em, rem).map(|t| t.0),
    );
    apply(
        &mut out.grid_auto_flow,
        get("grid-auto-flow"),
        &parent.grid_auto_flow,
        default.grid_auto_flow,
        |v| parsed(v, em, rem),
    );
    apply(
        &mut out.grid_column.start,
        get("grid-column-start"),
        &parent.grid_column.start,
        taffy::GridPlacement::Auto,
        |v| parsed(v, em, rem),
    );
    apply(
        &mut out.grid_column.end,
        get("grid-column-end"),
        &parent.grid_column.end,
        taffy::GridPlacement::Auto,
        |v| parsed(v, em, rem),
    );
    apply(
        &mut out.grid_row.start,
        get("grid-row-start"),
        &parent.grid_row.start,
        taffy::GridPlacement::Auto,
        |v| parsed(v, em, rem),
    );
    apply(
        &mut out.grid_row.end,
        get("grid-row-end"),
        &parent.grid_row.end,
        taffy::GridPlacement::Auto,
        |v| parsed(v, em, rem),
    );
    Some(Rc::new(out))
}
