//! `security encrypt|decrypt|permissions|sanitize`.

use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{flag, optional, required};
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Dict, Document, Encryption, NewEncryption, NewMethod, Object, Stream};
use serde_json::{Map, Value};

use crate::doc::{self, Lib};
use crate::meta::remove_catalog_key;

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("encrypt")
                .about("encrypt with AES-256")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("password")
                        .long("password")
                        .required(true)
                        .help("user password"),
                )
                .arg(Arg::new("owner").long("owner"))
                .arg(Arg::new("output").short('o').long("output")),
            encrypt,
        ),
    );
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("decrypt")
                .about("remove encryption")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("password").long("password").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            decrypt,
        ),
    );
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("permissions")
                .about("restrict printing, copying, or changes")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("owner").long("owner").required(true))
                .arg(Arg::new("user").long("user").default_value(""))
                .arg(
                    Arg::new("no_print")
                        .long("no-print")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no_copy")
                        .long("no-copy")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no_modify")
                        .long("no-modify")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("output").short('o').long("output")),
            permissions,
        ),
    );
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("sanitize")
                .about(
                    "remove JavaScript, embedded or attached files, XMP metadata, and thumbnails",
                )
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            sanitize,
        ),
    );
}

/// pikepdf's `Permissions()` default: everything but `modify_assembly`.
const DEFAULT_PERMISSIONS: i32 = -1028;
/// Every permission bit set, reserved bits 1 and 2 clear.
const ALL_PERMISSIONS: i32 = -4;

fn aes256(user: &str, owner: &str, permissions: i32) -> Encryption {
    Encryption::New(NewEncryption {
        method: NewMethod::Aes256,
        user_password: user.as_bytes().to_vec(),
        owner_password: owner.as_bytes().to_vec(),
        permissions,
        encrypt_metadata: true,
    })
}

fn encrypt(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_pikepdf(required::<String>(matches, "file")?)?;
    let out = doc::output_path(matches, &opened.display(), "encrypted")?;
    let password = required::<String>(matches, "password")?;
    let owner = optional::<String>(matches, "owner")?
        .filter(|owner| !owner.is_empty())
        .unwrap_or(password);
    let options = pdf_core::SaveOptions {
        encryption: aes256(password, owner, DEFAULT_PERMISSIONS),
        ..doc::pikepdf_save_options()
    };
    doc::save(&opened.doc, &out, &options)?;
    let mut result = doc::result("sec-encrypt", opened.inputs(), vec![out]);
    result.insert("algorithm".to_owned(), Value::String("AES-256".to_owned()));
    Ok(result)
}

fn decrypt(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let password = required::<String>(matches, "password")?;
    let opened = doc::open_with(
        required::<String>(matches, "file")?,
        Lib::PikePdf,
        Some(password.as_bytes()),
    )?;
    if opened.doc.needs_password() {
        return Err(doc::invalid_password(&opened.path));
    }
    let out = doc::output_path(matches, &opened.display(), "decrypted")?;
    doc::save(&opened.doc, &out, &doc::pikepdf_save_options())?;
    Ok(doc::result("sec-decrypt", opened.inputs(), vec![out]))
}

fn permissions(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_pikepdf(required::<String>(matches, "file")?)?;
    let out = doc::output_path(matches, &opened.display(), "restricted")?;
    let owner = required::<String>(matches, "owner")?;
    let user = required::<String>(matches, "user")?;
    let (no_print, no_copy, no_modify) = (
        flag(matches, "no_print")?,
        flag(matches, "no_copy")?,
        flag(matches, "no_modify")?,
    );
    let mut bits = ALL_PERMISSIONS;
    if no_print {
        bits &= !(4 | 2048);
    }
    if no_copy {
        bits &= !16;
    }
    if no_modify {
        bits &= !(8 | 32 | 256 | 1024);
    }
    let options = pdf_core::SaveOptions {
        encryption: aes256(user, owner, bits),
        ..doc::pikepdf_save_options()
    };
    doc::save(&opened.doc, &out, &options)?;
    let mut result = doc::result("sec-permissions", opened.inputs(), vec![out]);
    result.insert("no_print".to_owned(), Value::Bool(no_print));
    result.insert("no_copy".to_owned(), Value::Bool(no_copy));
    result.insert("no_modify".to_owned(), Value::Bool(no_modify));
    Ok(result)
}

/// A dictionary that PyMuPDF's scrub prints with `/S /JavaScript` somewhere inside.
fn mentions_javascript(object: &Object) -> bool {
    let mut found = false;
    doc::walk_direct(object, &mut |dict| {
        if dict.get_name(b"S") == Some(b"JavaScript") {
            found = true;
        }
    });
    found
}

/// Nulls every `/Metadata` entry inside `object`; true when one was found.
fn null_metadata_keys(object: &mut Object, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    let mut changed = false;
    match object {
        Object::Dict(dict) => {
            if dict.contains_key(b"Metadata") {
                dict.insert("Metadata", Object::Null);
                changed = true;
            }
            for (_, value) in dict.iter_mut() {
                changed |= null_metadata_keys(value, depth + 1);
            }
        }
        Object::Stream(stream) => {
            if stream.dict.contains_key(b"Metadata") {
                stream.dict.insert("Metadata", Object::Null);
                changed = true;
            }
        }
        Object::Array(items) => {
            for item in items {
                changed |= null_metadata_keys(item, depth + 1);
            }
        }
        _ => {}
    }
    changed
}

/// `doc.scrub(attached_files, embedded_files, javascript, xml_metadata, thumbnails)`.
fn scrub(document: &mut Document) -> Result<(), GoatError> {
    let pages = doc::pages(document)?;
    // Attached files: the embedded stream of every FileAttachment annotation becomes a blank.
    for attachment in doc::file_attachment_annotations(document, &pages) {
        let embedded = attachment
            .filespec
            .get(b"EF")
            .and_then(|ef| document.resolve_dict(ef).ok().flatten())
            .unwrap_or_default();
        for key in [&b"F"[..], b"UF"] {
            let Some(id) = embedded.get_ref(key) else {
                continue;
            };
            if let Ok(Object::Stream(mut stream)) = document.get(id) {
                stream.set_decoded(b" ".to_vec());
                document.set(id, stream);
            }
        }
    }
    // Thumbnails.
    for page in &pages {
        if let Ok(Object::Dict(mut stored)) = document.get(page.id)
            && stored.remove(b"Thumb").is_some()
        {
            document.set(page.id, stored);
        }
    }
    // Embedded files and XMP metadata at the catalog.
    document
        .set_names(b"EmbeddedFiles", Vec::new())
        .map_err(doc::pdf_error)?;
    remove_catalog_key(document, b"Metadata")?;
    // JavaScript and metadata streams anywhere in the file.
    for id in document.object_ids() {
        let Ok(mut object) = document.get(id) else {
            continue;
        };
        if mentions_javascript(&object) {
            let mut replacement = Dict::new();
            replacement.insert("S", Object::name("JavaScript"));
            replacement.insert("JS", Object::string(Vec::new()));
            document.set(id, replacement);
            continue;
        }
        if let Object::Stream(stream) = &object
            && stream.dict.has_type(b"Metadata")
        {
            document.set(id, Stream::new(Dict::new(), b"deleted".to_vec()));
            continue;
        }
        if null_metadata_keys(&mut object, 0) {
            document.set(id, object);
        }
    }
    Ok(())
}

fn sanitize(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open(required::<String>(matches, "file")?, Lib::PyMuPdf)?;
    let out = doc::output_path(matches, &opened.display(), "sanitized")?;
    if opened.doc.needs_password() {
        return Err(GoatError::value_error("closed or encrypted doc"));
    }
    scrub(&mut opened.doc)?;
    doc::save(&opened.doc, &out, &doc::mupdf_save_options())?;
    let mut result = doc::result("sec-sanitize", opened.inputs(), vec![out]);
    result.insert(
        "removed".to_owned(),
        Value::Array(
            [
                "javascript",
                "embedded_files",
                "attached_files",
                "xml_metadata",
                "thumbnails",
            ]
            .into_iter()
            .map(|item| Value::String(item.to_owned()))
            .collect(),
        ),
    );
    Ok(result)
}
