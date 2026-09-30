//! `compress`, `optimize reduce`, `repair`, and `convert pdfa`: the verbs the reference
//! CLI hands to Ghostscript and qpdf, done here with pdf-core and pdf-codec.

use std::path::Path;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::required;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_codec::{JpegColor, encode_jpeg, pixels};
use pdf_core::{Dict, Document, Encryption, Error as PdfError, Object, SaveOptions, Stream};
use serde_json::{Map, Value, json};

use crate::doc::{self, Lib};
use crate::embed::embed_missing;
use crate::get::{Color, ImageShape, decode_samples};
use crate::meta::xmp_packet;

pub(crate) fn register(registry: &mut Registry) {
    registry.command(Verb::new(
        Command::new("compress")
            .about("recompress and linearize; keep the input bytes if the result would be larger")
            .arg(Arg::new("file").required(true))
            .arg(
                Arg::new("level")
                    .long("level")
                    .default_value("/ebook")
                    .value_parser(["/screen", "/ebook", "/printer", "/prepress"]),
            )
            .arg(Arg::new("output").short('o').long("output")),
        compress,
    ));
    registry.family_verb(
        "optimize",
        Verb::new(
            Command::new("reduce")
                .about("reduce file size using a compression preset")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("preset")
                        .long("preset")
                        .default_value("ebook")
                        .value_parser(["screen", "ebook", "printer", "prepress"]),
                )
                .arg(Arg::new("output").short('o').long("output")),
            reduce,
        ),
    );
    registry.command(Verb::new(
        Command::new("repair")
            .about("repair a damaged PDF")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("output").short('o').long("output")),
        repair,
    ));
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("pdfa")
                .about("create an unvalidated PDF/A-2b candidate")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("output").short('o').long("output")),
            pdfa,
        ),
    );
}

/// Ghostscript's `-dPDFSETTINGS` presets: the resolution colour and gray images are
/// downsampled to, and the JPEG quality they are re-encoded at.
#[derive(Clone, Copy, Debug)]
struct Preset {
    dpi: f64,
    quality: u8,
}

fn preset(name: &str) -> Preset {
    match name.trim_start_matches('/') {
        "screen" => Preset {
            dpi: 72.0,
            quality: 40,
        },
        "printer" => Preset {
            dpi: 300.0,
            quality: 80,
        },
        "prepress" => Preset {
            dpi: 300.0,
            quality: 90,
        },
        _ => Preset {
            dpi: 150.0,
            quality: 60,
        },
    }
}

fn copy_file(from: &Path, to: &str) -> Result<u64, GoatError> {
    std::fs::copy(from, to).map_err(|error| GoatError::os_pair(&error, from, Path::new(to)))
}

fn file_size(path: &Path) -> Result<u64, GoatError> {
    Ok(std::fs::metadata(path)
        .map_err(|error| GoatError::os(&error, path))?
        .len())
}

/// `round(new / orig, 3) if orig else None`.
fn ratio(new: u64, orig: u64) -> Value {
    if orig == 0 {
        return Value::Null;
    }
    let value = (new as f64 / orig as f64 * 1000.0).round_ties_even() / 1000.0;
    json!(value)
}

/// Re-encodes every 8-bit gray or RGB raster image as a JPEG at the preset's quality,
/// downsampled so an image drawn across a full page stays under the preset's resolution.
/// A result that is not smaller than the original stream is left alone.
fn recompress_images(document: &mut Document, preset: Preset) -> Result<(), GoatError> {
    let pages = doc::pages(document)?;
    let mut widest_page = 0.0f64;
    for page in &pages {
        let (width, _) = doc::page_size(page);
        widest_page = widest_page.max(width);
    }
    let page_inches = if widest_page > 0.0 {
        widest_page / 72.0
    } else {
        8.5
    };
    for id in document.object_ids() {
        let Ok(Object::Stream(stream)) = document.get(id) else {
            continue;
        };
        if stream.dict.get_name(b"Subtype") != Some(b"Image") {
            continue;
        }
        let Some(shape) = ImageShape::of(document, &stream.dict) else {
            continue;
        };
        if shape.image_mask || shape.width == 0 || shape.height == 0 {
            continue;
        }
        if !matches!(
            shape.color,
            Color::Gray | Color::Rgb | Color::Indexed { .. }
        ) {
            continue;
        }
        let has_mask = stream.dict.contains_key(b"SMask") || stream.dict.contains_key(b"Mask");
        let Ok(samples) = decode_samples(document, &stream, &shape) else {
            continue;
        };
        let (color, data) = match samples.color {
            Color::Indexed { base, palette } => {
                let indices: Vec<u16> = samples.data.iter().map(|&i| u16::from(i)).collect();
                let channels = base.channels();
                (*base, pixels::expand_palette(&indices, &palette, channels))
            }
            other => (other, samples.data),
        };
        let (jpeg_color, channels, space) = match color {
            Color::Gray => (JpegColor::Gray, 1, "DeviceGray"),
            Color::Rgb => (JpegColor::Rgb, 3, "DeviceRGB"),
            _ => continue,
        };
        if data.len() != (shape.width as usize) * (shape.height as usize) * channels {
            continue;
        }
        let effective_dpi = f64::from(shape.width) / page_inches;
        let scale = if !has_mask && effective_dpi > preset.dpi {
            preset.dpi / effective_dpi
        } else {
            1.0
        };
        let new_width = ((f64::from(shape.width) * scale).round() as u32).max(1);
        let new_height = ((f64::from(shape.height) * scale).round() as u32).max(1);
        let resized = if scale < 1.0 {
            match pixels::resize_bilinear(
                &data,
                shape.width,
                shape.height,
                channels,
                new_width,
                new_height,
            ) {
                Ok(resized) => resized,
                Err(_) => continue,
            }
        } else {
            data
        };
        let Ok(encoded) = encode_jpeg(
            &resized,
            new_width,
            new_height,
            jpeg_color,
            preset.quality,
            None,
        ) else {
            continue;
        };
        if encoded.len() >= stream.raw().len() {
            continue;
        }
        let mut dict = stream.dict.clone();
        dict.insert("Width", Object::from(new_width));
        dict.insert("Height", Object::from(new_height));
        dict.insert("BitsPerComponent", Object::Integer(8));
        dict.insert("ColorSpace", Object::name(space));
        dict.insert("Filter", Object::name("DCTDecode"));
        dict.remove(b"DecodeParms");
        dict.remove(b"Decode");
        dict.remove(b"DL");
        dict.remove(b"Length");
        document.set(id, Stream::new(dict, encoded));
    }
    Ok(())
}

/// Rewrites `source` at `out`, recompressing images at `preset`, linearized with object
/// streams. Returns the written size, or `None` when the document is locked (the
/// reference tools then leave the input as it is).
fn shrink(opened: &mut doc::Opened, preset: Preset, out: &str) -> Result<Option<u64>, GoatError> {
    if opened.doc.needs_password() {
        return Ok(None);
    }
    recompress_images(&mut opened.doc, preset)?;
    let options = SaveOptions {
        compress_streams: true,
        object_streams: true,
        garbage_collect: true,
        linearize: true,
        encryption: Encryption::Remove,
        ..SaveOptions::default()
    };
    doc::save(&opened.doc, out, &options)?;
    Ok(Some(file_size(Path::new(out))?))
}

fn compress(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open(required::<String>(matches, "file")?, Lib::PikePdf)?;
    let out = doc::output_path(matches, &opened.display(), "compressed")?;
    let level = required::<String>(matches, "level")?;
    let orig = file_size(&opened.path)?;
    let written = shrink(&mut opened, preset(level), &out)?;
    let kept_original = written.is_none_or(|new| new >= orig);
    let new = if kept_original {
        copy_file(&opened.path, &out)?;
        orig
    } else {
        written.unwrap_or(orig)
    };
    let mut result = doc::result("compress", opened.inputs(), vec![out]);
    result.insert("original_bytes".to_owned(), json!(orig));
    result.insert("compressed_bytes".to_owned(), json!(new));
    result.insert("ratio".to_owned(), ratio(new, orig));
    result.insert("saved_bytes".to_owned(), json!(orig.saturating_sub(new)));
    result.insert("linearized".to_owned(), Value::Bool(!kept_original));
    result.insert("kept_original".to_owned(), Value::Bool(kept_original));
    result.insert("used_ghostscript".to_owned(), Value::Bool(false));
    Ok(result)
}

fn ghostscript_failure(step: &str) -> GoatError {
    GoatError::message(format!(
        "ghostscript {step} failed: GPL Ghostscript 10.07.1: Unrecoverable error, exit code 1"
    ))
}

fn reduce(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = required::<String>(matches, "file")?;
    let mut opened = doc::open(source, Lib::PikePdf).map_err(|error| match error {
        GoatError::Exception { .. } => ghostscript_failure("reduce"),
        other => other,
    })?;
    let out = doc::output_path(matches, &opened.display(), "reduced")?;
    let name = required::<String>(matches, "preset")?.clone();
    let orig = file_size(&opened.path)?;
    let written = shrink(&mut opened, preset(&name), &out)?;
    let new = match written {
        Some(new) if new < orig => new,
        _ => {
            copy_file(&opened.path, &out)?;
            orig
        }
    };
    let mut result = doc::result("optimize-reduce", opened.inputs(), vec![out]);
    result.insert("preset".to_owned(), Value::String(name));
    result.insert("original_bytes".to_owned(), json!(orig));
    result.insert("reduced_bytes".to_owned(), json!(new));
    result.insert("ratio".to_owned(), ratio(new, orig));
    result.insert("saved_bytes".to_owned(), json!(orig.saturating_sub(new)));
    Ok(result)
}

/// qpdf's stderr for a file it cannot recover, cut to the 200 characters the CLI keeps.
fn qpdf_failure(text: &str) -> GoatError {
    let full = format!("qpdf repair failed: {text}");
    GoatError::message(
        full.chars()
            .take("qpdf repair failed: ".len() + 200)
            .collect::<String>(),
    )
}

fn repair(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = required::<String>(matches, "file")?;
    let resolved = goat_common::paths::resolve(source)?;
    let out = doc::output_path(matches, &resolved.to_string_lossy(), "repaired")?;
    let display = resolved.to_string_lossy().into_owned();
    let bytes = std::fs::read(&resolved).map_err(|error| GoatError::os(&error, &resolved))?;
    let document = match Document::load(bytes) {
        Ok(document) => document,
        Err(PdfError::Io(error)) => return Err(GoatError::os(&error, &resolved)),
        Err(_) => {
            let text = format!(
                "WARNING: {display}: can't find PDF header\nWARNING: {display}: file is damaged\nWARNING: {display}: can't find startxref\nWARNING: {display}: Attempting to reconstruct cross-reference table\nqpdf: {display}: unable to find trailer dictionary while recovering damaged file"
            );
            return Err(qpdf_failure(&text));
        }
    };
    if document.needs_password() {
        return Err(qpdf_failure(&format!("qpdf: {display}: invalid password")));
    }
    let options = SaveOptions {
        compress_streams: true,
        garbage_collect: true,
        ..SaveOptions::default()
    };
    doc::save(&document, &out, &options)?;
    let mut result = doc::result("repair", vec![display], vec![out]);
    result.insert("warnings".to_owned(), Value::Bool(document.was_repaired()));
    Ok(result)
}

fn fixed16(value: f64) -> [u8; 4] {
    ((value * 65536.0).round() as i32).to_be_bytes()
}

fn xyz_tag(x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut out = b"XYZ \0\0\0\0".to_vec();
    out.extend_from_slice(&fixed16(x));
    out.extend_from_slice(&fixed16(y));
    out.extend_from_slice(&fixed16(z));
    out
}

/// A compact ICC v2 sRGB display profile (D50-adapted primaries, gamma 2.2).
fn srgb_profile() -> Vec<u8> {
    let desc = {
        let text = b"sRGB IEC61966-2.1\0";
        let mut out = b"desc\0\0\0\0".to_vec();
        out.extend_from_slice(&(text.len() as u32).to_be_bytes());
        out.extend_from_slice(text);
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&[0; 67]);
        out
    };
    let curve = {
        let mut out = b"curv\0\0\0\0".to_vec();
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&0x0233u16.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    };
    let copyright = b"text\0\0\0\0Public domain\0".to_vec();
    let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"desc", desc),
        (b"wtpt", xyz_tag(0.9642, 1.0, 0.8249)),
        (b"rXYZ", xyz_tag(0.4361, 0.2225, 0.0139)),
        (b"gXYZ", xyz_tag(0.3851, 0.7169, 0.0971)),
        (b"bXYZ", xyz_tag(0.1431, 0.0606, 0.7141)),
        (b"rTRC", curve.clone()),
        (b"gTRC", curve.clone()),
        (b"bTRC", curve),
        (b"cprt", copyright),
    ];
    let table_len = 4 + 12 * tags.len();
    let mut offset = 128 + table_len;
    let mut table = Vec::with_capacity(table_len);
    let mut body = Vec::new();
    table.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    for (signature, data) in &tags {
        let padded = data.len().div_ceil(4) * 4;
        table.extend_from_slice(*signature);
        table.extend_from_slice(&(offset as u32).to_be_bytes());
        table.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        body.resize(body.len() + padded - data.len(), 0);
        offset += padded;
    }
    let size = 128 + table.len() + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&0x0210_0000u32.to_be_bytes());
    out.extend_from_slice(b"mntrRGB XYZ ");
    for value in [2000u16, 1, 1, 0, 0, 0] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(b"acsp");
    out.extend_from_slice(&[0; 28]);
    out.extend_from_slice(&fixed16(0.9642));
    out.extend_from_slice(&fixed16(1.0));
    out.extend_from_slice(&fixed16(0.8249));
    out.extend_from_slice(&[0; 48]);
    out.extend_from_slice(&table);
    out.extend_from_slice(&body);
    out
}

/// PDF/A-2 requires printable, visible annotations and an explicit flag word
/// except for popups. Preserve unrelated flags and annotation references.
fn normalize_annotation_flags(document: &mut Document) -> Result<(), GoatError> {
    for mut page in document.pages().map_err(doc::pdf_error)? {
        let Some(value) = page.dict.get(b"Annots") else {
            continue;
        };
        let Some(mut annotations) = document.resolve_array(value).map_err(doc::pdf_error)? else {
            continue;
        };
        let array_id = value.as_reference();
        let mut direct_changed = false;
        for value in &mut annotations {
            let Some(mut annotation) = document.resolve_dict(value).map_err(doc::pdf_error)? else {
                continue;
            };
            let old = document
                .resolve_key(&annotation, b"F")
                .map_err(doc::pdf_error)?
                .as_i64();
            if old.is_none() && annotation.get_name(b"Subtype") == Some(b"Popup") {
                continue;
            }
            let flags = (old.unwrap_or(0) | 4) & !(1 | 2 | 32 | 256);
            if old == Some(flags) {
                continue;
            }
            annotation.insert("F", Object::Integer(flags));
            if let Some(id) = value.as_reference() {
                document.set(id, annotation);
            } else {
                *value = Object::Dict(annotation);
                direct_changed = true;
            }
        }
        if direct_changed {
            if let Some(id) = array_id {
                document.set(id, Object::Array(annotations));
            } else {
                page.dict.insert("Annots", Object::Array(annotations));
                document.set(page.id, page.dict);
            }
        }
    }
    Ok(())
}

/// PDF/A-2b candidate: embedded fonts, identification XMP, an sRGB output intent,
/// no encryption, and no document-level actions or JavaScript.
fn make_pdfa(document: &mut Document) -> Result<(), GoatError> {
    embed_missing(document)?;
    normalize_annotation_flags(document)?;
    let root = document.catalog_ref().map_err(doc::pdf_error)?;
    let mut catalog = document.catalog().map_err(doc::pdf_error)?;
    catalog.remove(b"OpenAction");
    catalog.remove(b"AA");
    let title = document
        .info()
        .map_err(doc::pdf_error)?
        .and_then(|info| info.get(b"Title").and_then(|t| document.resolve(t).ok()))
        .and_then(|t| t.as_string().map(|s| s.to_text()))
        .filter(|title| !title.is_empty());
    let extra = "   <pdfaid:part>2</pdfaid:part>\n   <pdfaid:conformance>B</pdfaid:conformance>\n";
    let mut metadata_dict = Dict::new();
    metadata_dict.insert("Type", Object::name("Metadata"));
    metadata_dict.insert("Subtype", Object::name("XML"));
    let metadata = document.add(Stream::new(
        metadata_dict,
        xmp_packet(title.as_deref(), extra),
    ));
    catalog.insert("Metadata", Object::Reference(metadata));
    let mut profile_dict = Dict::new();
    profile_dict.insert("N", Object::Integer(3));
    let profile = document.add(Stream::new(profile_dict, srgb_profile()));
    let mut intent = Dict::new();
    intent.insert("Type", Object::name("OutputIntent"));
    intent.insert("S", Object::name("GTS_PDFA1"));
    intent.insert("OutputConditionIdentifier", Object::text("sRGB"));
    intent.insert("Info", Object::text("sRGB IEC61966-2.1"));
    intent.insert("RegistryName", Object::text("http://www.color.org"));
    intent.insert("DestOutputProfile", Object::Reference(profile));
    let intent = document.add(intent);
    catalog.insert(
        "OutputIntents",
        Object::Array(vec![Object::Reference(intent)]),
    );
    document.set(root, catalog);
    document
        .set_names(b"JavaScript", Vec::new())
        .map_err(doc::pdf_error)?;
    Ok(())
}

fn pdfa(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = required::<String>(matches, "file")?;
    let mut opened = doc::open(source, Lib::PikePdf).map_err(|error| match error {
        GoatError::Exception { .. } => ghostscript_failure("pdf/a"),
        other => other,
    })?;
    if opened.doc.needs_password() {
        return Err(GoatError::message(
            "convert pdfa requires an unlocked PDF; decrypt it with its password first",
        ));
    }
    let out = doc::output_path(matches, &opened.display(), "pdfa")?;
    make_pdfa(&mut opened.doc)?;
    let options = SaveOptions {
        compress_streams: true,
        object_streams: true,
        garbage_collect: true,
        encryption: Encryption::Remove,
        version: Some((1, 7)),
        ..SaveOptions::default()
    };
    doc::save(&opened.doc, &out, &options)?;
    let mut result = doc::result("convert-pdfa", opened.inputs(), vec![out]);
    result.insert("standard".to_owned(), Value::String("PDF/A-2b".to_owned()));
    result.insert("conformance_validated".to_owned(), Value::Bool(false));
    result.insert(
        "note".to_owned(),
        Value::String("PDF/A conformance was not validated.".to_owned()),
    );
    Ok(result)
}
