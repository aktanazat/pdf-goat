//! Document access shared by every verb: opening a file with the reference CLI's error
//! texts, the metadata dictionary as PyMuPDF reports it, resource scans for fonts and
//! images, annotation and link classification, page labels, the pikepdf-style structure
//! walk, and embedded-file lookup.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use goat_common::GoatError;
use goat_common::args::optional;
use goat_common::paths;
use pdf_core::{
    Dict, Document, Encryption, Error as PdfError, ObjRef, Object, Page, PdfString, SaveOptions,
    Stream,
};
use serde_json::{Map, Value};

/// Nesting bound for walks over direct objects and resource trees.
const MAX_DEPTH: usize = 64;

/// The Python library that opened the file in the reference CLI; it decides the text of
/// the error a damaged file raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lib {
    PyMuPdf,
    PikePdf,
}

/// A loaded document with the resolved path the result reports.
pub(crate) struct Opened {
    pub doc: Document,
    pub path: PathBuf,
}

impl Opened {
    pub fn display(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    pub fn inputs(&self) -> Vec<String> {
        vec![self.display()]
    }
}

/// `resolve(path)` then open, without a password.
pub(crate) fn open(path: &str, lib: Lib) -> Result<Opened, GoatError> {
    open_with(path, lib, None)
}

/// `resolve(path)` then open with `password` (the empty password is tried as well).
pub(crate) fn open_with(
    path: &str,
    lib: Lib,
    password: Option<&[u8]>,
) -> Result<Opened, GoatError> {
    let resolved = paths::resolve(path)?;
    let bytes = std::fs::read(&resolved).map_err(|error| GoatError::os(&error, &resolved))?;
    let loaded = match password {
        Some(password) => Document::load_with_password(bytes, password),
        None => Document::load(bytes),
    };
    let doc = loaded.map_err(|error| load_error(error, &resolved, lib))?;
    Ok(Opened {
        doc,
        path: resolved,
    })
}

fn load_error(error: PdfError, path: &Path, lib: Lib) -> GoatError {
    match error {
        PdfError::Io(error) => GoatError::os(&error, path),
        PdfError::WrongPassword => invalid_password(path),
        _ => {
            let display = path.to_string_lossy();
            match lib {
                Lib::PyMuPdf => GoatError::exception(
                    "FileDataError",
                    format!("Failed to open file '{display}'."),
                ),
                Lib::PikePdf => GoatError::exception(
                    "PdfError",
                    format!(
                        "{display}: unable to find trailer dictionary while recovering damaged file"
                    ),
                ),
            }
        }
    }
}

/// pikepdf's `PasswordError` for a locked file or a wrong password.
pub(crate) fn invalid_password(path: &Path) -> GoatError {
    GoatError::exception(
        "PasswordError",
        format!("{}: invalid password", path.to_string_lossy()),
    )
}

/// PyMuPDF's error for touching pages or metadata of a locked document.
pub(crate) fn closed_or_encrypted() -> GoatError {
    GoatError::value_error("document closed or encrypted")
}

/// A pikepdf-backed verb: a file the empty password does not open is a `PasswordError`.
pub(crate) fn open_pikepdf(path: &str) -> Result<Opened, GoatError> {
    let opened = open(path, Lib::PikePdf)?;
    if opened.doc.needs_password() {
        return Err(invalid_password(&opened.path));
    }
    Ok(opened)
}

/// `_open_unlocked(src, command)`.
pub(crate) fn open_unlocked(path: &str, command: &str) -> Result<Opened, GoatError> {
    let opened = open(path, Lib::PyMuPdf)?;
    if opened.doc.needs_password() {
        return Err(GoatError::needs_password(command));
    }
    Ok(opened)
}

/// A PyMuPDF-backed verb that reads pages or metadata: a locked file is a `ValueError`.
pub(crate) fn open_pymupdf(path: &str) -> Result<Opened, GoatError> {
    let opened = open(path, Lib::PyMuPdf)?;
    if opened.doc.needs_password() {
        return Err(closed_or_encrypted());
    }
    Ok(opened)
}

/// Any pdf-core failure past opening, as a `PdfGoatError` carrying its text.
pub(crate) fn pdf_error(error: PdfError) -> GoatError {
    GoatError::message(error.to_string())
}

/// `{"verb": ..., "inputs": [...], "outputs": [...]}` in the order every result starts with.
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

/// `ensure_parent(a.output or default_out(src, suffix))`.
pub(crate) fn output_path(
    matches: &ArgMatches,
    source: &str,
    suffix: &str,
) -> Result<String, GoatError> {
    let requested = optional::<String>(matches, "output")?.filter(|path| !path.is_empty());
    let path = match requested {
        Some(path) => path.clone(),
        None => paths::default_out(source, suffix, "pdf")?,
    };
    Ok(paths::ensure_parent(&path)?.to_string_lossy().into_owned())
}

/// `_save_pdf`: MuPDF's save with `garbage=2`, `deflate=True`, and object streams. The
/// default `encryption=PDF_ENCRYPT_NONE` drops the document's encryption (an owner-locked
/// file reopens unencrypted; probed by Organize).
pub(crate) fn mupdf_save_options() -> SaveOptions {
    SaveOptions {
        compress_streams: true,
        object_streams: true,
        garbage_collect: true,
        encryption: Encryption::Remove,
        ..SaveOptions::default()
    }
}

/// pikepdf's `pdf.save(out)`: uncompressed streams are compressed and encryption is
/// dropped (pikepdf removes it unless asked to preserve it).
pub(crate) fn pikepdf_save_options() -> SaveOptions {
    SaveOptions {
        compress_streams: true,
        encryption: Encryption::Remove,
        ..SaveOptions::default()
    }
}

pub(crate) fn save(doc: &Document, out: &str, options: &SaveOptions) -> Result<(), GoatError> {
    doc.save(out, options).map_err(pdf_error)
}

/// Python `round(value, 1)` on a MuPDF single-precision coordinate.
pub(crate) fn round1(value: f64) -> f64 {
    let single = f64::from(value as f32);
    (single * 10.0).round_ties_even() / 10.0
}

// ----------------------------------------------------------------------------------- //
// Metadata and permissions
// ----------------------------------------------------------------------------------- //

/// PyMuPDF metadata keys after `format`, with their `/Info` entries.
const METADATA_KEYS: [(&str, &[u8]); 9] = [
    ("title", b"Title"),
    ("author", b"Author"),
    ("subject", b"Subject"),
    ("keywords", b"Keywords"),
    ("creator", b"Creator"),
    ("producer", b"Producer"),
    ("creationDate", b"CreationDate"),
    ("modDate", b"ModDate"),
    ("trapped", b"Trapped"),
];

/// `doc.metadata`: every key, absent `/Info` values as `""`, `encryption` null unless the
/// document is encrypted.
pub(crate) fn metadata(doc: &Document) -> Vec<(String, Value)> {
    let (major, minor) = doc.header_version();
    let info = doc.info().ok().flatten();
    let mut out = vec![(
        "format".to_owned(),
        Value::String(format!("PDF {major}.{minor}")),
    )];
    for (key, pdf_key) in METADATA_KEYS {
        let text = info
            .as_ref()
            .and_then(|info| info.get(pdf_key))
            .map_or_else(String::new, |value| info_text(doc, value));
        out.push((key.to_owned(), Value::String(text)));
    }
    out.push((
        "encryption".to_owned(),
        doc.encryption_description()
            .map_or(Value::Null, Value::String),
    ));
    out
}

fn info_text(doc: &Document, value: &Object) -> String {
    match doc.resolve(value) {
        Ok(Object::String(text)) => text.to_text(),
        Ok(Object::Name(name)) => String::from_utf8_lossy(name.as_bytes()).into_owned(),
        _ => String::new(),
    }
}

/// `{k: v for k, v in md.items() if v}`.
pub(crate) fn truthy(entries: Vec<(String, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .filter(|(_, value)| goat_common::py::truthy(value))
        .collect()
}

/// `_permissions(doc.permissions)`.
pub(crate) fn permissions(flags: i32) -> Map<String, Value> {
    const BITS: [(&str, i32); 8] = [
        ("print", 4),
        ("print_high_quality", 2048),
        ("modify", 8),
        ("copy", 16),
        ("annotate", 32),
        ("fill_forms", 256),
        ("accessibility", 512),
        ("assemble", 1024),
    ];
    BITS.iter()
        .map(|(key, bit)| ((*key).to_owned(), Value::Bool(flags & bit != 0)))
        .collect()
}

/// `doc.permissions`: every bit set for an unencrypted document.
pub(crate) fn permission_flags(doc: &Document) -> i32 {
    doc.permissions().unwrap_or(-1)
}

// ----------------------------------------------------------------------------------- //
// Pages
// ----------------------------------------------------------------------------------- //

/// `page.rotation`: `/Rotate` normalised to 0..360, zero unless a multiple of 90.
pub(crate) fn rotation(page: &Page) -> i64 {
    let raw = page
        .dict
        .get(b"Rotate")
        .and_then(Object::as_i64)
        .unwrap_or(0);
    let turned = raw.rem_euclid(360);
    if turned % 90 == 0 { turned } else { 0 }
}

/// `page.rect` width and height: the crop box, turned by the page rotation.
pub(crate) fn page_size(page: &Page) -> (f64, f64) {
    let rect = page.crop_box();
    let (width, height) = (rect.width(), rect.height());
    if rotation(page) % 180 == 90 {
        (height, width)
    } else {
        (width, height)
    }
}

/// Every page, or PyMuPDF's error when the document is locked.
pub(crate) fn pages(doc: &Document) -> Result<Vec<Page>, GoatError> {
    if doc.needs_password() {
        return Err(closed_or_encrypted());
    }
    doc.pages().map_err(pdf_error)
}

/// `pdf_text.page_text_and_words`: the page text and its words.
pub(crate) fn page_text(doc: &Document, index: usize) -> Result<(String, usize), GoatError> {
    let (text, words) = pdf_text::page_text_and_words(doc, index).map_err(GoatError::from)?;
    Ok((text, words.len()))
}

// ----------------------------------------------------------------------------------- //
// Annotations and links
// ----------------------------------------------------------------------------------- //

/// Annotation subtypes MuPDF knows; others never load as annotations.
const KNOWN_SUBTYPES: [&[u8]; 28] = [
    b"Text",
    b"Link",
    b"FreeText",
    b"Line",
    b"Square",
    b"Circle",
    b"Polygon",
    b"PolyLine",
    b"Highlight",
    b"Underline",
    b"Squiggly",
    b"StrikeOut",
    b"Redact",
    b"Stamp",
    b"Caret",
    b"Ink",
    b"Popup",
    b"FileAttachment",
    b"Sound",
    b"Movie",
    b"RichMedia",
    b"Widget",
    b"Screen",
    b"PrinterMark",
    b"TrapNet",
    b"Watermark",
    b"3D",
    b"Projection",
];

/// How PyMuPDF classifies a link (`link["kind"]`) and the target it reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkKind {
    /// `LINK_URI` with `link["uri"]`.
    Uri(String),
    /// `LINK_LAUNCH` with `link["file"]`.
    Launch(String),
    /// `LINK_GOTOR` with `link["file"]`.
    GotoR(String),
    /// `LINK_GOTO` or `LINK_NAMED`: internal.
    Internal,
}

impl LinkKind {
    /// `_link_preflight`: an external link that is not an HTTP, HTTPS, or mailto URI.
    pub fn is_external(&self) -> bool {
        !matches!(self, LinkKind::Internal)
    }

    pub fn is_unsafe(&self) -> bool {
        match self {
            LinkKind::Uri(uri) => {
                let scheme = uri
                    .split_once(':')
                    .map_or("", |(scheme, _)| scheme)
                    .to_lowercase();
                !matches!(scheme.as_str(), "http" | "https" | "mailto")
            }
            LinkKind::Launch(_) | LinkKind::GotoR(_) => true,
            LinkKind::Internal => false,
        }
    }
}

/// An annotation MuPDF loads: its object and dictionary.
#[derive(Clone, Debug)]
pub(crate) struct Annot {
    pub id: ObjRef,
    pub dict: Dict,
}

impl Annot {
    pub fn subtype(&self) -> String {
        self.dict
            .get_name(b"Subtype")
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .unwrap_or_default()
    }
}

/// What `page.annots()`, `page.widgets()`, and `page.get_links()` see on one page.
#[derive(Clone, Debug, Default)]
pub(crate) struct PageAnnots {
    /// Indirect annotations of a known subtype other than Link, Popup, and Widget.
    pub annotations: Vec<Annot>,
    /// Indirect Widget annotations.
    pub widgets: Vec<Annot>,
    /// Link annotations carrying an action or a destination.
    pub links: Vec<LinkKind>,
}

pub(crate) fn page_annots(doc: &Document, page: &Page) -> PageAnnots {
    let mut out = PageAnnots::default();
    let Some(items) = page
        .dict
        .get(b"Annots")
        .and_then(|annots| doc.resolve_array(annots).ok().flatten())
    else {
        return out;
    };
    for item in &items {
        let Some(dict) = doc.resolve_dict(item).ok().flatten() else {
            continue;
        };
        let subtype = dict.get_name(b"Subtype").unwrap_or(b"");
        if subtype == b"Link" {
            if let Some(kind) = classify_link(doc, &dict) {
                out.links.push(kind);
            }
            continue;
        }
        let Some(id) = item.as_reference() else {
            continue;
        };
        if subtype == b"Popup" || !KNOWN_SUBTYPES.contains(&subtype) {
            continue;
        }
        let is_widget = subtype == b"Widget";
        let annot = Annot { id, dict };
        if is_widget {
            out.widgets.push(annot);
        } else {
            out.annotations.push(annot);
        }
    }
    out
}

fn text_of(doc: &Document, value: Option<&Object>) -> String {
    match value.map(|value| doc.resolve(value)) {
        Some(Ok(Object::String(text))) => text.to_text(),
        Some(Ok(Object::Name(name))) => String::from_utf8_lossy(name.as_bytes()).into_owned(),
        _ => String::new(),
    }
}

/// `pdf_parse_file_spec`: the file an action names, `/F` as a string or the `/Unix`,
/// `/UF`, or `/F` string of a file specification dictionary. A missing or non-string
/// name is not a link at all.
fn action_file(doc: &Document, action: &Dict) -> Option<String> {
    match action.get(b"F").map(|value| doc.resolve(value)) {
        Some(Ok(Object::Dict(spec))) => {
            let name = spec
                .get(b"Unix")
                .or_else(|| spec.get(b"UF"))
                .or_else(|| spec.get(b"F"))?;
            match doc.resolve(name) {
                Ok(Object::String(text)) => Some(text.to_text()),
                _ => None,
            }
        }
        Some(Ok(Object::String(text))) => Some(text.to_text()),
        _ => None,
    }
}

/// `fz_is_external_link`: the URI starts with a scheme (`[A-Za-z][A-Za-z0-9+.-]*:`).
fn has_scheme(uri: &str) -> bool {
    let mut chars = uri.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')))
            == Some(':')
}

/// PyMuPDF's `linkDest` on an external MuPDF URI: `file:` URIs are launches, or remote
/// go-tos when their fragment is a page; anything else with a scheme is a URI link.
fn external_kind(uri: String) -> LinkKind {
    match uri.strip_prefix("file:") {
        Some(spec) => {
            let spec = spec.strip_prefix("//").unwrap_or(spec);
            match spec.split_once('#') {
                Some((file, fragment))
                    if fragment.starts_with("page=") && !fragment.contains('#') =>
                {
                    LinkKind::GotoR(file.to_owned())
                }
                _ => LinkKind::Launch(spec.to_owned()),
            }
        }
        None => LinkKind::Uri(uri),
    }
}

/// `pdf_parse_file_spec` followed by `linkDest`: a file plus an explicit destination
/// (or none, which MuPDF writes as `#page=1`) is a remote go-to; a named destination
/// keeps the `#nameddest=` fragment and is a launch.
fn file_link(doc: &Document, action: &Dict) -> Option<LinkKind> {
    let file = action_file(doc, action)?;
    let dest = action.get(b"D").map(|dest| doc.resolve(dest));
    Some(match dest {
        Some(Ok(Object::Name(name))) => LinkKind::Launch(format!(
            "{file}#nameddest={}",
            String::from_utf8_lossy(name.as_bytes())
        )),
        Some(Ok(Object::String(text))) => {
            LinkKind::Launch(format!("{file}#nameddest={}", text.to_text()))
        }
        _ => LinkKind::GotoR(file),
    })
}

/// `pdf_parse_link_dest`: a name, string, or array destination is an internal link.
fn dest_link(doc: &Document, dest: &Object) -> Option<LinkKind> {
    match doc.resolve(dest) {
        Ok(Object::Name(_) | Object::String(_) | Object::Array(_)) => Some(LinkKind::Internal),
        _ => None,
    }
}

/// `pdf_parse_link_action`: the actions MuPDF turns into a link URI; any other action
/// (JavaScript, Hide, SubmitForm, ...) is not a link.
fn action_link(doc: &Document, action: &Dict) -> Option<LinkKind> {
    match action.get_name(b"S")? {
        b"GoTo" => dest_link(doc, action.get(b"D")?),
        b"URI" => {
            let uri = text_of(doc, action.get(b"URI"));
            if has_scheme(&uri) {
                return Some(external_kind(uri));
            }
            let base = doc
                .catalog()
                .ok()
                .and_then(|catalog| {
                    catalog
                        .get(b"URI")
                        .and_then(|entry| doc.resolve_dict(entry).ok().flatten())
                })
                .and_then(|entry| entry.get(b"Base").map(|base| text_of(doc, Some(base))))
                .unwrap_or_else(|| "file://".to_owned());
            let uri = format!("{base}{uri}");
            Some(if has_scheme(&uri) {
                external_kind(uri)
            } else {
                LinkKind::Internal
            })
        }
        b"Launch" | b"GoToR" => file_link(doc, action),
        b"Named" => matches!(
            action.get_name(b"N"),
            Some(b"FirstPage" | b"LastPage" | b"PrevPage" | b"NextPage")
        )
        .then_some(LinkKind::Internal),
        _ => None,
    }
}

/// `pdf_load_link`: a `/Rect` is required; `/Dest` wins, else `/A`, else the additional
/// action's mouse-up (`/U`) or mouse-down (`/D`) entry; then PyMuPDF's kind.
fn classify_link(doc: &Document, dict: &Dict) -> Option<LinkKind> {
    match dict.get(b"Rect").map(|rect| doc.resolve(rect)) {
        Some(Ok(Object::Null)) | None => return None,
        _ => {}
    }
    if let Some(dest) = dict.get(b"Dest") {
        return dest_link(doc, dest);
    }
    let action = match dict.get(b"A") {
        Some(action) => doc.resolve_dict(action).ok().flatten(),
        None => dict
            .get(b"AA")
            .and_then(|extra| doc.resolve_dict(extra).ok().flatten())
            .and_then(|extra| extra.get(b"U").or_else(|| extra.get(b"D")).cloned())
            .and_then(|action| doc.resolve_dict(&action).ok().flatten()),
    };
    action_link(doc, &action?)
}

/// `_file_attachment_annotations`: the FileAttachment annotations of every page with the
/// name PyMuPDF reports (`/UF`, else `/F`, of the file specification).
pub(crate) struct AnnotAttachment {
    pub page: usize,
    pub annot: Annot,
    pub name: String,
    pub filespec: Dict,
}

pub(crate) fn file_attachment_annotations(doc: &Document, pages: &[Page]) -> Vec<AnnotAttachment> {
    let mut out = Vec::new();
    for page in pages {
        for annot in page_annots(doc, page).annotations {
            if annot.dict.get_name(b"Subtype") != Some(b"FileAttachment") {
                continue;
            }
            let filespec = annot
                .dict
                .get(b"FS")
                .and_then(|fs| doc.resolve_dict(fs).ok().flatten())
                .unwrap_or_default();
            let name = text_of(doc, filespec.get(b"UF").or_else(|| filespec.get(b"F")));
            out.push(AnnotAttachment {
                page: page.index,
                annot,
                name,
                filespec,
            });
        }
    }
    out
}

/// Removes `id` from the page's `/Annots` array, touching only that page object.
pub(crate) fn delete_annot(doc: &mut Document, page: &Page, id: ObjRef) -> Result<(), GoatError> {
    let mut stored = doc.get(page.id).map_err(pdf_error)?;
    let Some(page_dict) = stored.as_dict_mut() else {
        return Ok(());
    };
    let annots = page_dict.get(b"Annots").cloned().unwrap_or(Object::Null);
    let (array_ref, mut items) = match doc.resolve(&annots).map_err(pdf_error)? {
        Object::Array(items) => (annots.as_reference(), items),
        _ => return Ok(()),
    };
    items.retain(|item| item.as_reference() != Some(id));
    match array_ref {
        Some(array_id) => doc.set(array_id, Object::Array(items)),
        None => {
            page_dict.insert("Annots", Object::Array(items));
            doc.set(page.id, stored);
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------------- //
// Resource scans (JM_scan_resources)
// ----------------------------------------------------------------------------------- //

/// One entry of `doc.get_page_fonts(pno)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FontEntry {
    pub xref: u32,
    pub ext: String,
    pub subtype: String,
    pub name: String,
    pub encoding: String,
}

/// One entry of `page.get_images(full=True)`: the image object and its stream.
#[derive(Clone, Debug)]
pub(crate) struct ImageEntry {
    pub xref: u32,
    pub stream: Stream,
}

pub(crate) fn page_fonts(doc: &Document, page: &Page) -> Vec<FontEntry> {
    let mut out = Vec::new();
    let mut tracer = Vec::new();
    scan_resources(
        doc,
        &page.resources,
        &mut |resources| gather_fonts(doc, resources, &mut out),
        &mut tracer,
        0,
    );
    out
}

pub(crate) fn page_images(doc: &Document, page: &Page) -> Vec<ImageEntry> {
    let mut out = Vec::new();
    let mut tracer = Vec::new();
    scan_resources(
        doc,
        &page.resources,
        &mut |resources| gather_images(doc, resources, &mut out),
        &mut tracer,
        0,
    );
    out
}

fn scan_resources(
    doc: &Document,
    resources: &Dict,
    gather: &mut dyn FnMut(&Dict),
    tracer: &mut Vec<u32>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    gather(resources);
    let Some(xobjects) = resources
        .get(b"XObject")
        .and_then(|x| doc.resolve_dict(x).ok().flatten())
    else {
        return;
    };
    for (_, value) in xobjects.iter() {
        let xref = value.as_reference().map_or(0, |id| id.num);
        let Ok(Object::Stream(stream)) = doc.resolve(value) else {
            continue;
        };
        let Some(sub) = stream
            .dict
            .get(b"Resources")
            .and_then(|r| doc.resolve_dict(r).ok().flatten())
        else {
            continue;
        };
        if tracer.contains(&xref) {
            return;
        }
        tracer.push(xref);
        scan_resources(doc, &sub, gather, tracer, depth + 1);
    }
}

fn name_text(value: Option<&Object>) -> String {
    value
        .and_then(Object::as_name)
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_default()
}

fn gather_fonts(doc: &Document, resources: &Dict, out: &mut Vec<FontEntry>) {
    let Some(fonts) = resources
        .get(b"Font")
        .and_then(|f| doc.resolve_dict(f).ok().flatten())
    else {
        return;
    };
    for (_, value) in fonts.iter() {
        let Some(font) = doc.resolve_dict(value).ok().flatten() else {
            continue;
        };
        let xref = value.as_reference().map_or(0, |id| id.num);
        let base = font
            .get(b"BaseFont")
            .map(|b| doc.resolve(b).unwrap_or(Object::Null))
            .filter(|b| !b.is_null());
        let name = match base {
            Some(base) => name_text(Some(&base)),
            None => name_text(font.get(b"Name").and_then(|n| doc.resolve(n).ok()).as_ref()),
        };
        let encoding = match font.get(b"Encoding").map(|e| doc.resolve(e)) {
            Some(Ok(Object::Dict(encoding))) => name_text(encoding.get(b"BaseEncoding")),
            Some(Ok(other)) => name_text(Some(&other)),
            _ => String::new(),
        };
        let ext = if xref == 0 {
            "n/a".to_owned()
        } else {
            font_extension(doc, &font)
        };
        out.push(FontEntry {
            xref,
            ext,
            subtype: name_text(font.get(b"Subtype")),
            name,
            encoding,
        });
    }
}

/// `JM_get_fontextension`.
fn font_extension(doc: &Document, font: &Dict) -> String {
    let descriptor = match font
        .get(b"DescendantFonts")
        .and_then(|d| doc.resolve_array(d).ok().flatten())
    {
        Some(descendants) => descendants
            .first()
            .and_then(|d| doc.resolve_dict(d).ok().flatten())
            .and_then(|d| {
                d.get(b"FontDescriptor")
                    .and_then(|fd| doc.resolve_dict(fd).ok().flatten())
            }),
        None => font
            .get(b"FontDescriptor")
            .and_then(|fd| doc.resolve_dict(fd).ok().flatten()),
    };
    let Some(descriptor) = descriptor else {
        return "n/a".to_owned();
    };
    if descriptor.contains_key(b"FontFile") {
        return "pfa".to_owned();
    }
    if descriptor.contains_key(b"FontFile2") {
        return "ttf".to_owned();
    }
    let file3 = descriptor
        .get(b"FontFile3")
        .and_then(|f| doc.resolve_stream(f).ok().flatten());
    match file3.as_ref().and_then(|f| f.dict.get_name(b"Subtype")) {
        Some(b"Type1C") => "cff".to_owned(),
        Some(b"CIDFontType0C") => "cid".to_owned(),
        Some(b"OpenType") => "otf".to_owned(),
        _ => "n/a".to_owned(),
    }
}

fn gather_images(doc: &Document, resources: &Dict, out: &mut Vec<ImageEntry>) {
    let Some(xobjects) = resources
        .get(b"XObject")
        .and_then(|x| doc.resolve_dict(x).ok().flatten())
    else {
        return;
    };
    for (_, value) in xobjects.iter() {
        let Ok(Object::Stream(stream)) = doc.resolve(value) else {
            continue;
        };
        if stream.dict.get_name(b"Subtype") != Some(b"Image") {
            continue;
        }
        out.push(ImageEntry {
            xref: value.as_reference().map_or(0, |id| id.num),
            stream,
        });
    }
}

// ----------------------------------------------------------------------------------- //
// Page labels
// ----------------------------------------------------------------------------------- //

/// The `/PageLabels` rules sorted by start page.
pub(crate) fn page_label_rules(doc: &Document) -> Vec<(i64, Dict)> {
    let mut rules: Vec<(i64, Dict)> = doc
        .page_labels()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(start, rule)| {
            doc.resolve_dict(&rule)
                .ok()
                .flatten()
                .map(|dict| (start, dict))
        })
        .collect();
    rules.sort_by_key(|(start, _)| *start);
    rules
}

/// `page.get_label()`.
pub(crate) fn page_label(doc: &Document, rules: &[(i64, Dict)], index: usize) -> String {
    let pno = i64::try_from(index).unwrap_or(i64::MAX);
    let Some((start, rule)) = rules.iter().rev().find(|(start, _)| *start <= pno) else {
        return String::new();
    };
    let prefix = text_of(doc, rule.get(b"P"));
    let style = rule.get_name(b"S").unwrap_or(b"");
    let first = rule
        .get(b"St")
        .and_then(|st| doc.resolve_i64(st).ok().flatten())
        .unwrap_or(1);
    let delta = if matches!(style, b"a" | b"A") { -1 } else { 0 };
    let number = pno - start + first + delta;
    let digits = match style {
        b"D" => number.to_string(),
        b"r" => roman(number).to_lowercase(),
        b"R" => roman(number),
        b"a" => letters(number).to_lowercase(),
        b"A" => letters(number),
        _ => String::new(),
    };
    format!("{prefix}{digits}")
}

fn roman(mut number: i64) -> String {
    const NUMERALS: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, numeral) in NUMERALS {
        while number >= value {
            out.push_str(numeral);
            number -= value;
        }
    }
    out
}

/// PyMuPDF `integerToLetter`: `A`..`Z`, then `AA`..`ZZ`, and so on.
fn letters(number: i64) -> String {
    let index = number.max(0);
    let (repeat, offset) = (index / 26, index % 26);
    let letter = char::from(b'A' + u8::try_from(offset).unwrap_or(0));
    std::iter::repeat_n(letter, usize::try_from(repeat + 1).unwrap_or(1)).collect()
}

// ----------------------------------------------------------------------------------- //
// Structure walk (pikepdf)
// ----------------------------------------------------------------------------------- //

/// Counts `_structure_preflight` takes from every object in the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Structure {
    /// `/OpenAction` and `/AA` in the catalog.
    pub root_actions: u64,
    /// Dictionaries with `/JS` or `/S /JavaScript`.
    pub javascript: u64,
    /// Dictionaries with `/S /Launch`.
    pub launch: u64,
    /// Dictionaries with `/FT /Sig`.
    pub signatures: u64,
    /// The AcroForm carries `/XFA`.
    pub xfa: bool,
}

impl Structure {
    pub fn active_content(&self) -> u64 {
        self.root_actions + self.javascript + self.launch
    }
}

/// `_structure_preflight`: pikepdf's walk over every indirect dictionary and the direct
/// dictionaries and arrays inside it (streams and the trailer are not visited).
pub(crate) fn structure(doc: &Document) -> Structure {
    let mut out = Structure::default();
    if let Ok(catalog) = doc.catalog() {
        out.xfa = catalog
            .get(b"AcroForm")
            .and_then(|form| doc.resolve_dict(form).ok().flatten())
            .is_some_and(|form| form.contains_key(b"XFA"));
    }
    for id in doc.object_ids() {
        if let Ok(object @ (Object::Dict(_) | Object::Array(_))) = doc.get(id) {
            walk_direct(&object, &mut |dict| {
                let action = dict.get_name(b"S");
                out.root_actions += u64::from(dict.contains_key(b"OpenAction"))
                    + u64::from(dict.contains_key(b"AA"));
                if dict.contains_key(b"JS") || action == Some(b"JavaScript") {
                    out.javascript += 1;
                }
                if action == Some(b"Launch") {
                    out.launch += 1;
                }
                if dict.get_name(b"FT") == Some(b"Sig") {
                    out.signatures += 1;
                }
            });
        }
    }
    out
}

/// Every dictionary reachable from `object` without following references.
pub(crate) fn walk_direct(object: &Object, visit: &mut dyn FnMut(&Dict)) {
    fn walk(object: &Object, visit: &mut dyn FnMut(&Dict), depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match object {
            Object::Dict(dict) => {
                visit(dict);
                for (_, value) in dict.iter() {
                    walk(value, visit, depth + 1);
                }
            }
            Object::Stream(stream) => {
                visit(&stream.dict);
                for (_, value) in stream.dict.iter() {
                    walk(value, visit, depth + 1);
                }
            }
            Object::Array(items) => {
                for item in items {
                    walk(item, visit, depth + 1);
                }
            }
            _ => {}
        }
    }
    walk(object, visit, 0);
}

// ----------------------------------------------------------------------------------- //
// Forms, layers, and embedded files
// ----------------------------------------------------------------------------------- //

/// `doc.is_form_pdf`: the number of `/AcroForm /Fields`, zero without a form.
pub(crate) fn form_field_count(doc: &Document) -> usize {
    doc.catalog()
        .ok()
        .and_then(|catalog| {
            catalog
                .get(b"AcroForm")
                .and_then(|form| doc.resolve_dict(form).ok().flatten())
        })
        .and_then(|form| {
            form.get(b"Fields")
                .and_then(|fields| doc.resolve_array(fields).ok().flatten())
        })
        .map_or(0, |fields| fields.len())
}

/// `_has_xfa`: the AcroForm has an `/XFA` entry.
pub(crate) fn has_xfa(doc: &Document) -> bool {
    doc.catalog()
        .ok()
        .and_then(|catalog| {
            catalog
                .get(b"AcroForm")
                .and_then(|form| doc.resolve_dict(form).ok().flatten())
        })
        .is_some_and(|form| form.contains_key(b"XFA"))
}

/// `len(doc.get_ocgs())`: the distinct optional content groups.
pub(crate) fn layer_count(doc: &Document) -> usize {
    let groups = doc
        .catalog()
        .ok()
        .and_then(|catalog| {
            catalog
                .get(b"OCProperties")
                .and_then(|oc| doc.resolve_dict(oc).ok().flatten())
        })
        .and_then(|oc| {
            oc.get(b"OCGs")
                .and_then(|ocgs| doc.resolve_array(ocgs).ok().flatten())
        })
        .unwrap_or_default();
    let ids: HashSet<u32> = groups
        .iter()
        .filter_map(Object::as_reference)
        .map(|id| id.num)
        .collect();
    ids.len()
}

/// The catalog's embedded files: `(name, file specification)` in tree order.
pub(crate) fn embedded_files(doc: &Document) -> Result<Vec<(String, Dict)>, GoatError> {
    let entries = doc.names(b"EmbeddedFiles").map_err(pdf_error)?;
    let mut out = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let name =
            String::from_utf8(key.clone()).unwrap_or_else(|_| PdfString::literal(key).to_text());
        let filespec = doc
            .resolve_dict(&value)
            .map_err(pdf_error)?
            .unwrap_or_default();
        out.push((name, filespec));
    }
    Ok(out)
}

/// The decoded payload of a file specification's embedded stream (`/EF /F`, else `/EF /UF`).
pub(crate) fn filespec_data(doc: &Document, filespec: &Dict) -> Result<Vec<u8>, GoatError> {
    let embedded = filespec
        .get(b"EF")
        .and_then(|ef| doc.resolve_dict(ef).ok().flatten())
        .unwrap_or_default();
    let stream = embedded
        .get(b"F")
        .or_else(|| embedded.get(b"UF"))
        .and_then(|f| doc.resolve_stream(f).ok().flatten())
        .ok_or_else(|| GoatError::value_error("bad PDF: file entry not found"))?;
    Ok(doc.decode_stream(&stream).map_err(pdf_error)?.data)
}
