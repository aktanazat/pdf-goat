//! `meta get|set|strip` and `accessibility check|set`.

use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{many, optional, required};
use goat_common::py::repr_str;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Object, PdfDate, Stream};
use serde_json::{Map, Value, json};

use crate::doc::{self, Lib};
use crate::report::{is_tagged, language};

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "meta",
        Verb::new(
            Command::new("get")
                .about("read metadata")
                .arg(Arg::new("file").required(true)),
            meta_get,
        ),
    );
    registry.family_verb(
        "meta",
        Verb::new(
            Command::new("set")
                .about("set metadata key=value")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("set")
                        .long("set")
                        .action(ArgAction::Append)
                        .help("key=value (repeatable)"),
                )
                .arg(Arg::new("output").short('o').long("output")),
            meta_set,
        ),
    );
    registry.family_verb(
        "meta",
        Verb::new(
            Command::new("strip")
                .about("remove all metadata")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            meta_strip,
        ),
    );
    registry.family_verb(
        "accessibility",
        Verb::new(
            Command::new("check")
                .about("report basic tag, title, language, and image-alt checks")
                .arg(Arg::new("file").required(true)),
            access_check,
        ),
    );
    registry.family_verb(
        "accessibility",
        Verb::new(
            Command::new("set")
                .about("set title and language; set the Marked flag")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("title").long("title"))
                .arg(Arg::new("lang").long("lang").default_value("en"))
                .arg(Arg::new("output").short('o').long("output")),
            access_set,
        ),
    );
}

/// `doc.metadata` is `None` for a locked document, so `dict(doc.metadata)` raises this.
fn metadata_none() -> GoatError {
    GoatError::exception("TypeError", "'NoneType' object is not iterable")
}

/// The reference CLI computes `bool(doc.xref_xml_metadata)` on the bound method rather
/// than its result, so `has_xmp` is `true` for every document that opens.
const HAS_XMP: bool = true;

fn meta_get(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open(required::<String>(matches, "file")?, Lib::PyMuPdf)?;
    if opened.doc.needs_password() {
        return Err(metadata_none());
    }
    let mut result = doc::result("meta-get", opened.inputs(), Vec::new());
    result.insert(
        "metadata".to_owned(),
        Value::Object(doc::truthy(doc::metadata(&opened.doc))),
    );
    result.insert("has_xmp".to_owned(), Value::Bool(HAS_XMP));
    Ok(result)
}

/// PyMuPDF `set_metadata` key map; `format` and `encryption` are accepted and ignored.
const KEYMAP: [(&str, Option<&str>); 11] = [
    ("author", Some("Author")),
    ("producer", Some("Producer")),
    ("creator", Some("Creator")),
    ("title", Some("Title")),
    ("format", None),
    ("encryption", None),
    ("creationDate", Some("CreationDate")),
    ("modDate", Some("ModDate")),
    ("subject", Some("Subject")),
    ("keywords", Some("Keywords")),
    ("trapped", Some("Trapped")),
];

/// `doc.set_metadata(md)` for a non-empty dictionary.
fn set_metadata(document: &mut Document, entries: &[(String, Value)]) -> Result<(), GoatError> {
    let bad: Vec<String> = entries
        .iter()
        .filter(|(key, _)| !KEYMAP.iter().any(|(known, _)| known == key))
        .map(|(key, _)| repr_str(key))
        .collect();
    if !bad.is_empty() {
        return Err(GoatError::value_error(format!(
            "bad dict key(s): {{{}}}",
            bad.join(", ")
        )));
    }
    let mut info = document.info().map_err(doc::pdf_error)?.unwrap_or_default();
    for (key, value) in entries {
        let Some(Some(pdf_key)) = KEYMAP
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, pdf_key)| *pdf_key)
        else {
            continue;
        };
        let text = match value {
            Value::String(text) => text.as_str(),
            Value::Null => "",
            other => &goat_common::py::str_value(other),
        };
        if text.is_empty() || text == "none" || text == "null" {
            info.remove(pdf_key.as_bytes());
        } else {
            info.insert(pdf_key, Object::text(text));
        }
    }
    document.set_info(info);
    Ok(())
}

fn meta_set(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open(required::<String>(matches, "file")?, Lib::PyMuPdf)?;
    let out = doc::output_path(matches, &opened.display(), "meta")?;
    if opened.doc.needs_password() {
        return Err(metadata_none());
    }
    let mut entries = doc::metadata(&opened.doc);
    for item in many::<String>(matches, "set")? {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        let key = goat_common::py::strip(key).to_owned();
        match entries.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = Value::String(value.to_owned()),
            None => entries.push((key, Value::String(value.to_owned()))),
        }
    }
    set_metadata(&mut opened.doc, &entries)?;
    doc::save(&opened.doc, &out, &doc::mupdf_save_options())?;
    let mut result = doc::result("meta-set", opened.inputs(), vec![out]);
    result.insert("metadata".to_owned(), Value::Object(doc::truthy(entries)));
    Ok(result)
}

fn meta_strip(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open(required::<String>(matches, "file")?, Lib::PyMuPdf)?;
    let out = doc::output_path(matches, &opened.display(), "nometa")?;
    if opened.doc.needs_password() {
        return Err(doc::closed_or_encrypted());
    }
    // `set_metadata({})`: the trailer's /Info becomes null when there is one.
    if opened.doc.trailer().contains_key(b"Info") {
        opened.doc.trailer_mut().remove(b"Info");
    }
    // `del_xml_metadata()`.
    remove_catalog_key(&mut opened.doc, b"Metadata")?;
    doc::save(&opened.doc, &out, &doc::mupdf_save_options())?;
    Ok(doc::result("meta-strip", opened.inputs(), vec![out]))
}

/// Deletes `key` from the catalog dictionary.
pub(crate) fn remove_catalog_key(document: &mut Document, key: &[u8]) -> Result<(), GoatError> {
    let root = document.catalog_ref().map_err(doc::pdf_error)?;
    let mut catalog = document.catalog().map_err(doc::pdf_error)?;
    if catalog.remove(key).is_some() {
        document.set(root, catalog);
    }
    Ok(())
}

fn access_check(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_pikepdf(required::<String>(matches, "file")?)?;
    let document = &opened.doc;
    let tagged = is_tagged(document);
    let lang = language(document);
    let title = document
        .info()
        .map_err(doc::pdf_error)?
        .and_then(|info| {
            info.get(b"Title")
                .map(|title| match document.resolve(title) {
                    Ok(Object::String(text)) => text.to_text(),
                    Ok(other) => other.to_pretty_string(),
                    Err(_) => String::new(),
                })
        })
        .filter(|title| !title.is_empty());
    let mut alt: i64 = 0;
    for id in document.object_ids() {
        let has_alt = match document.get(id) {
            Ok(Object::Dict(dict)) => dict.contains_key(b"Alt"),
            Ok(Object::Stream(stream)) => stream.dict.contains_key(b"Alt"),
            _ => false,
        };
        alt += i64::from(has_alt);
    }
    let images: i64 = doc::pages(document)?
        .iter()
        .map(|page| i64::try_from(doc::page_images(document, page).len()).unwrap_or(i64::MAX))
        .sum();
    let issues: Vec<Value> = [
        ("untagged", !tagged),
        ("no_title", title.is_none()),
        ("no_lang", lang.as_deref().is_none_or(str::is_empty)),
        ("images_missing_alt", images - alt > 0),
    ]
    .into_iter()
    .filter(|(_, flagged)| *flagged)
    .map(|(code, _)| Value::String(code.to_owned()))
    .collect();
    let mut result = doc::result("access-check", opened.inputs(), Vec::new());
    result.insert("tagged".to_owned(), Value::Bool(tagged));
    result.insert("has_title".to_owned(), Value::Bool(title.is_some()));
    result.insert("title".to_owned(), title.map_or(Value::Null, Value::String));
    result.insert(
        "has_lang".to_owned(),
        Value::Bool(lang.as_deref().is_some_and(|lang| !lang.is_empty())),
    );
    result.insert("lang".to_owned(), lang.map_or(Value::Null, Value::String));
    result.insert("images".to_owned(), json!(images));
    result.insert(
        "images_without_alt".to_owned(),
        json!((images - alt).max(0)),
    );
    result.insert("issues".to_owned(), Value::Array(issues));
    Ok(result)
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// A minimal XMP packet carrying the given `dc:title`, plus any extra property elements.
pub(crate) fn xmp_packet(title: Option<&str>, extra: &str) -> Vec<u8> {
    let title = title.map_or(String::new(), |title| {
        format!(
            "   <dc:title>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </dc:title>\n",
            xml_escape(title)
        )
    });
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
<rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n\
{title}{extra}  \
</rdf:Description>\n \
</rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>\n"
    )
    .into_bytes()
}

const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_PDF: &str = "http://ns.adobe.com/pdf/1.3/";
/// pikepdf's metadata editor stamps `pdf:Producer` with its own name and version; this
/// port stamps its own.
const PRODUCER: &str = "pdf-goat";

/// How an XMP value becomes a DocumentInfo string.
#[derive(Clone, Copy)]
enum Convert {
    Text,
    /// Several authors join with `; `.
    Authors,
    /// An ISO date becomes a PDF date.
    Date,
}

/// pikepdf's `DOCINFO_MAPPING`: (namespace, conventional prefix, property, Info key).
const DOCINFO_MAPPING: [(&str, &str, &str, &str, Convert); 8] = [
    (NS_DC, "dc", "creator", "Author", Convert::Authors),
    (NS_DC, "dc", "description", "Subject", Convert::Text),
    (NS_DC, "dc", "title", "Title", Convert::Text),
    (NS_PDF, "pdf", "Keywords", "Keywords", Convert::Text),
    (NS_PDF, "pdf", "Producer", "Producer", Convert::Text),
    (NS_XMP, "xmp", "CreateDate", "CreationDate", Convert::Date),
    (NS_XMP, "xmp", "CreatorTool", "Creator", Convert::Text),
    (NS_XMP, "xmp", "ModifyDate", "ModDate", Convert::Date),
];

/// A property read back from a packet.
enum XmpValue {
    /// Simple text, an attribute, or the first `rdf:Alt` item.
    Text(String),
    /// The items of an `rdf:Seq` or `rdf:Bag`.
    Items(Vec<String>),
}

fn xml_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix('x') {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The prefixes the packet binds to `uri`, else the conventional one.
fn prefixes_for(text: &str, uri: &str, conventional: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices("xmlns:") {
        let Some((prefix, after)) = text[at + "xmlns:".len()..].split_once('=') else {
            continue;
        };
        let valid = !prefix.is_empty()
            && prefix
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
        let value = after.trim_start();
        let Some(quote) = value
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\''))
        else {
            continue;
        };
        if valid
            && value[1..]
                .find(quote)
                .is_some_and(|end| &value[1..1 + end] == uri)
            && !out.iter().any(|p| p == prefix)
        {
            out.push(prefix.to_owned());
        }
    }
    if out.is_empty() {
        out.push(conventional.to_owned());
    }
    out
}

/// The start of the `<tag` element (not a longer name sharing the prefix).
fn element_start(text: &str, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    text.match_indices(&open).map(|(at, _)| at).find(|at| {
        text[at + open.len()..]
            .chars()
            .next()
            .is_none_or(|ch| ch.is_ascii_whitespace() || matches!(ch, '>' | '/'))
    })
}

/// `[start, end)` of the whole element plus the end of its start tag.
fn element_span(text: &str, tag: &str) -> Option<(usize, usize, usize)> {
    let start = element_start(text, tag)?;
    let open_end = start + text[start..].find('>')?;
    if text[..open_end].ends_with('/') {
        return Some((start, open_end + 1, open_end + 1));
    }
    let close = format!("</{tag}>");
    let end = open_end + text[open_end..].find(&close)? + close.len();
    Some((start, end, open_end + 1))
}

/// The `rdf:li` texts inside a container element.
fn list_items(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut rest = inner;
    while let Some((_, end, body)) = element_span(rest, "rdf:li") {
        let li = &rest[..end];
        items.push(xml_unescape(
            li[body..].strip_suffix("</rdf:li>").unwrap_or("").trim(),
        ));
        rest = &rest[end..];
    }
    items
}

/// pikepdf's `XmpDocument.__getitem__`: the element's text, its `rdf:Alt` first item or
/// `rdf:Seq`/`rdf:Bag` items, or the property attribute on `rdf:Description`.
fn xmp_value(text: &str, uri: &str, conventional: &str, name: &str) -> Option<XmpValue> {
    for prefix in prefixes_for(text, uri, conventional) {
        let tag = format!("{prefix}:{name}");
        if let Some((start, end, body)) = element_span(text, &tag) {
            let inner = text[body..end]
                .strip_suffix(&format!("</{tag}>"))
                .unwrap_or("");
            let plain = inner.trim();
            if start + 1 < body && !plain.is_empty() && !plain.starts_with('<') {
                return Some(XmpValue::Text(xml_unescape(plain)));
            }
            if let Some((_, alt_end, alt_body)) = element_span(inner, "rdf:Alt") {
                return Some(XmpValue::Text(
                    list_items(&inner[alt_body..alt_end])
                        .into_iter()
                        .next()
                        .unwrap_or_default(),
                ));
            }
            for container in ["rdf:Seq", "rdf:Bag"] {
                if let Some((_, c_end, c_body)) = element_span(inner, container) {
                    return Some(XmpValue::Items(list_items(&inner[c_body..c_end])));
                }
            }
            return Some(XmpValue::Text(String::new()));
        }
        let attribute = format!(" {tag}=\"");
        if let Some(at) = text.find(&attribute) {
            let value = &text[at + attribute.len()..];
            let end = value.find('"')?;
            return Some(XmpValue::Text(xml_unescape(&value[..end])));
        }
    }
    None
}

/// pikepdf's `DateConverter.docinfo_from_xmp`: a bare year or month stays as digits,
/// otherwise the ISO date becomes `D:YYYYMMDDHHmmSS` with `+HH'mm` for a known offset.
fn pdf_date_from_iso(value: &str) -> Option<String> {
    if (value.len() == 4 || value.len() == 7) && !value.contains('T') {
        return Some(format!("D:{}", value.replace('-', "")));
    }
    let (stamp, offset) = match value.strip_suffix('Z') {
        Some(stamp) => (stamp, Some(0i16)),
        None => match value.rfind(['+', '-']).filter(|at| *at >= 10) {
            Some(at) => {
                let (stamp, zone) = value.split_at(at);
                let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
                let (hours, minutes) = match digits.len() {
                    2 => (digits.parse::<i16>().ok()?, 0),
                    4 => (
                        digits[..2].parse::<i16>().ok()?,
                        digits[2..].parse::<i16>().ok()?,
                    ),
                    _ => return None,
                };
                let sign = if zone.starts_with('-') { -1 } else { 1 };
                (stamp, Some(sign * (hours * 60 + minutes)))
            }
            None => (value, None),
        },
    };
    let (date, time) = stamp.split_once(['T', ' ']).unwrap_or((stamp, ""));
    let date_digits: String = date.chars().filter(char::is_ascii_digit).collect();
    if date_digits.len() != 8 || date.chars().filter(|ch| *ch == '-').count() > 2 {
        return None;
    }
    let time = time.split('.').next().unwrap_or("");
    let time_digits: String = time.chars().filter(char::is_ascii_digit).collect();
    if !matches!(time_digits.len(), 0 | 4 | 6) {
        return None;
    }
    let padded = format!("{time_digits:0<6}");
    let parsed = PdfDate::parse(&format!("{date_digits}{padded}"))?;
    let mut out = format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}",
        parsed.year, parsed.month, parsed.day, parsed.hour, parsed.minute, parsed.second
    );
    if let Some(offset) = offset {
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.unsigned_abs();
        out.push_str(&format!("{sign}{:02}'{:02}", abs / 60, abs % 60));
    }
    Some(out)
}

/// `_update_docinfo`: every mapped Info key follows the packet; a property the packet
/// lacks (or a date it cannot convert) removes the key.
fn sync_docinfo(document: &mut Document, packet: &str) -> Result<(), GoatError> {
    let mut info = document.info().map_err(doc::pdf_error)?.unwrap_or_default();
    for (uri, conventional, property, key, convert) in DOCINFO_MAPPING {
        let value = match (xmp_value(packet, uri, conventional, property), convert) {
            (None, _) => None,
            (Some(XmpValue::Text(text)), Convert::Text | Convert::Authors) => Some(text),
            (Some(XmpValue::Items(items)), Convert::Text | Convert::Authors) => {
                Some(items.join("; "))
            }
            (Some(XmpValue::Text(text)), Convert::Date) => pdf_date_from_iso(&text),
            (Some(XmpValue::Items(_)), Convert::Date) => None,
        };
        match value {
            Some(value) => {
                info.insert(key, Object::text(&value));
            }
            None => {
                info.remove(key.as_bytes());
            }
        }
    }
    document.set_info(info);
    Ok(())
}

/// `XmpDocument.set_value`: replace the element's content (or the property attribute),
/// else append a namespaced element to the description.
fn set_xmp_property(packet: &mut String, uri: &str, conventional: &str, name: &str, inner: &str) {
    for prefix in prefixes_for(packet, uri, conventional) {
        let tag = format!("{prefix}:{name}");
        if let Some((start, end, body)) = element_span(packet, &tag) {
            let mut start_tag = packet[start..body]
                .trim_end_matches("/>")
                .trim_end_matches('>')
                .to_owned();
            start_tag.push('>');
            packet.replace_range(start..end, &format!("{start_tag}{inner}</{tag}>"));
            return;
        }
        let attribute = format!(" {tag}=\"");
        if let Some(at) = packet.find(&attribute) {
            let value_start = at + attribute.len();
            if let Some(len) = packet[value_start..].find('"') {
                packet.replace_range(
                    value_start..value_start + len,
                    &xml_escape(&xml_unescape(inner)),
                );
                return;
            }
        }
    }
    let element = format!(
        "<{conventional}:{name} xmlns:{conventional}=\"{uri}\">{inner}</{conventional}:{name}>"
    );
    match packet.find("</rdf:Description>") {
        Some(at) => packet.insert_str(at, &element),
        None => {
            *packet = format!(
                "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"{PRODUCER}\">\n \
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n \
<rdf:Description rdf:about=\"\">{element}</rdf:Description></rdf:RDF>\n\
</x:xmpmeta>\n\n<?xpacket end=\"w\"?>\n"
            );
        }
    }
}

/// pikepdf's `open_metadata()` block setting `dc:title`: the packet gets the title, the
/// editor's `xmp:MetadataDate` and `pdf:Producer`, and DocumentInfo is rebuilt from it.
fn edit_xmp_title(document: &mut Document, title: &str) -> Result<(), GoatError> {
    let root = document.catalog_ref().map_err(doc::pdf_error)?;
    let mut catalog = document.catalog().map_err(doc::pdf_error)?;
    let existing = catalog.get(b"Metadata").cloned();
    let existing_stream = existing
        .as_ref()
        .and_then(|value| document.resolve_stream(value).ok().flatten());
    let mut packet = existing_stream
        .as_ref()
        .and_then(|stream| document.decode_stream(stream).ok())
        .map(|decoded| String::from_utf8_lossy(&decoded.data).into_owned())
        .filter(|text| text.contains("</rdf:Description>"))
        .unwrap_or_default();
    let alt = format!(
        "<rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt>",
        xml_escape(title)
    );
    set_xmp_property(&mut packet, NS_DC, "dc", "title", &alt);
    let now = PdfDate::now();
    let stamp = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    );
    set_xmp_property(&mut packet, NS_XMP, "xmp", "MetadataDate", &stamp);
    set_xmp_property(&mut packet, NS_PDF, "pdf", "Producer", PRODUCER);
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Metadata"));
    dict.insert("Subtype", Object::name("XML"));
    let stream = Stream::new(dict, packet.clone().into_bytes());
    match existing
        .as_ref()
        .and_then(Object::as_reference)
        .filter(|_| existing_stream.is_some())
    {
        Some(id) => document.set(id, stream),
        None => {
            let id = document.add(stream);
            catalog.insert("Metadata", Object::Reference(id));
            document.set(root, catalog);
        }
    }
    sync_docinfo(document, &packet)
}

fn access_set(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open_pikepdf(required::<String>(matches, "file")?)?;
    let out = doc::output_path(matches, &opened.display(), "accessible")?;
    let title = optional::<String>(matches, "title")?
        .cloned()
        .filter(|title| !title.is_empty());
    let lang = required::<String>(matches, "lang")?.clone();
    let document = &mut opened.doc;
    let root = document.catalog_ref().map_err(doc::pdf_error)?;
    let mut catalog = document.catalog().map_err(doc::pdf_error)?;
    if !lang.is_empty() {
        catalog.insert("Lang", Object::text(&lang));
    }
    let mut mark_info = Dict::new();
    mark_info.insert("Marked", Object::Bool(true));
    catalog.insert("MarkInfo", Object::Dict(mark_info));
    document.set(root, catalog);
    if let Some(title) = &title {
        edit_xmp_title(document, title)?;
        let mut info = document.info().map_err(doc::pdf_error)?.unwrap_or_default();
        info.insert("Title", Object::text(title));
        document.set_info(info);
    }
    doc::save(document, &out, &doc::pikepdf_save_options())?;
    let mut result = doc::result("access-set", opened.inputs(), vec![out]);
    result.insert(
        "title".to_owned(),
        optional::<String>(matches, "title")?
            .cloned()
            .map_or(Value::Null, Value::String),
    );
    result.insert("lang".to_owned(), Value::String(lang));
    result.insert(
        "note".to_owned(),
        Value::String(
            "Sets the Marked, Lang, and Title entries. It does not create a full tag tree."
                .to_owned(),
        ),
    );
    Ok(result)
}
