//! `redact`, `edit text|add-text|add-image`, and the `convert tables|xlsx|docx|pptx` verbs.
//!
//! Redaction rewrites matched glyph strings and affected image samples, removes
//! covered vector segments, and saves a garbage-collected full rewrite. Effective
//! page resources are materialized before editing so ancestor dictionaries cannot
//! retain the old form streams. Hidden optional content is included in removal.
//! Matched text/combo fields are cleared through `pdf_forms`, including inherited
//! values, reset defaults, rich text and every shared widget appearance. This
//! intentionally fixes the Python path's appearance-only field clearing.
//!
//! The matcher searches page words and field values, not arbitrary metadata or
//! attachments. Table exports follow pdfplumber's default ruled-table extraction.
//! DOCX output groups nearby lines into editable, page-positioned paragraphs
//! without joining separate columns. Ruled tables retain native merged cells.
//! A transparent graphics layer preserves paths, images, clipping and blending
//! without baking the editable body text into pixels. Standard-14 font names map
//! to metrically compatible Office families. PPTX uses one page image per slide.

mod commands;
mod edit_text;
mod image_redact;
mod ooxml;
mod path_redact;
mod place;
mod plumber;
mod pyfmt;
mod redact;
mod word;

use std::fmt;

use clap::{Arg, Command};
use goat_common::args::{float_value, int_value};
use goat_common::{GoatError, Registry, Verb};

/// Why an edit failed.
#[derive(Debug)]
pub enum EditError {
    Pdf(pdf_core::Error),
    Interp(pdf_interp::InterpError),
    Message(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Pdf(error) => write!(f, "{error}"),
            EditError::Interp(error) => write!(f, "{error}"),
            EditError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<pdf_core::Error> for EditError {
    fn from(error: pdf_core::Error) -> EditError {
        EditError::Pdf(error)
    }
}

impl From<pdf_interp::InterpError> for EditError {
    fn from(error: pdf_interp::InterpError) -> EditError {
        EditError::Interp(error)
    }
}

impl From<EditError> for GoatError {
    fn from(error: EditError) -> GoatError {
        GoatError::message(error.to_string())
    }
}

const ADD_TEXT_HELP: &str = "\
Use this to put a typed signature, a date or a check mark on a flat form (one without fillable
fields). Find the label with `search`, pass a point beside its rectangle to --at, then check the
result with `render --clip`. Each run writes a new file.

Examples:
  pdf-goat search form.pdf 'MEMBER SIGNATURE'
  pdf-goat edit add-text form.pdf --text 'Jane Q. Member' --font 'Brush Script MT' --size 20 --at 90,612 -o step1.pdf
  pdf-goat edit add-text step1.pdf --text 09/30/2026 --at 420,612 -o step2.pdf
  pdf-goat edit add-text step2.pdf --text ✔ --font ZapfDingbats --at 74,660 -o signed.pdf
  pdf-goat render signed.pdf --pages 1 --clip 60,560,560,680 --dpi 200 -o check";

/// Registers the crate's verbs; the CLI calls this once.
pub fn register(registry: &mut Registry) {
    let file = || Arg::new("file").required(true);
    let output = || Arg::new("output").short('o').long("output");
    registry.command(Verb::new(
        Command::new("redact")
            .about("redact words matching a regular expression")
            .arg(file())
            .arg(
                Arg::new("find").long("find").required(true).help(
                    "case-insensitive regex over the page's words; the same matcher as search",
                ),
            )
            .arg(output()),
        commands::redact,
    ));
    registry.family_verb(
        "edit",
        Verb::new(
            Command::new("text")
                .about("find and replace simple text runs; no reflow or embedded-font matching")
                .arg(file())
                .arg(Arg::new("find").long("find").required(true))
                .arg(Arg::new("replace").long("replace").required(true))
                .arg(output()),
            commands::edit_text,
        ),
    );
    let page = || {
        Arg::new("page")
            .long("page")
            .value_parser(int_value)
            .default_value("1")
            .help("1-based page number")
    };
    registry.family_verb(
        "edit",
        Verb::new(
            Command::new("add-text")
                .about("draw text into the page content: a typed signature, date or check mark on a flat form")
                .after_help(ADD_TEXT_HELP)
                .arg(file())
                .arg(Arg::new("text").long("text").required(true).help("one line of text"))
                .arg(Arg::new("at").long("at").required(true).help(
                    "x,y start of the baseline, in search's frame: points from the crop box's top-left, y down",
                ))
                .arg(page())
                .arg(Arg::new("font").long("font").default_value("Helvetica").help(
                    "a Standard 14 name, an installed font such as 'Brush Script MT', or a .ttf/.ttc path; \
                     other than Standard 14, the font is embedded as a subset",
                ))
                .arg(
                    Arg::new("size")
                        .long("size")
                        .value_parser(float_value)
                        .default_value("12")
                        .help("font size in points"),
                )
                .arg(Arg::new("color").long("color").help("#rrggbb, a gray level, or r,g,b from 0 to 1; default black"))
                .arg(output()),
            commands::add_text,
        ),
    );
    registry.family_verb(
        "edit",
        Verb::new(
            Command::new("add-image")
                .about(
                    "draw a PNG or JPEG into the page content, fitted and centred in a rectangle",
                )
                .arg(file())
                .arg(
                    Arg::new("image")
                        .long("image")
                        .required(true)
                        .help("PNG (transparency kept) or JPEG (embedded unchanged)"),
                )
                .arg(
                    Arg::new("rect")
                        .long("rect")
                        .required(true)
                        .help("x0,y0,x1,y1 in search's frame; the image keeps its aspect ratio"),
                )
                .arg(page())
                .arg(output()),
            commands::add_image,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("tables")
                .about("extract ruled tables to CSV")
                .arg(file())
                .arg(Arg::new("outdir").short('o').long("outdir")),
            commands::tables,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("xlsx")
                .about("convert extracted PDF tables to Excel (.xlsx)")
                .arg(file())
                .arg(output()),
            commands::xlsx,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("docx")
                .about("convert PDF to editable Word (.docx)")
                .arg(file())
                .arg(output()),
            commands::docx,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("pptx")
                .about("render each PDF page as one PowerPoint slide")
                .arg(file())
                .arg(
                    Arg::new("dpi")
                        .long("dpi")
                        .value_parser(int_value)
                        .default_value("150"),
                )
                .arg(output()),
            commands::pptx,
        ),
    );
}
