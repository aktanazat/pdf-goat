use std::collections::HashSet;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{float_value, int_value, optional, required};
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Matrix, Object, Point, Rect};
use pdf_font::Standard14;
use serde_json::{Map, Value, json};

use crate::appearance::{
    VariableText, circle_path, color, dict, font_resource, font_resources, literal, mu, numbers,
    store_child, stream, variable_text,
};
use crate::forms::{append_annot, base, next_name, out, output, page, save};
use crate::{error, open, rect_json, result, text};

type ResultMap = Result<Map<String, Value>, GoatError>;

fn expanded(r: Rect, amount: f64) -> Rect {
    Rect::new(r.x0 - amount, r.y0 - amount, r.x1 + amount, r.y1 + amount)
}
fn bounds(points: &[Point]) -> Rect {
    points.iter().fold(
        Rect::new(
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ),
        |r, p| Rect::new(r.x0.min(p.x), r.y0.min(p.y), r.x1.max(p.x), r.y1.max(p.y)),
    )
}
fn point_string(point: Point) -> String {
    format!("{} {}", mu(point.x), mu(point.y))
}
fn point_arg(matches: &ArgMatches, key: &str, inverse: &Matrix) -> Result<Point, GoatError> {
    let (x, y) = goat_common::parse::parse_point(required::<String>(matches, key)?)?;
    Ok(Point::new(x, y).transform(inverse))
}
fn rect_arg(matches: &ArgMatches, inverse: &Matrix) -> Result<Rect, GoatError> {
    let a = goat_common::parse::parse_rect(required::<String>(matches, "rect")?)?;
    Ok(Rect::new(a[0], a[1], a[2], a[3]).transform(inverse))
}
fn color_arg(matches: &ArgMatches, key: &str, fallback: &[f64]) -> Result<Vec<f64>, GoatError> {
    Ok(
        goat_common::parse::parse_color(optional::<String>(matches, key)?.map(String::as_str))?
            .unwrap_or_else(|| fallback.to_vec()),
    )
}
fn points_arg(matches: &ArgMatches) -> Result<Vec<Point>, GoatError> {
    required::<String>(matches, "points")?
        .split(';')
        .map(|s| goat_common::parse::parse_point(s).map(|(x, y)| Point::new(x, y)))
        .collect()
}

struct Context {
    doc: Document,
    source: String,
    index: usize,
    inverse: Matrix,
}
impl Context {
    fn new(matches: &ArgMatches) -> Result<Self, GoatError> {
        let (doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
        let index = goat_common::parse::selected_page(
            *required::<i64>(matches, "page")?,
            doc.page_count().map_err(error)?,
        )?;
        let inverse = crate::page_transform(&mut doc.page(index).map_err(error)?)
            .invert()
            .ok_or_else(|| GoatError::value_error("invalid page transform"))?;
        Ok(Self {
            doc,
            source,
            index,
            inverse,
        })
    }
    fn finish(self, matches: &ArgMatches, kind: &str) -> ResultMap {
        let path = output(matches, &self.source, kind, "pdf")?;
        save(&self.doc, &path, true)?;
        let mut result = result(
            &format!("annot-{kind}"),
            &self.source,
            vec![path.to_string_lossy().into_owned()],
        );
        result.insert("page".into(), json!(self.index + 1));
        Ok(result)
    }
}

fn annotation(doc: &Document, index: usize, subtype: &str, rect: Rect) -> Result<Dict, GoatError> {
    let mut d = Dict::new();
    d.insert("Type", Object::name("Annot"));
    d.insert("Subtype", Object::name(subtype));
    d.insert("Rect", rect.to_object());
    d.insert("P", Object::Reference(doc.page(index).map_err(error)?.id));
    d.insert("F", 4_i64);
    d.insert("NM", Object::text(&next_name(doc, index, "fitz-A")?));
    Ok(d)
}
fn border(d: &mut Dict, width: f64) {
    let mut bs = Dict::new();
    bs.insert("S", Object::name("S"));
    bs.insert("W", width);
    d.insert("BS", bs);
}
fn add(
    doc: &mut Document,
    index: usize,
    mut d: Dict,
    bbox: Rect,
    content: Vec<u8>,
    resources: Dict,
) -> Result<(), GoatError> {
    let normal = stream(doc, bbox, content, resources);
    let mut ap = Dict::new();
    ap.insert("N", Object::Reference(normal));
    d.insert("AP", ap);
    append_annot(doc, index, d)?;
    Ok(())
}

fn note(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let mut ctx = Context::new(matches)?;
    let (x, y) = goat_common::parse::parse_point(required::<String>(matches, "at")?)?;
    let rect = Rect::new(x, y, x + 16.0, y + 16.0).transform(&ctx.inverse);
    let contents = required::<String>(matches, "text")?;
    let mut d = annotation(&ctx.doc, ctx.index, "Text", rect)?;
    d.insert("C", numbers(&[1.0, 1.0, 0.0]));
    d.insert("F", 28_i64);
    d.insert("Name", Object::name("Note"));
    d.insert("Contents", Object::text(contents));
    let normal=stream(&mut ctx.doc,Rect::new(0.0,0.0,16.0,16.0),b"1 1 0 rg\n1 w\n0.5 0.5 15 15 re\nb\n1 0 0 -1 4 12 cm\n0 g\n0 0 8 1 re\n0 2 8 1 re\n0 4 8 1 re\n0 6 8 1 re\nf\n".to_vec(),Dict::new());
    let mut ap = Dict::new();
    ap.insert("N", Object::Reference(normal));
    d.insert("AP", ap);
    let id = append_annot(&mut ctx.doc, ctx.index, d)?;
    let mut popup = Dict::new();
    popup.insert("Type", Object::name("Annot"));
    popup.insert("Subtype", Object::name("Popup"));
    popup.insert(
        "Rect",
        Rect::new(32.0, 12.0, 232.0, 112.0)
            .transform(&ctx.inverse)
            .to_object(),
    );
    popup.insert("Parent", Object::Reference(id));
    let popup = append_annot(&mut ctx.doc, ctx.index, popup)?;
    let mut d = dict(&ctx.doc, &Object::Reference(id))?;
    d.insert("Popup", Object::Reference(popup));
    ctx.doc.set(id, d);
    let mut result = ctx.finish(matches, "note")?;
    result.insert("text".into(), json!(contents));
    Ok(result)
}

struct FreeText<'a> {
    rect: Rect,
    contents: &'a str,
    size: f64,
    ink: &'a [f64],
    fill: &'a [f64],
    inverse: Matrix,
}
fn free_text(doc: &mut Document, index: usize, args: FreeText<'_>) -> Result<(), GoatError> {
    let FreeText {
        rect,
        contents,
        size,
        ink,
        fill,
        inverse,
    } = args;
    let mut d = annotation(doc, index, "FreeText", rect)?;
    d.insert("Contents", Object::text(contents));
    let transform = inverse
        .invert()
        .ok_or_else(|| GoatError::value_error("invalid page transform"))?;
    let page_rect = rect.transform(&transform);
    let center = Point::new(
        (page_rect.x0 + page_rect.x1) / 2.0,
        (page_rect.y0 + page_rect.y1) / 2.0,
    );
    let scale = if center.x == 0.0 || center.y == 0.0 {
        0.0
    } else {
        (page_rect.x0 / center.x).max(page_rect.y0 / center.y)
    };
    let start = Point::new(0.0, 0.0).transform(&inverse);
    let end = Point::new(center.x * scale, center.y * scale).transform(&inverse);
    d.insert("CL", numbers(&[start.x, start.y, end.x, end.y]));
    let mut bs = Dict::new();
    bs.insert("W", 0_i64);
    bs.insert("Type", Object::name("Border"));
    d.insert("BS", bs);
    d.insert(
        "DA",
        Object::text(&format!(
            "{} /Helv {} Tf",
            color(ink, false).trim_end(),
            goat_common::py::float_repr(size)
        )),
    );
    d.insert("Q", 0_i64);
    d.insert("RD", numbers(&[0.0; 4]));
    let path = format!(
        "{} {} {} {} re\n",
        mu(rect.x0),
        mu(rect.y0),
        mu(rect.width()),
        mu(rect.height())
    );
    let mut content = Vec::new();
    if !fill.is_empty() {
        d.insert("C", numbers(fill));
        content.extend_from_slice(color(fill, false).as_bytes());
    }
    content.extend_from_slice(format!("{}0 w\n", color(ink, true)).as_bytes());
    if !fill.is_empty() {
        content.extend_from_slice(path.as_bytes());
        content.extend_from_slice(b"f\n");
    }
    content.extend_from_slice(
        format!(
            "{path}W\nn\nq\n1 0 -0 1 {} {} cm\n",
            mu(rect.x0),
            mu(rect.y0)
        )
        .as_bytes(),
    );
    content.extend(variable_text(VariableText {
        text: contents,
        font: Standard14::Helvetica,
        name: "Helv",
        size,
        width: rect.width(),
        height: rect.height(),
        padding: 0.0,
        multiline: true,
        align: 0,
        ink,
    }));
    content.extend_from_slice(b"Q\n");
    let font = font_resource(doc, Standard14::Helvetica);
    add(doc, index, d, rect, content, font_resources("Helv", font))
}
fn textbox(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let mut ctx = Context::new(matches)?;
    let rect = rect_arg(matches, &ctx.inverse)?;
    let contents = required::<String>(matches, "text")?;
    let size = *required::<f64>(matches, "size")?;
    let ink = color_arg(matches, "color", &[0.0, 0.0, 0.0])?;
    let fill = color_arg(matches, "fill", &[])?;
    free_text(
        &mut ctx.doc,
        ctx.index,
        FreeText {
            rect,
            contents,
            size,
            ink: &ink,
            fill: &fill,
            inverse: ctx.inverse,
        },
    )?;
    let mut result = ctx.finish(matches, "textbox")?;
    result.insert("text".into(), json!(contents));
    Ok(result)
}

fn square(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    shape(matches, false, false)
}
fn circle(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    shape(matches, true, false)
}
fn area(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    shape(matches, false, true)
}
fn shape(matches: &ArgMatches, circle: bool, area: bool) -> ResultMap {
    let opacity = if area {
        *required::<f64>(matches, "opacity")?
    } else {
        1.0
    };
    if !(0.0..=1.0).contains(&opacity) {
        return Err(GoatError::message("--opacity must be between 0 and 1"));
    }
    let mut ctx = Context::new(matches)?;
    let rect = rect_arg(matches, &ctx.inverse)?;
    let rect = expanded(rect, 1.0);
    let width = if area {
        0.0
    } else {
        *required::<f64>(matches, "width")?
    };
    let ink = if area {
        vec![1.0, 0.0, 0.0]
    } else {
        color_arg(matches, "color", &[1.0, 0.0, 0.0])?
    };
    let fill = if area {
        color_arg(matches, "color", &[1.0, 1.0, 0.0])?
    } else {
        color_arg(matches, "fill", &[])?
    };
    let mut d = annotation(
        &ctx.doc,
        ctx.index,
        if circle { "Circle" } else { "Square" },
        rect,
    )?;
    d.insert("C", numbers(&ink));
    if !fill.is_empty() {
        d.insert("IC", numbers(&fill));
    }
    border(&mut d, width);
    let rd = 1.0_f64.max(width / 2.0);
    d.insert("RD", numbers(&[rd; 4]));
    let mut resources = Dict::new();
    let mut content = Vec::new();
    if area {
        d.insert("CA", opacity);
        if opacity < 1.0 {
            let mut h = Dict::new();
            h.insert("CA", opacity);
            h.insert("ca", opacity);
            let mut state = Dict::new();
            state.insert("H", h);
            resources.insert("ExtGState", state);
            content.extend_from_slice(b"q\n/H gs\n/H gs\n");
        }
    }
    content.extend_from_slice(
        format!(
            "{} w\n{}{}",
            mu(width),
            color(&ink, true),
            if fill.is_empty() {
                String::new()
            } else {
                color(&fill, false)
            }
        )
        .as_bytes(),
    );
    let draw = expanded(rect, -rd);
    content.extend_from_slice(
        if circle {
            circle_path(draw)
        } else {
            format!(
                "{} {} {} {} re\n",
                mu(draw.x0),
                mu(draw.y0),
                mu(draw.width().max(1.0)),
                mu(draw.height().max(1.0))
            )
        }
        .as_bytes(),
    );
    content.extend_from_slice(if fill.is_empty() { b"S\n" } else { b"b\n" });
    if area && opacity < 1.0 {
        content.extend_from_slice(b"\nQ\n");
    }
    add(&mut ctx.doc, ctx.index, d, rect, content, resources)?;
    let mut result = ctx.finish(
        matches,
        if area {
            "area-highlight"
        } else if circle {
            "circle"
        } else {
            "rect"
        },
    )?;
    if area {
        result.insert("opacity".into(), json!(opacity));
    }
    Ok(result)
}

struct Line<'a> {
    start: Point,
    end: Point,
    width: f64,
    ink: &'a [f64],
    arrow: bool,
}
fn draw_line(doc: &mut Document, index: usize, line: Line<'_>) -> Result<(), GoatError> {
    let Line {
        start,
        end,
        width,
        ink,
        arrow,
    } = line;
    let mut points = vec![start, end];
    let mut content = format!(
        "{} w\n{}{} m\n{} l\nS\n",
        mu(width),
        color(ink, true),
        point_string(start),
        point_string(end)
    );
    if arrow {
        let (dx, dy) = ((start.x - end.x) as f32, (start.y - end.y) as f32);
        let norm = (dx * dx + dy * dy).sqrt();
        let norm = if norm == 0.0 { 1.0 } else { norm };
        let (dx, dy) = (dx / norm, dy / norm);
        let r = width.max(1.0) as f32;
        let a = Point::new(
            f64::from(end.x as f32 + 8.8 * r * dx - 4.5 * r * dy),
            f64::from(end.y as f32 + 8.8 * r * dy + 4.5 * r * dx),
        );
        let b = Point::new(
            f64::from(end.x as f32 + 8.8 * r * dx + 4.5 * r * dy),
            f64::from(end.y as f32 + 8.8 * r * dy - 4.5 * r * dx),
        );
        points.extend([a, b]);
        content.push_str(&format!(
            "{} m\n{} l\n{} l\nS\n",
            point_string(a),
            point_string(end),
            point_string(b)
        ));
    }
    let rect = expanded(
        bounds(&points),
        width.max(1.0) * if arrow { 2.0 } else { 1.0 },
    );
    let mut d = annotation(doc, index, "Line", rect)?;
    d.insert("L", numbers(&[start.x, start.y, end.x, end.y]));
    d.insert("C", numbers(ink));
    border(&mut d, width);
    if arrow {
        d.insert(
            "LE",
            Object::Array(vec![Object::name("None"), Object::name("OpenArrow")]),
        );
    }
    add(doc, index, d, rect, content.into_bytes(), Dict::new())
}
fn line(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    line_command(matches, false)
}
fn arrow(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    line_command(matches, true)
}
fn line_command(matches: &ArgMatches, arrow: bool) -> ResultMap {
    let mut ctx = Context::new(matches)?;
    let start = point_arg(matches, "start", &ctx.inverse)?;
    let end = point_arg(matches, "end", &ctx.inverse)?;
    let ink = color_arg(matches, "color", &[1.0, 0.0, 0.0])?;
    let width = *required::<f64>(matches, "width")?;
    draw_line(
        &mut ctx.doc,
        ctx.index,
        Line {
            start,
            end,
            width,
            ink: &ink,
            arrow,
        },
    )?;
    ctx.finish(matches, if arrow { "arrow" } else { "line" })
}
fn callout(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let mut ctx = Context::new(matches)?;
    let rect = rect_arg(matches, &ctx.inverse)?;
    let contents = required::<String>(matches, "text")?;
    let size = *required::<f64>(matches, "size")?;
    let end = point_arg(matches, "target", &ctx.inverse)?;
    let coords = goat_common::parse::parse_rect(required::<String>(matches, "rect")?)?;
    let start = Point::new(coords[0], coords[3]).transform(&ctx.inverse);
    free_text(
        &mut ctx.doc,
        ctx.index,
        FreeText {
            rect,
            contents,
            size,
            ink: &[0.0, 0.0, 0.0],
            fill: &[1.0, 1.0, 0.7],
            inverse: ctx.inverse,
        },
    )?;
    draw_line(
        &mut ctx.doc,
        ctx.index,
        Line {
            start,
            end,
            width: 1.0,
            ink: &[1.0, 0.0, 0.0],
            arrow: true,
        },
    )?;
    ctx.finish(matches, "callout")
}

fn ink(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    path_command(matches, false)
}
fn polygon(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    path_command(matches, true)
}
fn path_command(matches: &ArgMatches, polygon: bool) -> ResultMap {
    let points = points_arg(matches)?;
    if polygon && points.len() < 3 {
        return Err(GoatError::message(
            "--points requires at least three x,y pairs",
        ));
    }
    let mut ctx = Context::new(matches)?;
    let points: Vec<_> = points.iter().map(|p| p.transform(&ctx.inverse)).collect();
    let width = *required::<f64>(matches, "width")?;
    let ink = color_arg(
        matches,
        "color",
        if polygon {
            &[1.0, 0.0, 0.0]
        } else {
            &[0.0, 0.0, 1.0]
        },
    )?;
    let fill = if polygon {
        color_arg(matches, "fill", &[])?
    } else {
        Vec::new()
    };
    let rd = width + if polygon { 0.0 } else { 6.0 };
    let rect = expanded(bounds(&points), rd);
    let mut d = annotation(
        &ctx.doc,
        ctx.index,
        if polygon { "Polygon" } else { "Ink" },
        rect,
    )?;
    d.insert("C", numbers(&ink));
    if !fill.is_empty() {
        d.insert("IC", numbers(&fill));
    }
    border(&mut d, width);
    d.insert("RD", numbers(&[rd; 4]));
    let coordinates = numbers(&points.iter().flat_map(|p| [p.x, p.y]).collect::<Vec<_>>());
    d.insert(
        if polygon { "Vertices" } else { "InkList" },
        if polygon {
            coordinates
        } else {
            Object::Array(vec![coordinates])
        },
    );
    let mut content = if polygon {
        String::from("q\n")
    } else {
        String::new()
    };
    content.push_str(&format!("{} w\n{}", mu(width), color(&ink, true)));
    if !fill.is_empty() {
        content.push_str(&color(&fill, false));
    }
    if !polygon {
        content.push_str("1 J\n1 j\n");
    }
    for (i, point) in points.iter().enumerate() {
        content.push_str(&format!(
            "{} {}\n",
            point_string(*point),
            if i == 0 { "m" } else { "l" }
        ));
    }
    if polygon {
        content.push_str("h\n");
        if fill.is_empty() {
            content.push_str("s\nQ\n");
        } else {
            content.push_str(&format!("{}b\nQ\n", color(&fill, false)));
        }
    } else {
        content.push_str("S\n");
    }
    add(
        &mut ctx.doc,
        ctx.index,
        d,
        rect,
        content.into_bytes(),
        Dict::new(),
    )?;
    let mut result = ctx.finish(matches, if polygon { "polygon" } else { "ink" })?;
    result.insert("points".into(), json!(points.len()));
    Ok(result)
}

fn stamp(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let stamp = *required::<i64>(matches, "stamp")?;
    let mut ctx = Context::new(matches)?;
    let rect = rect_arg(matches, &ctx.inverse)?;
    let (name, lines): (&str, &[(&str, f64, f64)]) = match stamp {
        0 => ("Approved", &[("APPROVED", 13.0, 30.0)]),
        1 => ("AsIs", &[("AS IS", 13.0, 30.0)]),
        2 => ("Confidential", &[("CONFIDENTIAL", 17.0, 20.0)]),
        3 => ("Departmental", &[("DEPARTMENTAL", 17.0, 20.0)]),
        4 => ("Experimental", &[("EXPERIMENTAL", 17.0, 20.0)]),
        5 => ("Expired", &[("EXPIRED", 13.0, 30.0)]),
        6 => ("Final", &[("FINAL", 13.0, 30.0)]),
        7 => ("ForComment", &[("FOR COMMENT", 17.0, 20.0)]),
        8 => (
            "ForPublicRelease",
            &[("FOR PUBLIC", 26.0, 18.0), ("RELEASE", 8.5, 18.0)],
        ),
        9 => ("NotApproved", &[("NOT APPROVED", 17.0, 20.0)]),
        10 => (
            "NotForPublicRelease",
            &[("NOT FOR", 26.0, 18.0), ("PUBLIC RELEASE", 8.5, 18.0)],
        ),
        11 => ("Sold", &[("SOLD", 13.0, 30.0)]),
        12 => ("TopSecret", &[("TOP SECRET", 14.0, 26.0)]),
        13 => ("Draft", &[("DRAFT", 13.0, 30.0)]),
        _ => return Err(GoatError::value_error("bad stamp number")),
    };
    let scale = (rect.width() / 190.0).min(rect.height() / 50.0);
    let (cx, cy) = ((rect.x0 + rect.x1) / 2.0, (rect.y0 + rect.y1) / 2.0);
    let rect = Rect::new(
        cx - 95.0 * scale,
        cy - 25.0 * scale,
        cx + 95.0 * scale,
        cy + 25.0 * scale,
    );
    let mut d = annotation(&ctx.doc, ctx.index, "Stamp", rect)?;
    d.insert("Name", Object::name(name));
    d.insert("Contents", Object::text(name));
    d.insert("C", numbers(&[1.0, 0.0, 0.0]));
    let mut content=b"1 0 0 rg\n1 0 0 RG\n.99994519 .0104717849 -.0104717849 .99994519 0 0 cm\n2 w\n2 2 186 44 re\nS\n".to_vec();
    for (text, y, size) in lines {
        let width = Standard14::TimesBold.text_width(text, *size as f32);
        content.extend_from_slice(
            format!(
                "BT\n/Times {} Tf\n{} {} Td\n",
                mu(*size),
                mu(f64::from((190.0 - width) / 2.0)),
                mu(*y)
            )
            .as_bytes(),
        );
        content.extend(literal(text));
        content.extend_from_slice(b" Tj\nET\n");
    }
    let font = font_resource(&mut ctx.doc, Standard14::TimesBold);
    add(
        &mut ctx.doc,
        ctx.index,
        d,
        Rect::new(0.0, 0.0, 190.0, 50.0),
        content,
        font_resources("Times", font),
    )?;
    let mut result = ctx.finish(matches, "stamp")?;
    result.insert("stamp".into(), json!(stamp));
    Ok(result)
}

fn highlight(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    markup(matches, "highlight")
}
fn underline(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    markup(matches, "underline")
}
fn strikeout(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    markup(matches, "strikeout")
}
fn markup(matches: &ArgMatches, kind: &str) -> ResultMap {
    let (mut doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
    let find = required::<String>(matches, "find")?;
    let ink = color_arg(
        matches,
        "color",
        if kind == "highlight" {
            &[1.0, 0.9, 0.0]
        } else {
            &[1.0, 0.0, 0.0]
        },
    )?;
    let pages = goat_common::parse::page_indices(
        optional::<String>(matches, "pages")?.map(String::as_str),
        doc.page_count().map_err(error)?,
    )?;
    let mut marks = 0;
    for index in pages {
        let inverse = crate::page_transform(&mut doc.page(index).map_err(error)?)
            .invert()
            .ok_or_else(|| GoatError::value_error("invalid page transform"))?;
        let hits =
            pdf_text::extract_page(&doc, index, pdf_text::TextFlags::SEARCH_FOR)?.search(find);
        for quad in hits {
            let points = [quad.ul, quad.ur, quad.ll, quad.lr].map(|p| p.transform(&inverse));
            let [ul, ur, ll, lr] = points;
            let height = ((ul.x - ll.x).powi(2) + (ul.y - ll.y).powi(2)).sqrt();
            let mut rect = expanded(bounds(&points), height / 16.0);
            let mut resources = Dict::new();
            let mut content = if kind == "highlight" {
                let norm = ((lr.x - ll.x).powi(2) + (lr.y - ll.y).powi(2)).sqrt();
                let multiplier = if norm == 0.0 {
                    0.0
                } else {
                    height / 4.2425 / norm
                };
                let (x, y) = ((lr.x - ll.x) * multiplier, (lr.y - ll.y) * multiplier);
                let mll = Point::new(ll.x - x - y, ll.y - y + x);
                let mul = Point::new(ul.x - x + y, ul.y - y - x);
                let mlr = Point::new(lr.x + x - y, lr.y + y + x);
                let mur = Point::new(ur.x + x + y, ur.y + y - x);
                rect = rect.union(&bounds(&[mll, mul, mlr, mur]));
                let mut state = Dict::new();
                state.insert("BM", Object::name("Multiply"));
                let mut gs = Dict::new();
                gs.insert("H", state);
                resources.insert("ExtGState", gs);
                format!(
                    "q\n/H gs\n/H gs\n{}{} m\n{} {} {} c\n{} l\n{} {} {} c\nf\n\nQ\n",
                    color(&ink, false),
                    point_string(ll),
                    point_string(mll),
                    point_string(mul),
                    point_string(ul),
                    point_string(ur),
                    point_string(mur),
                    point_string(mlr),
                    point_string(lr)
                )
                .into_bytes()
            } else {
                let t = if kind == "underline" {
                    1.0 / 7.0
                } else {
                    3.0 / 7.0
                };
                let a = Point::new(ll.x + (ul.x - ll.x) * t, ll.y + (ul.y - ll.y) * t);
                let b = Point::new(lr.x + (ur.x - lr.x) * t, lr.y + (ur.y - lr.y) * t);
                format!(
                    "{}{} w\n{} m\n{} l\nS\n",
                    color(&ink, true),
                    mu(height / 16.0),
                    point_string(a),
                    point_string(b)
                )
                .into_bytes()
            };
            let mut d = annotation(
                &doc,
                index,
                match kind {
                    "highlight" => "Highlight",
                    "underline" => "Underline",
                    _ => "StrikeOut",
                },
                rect,
            )?;
            d.insert("C", numbers(&ink));
            d.insert(
                "QuadPoints",
                numbers(&points.iter().flat_map(|p| [p.x, p.y]).collect::<Vec<_>>()),
            );
            if kind == "highlight" {
                d.insert("BM", Object::name("Multiply"));
            }
            add(
                &mut doc,
                index,
                d,
                rect,
                std::mem::take(&mut content),
                resources,
            )?;
            marks += 1;
        }
    }
    let output = output(matches, &source, kind, "pdf")?;
    save(&doc, &output, true)?;
    let mut result = result(
        &format!("annot-{kind}"),
        &source,
        vec![output.to_string_lossy().into_owned()],
    );
    result.insert("find".into(), json!(find));
    result.insert("marks".into(), json!(marks));
    Ok(result)
}

fn listed(subtype: &[u8]) -> bool {
    !matches!(subtype, b"Link" | b"Popup" | b"Widget")
}
fn list(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let (doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
    let mut items = Vec::new();
    for index in 0..doc.page_count().map_err(error)? {
        let mut page = doc.page(index).map_err(error)?;
        let matrix = crate::page_transform(&mut page);
        let annots = doc.resolve_key(&page.dict, b"Annots").map_err(error)?;
        for object in annots.as_array().unwrap_or(&[]) {
            let d = dict(&doc, object)?;
            let subtype = d.get_name(b"Subtype").unwrap_or(b"Unknown");
            if !listed(subtype) {
                continue;
            }
            let rect = crate::annotation_rect(&d, page.rotation(), &matrix);
            let content = text(&doc.resolve_key(&d, b"Contents").map_err(error)?);
            let author = text(&doc.resolve_key(&d, b"T").map_err(error)?);
            items.push(json!({"page":index+1,"type":String::from_utf8_lossy(subtype),"rect":rect_json(rect,1),"content":if content.is_empty(){Value::Null}else{json!(content)},"author":if author.is_empty(){Value::Null}else{json!(author)}}));
        }
    }
    let mut result = result("annot-list", &source, Vec::new());
    result.insert("count".into(), json!(items.len()));
    result.insert("annotations".into(), Value::Array(items));
    Ok(result)
}
fn delete(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let (mut doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
    let pages = goat_common::parse::page_indices(
        optional::<String>(matches, "pages")?.map(String::as_str),
        doc.page_count().map_err(error)?,
    )?;
    let kind = optional::<String>(matches, "type")?;
    let mut removed = 0;
    for index in pages {
        let page = doc.page(index).map_err(error)?;
        let mut d = page.dict;
        let annots = doc.resolve_key(&d, b"Annots").map_err(error)?;
        let annots = annots.as_array().unwrap_or(&[]);
        let mut deleted = HashSet::new();
        let mut remove_indices = HashSet::new();
        for (i, object) in annots.iter().enumerate() {
            let annot = dict(&doc, object)?;
            let subtype = annot.get_name(b"Subtype").unwrap_or(b"Unknown");
            if listed(subtype)
                && kind.is_none_or(|k| String::from_utf8_lossy(subtype).eq_ignore_ascii_case(k))
            {
                remove_indices.insert(i);
                if let Some(popup) = annot.get_ref(b"Popup") {
                    deleted.insert(popup);
                }
                removed += 1;
            }
        }
        let kept = annots
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                !remove_indices.contains(i)
                    && !o.as_reference().is_some_and(|id| deleted.contains(&id))
            })
            .map(|(_, o)| o.clone())
            .collect::<Vec<_>>();
        store_child(&mut doc, &mut d, "Annots", Object::Array(kept));
        doc.set(page.id, d);
    }
    let output = output(matches, &source, "noannots", "pdf")?;
    save(&doc, &output, true)?;
    let mut result = result(
        "annot-delete",
        &source,
        vec![output.to_string_lossy().into_owned()],
    );
    result.insert("removed".into(), json!(removed));
    Ok(result)
}
fn flatten(matches: &ArgMatches, _: &Ctx) -> ResultMap {
    let (mut doc, source) = open(required::<String>(matches, "file")?, "pymupdf")?;
    let output = output(matches, &source, "flat", "pdf")?;
    crate::flatten::flatten(&mut doc, false)?;
    save(&doc, &output, true)?;
    Ok(result(
        "annot-flatten",
        &source,
        vec![output.to_string_lossy().into_owned()],
    ))
}

fn option(command: Command, name: &'static str) -> Command {
    command.arg(Arg::new(name).long(name))
}
fn required_arg(command: Command, name: &'static str) -> Command {
    command.arg(Arg::new(name).long(name).required(true))
}
fn width(command: Command) -> Command {
    command.arg(
        Arg::new("width")
            .long("width")
            .value_parser(float_value)
            .default_value("1.5"),
    )
}
fn size(command: Command, default: &'static str) -> Command {
    command.arg(
        Arg::new("size")
            .long("size")
            .value_parser(float_value)
            .default_value(default),
    )
}
pub(crate) fn register(registry: &mut Registry) {
    for (name, help, handler) in [
        (
            "highlight",
            "highlight text matching a string",
            highlight as goat_common::Handler,
        ),
        ("underline", "underline text matching a string", underline),
        ("strikeout", "strikeout text matching a string", strikeout),
    ] {
        registry.family_verb(
            "annotate",
            Verb::new(
                out(option(
                    option(required_arg(base(name, help), "find"), "pages"),
                    "color",
                )),
                handler,
            ),
        );
    }
    registry.family_verb(
        "annotate",
        Verb::new(
            out(required_arg(
                page(base("note", "sticky note"))
                    .arg(Arg::new("at").long("at").required(true).help("x,y")),
                "text",
            )),
            note,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(option(
                option(
                    size(
                        required_arg(
                            required_arg(page(base("textbox", "free-text box")), "rect"),
                            "text",
                        ),
                        "12",
                    ),
                    "color",
                ),
                "fill",
            )),
            textbox,
        ),
    );
    for (name, help, handler) in [
        ("rect", "draw a rect", square as goat_common::Handler),
        ("circle", "draw a circle", circle),
    ] {
        registry.family_verb(
            "annotate",
            Verb::new(
                out(width(option(
                    option(required_arg(page(base(name, help)), "rect"), "color"),
                    "fill",
                ))),
                handler,
            ),
        );
    }
    for (name, help, handler) in [
        ("line", "draw a line", line as goat_common::Handler),
        ("arrow", "draw a arrow", arrow),
    ] {
        registry.family_verb(
            "annotate",
            Verb::new(
                out(width(option(
                    page(base(name, help))
                        .arg(Arg::new("start").long("start").required(true).help("x,y"))
                        .arg(Arg::new("end").long("end").required(true).help("x,y")),
                    "color",
                ))),
                handler,
            ),
        );
    }
    registry.family_verb(
        "annotate",
        Verb::new(
            out(width(option(
                page(base("ink", "freehand ink stroke")).arg(
                    Arg::new("points")
                        .long("points")
                        .required(true)
                        .help("x,y;x,y;..."),
                ),
                "color",
            ))),
            ink,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(
                required_arg(page(base("stamp", "rubber stamp")), "rect").arg(
                    Arg::new("stamp")
                        .long("stamp")
                        .value_parser(int_value)
                        .default_value("0")
                        .help("predefined stamp index from 0 to 13"),
                ),
            ),
            stamp,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(size(
                required_arg(
                    required_arg(page(base("callout", "callout box with arrow")), "rect").arg(
                        Arg::new("target")
                            .long("target")
                            .required(true)
                            .help("arrow endpoint as x,y"),
                    ),
                    "text",
                ),
                "11",
            )),
            callout,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(option(
                required_arg(
                    page(base(
                        "area-highlight",
                        "add a transparent rectangular highlight",
                    )),
                    "rect",
                ),
                "color",
            )
            .arg(
                Arg::new("opacity")
                    .long("opacity")
                    .value_parser(float_value)
                    .default_value("0.25")
                    .help("0 to 1"),
            )),
            area,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(width(option(
                option(
                    page(base("polygon", "draw a polygon")).arg(
                        Arg::new("points")
                            .long("points")
                            .required(true)
                            .help("x,y;x,y;x,y;..."),
                    ),
                    "color",
                ),
                "fill",
            ))),
            polygon,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(base("list", "list annotations"), list),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(base("flatten", "flatten annotations into content")),
            flatten,
        ),
    );
    registry.family_verb(
        "annotate",
        Verb::new(
            out(option(
                base("delete", "delete annotations").arg(
                    Arg::new("type")
                        .long("type")
                        .help("only this annotation type"),
                ),
                "pages",
            )),
            delete,
        ),
    );
}
