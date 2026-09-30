//! `redact`, `edit text`, and the `convert tables|xlsx|docx|pptx` verbs.
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
mod plumber;
mod pyfmt;
mod redact;
mod word;

use std::fmt;

use clap::{Arg, Command};
use goat_common::args::int_value;
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
