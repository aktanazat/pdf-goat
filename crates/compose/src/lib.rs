//! Composition verbs: `from-images` (img2pdf rules), `from-html`, and `from-md`.
//!
//! `from-html` and `from-md` share an in-process print renderer: HTML parsing,
//! CSS cascade and inheritance, blocks and inline runs, floats, flex/grid,
//! relative/absolute/fixed positioning, lists, tables, links, heading outlines,
//! and paged layout. Oversized table rows split at line boundaries; table headers
//! and fixed content repeat on later pages.
//!
//! Local, data-URI, and HTTP(S) images and linked/imported stylesheets resolve
//! against the document or stylesheet URL. SVG is rendered through resvg at
//! twice its CSS pixel size. Visible SVG labels retain the resolved glyph positions
//! in a searchable text layer. System TrueType fonts are shaped with rustybuzz and
//! Unicode bidi ordering, then subset and embedded through `pdf-font`.
//! Documents are written through `pdf-core`; no browser, script engine, external
//! renderer, or PDF-producing dependency is used.

use std::path::{Path, PathBuf};
use std::str::Utf8Error;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{many, optional, required};
use goat_common::{Ctx, GoatError, Registry, Verb, paths};
use serde_json::{Map, Value};

mod html;
mod images;
mod markdown;

/// `DEFAULT_CSS`, the stylesheet `from-md` uses without `--css`.
const DEFAULT_CSS: &str = r#"
@page { size: A4; margin: 2cm; }
body { font-family: -apple-system, "Helvetica Neue", Arial, sans-serif;
  font-size: 11pt; line-height: 1.55; color: #1a1a1a; }
h1, h2, h3, h4 { font-weight: 600; line-height: 1.25; margin: 1.4em 0 .5em; color: #111; }
h1 { font-size: 1.9em; } h2 { font-size: 1.45em; } h3 { font-size: 1.2em; }
p { margin: 0 0 .8em; }
a { color: #0b5cad; text-decoration: none; }
ul, ol { margin: 0 0 .8em 1.4em; }
li { margin: .2em 0; }
code { font-family: "SF Mono", ui-monospace, Menlo, monospace; font-size: .9em;
  background: #f3f4f6; padding: .1em .3em; border-radius: 3px; }
pre { background: #f3f4f6; padding: .9em 1em; border-radius: 6px; overflow: auto; }
pre code { background: none; padding: 0; }
blockquote { margin: 0 0 .8em; padding: .2em 1em; border-left: 3px solid #d0d5dd; color: #475467; }
table { border-collapse: collapse; width: 100%; margin: 0 0 1em; font-size: .95em; }
th, td { border: 1px solid #d0d5dd; padding: .45em .7em; text-align: left; }
th { background: #f9fafb; font-weight: 600; }
img { max-width: 100%; }
hr { border: none; border-top: 1px solid #e4e7ec; margin: 1.5em 0; }
"#;

/// Registers `from-images`, `from-html`, and `from-md`.
pub fn register(registry: &mut Registry) {
    let output = || Arg::new("output").short('o').long("output");
    registry.command(Verb::new(
        Command::new("from-images")
            .about("build a PDF from images")
            .arg(Arg::new("images").required(true).num_args(1..))
            .arg(output()),
        from_images,
    ));
    registry.command(Verb::new(
        Command::new("from-html")
            .about("render an HTML file to PDF")
            .arg(Arg::new("file").required(true))
            .arg(output()),
        from_html,
    ));
    registry.command(Verb::new(
        Command::new("from-md")
            .about("render Markdown to a styled PDF")
            .arg(Arg::new("file").required(true))
            .arg(
                Arg::new("css")
                    .long("css")
                    .help("path to a CSS file (overrides the default stylesheet)"),
            )
            .arg(output()),
        from_md,
    ));
}

pub(crate) fn pdf_error(error: pdf_core::Error) -> GoatError {
    GoatError::message(error.to_string())
}

fn path_text(path: &Path) -> Value {
    Value::from(path.to_string_lossy().into_owned())
}

/// `a.output or default`: an empty `--output` counts as absent.
fn output_arg<'m>(matches: &'m ArgMatches, default: &'m str) -> Result<&'m str, GoatError> {
    Ok(optional::<String>(matches, "output")?
        .map(String::as_str)
        .filter(|out| !out.is_empty())
        .unwrap_or(default))
}

/// `str(src.with_suffix(".pdf"))` for a resolved source.
fn with_pdf_suffix(src: &Path) -> String {
    let text = src.to_string_lossy();
    src.with_file_name(format!("{}.pdf", paths::stem(&text)))
        .to_string_lossy()
        .into_owned()
}

/// Python's `UnicodeDecodeError` for bytes that are not UTF-8.
fn utf8_error(bytes: &[u8], error: &Utf8Error) -> GoatError {
    let start = error.valid_up_to();
    let (len, reason) = match error.error_len() {
        None => (bytes.len() - start, "unexpected end of data"),
        Some(len) => {
            let lead = bytes.get(start).copied().unwrap_or(0);
            let valid_lead = matches!(lead, 0xC2..=0xF4);
            (
                len,
                if valid_lead {
                    "invalid continuation byte"
                } else {
                    "invalid start byte"
                },
            )
        }
    };
    let message = if len == 1 {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            bytes[start]
        )
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            start + len - 1
        )
    };
    GoatError::exception("UnicodeDecodeError", message)
}

/// `Path.read_text(encoding="utf-8")`: strict UTF-8 with universal newlines.
fn read_text(path: &Path) -> Result<String, GoatError> {
    let bytes = std::fs::read(path).map_err(|error| GoatError::os(&error, path))?;
    let text = std::str::from_utf8(&bytes).map_err(|error| utf8_error(&bytes, &error))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

fn write_output(out: &Path, data: &[u8]) -> Result<(), GoatError> {
    std::fs::write(out, data).map_err(|error| GoatError::os(&error, out))
}

fn from_images(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut sources = Vec::new();
    for image in many::<String>(matches, "images")? {
        sources.push(paths::resolve(image)?);
    }
    let out = paths::ensure_parent(output_arg(matches, "images.pdf")?)?;
    let refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
    let normalized = images::normalize_all(&refs)?;
    // Python opens the output before img2pdf runs, so an img2pdf error leaves it empty.
    std::fs::File::create(&out).map_err(|error| GoatError::os(&error, &out))?;
    let pdf = images::convert(normalized)?;
    write_output(&out, &pdf)?;
    let mut result = Map::new();
    result.insert("verb".into(), "from-images".into());
    result.insert(
        "inputs".into(),
        sources.iter().map(|source| path_text(source)).collect(),
    );
    result.insert("outputs".into(), Value::Array(vec![path_text(&out)]));
    result.insert("image_count".into(), sources.len().into());
    Ok(result)
}

fn rendered(verb: &str, src: &Path, out: &Path, bytes: usize) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("verb".into(), verb.into());
    result.insert("inputs".into(), Value::Array(vec![path_text(src)]));
    result.insert("outputs".into(), Value::Array(vec![path_text(out)]));
    result.insert("output_bytes".into(), bytes.into());
    result
}

fn from_html(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = paths::resolve(required::<String>(matches, "file")?)?;
    let default_out = with_pdf_suffix(&src);
    let out = paths::ensure_parent(output_arg(matches, &default_out)?)?;
    let bytes = std::fs::read(&src).map_err(|error| GoatError::os(&error, &src))?;
    let text = String::from_utf8_lossy(&bytes);
    let base = src.parent().unwrap_or(Path::new("/"));
    let pdf = html::render(&text, base)?;
    write_output(&out, &pdf)?;
    Ok(rendered("from-html", &src, &out, pdf.len()))
}

fn from_md(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = paths::resolve(required::<String>(matches, "file")?)?;
    let default_out = with_pdf_suffix(&src);
    let out = paths::ensure_parent(output_arg(matches, &default_out)?)?;
    let body = markdown::to_html(&read_text(&src)?);
    let css = match optional::<String>(matches, "css")?.filter(|css| !css.is_empty()) {
        Some(css) => read_text(&paths::expanduser(css))?,
        None => DEFAULT_CSS.to_owned(),
    };
    let text = src.to_string_lossy();
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset='utf-8'><title>{}</title><style>{css}</style></head>\
         <body>{body}</body></html>",
        paths::stem(&text)
    );
    let base = src.parent().unwrap_or(Path::new("/"));
    let pdf = html::render(&html, base)?;
    write_output(&out, &pdf)?;
    Ok(rendered("from-md", &src, &out, pdf.len()))
}
