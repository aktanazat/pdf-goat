//! Redaction removes recoverable text, preserves neighbours and shared resources,
//! and uses the same case/ligature/word-join matches as search.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{Paint, PdfBuilder};
use pdf_core::{Dict, Document, Object, Stream};
use pdf_text::TextFlags;
use serde_json::{Map, Value};

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_edit::register(&mut registry);
    pdf_forms::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["redact", "convert", "edit", "form"],
        &[],
    );
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    cli.parse(&args)
        .expect("valid edit arguments")
        .run(&Ctx::new(home))
}

fn fixture(dir: &Path, text: &str) -> String {
    let mut builder = PdfBuilder::new();
    builder.page(612.0, 792.0).text(72.0, 72.0, text);
    builder
        .save(dir.join("input.pdf"))
        .to_string_lossy()
        .into_owned()
}

fn output(result: &Map<String, Value>) -> &str {
    result["outputs"][0].as_str().expect("one output")
}

fn words(doc: &Document) -> Vec<String> {
    pdf_text::extract_page(doc, 0, TextFlags::WORDS)
        .expect("read page")
        .words()
        .into_iter()
        .map(|word| word.text)
        .collect()
}

fn assert_streams_omit(doc: &Document, forbidden: &[u8]) {
    for id in doc.object_ids() {
        let object = doc.get(id).expect("read object");
        assert_object_omits(&object, forbidden);
        if let Some(stream) = object.as_stream() {
            let data = doc
                .decode_stream(stream)
                .expect("decode output stream")
                .data;
            assert!(
                !data
                    .windows(forbidden.len())
                    .any(|window| window.eq_ignore_ascii_case(forbidden)),
                "removed bytes survive in {id:?}"
            );
            // PDF strings may be hex-encoded, so check their decoded payloads too.
            if stream.dict.get_name(b"Subtype") != Some(b"Image") {
                for op in pdf_core::parse_content(&data).expect("parse content") {
                    for operand in &op.operands {
                        assert_object_omits(operand, forbidden);
                    }
                }
            }
        }
    }
}

fn assert_object_omits(object: &Object, forbidden: &[u8]) {
    if let Some(string) = object.as_string() {
        assert!(
            !string
                .as_bytes()
                .windows(forbidden.len())
                .any(|w| w.eq_ignore_ascii_case(forbidden)),
            "removed text remains in a PDF string"
        );
    } else if let Some(array) = object.as_array() {
        for value in array {
            assert_object_omits(value, forbidden);
        }
    } else if let Some(dict) = object
        .as_dict()
        .or_else(|| object.as_stream().map(|s| &s.dict))
    {
        for (_, value) in dict.iter() {
            assert_object_omits(value, forbidden);
        }
    }
}

#[test]
fn case_insensitive_redaction_removes_whole_words_without_moving_the_neighbour() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fixture(
        dir.path(),
        "Commission COMMISSION commission Commissioner unrelated",
    );
    let before = Document::open(&src).expect("source");
    let text = pdf_text::extract_page(&before, 0, TextFlags::WORDS).expect("before text");
    let neighbour = text
        .words()
        .into_iter()
        .find(|word| word.text == "unrelated")
        .expect("neighbour");
    let result = run(dir.path(), &["redact", &src, "--find", "Commission"]).expect("redact");
    assert_eq!(result["redactions"], 4);
    let doc = Document::open(output(&result)).expect("redacted PDF");
    assert_eq!(doc.page_count().expect("page count"), 1);
    assert_eq!(words(&doc), ["unrelated"]);
    assert_streams_omit(&doc, b"commission");
    let remaining = pdf_text::extract_page(&doc, 0, TextFlags::WORDS)
        .expect("after text")
        .words();
    let rect = remaining[0].rect;
    for (actual, expected) in [rect.x0, rect.y0, rect.x1, rect.y1].into_iter().zip([
        neighbour.rect.x0,
        neighbour.rect.y0,
        neighbour.rect.x1,
        neighbour.rect.y1,
    ]) {
        assert!(
            (actual - expected).abs() <= 0.5,
            "neighbour moved: {actual} vs {expected}"
        );
    }
}

fn ligature_fixture(dir: &Path) -> String {
    let mut encoding = Dict::new();
    encoding.insert("BaseEncoding", Object::name("WinAnsiEncoding"));
    encoding.insert(
        "Differences",
        vec![Object::Integer(128), Object::name("fi"), Object::name("fl")],
    );
    let mut font = Dict::new();
    font.insert("Type", Object::name("Font"));
    font.insert("Subtype", Object::name("Type1"));
    font.insert("BaseFont", Object::name("Helvetica"));
    font.insert("Encoding", encoding);
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .resource("Font", "Lig", Object::Dict(font))
        .raw_content(b"BT /Lig 11 Tf 1 0 0 1 72 720 Tm <4465806e696e672074686520816f77> Tj ET");
    builder
        .save(dir.join("ligatures.pdf"))
        .to_string_lossy()
        .into_owned()
}

#[test]
fn ligature_queries_and_glyphs_match_the_same_word() {
    for query in ["flow", "\u{fb02}ow"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = ligature_fixture(dir.path());
        let result = run(dir.path(), &["redact", &src, "--find", query]).expect("redact ligature");
        assert_eq!(result["redactions"], 1);
        let doc = Document::open(output(&result)).expect("open result");
        assert_eq!(words(&doc), ["De\u{fb01}ning", "the"]);
        assert_streams_omit(&doc, &[0x81, b'o', b'w']);
    }
}

#[test]
fn repeated_matches_count_a_word_once_and_join_matches_leave_the_previous_word() {
    for (text, query, expected) in [
        ("Mississippi river", "s", vec!["river"]),
        ("keep New York tail", r"\s+New\s+York", vec!["keep", "tail"]),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = fixture(dir.path(), text);
        let result = run(dir.path(), &["redact", &src, "--find", query]).expect("redact");
        assert_eq!(result["redactions"], 1);
        let doc = Document::open(output(&result)).expect("output");
        assert_eq!(words(&doc), expected);
    }
}

#[test]
fn a_plain_space_in_the_pattern_matches_the_break_between_words() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fixture(dir.path(), "paid Acme Corp today");
    let result = run(dir.path(), &["redact", &src, "--find", "Acme Corp"]).expect("redact");
    let doc = Document::open(output(&result)).expect("output");
    assert_eq!(
        (&result["redactions"], words(&doc)),
        (&Value::from(1), vec!["paid".to_owned(), "today".to_owned()])
    );
}

#[test]
fn an_anchored_pattern_still_matches_one_whole_word() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fixture(dir.path(), "SSN 123-45-6789 and 123-45-67890");
    let result = run(
        dir.path(),
        &["redact", &src, "--find", r"^\d{3}-\d{2}-\d{4}$"],
    )
    .expect("redact");
    let doc = Document::open(output(&result)).expect("output");
    assert_eq!(
        (&result["redactions"], words(&doc)),
        (
            &Value::from(1),
            vec![
                "SSN".to_owned(),
                "and".to_owned(),
                "123-45-67890".to_owned()
            ]
        )
    );
}

#[test]
fn tj_spacing_and_multibyte_string_segments_preserve_kept_text_positions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder.page(612.0, 792.0).text(72.0, 72.0, "")
        .raw_content(b"BT /helv 12 Tf 2 Tc 5 Tw 80 Tz 1 0 0 1 72 690 Tm [(keep ) 25 (secret) -300 ( tail)] TJ ET");
    let src = builder
        .save(dir.path().join("spacing.pdf"))
        .to_string_lossy()
        .into_owned();
    let before = Document::open(&src).expect("before");
    let before = pdf_text::extract_page(&before, 0, TextFlags::WORDS)
        .expect("before words")
        .words();
    let result = run(dir.path(), &["redact", &src, "--find", "secret"]).expect("redact");
    assert_eq!(result["redactions"], 1);
    let after = Document::open(output(&result)).expect("after");
    assert_eq!(words(&after), ["keep", "tail"]);
    let after = pdf_text::extract_page(&after, 0, TextFlags::WORDS)
        .expect("after words")
        .words();
    for kept in after {
        let original = before
            .iter()
            .find(|w| w.text == kept.text)
            .expect("same word");
        assert!((kept.rect.x0 - original.rect.x0).abs() < 0.5);
        assert!((kept.rect.y0 - original.rect.y0).abs() < 0.5);
    }
}

#[test]
fn form_redaction_removes_the_original_stream_instead_of_leaving_a_hidden_copy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut font = Dict::new();
    font.insert("Type", Object::name("Font"));
    font.insert("Subtype", Object::name("Type1"));
    font.insert("BaseFont", Object::name("Helvetica"));
    let mut fonts = Dict::new();
    fonts.insert("F", font);
    let mut resources = Dict::new();
    resources.insert("Font", fonts);
    let mut form = Dict::new();
    form.insert("Type", Object::name("XObject"));
    form.insert("Subtype", Object::name("Form"));
    form.insert(
        "BBox",
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(300),
            Object::Integer(30),
        ],
    );
    form.insert("Resources", resources);
    let form = Stream::new(
        form,
        b"BT /F 11 Tf 1 0 0 1 0 15 Tm (secret neighbour) Tj ET".to_vec(),
    );
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .resource("XObject", "Fm", Object::Stream(form))
        .raw_content(b"q 1 0 0 1 72 650 cm /Fm Do Q q 1 0 0 1 72 600 cm /Fm Do Q");
    let mut doc = builder.build();
    let page = doc.page(0).expect("page");
    let parent = page.dict.get_ref(b"Parent").expect("parent");
    let mut parent_dict = doc
        .get(parent)
        .expect("parent object")
        .as_dict()
        .expect("parent dictionary")
        .clone();
    parent_dict.insert("Resources", page.resources);
    doc.set(parent, parent_dict);
    for (kind, resource) in [
        ("absent", None),
        ("null", Some(Object::Null)),
        ("explicit", page.dict.get(b"Resources").cloned()),
    ] {
        let mut dict = page.dict.clone();
        if let Some(resource) = resource {
            dict.insert("Resources", resource);
        } else {
            dict.remove(b"Resources");
        }
        doc.set(page.id, dict);
        let src = dir
            .path()
            .join(format!("{kind}.pdf"))
            .to_string_lossy()
            .into_owned();
        doc.save(&src, &pdf_core::SaveOptions::default())
            .expect("save inherited resources");
        assert_eq!(
            words(&Document::open(&src).expect("reopen fixture")),
            ["secret", "neighbour", "secret", "neighbour"],
            "{kind}"
        );
        let result = run(dir.path(), &["redact", &src, "--find", "secret"]).expect("redact forms");
        assert_eq!(result["redactions"], 2, "{kind}");
        let doc = Document::open(output(&result)).expect("open");
        assert_eq!(words(&doc), ["neighbour", "neighbour"], "{kind}");
        assert_streams_omit(&doc, b"secret");
    }
}

#[test]
fn bad_regex_is_rejected_before_the_pdf_is_opened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("not-a-pdf.pdf");
    fs::write(&src, b"not a PDF").expect("write");
    let error = run(
        dir.path(),
        &["redact", src.to_str().expect("utf8"), "--find", "["],
    )
    .expect_err("bad regex");
    assert!(
        error.envelope_text().contains("unterminated character set"),
        "{error}"
    );
}

#[test]
fn editing_replaces_the_matching_span_and_does_not_replace_other_case() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fixture(dir.path(), "old value OLD");
    let result = run(
        dir.path(),
        &["edit", "text", &src, "--find", "old", "--replace", "new"],
    )
    .expect("edit");
    assert_eq!(result["replacements"], 1);
    let doc = Document::open(output(&result)).expect("open");
    assert_eq!(words(&doc), ["new", "value", "OLD"]);
    assert_streams_omit(&doc, b"old value OLD");
}

#[test]
fn ruled_table_cells_are_written_in_row_and_column_order_with_csv_escaping() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let page = builder.page(612.0, 792.0);
    for x in [72.0, 192.0, 312.0] {
        page.line((x, 100.0), (x, 180.0), Paint::stroke([0.0; 3], 1.0));
    }
    for y in [100.0, 140.0, 180.0] {
        page.line((72.0, y), (312.0, y), Paint::stroke([0.0; 3], 1.0));
    }
    page.text(80.0, 125.0, "a,b")
        .text(200.0, 125.0, "quoted \"x\"")
        .text(80.0, 165.0, "3")
        .text(200.0, 165.0, "4");
    let src = builder
        .save(dir.path().join("table.pdf"))
        .to_string_lossy()
        .into_owned();
    let table_dir = dir.path().join("tables");
    let result = run(
        dir.path(),
        &[
            "convert",
            "tables",
            &src,
            "-o",
            table_dir.to_str().expect("path"),
        ],
    )
    .expect("tables");
    assert_eq!(result["tables"], 1);
    assert_eq!(
        fs::read(output(&result)).expect("csv"),
        b"\"a,b\",\"quoted \"\"x\"\"\"\r\n3,4\r\n"
    );
}

#[test]
fn redaction_removes_image_samples_and_covered_vector_content_not_just_their_appearance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let rgb = [12_u8, 34, 56].repeat(64 * 32);
    builder
        .page(612.0, 792.0)
        .image_rgb([72.0, 60.0, 200.0, 124.0], 64, 32, &rgb)
        .rect([83.0, 63.0, 90.0, 69.0], Paint::fill([1.0, 0.0, 0.0]))
        .text(80.0, 72.0, "secret")
        .link_uri([80.0, 61.0, 108.0, 74.0], "https://example.com/secret");
    let src = builder
        .save(dir.path().join("pixels.pdf"))
        .to_string_lossy()
        .into_owned();
    let result = run(dir.path(), &["redact", &src, "--find", "secret"]).expect("redact");
    let doc = Document::open(output(&result)).expect("open");
    assert_streams_omit(&doc, b"secret");
    let mut images = Vec::new();
    for id in doc.object_ids() {
        let object = doc.get(id).expect("object");
        if let Some(stream) = object.as_stream()
            && stream.dict.get_name(b"Subtype") == Some(b"Image")
        {
            images.push(doc.decode_stream(stream).expect("image samples").data);
        }
    }
    assert_eq!(
        images.len(),
        1,
        "the original image must not remain as a hidden copy"
    );
    assert_eq!(
        &images[0][(4 * 64 + 8) * 3..(4 * 64 + 8) * 3 + 3],
        &[255, 255, 255]
    );
    assert_eq!(
        &images[0][(16 * 64 + 32) * 3..(16 * 64 + 32) * 3 + 3],
        &[12, 34, 56]
    );
    let page = doc.page(0).expect("page");
    let ops = pdf_core::parse_content(&doc.page_content(&page).expect("contents")).expect("ops");
    assert!(
        !ops.iter()
            .any(|op| op.operator == b"re"
                && op.operands.first().and_then(Object::as_f64) == Some(83.0)),
        "covered vector survived"
    );
}

#[test]
fn hidden_text_and_marked_replacement_text_are_removed_with_the_visible_word() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let hidden = builder.layer("private", false);
    let mut properties = Dict::new();
    properties.insert("ActualText", Object::string(b"secret".to_vec()));
    properties.insert("Alt", Object::string(b"secret".to_vec()));
    let page = builder.page(612.0, 792.0);
    page.begin_layer(hidden)
        .text(72.0, 72.0, "secret")
        .end_layer();
    page.resource("Properties", "Sensitive", Object::Dict(properties))
        .raw_content(b"/Span /Sensitive BDC")
        .text(72.0, 72.0, "secret")
        .raw_content(b"EMC")
        .text(72.0, 120.0, "unrelated");
    let src = builder
        .save(dir.path().join("tagged.pdf"))
        .to_string_lossy()
        .into_owned();
    let result = run(dir.path(), &["redact", &src, "--find", "secret"]).expect("redact");
    let doc = Document::open(output(&result)).expect("open");
    assert_eq!(words(&doc), ["unrelated"]);
    assert_streams_omit(&doc, b"secret");
}

#[test]
fn offset_crop_and_rotation_do_not_move_the_redaction_away_from_the_word() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text(120.0, 150.0, "secret neighbour")
        .set_box("CropBox", [50.0, 100.0, 550.0, 700.0])
        .rotate(90);
    let src = builder
        .save(dir.path().join("crop.pdf"))
        .to_string_lossy()
        .into_owned();
    let result = run(dir.path(), &["redact", &src, "--find", "secret"]).expect("redact");
    let doc = Document::open(output(&result)).expect("open");
    assert_eq!(words(&doc), ["neighbour"]);
    assert_eq!(doc.page(0).expect("page").rotation(), 90);
    assert_streams_omit(&doc, b"secret");
}

#[test]
fn redact_receipts_do_not_leak_search_patterns_into_the_durable_job_record() {
    use goat_common::ledger::{Job, ledger_detail, record_job};

    let dir = tempfile::tempdir().expect("tempdir");
    let src = fixture(dir.path(), "unrelated");
    let secret = "agent-secret-7Qx";
    let result = run(dir.path(), &["redact", &src, "--find", secret]).expect("redact");
    assert_eq!(result["redactions"], 0);
    let detail = Value::Object(ledger_detail(&result));
    record_job(
        dir.path(),
        &Job {
            verb: result["verb"].as_str().expect("verb"),
            status: "success",
            inputs: &result["inputs"],
            outputs: &result["outputs"],
            detail: &detail,
            message: None,
            duration_ms: 0,
        },
    )
    .expect("record job");
    let db = rusqlite::Connection::open(dir.path().join("ledger.db")).expect("ledger");
    let row: (String, String, String, Option<String>) = db
        .query_row(
            "SELECT inputs, outputs, detail, message FROM jobs WHERE verb = 'redact'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("stored redact job");
    assert!(
        !serde_json::to_string(&row)
            .expect("serialize record")
            .contains(secret)
    );
}

#[test]
fn field_redaction_clears_shared_values_reset_defaults_rich_text_and_all_appearances() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text_field("shared", [72.0, 70.0, 250.0, 95.0], "clear-me")
        .combo(
            "choice",
            [72.0, 120.0, 250.0, 145.0],
            &["other", "different"],
            "clear-me",
        )
        .text_field("safe", [72.0, 170.0, 250.0, 195.0], "unrelated");
    builder
        .page(612.0, 792.0)
        .text_field("shared", [72.0, 70.0, 250.0, 95.0], "clear-me");
    let mut doc = builder.build();
    let mut owners = std::collections::HashSet::new();
    for index in 0..2 {
        for widget in pdf_forms::page_widgets(&doc, index).expect("widgets") {
            let object = doc.get(widget).expect("widget");
            let dict = object.as_dict().expect("widget dictionary");
            if pdf_forms::field_full_name(&doc, dict).expect("name") == "safe" {
                continue;
            }
            let owner = dict.get_ref(b"Parent").unwrap_or(widget);
            if owners.insert(owner) {
                let mut field = doc
                    .get(owner)
                    .expect("field")
                    .as_dict()
                    .expect("field dictionary")
                    .clone();
                if field.get_name(b"FT") == Some(b"Ch") {
                    field.insert("Ff", field.get_i64(b"Ff").unwrap_or(0) | (1 << 18));
                }
                field.insert("DV", Object::string(b"clear-me".to_vec()));
                let rich = doc.add(Stream::new(Dict::new(), b"<body>clear-me</body>".to_vec()));
                field.insert("RV", rich);
                doc.set(owner, field);
            }
        }
    }
    let src = dir.path().join("fields.pdf").to_string_lossy().into_owned();
    doc.save(&src, &pdf_core::SaveOptions::default())
        .expect("save fields");
    let result = run(dir.path(), &["redact", &src, "--find", "clear-me"]).expect("redact fields");
    assert_eq!(result["field_redactions"], 3);
    assert_eq!(result["redactions"], 3);
    let reopened = Document::open(output(&result)).expect("independently reopen");
    for index in 0..2 {
        for widget in pdf_forms::page_widgets(&reopened, index).expect("widgets") {
            let object = reopened.get(widget).expect("widget");
            let dict = object.as_dict().expect("widget dictionary");
            let name = pdf_forms::field_full_name(&reopened, dict).expect("name");
            let value = pdf_forms::field_value_text(&reopened, dict).expect("effective value");
            if name == "safe" {
                assert_eq!(value, "unrelated");
            } else {
                assert_eq!(value, "", "{name} has recoverable /V");
                for key in [b"DV".as_slice(), b"RV"] {
                    assert_eq!(
                        pdf_forms::inherited(&reopened, dict, key)
                            .expect("inherited reset/rich value"),
                        Object::Null,
                        "{name} can revive a redacted value"
                    );
                }
                let ap = reopened.resolve_key(dict, b"AP").expect("appearance");
                let normal = reopened
                    .resolve_key(ap.as_dict().expect("AP dictionary"), b"N")
                    .expect("normal appearance");
                assert!(
                    normal.as_stream().is_some(),
                    "every widget gets a rebuilt appearance"
                );
            }
        }
        let text = pdf_text::extract_page(&reopened, index, TextFlags::TEXT)
            .expect("displayed text")
            .text();
        assert!(!text.contains("clear-me"));
    }
    let exported = run(dir.path(), &["form", "export", output(&result)]).expect("export");
    let exported: Value =
        serde_json::from_slice(&fs::read(output(&exported)).expect("exported file"))
            .expect("exported JSON");
    assert_eq!(exported["shared"].as_str().unwrap_or(""), "");
    assert_eq!(exported["choice"].as_str().unwrap_or(""), "");
    assert_eq!(exported["safe"], "unrelated");
    assert_streams_omit(&reopened, b"clear-me");
}

#[test]
fn hidden_forms_and_inline_images_are_removed_where_the_visible_word_was_redacted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut font = Dict::new();
    font.insert("Type", Object::name("Font"));
    font.insert("Subtype", Object::name("Type1"));
    font.insert("BaseFont", Object::name("Helvetica"));
    let mut fonts = Dict::new();
    fonts.insert("F", font);
    let mut resources = Dict::new();
    resources.insert("Font", fonts);
    let mut form = Dict::new();
    form.insert("Type", Object::name("XObject"));
    form.insert("Subtype", Object::name("Form"));
    form.insert(
        "BBox",
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(300),
            Object::Integer(30),
        ],
    );
    form.insert("Resources", resources);
    let form = Stream::new(
        form,
        b"BT /F 11 Tf 1 0 0 1 0 15 Tm (secret neighbour) Tj ET".to_vec(),
    );
    let mut builder = PdfBuilder::new();
    let hidden = builder.layer("private", false);
    let page = builder.page(612.0, 792.0);
    page.resource("XObject", "HiddenForm", Object::Stream(form))
        .begin_layer(hidden)
        .raw_content(b"q 1 0 0 1 72 705 cm /HiddenForm Do Q")
        .raw_content(b"q 8 0 0 8 72 717 cm BI /W 1 /H 1 /CS /RGB /BPC 8 ID \x0c\x22\x38 EI Q")
        .end_layer()
        .text(72.0, 72.0, "secret")
        .text(72.0, 120.0, "unrelated");
    let src = builder
        .save(dir.path().join("hidden-resources.pdf"))
        .to_string_lossy()
        .into_owned();
    let result =
        run(dir.path(), &["redact", &src, "--find", "secret"]).expect("remove hidden resources");
    let reopened = Document::open(output(&result)).expect("reopen");
    assert_streams_omit(&reopened, b"secret");
    let mut neighbour_survives = false;
    for id in reopened.object_ids() {
        let object = reopened.get(id).expect("object");
        if let Some(stream) = object.as_stream() {
            assert_ne!(
                stream.dict.get_name(b"Subtype"),
                Some(b"Image".as_slice()),
                "covered hidden image must be removed"
            );
            let data = reopened.decode_stream(stream).expect("decoded stream").data;
            neighbour_survives |= data.windows(b"neighbour".len()).any(|s| s == b"neighbour");
            for op in pdf_core::parse_content(&data).expect("content") {
                assert_ne!(op.operator, b"BI", "inline samples remain recoverable");
            }
        }
    }
    assert!(neighbour_survives);
    assert!(words(&reopened).iter().any(|word| word == "unrelated"));
}
