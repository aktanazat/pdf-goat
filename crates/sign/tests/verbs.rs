//! `security sign` and `security verify` through the CLI surface: a signature this crate
//! writes verifies as pyhanko reports it, tampering breaks integrity, and an incremental
//! update after signing is reviewed rather than rejected.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{PdfBuilder, incremental_update};
use pdf_core::Dict;
use serde_json::{Map, Value};

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_sign::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["security", "convert"],
        &[],
    );
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let invocation = cli
        .parse(&args)
        .unwrap_or_else(|failure| panic!("{failure:?}"));
    invocation.run(&Ctx::new(home))
}

fn plain_pdf(dir: &Path) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text(72.0, 700.0, "Hello, signatures");
    builder
        .save(dir.join("plain.pdf"))
        .to_string_lossy()
        .into_owned()
}

fn signatures(result: &Map<String, Value>) -> &Vec<Value> {
    result["signatures"]
        .as_array()
        .expect("signatures is a list")
}

#[test]
fn sign_then_verify_reports_an_intact_trusted_signature_over_the_entire_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let signed = run(
        dir.path(),
        &[
            "security",
            "sign",
            &src,
            "--name",
            "Goat, Inc",
            "--reason",
            "Testing",
        ],
    )
    .expect("sign succeeds");
    assert_eq!(signed["verb"], "sec-sign");
    assert_eq!(signed["signer"], "Goat, Inc");
    assert_eq!(signed["self_signed"], true);
    let out = signed["outputs"][0].as_str().expect("one output");
    assert!(
        out.ends_with("plain.signed.pdf"),
        "default output beside the source: {out}"
    );

    let verified = run(dir.path(), &["security", "verify", out]).expect("verify succeeds");
    assert_eq!(verified["signature_count"], 1);
    let sig = &signatures(&verified)[0];
    assert_eq!(sig["field"], "Signature1");
    assert_eq!(sig["signer"], "Common Name: Goat, Inc");
    assert_eq!(sig["intact"], true);
    assert_eq!(sig["valid"], true);
    assert_eq!(sig["trusted"], true);
    assert_eq!(sig["coverage"], "SignatureCoverageLevel.ENTIRE_FILE");
    assert_eq!(sig["modified"], "NONE");
}

#[test]
fn tampering_with_signed_bytes_breaks_integrity_but_not_the_signature_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let signed = run(
        dir.path(),
        &["security", "sign", &src, "--reason", "Untouched"],
    )
    .expect("sign succeeds");
    let out = signed["outputs"][0]
        .as_str()
        .expect("one output")
        .to_owned();
    let mut data = fs::read(&out).expect("signed file");
    // The signature dictionary's own /Reason lies inside the signed byte range.
    let at = data
        .windows(9)
        .position(|w| w == b"Untouched")
        .expect("the reason is stored plain");
    data[at] = b'A';
    fs::write(&out, data).expect("rewrite");

    let verified = run(dir.path(), &["security", "verify", &out]).expect("verify succeeds");
    let sig = &signatures(&verified)[0];
    assert_eq!(sig["intact"], false);
    assert_eq!(sig["valid"], true);
    assert_eq!(sig["trusted"], false);
    assert_eq!(sig["coverage"], "SignatureCoverageLevel.ENTIRE_FILE");
}

#[test]
fn an_incremental_info_update_after_signing_is_reviewed_as_an_lta_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let signed = run(dir.path(), &["security", "sign", &src]).expect("sign succeeds");
    let out = signed["outputs"][0]
        .as_str()
        .expect("one output")
        .to_owned();
    let base = fs::read(&out).expect("signed file");
    let updated = incremental_update(&base, |doc| {
        let mut info = Dict::new();
        info.insert("Title", pdf_core::Object::text("Retitled after signing"));
        doc.set_info(info);
    });
    fs::write(&out, updated).expect("rewrite");

    let verified = run(dir.path(), &["security", "verify", &out]).expect("verify succeeds");
    let sig = &signatures(&verified)[0];
    assert_eq!(sig["intact"], true);
    assert_eq!(sig["trusted"], true);
    assert_eq!(sig["coverage"], "SignatureCoverageLevel.ENTIRE_REVISION");
    assert_eq!(sig["modified"], "LTA_UPDATES");
}

#[test]
fn a_second_signature_leaves_the_first_at_form_filling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let first = run(
        dir.path(),
        &["security", "sign", &src, "--name", "first signer"],
    )
    .expect("first sign");
    let once = first["outputs"][0].as_str().expect("one output").to_owned();
    let twice = dir.path().join("twice.pdf").to_string_lossy().into_owned();
    run(
        dir.path(),
        &[
            "security",
            "sign",
            &once,
            "--name",
            "second signer",
            "--field",
            "Signature2",
            "-o",
            &twice,
        ],
    )
    .expect("second sign");

    let verified = run(dir.path(), &["security", "verify", &twice]).expect("verify succeeds");
    assert_eq!(verified["signature_count"], 2);
    let sigs = signatures(&verified);
    assert_eq!(sigs[0]["field"], "Signature1");
    assert_eq!(sigs[0]["signer"], "Common Name: first signer");
    assert_eq!(
        sigs[0]["coverage"],
        "SignatureCoverageLevel.ENTIRE_REVISION"
    );
    assert_eq!(sigs[0]["modified"], "FORM_FILLING");
    assert_eq!(sigs[1]["field"], "Signature2");
    assert_eq!(sigs[1]["coverage"], "SignatureCoverageLevel.ENTIRE_FILE");
    assert_eq!(sigs[1]["modified"], "NONE");
}

#[test]
fn signing_an_already_signed_field_name_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let signed = run(dir.path(), &["security", "sign", &src]).expect("sign succeeds");
    let out = signed["outputs"][0]
        .as_str()
        .expect("one output")
        .to_owned();
    let error = run(dir.path(), &["security", "sign", &out]).expect_err("the field is taken");
    assert_eq!(
        error,
        GoatError::message("Signature field with name Signature1 appears to be filled already.")
    );
}

#[test]
fn verify_reports_no_signatures_for_an_unsigned_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let verified = run(dir.path(), &["security", "verify", &src]).expect("verify succeeds");
    assert_eq!(verified["signature_count"], 0);
    assert_eq!(verified["outputs"], Value::Array(Vec::new()));
}

/// A composite font without a descendant font cannot be embedded, so the
/// file cannot become PDF/A. The page already has text, so no recognition
/// runs and this holds on every platform.
#[test]
fn ocr_keeps_its_result_and_names_the_reason_when_pdfa_conversion_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut font = Dict::new();
    font.insert("Type", pdf_core::Object::name("Font"));
    font.insert("Subtype", pdf_core::Object::name("Type0"));
    font.insert("BaseFont", pdf_core::Object::name("Orphan"));
    font.insert("Encoding", pdf_core::Object::name("Identity-H"));
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .resource("Font", "F9", pdf_core::Object::Dict(font))
        .raw_content(b"BT /F9 12 Tf 72 700 Td <0001> Tj ET");
    let src = builder.save(dir.path().join("typed.pdf"));
    let out = dir.path().join("typed-ocr.pdf");
    let result = run(
        dir.path(),
        &[
            "convert",
            "ocr",
            &src.to_string_lossy(),
            "-o",
            &out.to_string_lossy(),
        ],
    )
    .expect("OCR succeeds without PDF/A");
    assert_eq!(result["standard"], Value::Null);
    let warnings = result["warnings"].as_array().expect("warnings is a list");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let warning = warnings[0].as_str().expect("a warning is text");
    assert!(
        warning.contains("PDF/A-2b") && warning.contains("descendant"),
        "the warning names the standard and the reason: {warning}"
    );
    assert!(
        fs::read(&out)
            .expect("the OCR result is written")
            .starts_with(b"%PDF-")
    );
}

/// `convert ocr` on `builder`'s file with `extra` arguments: the JSON result
/// and the conformance level the output's XMP declares, once the file's
/// PDF/A-2 structure is checked. The result's warnings are left unchecked:
/// they report the state of the machine's recognizer models.
#[cfg(target_os = "macos")]
fn ocr_pdfa(builder: &PdfBuilder, extra: &[&str]) -> (Map<String, Value>, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = builder
        .save(dir.path().join("in.pdf"))
        .to_string_lossy()
        .into_owned();
    let out = dir.path().join("in-ocr.pdf");
    let out_text = out.to_string_lossy().into_owned();
    let mut args = vec!["convert", "ocr", src.as_str(), "-o", out_text.as_str()];
    args.extend_from_slice(extra);
    let result = run(dir.path(), &args).expect("OCR succeeds");
    let bytes = fs::read(&out).expect("the OCR result is written");
    assert!(bytes.starts_with(b"%PDF-1.7"), "PDF/A-2 files are PDF 1.7");
    let doc = pdf_core::Document::load(bytes).expect("reopen");
    let catalog = doc.catalog().expect("catalog");
    let intents = doc
        .resolve_array(catalog.get(b"OutputIntents").expect("OutputIntents"))
        .expect("resolve OutputIntents")
        .expect("OutputIntents is an array");
    let intent = doc
        .resolve_dict(intents.first().expect("one output intent"))
        .expect("resolve the intent")
        .expect("the intent is a dictionary");
    assert_eq!(intent.get_name(b"S"), Some(&b"GTS_PDFA1"[..]));
    let metadata = doc
        .resolve_stream(catalog.get(b"Metadata").expect("catalog Metadata"))
        .expect("resolve Metadata")
        .expect("Metadata is a stream");
    let xmp = doc.decode_stream(&metadata).expect("decode Metadata");
    let xmp = String::from_utf8_lossy(&xmp.data);
    assert!(xmp.contains("<pdfaid:part>2</pdfaid:part>"), "{xmp}");
    let level = xmp
        .split("<pdfaid:conformance>")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("a conformance level");
    (result, level.to_owned())
}

#[cfg(target_os = "macos")]
#[test]
fn ocr_claims_pdfa_2u_when_every_font_maps_to_unicode() {
    // Forced OCR replaces the page's own font by the OCR fonts, which map
    // every glyph to Unicode.
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text(72.0, 700.0, "Quarterly report for the northern region");
    let (result, level) = ocr_pdfa(&builder, &["--force"]);
    assert_eq!(result["standard"], "PDF/A-2u");
    assert_eq!(level, "U");
}

#[cfg(target_os = "macos")]
#[test]
fn ocr_claims_only_pdfa_2b_when_a_kept_font_has_no_unicode_map() {
    // The first page keeps its standard font, which has no ToUnicode map;
    // OCR reads only the blank second page.
    let mut builder = PdfBuilder::new();
    builder.page(612.0, 792.0).text(72.0, 700.0, "Kept text");
    builder.page(612.0, 792.0);
    let (result, level) = ocr_pdfa(&builder, &[]);
    assert_eq!(result["standard"], "PDF/A-2b");
    assert_eq!(level, "B");
}
