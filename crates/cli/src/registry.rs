//! The one place that composes every verb family into the `pdf-goat` parser.

use std::sync::LazyLock;

use clap::{Arg, ArgAction, Command};
use goat_common::{Cli, Registry};

use crate::{builtin, office};

/// Top-level commands in the Python parser's order; `capabilities` and help list them so.
const ORDER: [&str; 41] = [
    "transcript",
    "office",
    "capabilities",
    "inspect",
    "preflight",
    "info",
    "merge",
    "split",
    "extract",
    "delete",
    "reorder",
    "rotate",
    "render",
    "from-images",
    "redact",
    "watermark",
    "compress",
    "text",
    "from-html",
    "from-md",
    "form",
    "annotate",
    "security",
    "meta",
    "pages",
    "get",
    "bookmarks",
    "links",
    "convert",
    "optimize",
    "edit",
    "accessibility",
    "compare",
    "repair",
    "attach",
    "detach",
    "search",
    "overlay",
    "count",
    "setup",
    "jobs",
];

/// Each family's help line.
const FAMILY_HELP: [(&str, &str); 16] = [
    ("transcript", "extract academic transcript data"),
    (
        "office",
        "create, edit, and export with LibreOffice (macOS)",
    ),
    ("form", "form fields"),
    ("annotate", "annotations"),
    ("security", "encryption, signatures, and sanitization"),
    ("meta", "read and edit metadata"),
    ("pages", "layout, numbering, imposition"),
    ("get", "extract assets"),
    ("bookmarks", "edit the document outline"),
    ("links", "edit links"),
    ("convert", "conversion and OCR"),
    ("optimize", "reduce file size"),
    ("edit", "edit page content"),
    ("accessibility", "check and set accessibility metadata"),
    ("compare", "compare two PDFs"),
    ("setup", "install the optional local model"),
];

/// The composed parser, built once per process.
static CLI: LazyLock<Cli> = LazyLock::new(|| {
    let mut registry = Registry::new();
    office::register(&mut registry);
    builtin::register(&mut registry);
    pdf_compose::register(&mut registry);
    pdf_sign::register(&mut registry);
    pdf_inspect::register(&mut registry);
    pdf_organize::register(&mut registry);
    pdf_render::register(&mut registry);
    pdf_forms::register(&mut registry);
    pdf_edit::register(&mut registry);
    pdf_text::register(&mut registry);
    registry.into_cli(root(), &ORDER, &FAMILY_HELP)
});

pub fn cli() -> &'static Cli {
    &CLI
}

fn root() -> Command {
    Command::new("pdf-goat")
        .about("Local PDF editing and inspection tool")
        .arg(
            Arg::new("agent")
                .long("agent")
                .action(ArgAction::SetTrue)
                .help("write JSON output even on a TTY"),
        )
}
