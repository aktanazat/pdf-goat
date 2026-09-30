//! Opening and saving documents with the error texts and save settings of the two Python
//! libraries the reference CLI uses (pikepdf and PyMuPDF), and the argument plumbing
//! every verb shares.

use std::path::{Path, PathBuf};

use clap::ArgMatches;
use goat_common::GoatError;
use goat_common::args::{optional, required};
use goat_common::paths::{self, AtomicOutput};
use pdf_core::{Document, Encryption, Error as PdfError, SaveOptions};
use serde_json::{Map, Value};

/// The Python library a verb opens its input with; it decides the error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lib {
    PyMuPdf,
    PikePdf,
}

/// The resolved `file` argument: `resolve(a.file)`.
pub(crate) fn source(matches: &ArgMatches) -> Result<PathBuf, GoatError> {
    paths::resolve(required::<String>(matches, "file")?)
}

/// `ensure_parent(a.output or default_out(src, suffix))`.
pub(crate) fn out_path(
    matches: &ArgMatches,
    src: &Path,
    suffix: &str,
) -> Result<PathBuf, GoatError> {
    match optional::<String>(matches, "output")?.filter(|path| !path.is_empty()) {
        Some(path) => paths::ensure_parent(path),
        None => paths::ensure_parent(&paths::default_out(&display(src), suffix, "pdf")?),
    }
}

/// Opens a resolved input without a password. A pikepdf verb fails on a file the empty
/// password does not open; PyMuPDF opens it and fails later, where it first touches
/// locked content (see [`closed_or_encrypted`] and [`still_encrypted`]).
pub(crate) fn load(path: &Path, lib: Lib) -> Result<Document, GoatError> {
    let bytes = std::fs::read(path).map_err(|error| GoatError::os(&error, path))?;
    let doc = Document::load(bytes).map_err(|error| load_error(error, path, lib))?;
    if lib == Lib::PikePdf && doc.needs_password() {
        return Err(GoatError::exception(
            "PasswordError",
            format!("{}: invalid password", path.to_string_lossy()),
        ));
    }
    Ok(doc)
}

fn load_error(error: PdfError, path: &Path, lib: Lib) -> GoatError {
    if let PdfError::Io(error) = error {
        return GoatError::os(&error, path);
    }
    let display = path.to_string_lossy();
    match lib {
        Lib::PyMuPdf => {
            GoatError::exception("FileDataError", format!("Failed to open file '{display}'."))
        }
        Lib::PikePdf => GoatError::exception(
            "PdfError",
            format!("{display}: unable to find trailer dictionary while recovering damaged file"),
        ),
    }
}

/// PyMuPDF's error for editing pages of a locked document.
pub(crate) fn closed_or_encrypted() -> GoatError {
    GoatError::value_error("document closed or encrypted")
}

/// PyMuPDF's error for reading the outline of a locked document (`init_doc`).
pub(crate) fn still_encrypted() -> GoatError {
    GoatError::value_error("cannot initialize - document still encrypted")
}

/// Any other pdf-core failure, worded by pdf-core.
pub(crate) fn pdf_error(error: PdfError) -> GoatError {
    GoatError::message(error.to_string())
}

/// `_save_pdf`: PyMuPDF `save(garbage=2, deflate=True, use_objstms=1)`. PyMuPDF's default
/// `encryption=PDF_ENCRYPT_NONE` writes an owner-password-only input unencrypted.
pub(crate) fn save_mupdf(doc: &Document, out: &Path) -> Result<(), GoatError> {
    let options = SaveOptions {
        compress_streams: true,
        object_streams: true,
        garbage_collect: true,
        encryption: Encryption::Remove,
        ..SaveOptions::default()
    };
    save(doc, out, &options)
}

/// pikepdf `Pdf.save(out)`: a rewrite of the reachable objects, streams compressed, and
/// no encryption.
pub(crate) fn save_qpdf(doc: &Document, out: &Path) -> Result<(), GoatError> {
    let options = SaveOptions {
        compress_streams: true,
        garbage_collect: true,
        encryption: Encryption::Remove,
        ..SaveOptions::default()
    };
    save(doc, out, &options)
}

/// An initially empty pikepdf document starts at PDF 1.3, independently of its inputs.
pub(crate) fn save_qpdf_new(doc: &Document, out: &Path) -> Result<(), GoatError> {
    save(
        doc,
        out,
        &SaveOptions {
            compress_streams: true,
            garbage_collect: true,
            encryption: Encryption::Remove,
            version: Some((1, 3)),
            ..SaveOptions::default()
        },
    )
}

fn save(doc: &Document, out: &Path, options: &SaveOptions) -> Result<(), GoatError> {
    doc.save(out, options).map_err(|error| match error {
        PdfError::Io(error) => GoatError::os(&error, out),
        PdfError::NeedsPassword => closed_or_encrypted(),
        other => pdf_error(other),
    })
}

/// `with AtomicOutput(out) as partial`: write `{out}.part`, then rename it over `out`. A
/// failed write leaves no partial behind.
pub(crate) fn save_atomic(
    out: &Path,
    save: impl FnOnce(&Path) -> Result<(), GoatError>,
) -> Result<(), GoatError> {
    let atomic = AtomicOutput::new(out);
    save(atomic.partial())?;
    atomic.commit()
}

/// A path in the CLI's text representation.
pub(crate) fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Zero-based page indices as the one-based list a result reports.
pub(crate) fn one_based(indices: &[usize]) -> Value {
    Value::Array(
        indices
            .iter()
            .map(|&index| Value::from(index + 1))
            .collect(),
    )
}

/// `{"verb": ..., "inputs": [...], "outputs": [...]}`, the keys every result starts with.
pub(crate) fn result(verb: &str, inputs: Vec<String>, outputs: Vec<String>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("verb".to_owned(), Value::String(verb.to_owned()));
    map.insert(
        "inputs".to_owned(),
        Value::Array(inputs.into_iter().map(Value::String).collect()),
    );
    map.insert(
        "outputs".to_owned(),
        Value::Array(outputs.into_iter().map(Value::String).collect()),
    );
    map
}
