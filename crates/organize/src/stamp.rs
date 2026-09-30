//! Headers, footers, numbering, and diagonal text watermarks.

use std::borrow::Cow;
use std::fmt::Write;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{float_value, int_value, optional, required};
use goat_common::parse::parse_color;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Matrix, Object, Point};
use pdf_font::Standard14;
use serde_json::{Map, Value};

use crate::geometry;
use crate::open::{self, Lib, display, out_path, pdf_error, result, source};
use crate::show::{append_contents, wrap_contents};

type Output = Result<Map<String, Value>, GoatError>;

fn command(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(Arg::new("file").required(true))
}
fn size(default: &'static str) -> Arg {
    Arg::new("size")
        .long("size")
        .value_parser(float_value)
        .default_value(default)
}
fn align() -> Arg {
    Arg::new("align")
        .long("align")
        .value_parser(["left", "center", "right"])
        .default_value("center")
}
fn start() -> Arg {
    Arg::new("start")
        .long("start")
        .value_parser(int_value)
        .default_value("1")
}

pub(crate) fn register(registry: &mut Registry) {
    for (name, about, handler) in [
        ("header", "add a header", header as goat_common::Handler),
        ("footer", "add a footer", footer as goat_common::Handler),
    ] {
        registry.family_verb(
            "pages",
            Verb::new(
                command(name, about)
                    .arg(
                        Arg::new("text")
                            .long("text")
                            .required(true)
                            .help("supports {page} {pages}"),
                    )
                    .arg(align())
                    .arg(size("10"))
                    .arg(Arg::new("color").long("color"))
                    .arg(Arg::new("output").short('o').long("output")),
                handler,
            ),
        );
    }
    registry.family_verb(
        "pages",
        Verb::new(
            command("numbers", "add page numbers")
                .arg(
                    Arg::new("format")
                        .long("format")
                        .default_value("{page}")
                        .help("e.g. 'Page {page} of {pages}'"),
                )
                .arg(start())
                .arg(align())
                .arg(size("10"))
                .arg(Arg::new("output").short('o').long("output")),
            numbers,
        ),
    );
    registry.family_verb(
        "pages",
        Verb::new(
            command("bates", "Bates numbering")
                .arg(Arg::new("prefix").long("prefix").default_value(""))
                .arg(start())
                .arg(
                    Arg::new("digits")
                        .long("digits")
                        .value_parser(int_value)
                        .default_value("6"),
                )
                .arg(size("9"))
                .arg(Arg::new("output").short('o').long("output")),
            bates,
        ),
    );
    registry.command(Verb::new(
        command("watermark", "stamp a diagonal text watermark")
            .arg(Arg::new("text").long("text").default_value("DRAFT"))
            .arg(size("72"))
            .arg(
                Arg::new("opacity")
                    .long("opacity")
                    .value_parser(float_value)
                    .default_value("0.15"),
            )
            .arg(
                Arg::new("angle")
                    .long("angle")
                    .value_parser(float_value)
                    .default_value("45"),
            )
            .arg(Arg::new("output").short('o').long("output")),
        watermark,
    ));
}

/// MuPDF stores each advance as a single-precision em fraction before summing in Python.
fn text_width(text: &str, size: f64) -> f64 {
    let em: f64 = text
        .chars()
        .map(|ch| {
            let code = u32::from(ch);
            let units = if (32..=255).contains(&code) && !(127..=159).contains(&code) {
                Standard14::Helvetica.code_width(code as u8)
            } else {
                278
            };
            f64::from(f32::from(units) / 1000.0)
        })
        .sum();
    em * size
}

fn number(value: f64) -> String {
    let value = value as f32;
    let value = if value.is_nan() {
        0.0
    } else if value.is_infinite() {
        value.signum() * f32::MAX
    } else {
        value
    };
    let text = value.to_string();
    if let Some(rest) = text.strip_prefix("0.") {
        format!(".{rest}")
    } else if let Some(rest) = text.strip_prefix("-0.") {
        format!("-.{rest}")
    } else {
        text
    }
}

fn encoded(text: &str) -> String {
    let mut hex = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        let code = u8::try_from(u32::from(ch)).unwrap_or(0xb7);
        let _ = write!(hex, "{code:02x}");
    }
    hex
}

fn font(doc: &mut Document, index: usize) -> Result<(), GoatError> {
    let mut page = doc.page(index).map_err(pdf_error)?;
    let mut fonts = match page.resources.get(b"Font") {
        Some(value) => doc
            .resolve_dict(value)
            .map_err(pdf_error)?
            .unwrap_or_default(),
        None => Dict::new(),
    };
    if fonts.contains_key(b"helv") {
        return Ok(());
    }
    let mut font = Dict::new();
    font.insert("Type", Object::name("Font"));
    font.insert("Subtype", Object::name("Type1"));
    font.insert("BaseFont", Object::name("Helvetica"));
    font.insert("Encoding", Object::name("WinAnsiEncoding"));
    fonts.insert("helv", doc.add(font));
    page.resources.insert("Font", fonts);
    page.dict.insert("Resources", page.resources);
    doc.set(page.id, page.dict);
    Ok(())
}

fn color_ops(color: &[f64]) -> Result<String, GoatError> {
    if !matches!(color.len(), 1 | 3 | 4) || color.iter().any(|value| !(0.0..=1.0).contains(value)) {
        return Err(GoatError::value_error(
            "need 1, 3 or 4 color components in range 0 to 1",
        ));
    }
    let components = color
        .iter()
        .map(|value| number(*value))
        .collect::<Vec<_>>()
        .join(" ");
    let (stroke, fill) = match color.len() {
        1 => ("G", "g"),
        4 => ("K", "k"),
        _ => ("RG", "rg"),
    };
    Ok(format!("{components} {stroke} {components} {fill}"))
}

fn insert_text(
    doc: &mut Document,
    index: usize,
    point: Point,
    text: &str,
    size: f64,
    color: &[f64],
) -> Result<(), GoatError> {
    if text.is_empty() {
        return Ok(());
    }
    let page = doc.page(index).map_err(pdf_error)?;
    let media = page.media_box();
    let crop = page.crop_box();
    let top = crop.y1 - point.y;
    let left = point.x + crop.x0;
    let color = color_ops(color)?;
    font(doc, index)?;
    let height = f64::from(size as f32 * (1.075f32 + 0.299f32));
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return Ok(());
    };
    let mut content = format!(
        "\nq\nBT\n1 0 0 1 {} {} Tm\n/helv {} Tf {} [<{}>]TJ\n",
        number(left),
        number(top),
        number(size),
        color,
        encoded(first)
    );
    let mut remaining = media.height() - point.y;
    for (i, line) in lines.enumerate() {
        if remaining < height {
            break;
        }
        remaining -= height;
        if i == 0 {
            let _ = writeln!(content, "0 -{} TD", number(height));
        } else {
            content.push_str("T* ");
        }
        let _ = writeln!(content, "[<{}>]TJ", encoded(line));
    }
    content.push_str("ET\nQ\n");
    wrap_contents(doc, index)?;
    append_contents(doc, index, content.into_bytes())
}

fn stamp_pages(
    doc: &mut Document,
    text: impl Fn(usize, usize) -> String,
    top: bool,
    align: &str,
    size: f64,
    color: &[f64],
) -> Result<(), GoatError> {
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let count = doc.page_count().map_err(pdf_error)?;
    for index in 0..count {
        let text = text(index, count);
        let bounds = geometry::page_rect(&doc.page(index).map_err(pdf_error)?);
        let width = text_width(&text, size);
        let x = match align {
            "left" => 40.0,
            "right" => bounds.width() - width - 40.0,
            _ => (bounds.width() - width) / 2.0,
        };
        let y = if top { 50.0 } else { bounds.height() - 36.0 };
        insert_text(doc, index, Point::new(x, y), &text, size, color)?;
    }
    Ok(())
}

fn header(matches: &ArgMatches, ctx: &Ctx) -> Output {
    header_footer(matches, ctx, true)
}
fn footer(matches: &ArgMatches, ctx: &Ctx) -> Output {
    header_footer(matches, ctx, false)
}
fn header_footer(matches: &ArgMatches, _ctx: &Ctx, top: bool) -> Output {
    let src = source(matches)?;
    let name = if top { "header" } else { "footer" };
    let out = out_path(matches, &src, name)?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let text = required::<String>(matches, "text")?;
    let color = parse_color(optional::<String>(matches, "color")?.map(String::as_str))?
        .unwrap_or_else(|| vec![0.2; 3]);
    stamp_pages(
        &mut doc,
        |page, count| {
            text.replace("{page}", &(page + 1).to_string())
                .replace("{pages}", &count.to_string())
        },
        top,
        required::<String>(matches, "align")?,
        *required::<f64>(matches, "size")?,
        &color,
    )?;
    open::save_mupdf(&doc, &out)?;
    let mut map = result(
        &format!("pages-{name}"),
        vec![display(&src)],
        vec![display(&out)],
    );
    map.insert("text".into(), text.clone().into());
    Ok(map)
}

fn numbers(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "numbered")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let template = required::<String>(matches, "format")?;
    let start = i128::from(*required::<i64>(matches, "start")?);
    stamp_pages(
        &mut doc,
        |page, count| {
            template
                .replace("{page}", &(start + page as i128).to_string())
                .replace("{pages}", &count.to_string())
        },
        false,
        required::<String>(matches, "align")?,
        *required::<f64>(matches, "size")?,
        &[0.2; 3],
    )?;
    open::save_mupdf(&doc, &out)?;
    Ok(result(
        "pages-numbers",
        vec![display(&src)],
        vec![display(&out)],
    ))
}

fn bates(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "bates")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    let prefix = required::<String>(matches, "prefix")?;
    let start = i128::from(*required::<i64>(matches, "start")?);
    let digits = usize::try_from(*required::<i64>(matches, "digits")?).unwrap_or(0);
    if digits > 1_000_000 {
        return Err(GoatError::message("Bates number exceeds 1000000 digits"));
    }
    let label = |page: usize, _| format!("{prefix}{:0digits$}", start + page as i128);
    let first = label(0, 0);
    stamp_pages(
        &mut doc,
        label,
        false,
        "right",
        *required::<f64>(matches, "size")?,
        &[0.1; 3],
    )?;
    open::save_mupdf(&doc, &out)?;
    let mut map = result("pages-bates", vec![display(&src)], vec![display(&out)]);
    map.insert("first".into(), first.into());
    Ok(map)
}

fn watermark(matches: &ArgMatches, _ctx: &Ctx) -> Output {
    let src = source(matches)?;
    let out = out_path(matches, &src, "watermarked")?;
    let mut doc = open::load(&src, Lib::PyMuPdf)?;
    if doc.needs_password() {
        return Err(open::closed_or_encrypted());
    }
    let text = required::<String>(matches, "text")?;
    let size = *required::<f64>(matches, "size")?;
    let opacity = *required::<f64>(matches, "opacity")?;
    let angle = *required::<f64>(matches, "angle")?;
    let width = text_width(text, size);
    let drawing_text = if text.contains(['\n', '\r', '\t']) {
        Cow::Owned(text.replace(['\n', '\r', '\t'], " "))
    } else {
        Cow::Borrowed(text.as_str())
    };
    let unicode = if drawing_text.is_ascii() {
        None
    } else {
        Some(crate::watermark_text::UnicodeText::prepare(
            &mut doc,
            &drawing_text,
        )?)
    };
    for index in 0..doc.page_count().map_err(pdf_error)? {
        let page = doc.page(index).map_err(pdf_error)?;
        let bounds = geometry::page_rect(&page);
        let point = Point::new((bounds.width() - width) / 2.0, bounds.height() / 2.0);
        let pivot = Point::new(point.x, bounds.height() - point.y);
        let matrix = Matrix::translate(-pivot.x, -pivot.y)
            .concat(&Matrix::rotate(angle))
            .concat(&Matrix::translate(pivot.x, pivot.y));
        let values = [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f].map(|value| {
            if value.abs() < 0.0001 {
                0.0
            } else {
                (value * 100000.0).round() / 100000.0
            }
        });
        let mut content = String::from("q\n");
        let crop = page.crop_box();
        let media = page.media_box();
        let dy = media.y0 + media.y1 - crop.y0 - bounds.height();
        if crop.x0 != 0.0 || dy != 0.0 {
            let _ = writeln!(content, "1 0 0 1 {} {} cm", number(crop.x0), number(dy));
        }
        let _ = writeln!(content, "{} cm", values.map(number).join(" "));
        if unicode.is_none() {
            font(&mut doc, index)?;
        }
        if (0.0..1.0).contains(&opacity) {
            let mut page = doc.page(index).map_err(pdf_error)?;
            let mut states = match page.resources.get(b"ExtGState") {
                Some(value) => doc
                    .resolve_dict(value)
                    .map_err(pdf_error)?
                    .unwrap_or_default(),
                None => Dict::new(),
            };
            let mut suffix = 0;
            while states.contains_key(format!("Alp{suffix}").as_bytes()) {
                suffix += 1;
            }
            let name = format!("Alp{suffix}");
            let mut alpha = Dict::new();
            alpha.insert("CA", Object::Real(opacity));
            alpha.insert("ca", Object::Real(opacity));
            states.insert(name.as_bytes(), alpha);
            page.resources.insert("ExtGState", states);
            page.dict.insert("Resources", page.resources);
            doc.set(page.id, page.dict);
            let _ = writeln!(content, "/{name} gs");
        }
        content.push_str(".5 .5 .5 RG\n.5 .5 .5 rg\nBT\n0 Tr\n1 w\n");
        if let Some(unicode) = &unicode {
            content.push_str(&unicode.draw(&mut doc, index, pivot, size)?);
        } else {
            let _ = write!(
                content,
                "/helv {} Tf\n1 0 0 1 {} {} Tm\n[<{}>]TJ\n",
                number(size),
                number(pivot.x),
                number(pivot.y),
                encoded(&drawing_text)
            );
        }
        content.push_str("ET\nQ\n");
        append_contents(&mut doc, index, content.into_bytes())?;
    }
    open::save_mupdf(&doc, &out)?;
    let mut map = result("watermark", vec![display(&src)], vec![display(&out)]);
    map.insert("text".into(), text.clone().into());
    Ok(map)
}
