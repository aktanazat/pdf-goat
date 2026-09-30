//! The inspection, metadata, attachment, security, and size verbs through the CLI
//! surface: results the reference `pdf-goat-cli` prints for the same inputs, and the
//! errors it raises for locked or damaged files.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{PdfBuilder, declined_image_pdf, empty_page_pdf, image_pdf, jpeg_bytes};
use pdf_codec::JpegColor;
use pdf_core::Document;
use serde_json::{Map, Value};

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_inspect::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &[
            "info",
            "inspect",
            "preflight",
            "meta",
            "get",
            "attach",
            "detach",
            "compare",
            "accessibility",
            "security",
            "repair",
            "compress",
            "optimize",
            "convert",
        ],
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
        .page(400.0, 300.0)
        .text(72.0, 200.0, "Hello, inspection");
    builder.page(612.0, 792.0).text(72.0, 700.0, "Second page");
    builder.info("Title", "Fixture Title");
    builder
        .save(dir.join("plain.pdf"))
        .to_string_lossy()
        .into_owned()
}

fn write(dir: &Path, name: &str, data: &[u8]) -> String {
    let path = dir.join(name);
    fs::write(&path, data).expect("fixture written");
    path.to_string_lossy().into_owned()
}

fn output(result: &Map<String, Value>) -> String {
    result["outputs"][0]
        .as_str()
        .expect("one output")
        .to_owned()
}

fn error_text(error: &GoatError) -> String {
    error.to_string()
}

#[test]
fn info_reports_pages_size_and_title_and_preflight_flags_the_empty_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let info = run(dir.path(), &["info", &src]).expect("info succeeds");
    assert_eq!(info["verb"], "info");
    assert_eq!(info["pages"], 2);
    assert_eq!(
        info["page_sizes_pt"][0],
        serde_json::json!({"width": 400.0, "height": 300.0})
    );
    assert_eq!(info["metadata"]["title"], "Fixture Title");
    assert_eq!(info["encrypted"], false);

    let empty = write(dir.path(), "empty.pdf", &empty_page_pdf());
    let preflight = run(dir.path(), &["preflight", &empty]).expect("preflight succeeds");
    assert_eq!(preflight["pages"], 3);
    let finding = preflight["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|finding| finding["code"] == "empty_pages")
        .expect("the blank page is reported");
    assert_eq!(
        finding["pages"],
        serde_json::json!([1]),
        "the page with borrowed resources is not empty"
    );
    assert_eq!(finding["count"], 1);
}

#[test]
fn meta_set_writes_info_keys_and_strip_removes_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let set = run(
        dir.path(),
        &[
            "meta",
            "set",
            &src,
            "--set",
            "author=Goat",
            "--set",
            "title=none",
        ],
    )
    .expect("meta set succeeds");
    assert_eq!(set["verb"], "meta-set");
    assert_eq!(set["metadata"]["author"], "Goat");
    let out = output(&set);
    assert!(
        out.ends_with("plain.meta.pdf"),
        "default output beside the source: {out}"
    );
    let read = run(dir.path(), &["meta", "get", &out]).expect("meta get succeeds");
    assert_eq!(read["metadata"]["author"], "Goat");
    assert_eq!(
        read["metadata"].get("title"),
        None,
        "a `none` value deletes the key"
    );

    let bad = run(dir.path(), &["meta", "set", &src, "--set", "bogus=1"])
        .expect_err("unknown key is refused");
    assert_eq!(error_text(&bad), "ValueError: bad dict key(s): {'bogus'}");

    let stripped = run(dir.path(), &["meta", "strip", &out]).expect("meta strip succeeds");
    let read = run(dir.path(), &["meta", "get", &output(&stripped)]).expect("meta get succeeds");
    assert_eq!(
        read["metadata"],
        serde_json::json!({"format": "PDF 1.7"}),
        "only PyMuPDF's synthetic format key survives"
    );
    assert_eq!(
        read["has_xmp"], true,
        "the reference CLI tests the bound method, not its result"
    );
}

#[test]
fn encrypt_then_decrypt_round_trips_and_locked_files_report_the_reference_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let encrypted = run(
        dir.path(),
        &[
            "security",
            "encrypt",
            &src,
            "--password",
            "usr",
            "--owner",
            "own",
        ],
    )
    .expect("encrypt succeeds");
    assert_eq!(encrypted["algorithm"], "AES-256");
    let locked = output(&encrypted);
    let bytes = fs::read(&locked).expect("encrypted output");
    let document = Document::load(bytes).expect("encrypted output loads");
    assert!(
        document.needs_password(),
        "the output opens only with the password"
    );

    let info =
        run(dir.path(), &["info", &locked]).expect_err("info cannot read a locked file's pages");
    assert_eq!(
        error_text(&info),
        "ValueError: document closed or encrypted"
    );

    let get = run(dir.path(), &["get", "fonts", &locked])
        .expect_err("pymupdf verbs refuse a locked file");
    assert_eq!(error_text(&get), "ValueError: document closed or encrypted");
    let check = run(dir.path(), &["accessibility", "check", &locked])
        .expect_err("pikepdf verbs refuse a locked file");
    assert_eq!(
        error_text(&check),
        format!("PasswordError: {locked}: invalid password")
    );
    let object = run(dir.path(), &["get", "object", &locked, "catalog"])
        .expect_err("get object needs the password");
    assert!(
        error_text(&object).contains("password"),
        "{}",
        error_text(&object)
    );

    let decrypted = run(
        dir.path(),
        &["security", "decrypt", &locked, "--password", "usr"],
    )
    .expect("decrypt succeeds");
    let open = Document::load(fs::read(output(&decrypted)).expect("decrypted output"))
        .expect("decrypted output loads");
    assert!(!open.needs_password());
    let info = run(dir.path(), &["info", &output(&decrypted)]).expect("info on the decrypted file");
    assert_eq!(info["encrypted"], false);
    assert_eq!(info["metadata"]["title"], "Fixture Title");
}

#[test]
fn permissions_clears_the_flag_bits_the_switches_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let restricted = run(
        dir.path(),
        &[
            "security",
            "permissions",
            &src,
            "--owner",
            "own",
            "--no-copy",
        ],
    )
    .expect("permissions succeeds");
    assert_eq!(restricted["no_copy"], true);
    assert_eq!(restricted["no_print"], false);
    let info = run(dir.path(), &["info", &output(&restricted)])
        .expect("an empty user password opens the file");
    assert_eq!(
        info["encrypted"], false,
        "PyMuPDF clears is_encrypted once the empty user password authenticates"
    );
    assert_eq!(info["needs_password"], false);
    assert_eq!(info["permissions"]["copy"], false);
    assert_eq!(info["permissions"]["print"], true);
}

#[test]
fn attach_get_attachments_and_detach_round_trip_the_embedded_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let payload = write(dir.path(), "notes.txt", b"attached bytes");
    let attached = run(dir.path(), &["attach", &src, &payload]).expect("attach succeeds");
    assert_eq!(attached["attached"], "notes.txt");
    let with_file = output(&attached);

    let again = run(dir.path(), &["attach", &with_file, &payload])
        .expect_err("a duplicate name is refused");
    assert_eq!(
        error_text(&again),
        "ValueError: Name 'notes.txt' already exists."
    );

    let outdir = dir.path().join("extracted");
    let extracted = run(
        dir.path(),
        &[
            "get",
            "attachments",
            &with_file,
            "-o",
            &outdir.to_string_lossy(),
        ],
    )
    .expect("get attachments succeeds");
    assert_eq!(extracted["count"], 1);
    assert_eq!(
        fs::read(outdir.join("notes.txt")).expect("extracted file"),
        b"attached bytes"
    );
    let clash = run(
        dir.path(),
        &[
            "get",
            "attachments",
            &with_file,
            "-o",
            &outdir.to_string_lossy(),
        ],
    )
    .expect_err("existing output is refused");
    assert!(
        error_text(&clash).starts_with("attachment output already exists: "),
        "{}",
        error_text(&clash)
    );

    let missing = run(dir.path(), &["detach", &with_file, "--name", "other.txt"])
        .expect_err("unknown name is refused");
    assert_eq!(error_text(&missing), "attachment not found: other.txt");
    let detached = run(dir.path(), &["detach", &with_file, "--all"]).expect("detach succeeds");
    assert_eq!(detached["removed"], serde_json::json!(["notes.txt"]));
    assert_eq!(detached["count"], 1);
    let none = run(
        dir.path(),
        &[
            "get",
            "attachments",
            &output(&detached),
            "-o",
            &dir.path().join("none").to_string_lossy(),
        ],
    )
    .expect("get attachments succeeds");
    assert_eq!(none["count"], 0);
}

#[test]
fn get_images_copies_a_plain_jpeg_and_decodes_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rgb = jpeg_bytes(JpegColor::Rgb, 24, 18);
    let cmyk = jpeg_bytes(JpegColor::Cmyk, 16, 12);
    let src = write(dir.path(), "images.pdf", &image_pdf(&rgb, &cmyk));
    let result = run(dir.path(), &["get", "images", &src]).expect("get images succeeds");
    let outputs: Vec<&str> = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        result["count"], 2,
        "the two page images, not the soft mask: {outputs:?}"
    );
    let mut extracted: Vec<Vec<u8>> = outputs
        .iter()
        .map(|path| fs::read(path).expect("extracted image"))
        .collect();
    extracted.sort();
    let mut stored = vec![rgb, cmyk];
    stored.sort();
    assert_eq!(
        extracted, stored,
        "both JPEGs keep their stored bytes; the CMYK one through the MuPDF-style fallback"
    );
    assert!(
        outputs.iter().all(|path| path.ends_with(".jpg")),
        "{outputs:?}"
    );
    assert!(
        Path::new(outputs[0])
            .parent()
            .expect("parent")
            .ends_with("images_images")
    );

    let declined = write(dir.path(), "declined.pdf", &declined_image_pdf());
    let result = run(dir.path(), &["get", "images", &declined])
        .expect("declined images fall back to a decode");
    assert_eq!(result["count"], 2);
    for path in result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter_map(Value::as_str)
    {
        assert!(
            fs::metadata(path).expect("output").len() > 0,
            "{path} is empty"
        );
    }
}

#[test]
fn get_object_shows_the_catalog_trailer_and_page_and_rejects_bad_targets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let catalog = run(dir.path(), &["get", "object", &src, "catalog"]).expect("catalog");
    assert!(
        catalog["object"]
            .as_str()
            .expect("object text")
            .contains("/Type /Catalog")
    );
    assert_eq!(catalog["stream"], Value::Null);
    let trailer = run(dir.path(), &["get", "object", &src, "trailer"]).expect("trailer");
    assert_eq!(trailer["xref"], Value::Null);
    assert!(
        trailer["object"]
            .as_str()
            .expect("object text")
            .contains("/Root")
    );
    let page = run(dir.path(), &["get", "object", &src, "page:2"]).expect("page");
    assert!(
        page["object"]
            .as_str()
            .expect("object text")
            .contains("/Type /Page")
    );
    let content = run(dir.path(), &["get", "object", &src, "page:1"]).expect("page");
    let xref = content["xref"].as_u64().expect("xref");
    assert!(xref > 0);

    let bad = run(dir.path(), &["get", "object", &src, "99999"]).expect_err("out of range");
    assert!(
        error_text(&bad)
            .starts_with("'99999' is not catalog, trailer, page:N, or an object number from 1 to "),
        "{}",
        error_text(&bad)
    );
    let range = run(dir.path(), &["get", "object", &src, "page:1-2"]).expect_err("one page only");
    assert_eq!(error_text(&range), "'page:1-2' must name one page");
    let limit = run(
        dir.path(),
        &["get", "object", &src, "catalog", "--max-bytes", "0"],
    )
    .expect_err("limit checked");
    assert_eq!(
        error_text(&limit),
        "--max-bytes must be between 1 and 1000000"
    );
}

#[test]
fn compare_structure_reports_page_and_document_differences() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let same = run(dir.path(), &["compare", "structure", &src, &src]).expect("compare succeeds");
    assert_eq!(same["identical"], true);
    let empty = write(dir.path(), "empty.pdf", &empty_page_pdf());
    let differ =
        run(dir.path(), &["compare", "structure", &src, &empty]).expect("compare succeeds");
    assert_eq!(differ["identical"], false);
    assert_eq!(differ["pages"][0]["page"], 1);
    assert_eq!(
        differ["pages"][0]["changes"]["size"]["file"],
        serde_json::json!([400.0, 300.0])
    );
    assert_eq!(
        differ["pages"][0]["changes"]["size"]["other"],
        serde_json::json!([200.0, 200.0])
    );
}

#[test]
fn accessibility_set_makes_check_pass_for_title_and_language() {
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = write(dir.path(), "empty.pdf", &empty_page_pdf());
    let before = run(dir.path(), &["accessibility", "check", &empty]).expect("check succeeds");
    assert_eq!(
        before["issues"],
        serde_json::json!(["untagged", "no_title", "no_lang"])
    );
    let set = run(
        dir.path(),
        &[
            "accessibility",
            "set",
            &empty,
            "--title",
            "Accessible",
            "--lang",
            "de",
        ],
    )
    .expect("set succeeds");
    assert_eq!(set["title"], "Accessible");
    assert_eq!(set["lang"], "de");
    let after =
        run(dir.path(), &["accessibility", "check", &output(&set)]).expect("check succeeds");
    assert_eq!(after["has_title"], true);
    assert_eq!(after["title"], "Accessible");
    assert_eq!(after["lang"], "de");
    assert_eq!(after["tagged"], true);
    assert_eq!(after["issues"], serde_json::json!([]));
}

#[test]
fn compress_keeps_the_input_bytes_when_the_result_is_not_smaller() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let result = run(dir.path(), &["compress", &src]).expect("compress succeeds");
    let orig = fs::metadata(&src).expect("source").len();
    assert_eq!(result["original_bytes"], orig);
    assert_eq!(result["used_ghostscript"], false);
    let out = output(&result);
    let new = fs::metadata(&out).expect("output").len();
    assert_eq!(result["compressed_bytes"], new);
    assert_eq!(result["kept_original"], new >= orig);
    assert_eq!(result["linearized"], new < orig);
    assert_eq!(result["saved_bytes"], orig - new);
    if new >= orig {
        assert_eq!(
            fs::read(&out).expect("output bytes"),
            fs::read(&src).expect("source bytes")
        );
    }
}

#[test]
fn reduce_recompresses_the_raster_image_below_the_original_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder.page(612.0, 792.0).image_jpeg(
        [36.0, 36.0, 576.0, 756.0],
        &jpeg_bytes(JpegColor::Rgb, 1200, 1600),
    );
    let src = builder
        .save(dir.path().join("big.pdf"))
        .to_string_lossy()
        .into_owned();
    let result = run(
        dir.path(),
        &["optimize", "reduce", &src, "--preset", "screen"],
    )
    .expect("reduce succeeds");
    assert_eq!(result["preset"], "screen");
    let orig = result["original_bytes"].as_u64().expect("original bytes");
    let new = result["reduced_bytes"].as_u64().expect("reduced bytes");
    assert!(
        new < orig,
        "the 1200x1600 image downsampled to 72 dpi is smaller: {new} vs {orig}"
    );
    assert_eq!(result["saved_bytes"], orig - new);
    let info = run(dir.path(), &["info", &output(&result)]).expect("the reduced file opens");
    assert_eq!(info["pages"], 1);
}

#[test]
fn repair_rewrites_a_file_with_a_broken_xref_and_reports_warnings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let mut bytes = fs::read(&src).expect("source");
    let at = bytes
        .windows(9)
        .rposition(|w| w == b"startxref")
        .expect("startxref");
    bytes[at + 10..at + 12].copy_from_slice(b"99");
    let broken = write(dir.path(), "broken.pdf", &bytes);
    let result = run(dir.path(), &["repair", &broken]).expect("repair succeeds");
    assert_eq!(result["warnings"], true);
    let info = run(dir.path(), &["info", &output(&result)]).expect("repaired file opens");
    assert_eq!(info["pages"], 2);

    let garbage = write(dir.path(), "garbage.pdf", b"this is not a pdf at all\n");
    let resolved = fs::canonicalize(&garbage)
        .expect("resolved path")
        .to_string_lossy()
        .into_owned();
    let error = run(dir.path(), &["repair", &garbage]).expect_err("garbage cannot be repaired");
    assert!(
        error_text(&error).starts_with(&format!(
            "qpdf repair failed: WARNING: {resolved}: can't find PDF header"
        )),
        "{}",
        error_text(&error)
    );
}

#[test]
fn convert_pdfa_adds_identification_metadata_and_an_output_intent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let result = run(dir.path(), &["convert", "pdfa", &src]).expect("pdfa succeeds");
    assert_eq!(result["standard"], "PDF/A-2b");
    let document =
        Document::load(fs::read(output(&result)).expect("output")).expect("output loads");
    let catalog = document.catalog().expect("catalog");
    assert!(catalog.contains_key(b"OutputIntents"));
    let metadata = catalog.get(b"Metadata").expect("XMP metadata");
    let stream = document
        .resolve_stream(metadata)
        .expect("resolves")
        .expect("is a stream");
    let xmp = document.decode_stream(&stream).expect("decodes").data;
    let text = String::from_utf8_lossy(&xmp);
    assert!(
        text.contains("<pdfaid:part>2</pdfaid:part>")
            && text.contains("<pdfaid:conformance>B</pdfaid:conformance>"),
        "{text}"
    );
    assert!(text.contains("Fixture Title"));
}

#[test]
fn sanitize_blanks_javascript_and_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let with_title = run(
        dir.path(),
        &["accessibility", "set", &src, "--title", "Has XMP"],
    )
    .expect("set succeeds");
    let before =
        Document::load(fs::read(output(&with_title)).expect("input")).expect("input loads");
    assert!(
        before.catalog().expect("catalog").contains_key(b"Metadata"),
        "accessibility set wrote XMP"
    );
    let result = run(dir.path(), &["security", "sanitize", &output(&with_title)])
        .expect("sanitize succeeds");
    assert_eq!(
        result["removed"],
        serde_json::json!([
            "javascript",
            "embedded_files",
            "attached_files",
            "xml_metadata",
            "thumbnails"
        ])
    );
    let after = Document::load(fs::read(output(&result)).expect("output")).expect("output loads");
    assert!(
        !after.catalog().expect("catalog").contains_key(b"Metadata"),
        "the XMP stream is gone"
    );
    let read = run(dir.path(), &["meta", "get", &output(&result)]).expect("meta get succeeds");
    assert_eq!(
        read["metadata"]["title"], "Has XMP",
        "the Info dictionary survives a scrub"
    );
}
