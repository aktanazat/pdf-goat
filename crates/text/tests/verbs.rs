use std::ffi::OsString;
use std::fs;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::PdfBuilder;
use serde_json::{Value, json};

fn run(home: &Path, args: &[&str]) -> Result<Value, GoatError> {
    let mut registry = Registry::new();
    pdf_text::register(&mut registry);
    let cli = registry.into_cli(clap::Command::new("pdf-goat"), &[], &[]);
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let invocation = cli.parse(&args).unwrap_or_else(|e| panic!("{e:?}"));
    invocation
        .run(&Ctx::new(home.join("home")))
        .map(Value::Object)
}
fn work() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pdf-text-test-")
        .tempdir_in("/private/var/tmp")
        .expect("isolated work directory")
}
fn document(dir: &Path, name: &str, pages: &[&[&str]]) -> String {
    let mut builder = PdfBuilder::new();
    for lines in pages {
        let page = builder.page(400.0, 400.0);
        for (i, line) in lines.iter().enumerate() {
            page.text(40.0, 70.0 + i as f64 * 18.0, line);
        }
    }
    builder.save(dir.join(name)).to_string_lossy().into_owned()
}

#[test]
fn text_mask_and_export_preserve_page_boundaries_and_character_counts() {
    let work = work();
    let dir = work.path();
    let source = document(
        dir,
        "input.pdf",
        &[&["Account: 123", "café"], &["Account: 456"]],
    );
    let args = [
        "text",
        &source,
        "--mask",
        r"(?<=Account: )\d+",
        "--no-cache",
    ];
    let value = run(dir, &args).expect("lookbehind mask");
    assert_eq!(
        value["pages"],
        json!([{"page":1,"text":"Account: [REDACTED]\ncafé\n"},{"page":2,"text":"Account: [REDACTED]\n"}])
    );
    assert_eq!(value["char_count"], 46);
    let output = dir.join("text.txt");
    let output = output.to_str().expect("UTF-8 path");
    let exported = run(
        dir,
        &[
            "text",
            &source,
            "--mask",
            r"(?<=Account: )\d+",
            "--output",
            output,
        ],
    )
    .expect("export masked text");
    assert_eq!(exported["page_count"], 2);
    assert!(exported.get("pages").is_none());
    assert_eq!(
        fs::read_to_string(output).expect("text output"),
        "Account: [REDACTED]\ncafé\n\nAccount: [REDACTED]\n"
    );
    assert_eq!(exported["char_count"], value["char_count"]);
    let count = run(dir, &["count", &source]).expect("unmasked count");
    assert_eq!(
        (
            count["pages"].clone(),
            count["words"].clone(),
            count["chars"].clone()
        ),
        (json!(2), json!(5), json!(31))
    );
}

#[test]
fn search_limit_alias_uses_the_last_option_and_marks_unvisited_pages() {
    let work = work();
    let dir = work.path();
    let source = document(dir, "input.pdf", &[&["needle needle"], &["needle"]]);
    let all = run(dir, &["search", &source, "needle"]).expect("all hits");
    assert_eq!(all["count"], 3);
    assert_eq!(all["truncated"], false);
    for extra in [
        vec!["--first"],
        vec!["--limit", "2", "--first"],
        vec!["--limit", "0", "--first"],
    ] {
        let mut args = vec!["search", &source, "needle"];
        args.extend(extra);
        let value = run(dir, &args).expect("first wins");
        assert_eq!(value["count"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["hits"][0], all["hits"][0]);
    }
    let two = run(
        dir,
        &["search", &source, "needle", "--first", "--limit", "2"],
    )
    .expect("limit wins");
    assert_eq!(two["count"], 2);
    assert_eq!(two["truncated"], true);
    let last = run(
        dir,
        &[
            "search",
            &source,
            "needle",
            "--first",
            "--pages",
            "2",
            "--no-cache",
        ],
    )
    .expect("last page only");
    assert_eq!(last["count"], 1);
    assert_eq!(last["truncated"], false);
    let error = run(
        dir,
        &["search", &source, "needle", "--first", "--limit", "0"],
    )
    .expect_err("zero limit rejected");
    assert_eq!(error.to_string(), "--limit must be 1 or more");
}

#[test]
fn search_rectangles_ignore_rotation_and_use_the_offset_crop_origin() {
    let work = work();
    let dir = work.path();
    let mut reference = None;
    for rotation in [0, 90, 180, 270] {
        let mut builder = PdfBuilder::new();
        builder
            .page(400.0, 400.0)
            .set_box("CropBox", [20.0, 30.0, 380.0, 350.0])
            .rotate(rotation)
            .text(40.0, 90.0, "needle");
        let source = builder
            .save(dir.join(format!("rot-{rotation}.pdf")))
            .to_string_lossy()
            .into_owned();
        let value = run(dir, &["search", &source, "needle", "--no-cache"]).expect("cropped search");
        assert_eq!(
            value["hits"],
            json!([{"page":1,"rect":[20.0,28.2,53.0,43.3]}])
        );
        if let Some(reference) = &reference {
            assert_eq!(&value["hits"], reference);
        } else {
            reference = Some(value["hits"].clone());
        }
    }
}

#[test]
fn phrase_search_crosses_lines_but_keeps_separate_rectangles() {
    let work = work();
    let dir = work.path();
    let source = document(dir, "input.pdf", &[&["revenue", "grew rapidly"]]);
    let value = run(dir, &["search", &source, "revenue grew", "--no-cache"])
        .expect("phrase across PDF lines");
    assert_eq!(value["count"], 2);
    assert_eq!(value["hits"][0]["rect"], json!([40.0, 58.2, 79.7, 73.3]));
    assert_eq!(value["hits"][1]["rect"], json!([40.0, 76.2, 63.8, 91.3]));
}

#[test]
fn comparison_maps_pages_before_diffing_and_masks_only_the_report() {
    let work = work();
    let dir = work.path();
    let left = document(
        dir,
        "left.pdf",
        &[&["alpha", "old 123", "omega"], &["other"]],
    );
    let right = document(
        dir,
        "right.pdf",
        &[&["alpha", "new 456", "omega"], &["other"]],
    );
    let value = run(
        dir,
        &[
            "compare",
            "text",
            &left,
            &right,
            "--mask",
            r"\d+",
            "--max-lines",
            "20",
            "--no-cache",
        ],
    )
    .expect("comparison");
    assert_eq!(value["identical"], false);
    assert_eq!(value["added"], 1);
    assert_eq!(value["removed"], 1);
    assert_eq!(value["unmatched"], json!([]));
    assert_eq!(
        value["pages"][0]["match"],
        json!({"file":right,"page":1,"ratio":0.667})
    );
    assert_eq!(
        value["pages"][0]["diff"],
        json!([
            "@@ -[REDACTED],[REDACTED] +[REDACTED],[REDACTED] @@",
            " alpha",
            "-old [REDACTED]",
            "+new [REDACTED]",
            " omega"
        ])
    );
    assert_eq!(value["pages"][1]["match"]["page"], 2);
    assert_eq!(value["pages"][1]["diff"], json!([]));
    let truncated = run(dir, &["compare", "text", &left, &right, "--max-lines", "0"])
        .expect("zero returned diff lines");
    assert_eq!(truncated["pages"][0]["diff"], json!([]));
    assert_eq!(truncated["pages"][0]["diff_truncated"], true);
    assert_eq!(truncated["added"], 1);
}

#[test]
fn bounded_blocks_exposes_a_cursor_and_does_not_repeat_consumed_blocks() {
    let work = work();
    let dir = work.path();
    let source = document(dir, "input.pdf", &[&["first"], &["second"], &["third"]]);
    let first =
        run(dir, &["get", "text-blocks", &source, "--max-blocks", "2"]).expect("bounded blocks");
    assert_eq!(first["count"], 2);
    assert_eq!(first["next_block"], 2);
    assert_eq!(first["block_truncated"], true);
    let rest = run(
        dir,
        &[
            "get",
            "text-blocks",
            &source,
            "--max-blocks",
            "2",
            "--start-block",
            "2",
        ],
    )
    .expect("remaining block");
    assert_eq!(rest["count"], 1);
    assert_eq!(rest["blocks"][0]["text"], "third");
    assert_eq!(rest["next_block"], Value::Null);
    assert_eq!(rest["truncated"], false);
}

#[test]
fn invalid_mask_is_rejected_before_reading_or_releasing_text() {
    let work = work();
    let dir = work.path();
    let source = document(dir, "input.pdf", &[&["private account"]]);
    let error = run(dir, &["text", &source, "--mask", "["]).expect_err("bad mask cannot leak text");
    assert_eq!(
        error.to_string(),
        "error: unterminated character set at position 0"
    );
    let missing = run(dir, &["search", &source, "private", "--meaning"])
        .expect_err("meaning does not download silently");
    assert!(missing.to_string().contains("run: pdf-goat setup meaning"));
}

#[test]
fn layout_preserves_physical_gaps_but_caps_empty_vertical_space() {
    let work = work();
    let dir = work.path();
    let mut pdf = PdfBuilder::new();
    pdf.page(400.0, 400.0)
        .text(40.0, 50.0, "Heading")
        .text(40.0, 200.0, "alpha WWWW");
    let source = pdf
        .save(dir.join("spaced.pdf"))
        .to_string_lossy()
        .into_owned();
    let value = run(dir, &["text", &source, "--layout"]).expect("positioned text");
    assert_eq!(value["pages"][0]["text"], "Heading\n\n\n\n\n\nalpha WWWW");
    assert_eq!(value["pages"][0]["reading_order"][1]["text"], "alpha WWWW");
    assert_eq!(value["pages"][0]["word_count"], 3);
}

#[test]
fn extraction_clips_actual_empty_glyphs_at_the_crop_edge() {
    let work = work();
    let dir = work.path();
    let mut pdf = PdfBuilder::new();
    pdf.page(400.0, 400.0)
        .text(40.0, 50.0, "Heading")
        .raw_content(b"BT /helv 12 Tf -40 200 Td (outside inside) Tj ET");
    let source = pdf
        .save(dir.join("offpage.pdf"))
        .to_string_lossy()
        .into_owned();
    let value = run(dir, &["text", &source, "--no-cache"]).expect("clipped text");
    assert_eq!(value["pages"][0]["text"], "Heading\ninside\n");
    let count = run(dir, &["count", &source, "--no-cache"]).expect("clipped counts");
    assert_eq!(count["chars"], 15);
    assert_eq!(count["words"], 2);
}

#[test]
fn cache_cold_warm_and_bypassed_receipts_agree_for_every_cached_verb() {
    let work = work();
    let dir = work.path();
    let source = document(dir, "cache.pdf", &[&["New", "York"], &["cache page three"]]);
    let output = dir.join("text.txt");
    let output = output.to_str().expect("output path");
    for args in [
        vec!["text", &source],
        vec!["text", &source, "-o", output],
        vec!["count", &source],
        vec!["search", &source, "New York"],
        vec!["compare", "text", &source, &source],
    ] {
        let cold = run(dir, &args).expect("cold extraction");
        let warm = run(dir, &args).expect("cached extraction");
        assert_eq!(warm, cold);
        let mut bypassed = args;
        bypassed.push("--no-cache");
        assert_eq!(run(dir, &bypassed).expect("uncached extraction"), cold);
    }
    assert_eq!(
        fs::read_to_string(output).expect("export"),
        "New\nYork\n\ncache page three\n"
    );
}

#[test]
fn stale_cached_page_counts_do_not_hide_later_limited_search_hits() {
    use goat_common::textcache::{Cache, Form, PageEntry, document_key};
    let work = work();
    let dir = work.path();
    let source = document(
        dir,
        "cache.pdf",
        &[&["page one"], &["page two"], &["target"]],
    );
    let expected = run(dir, &["search", &source, "target", "--first"]).expect("uncorrupted search");
    let key = document_key(Path::new(&source)).expect("file key");
    let cache = Cache::open(dir.join("home/cache.sqlite"), 1 << 20);
    cache.write(
        &key,
        Path::new(&source),
        1,
        Form::Text,
        [(0, &PageEntry::Text("page one\n".into()))],
    );
    assert_eq!(
        run(dir, &["search", &source, "target", "--first"]).expect("contradicted cache recovery"),
        expected
    );
    assert_eq!(
        run(dir, &["text", &source]).expect("all pages restored")["pages"]
            .as_array()
            .expect("pages")
            .len(),
        3
    );
}

#[test]
fn a_valid_cached_value_is_used_but_no_cache_reads_the_current_pdf() {
    use goat_common::textcache::{Cache, Form, PageEntry, document_key};
    let work = work();
    let dir = work.path();
    let source = document(dir, "cache.pdf", &[&["live text"]]);
    let live = run(dir, &["text", &source]).expect("live extraction");
    let key = document_key(Path::new(&source)).expect("file key");
    let cache = Cache::open(dir.join("home/cache.sqlite"), 1 << 20);
    cache.write(
        &key,
        Path::new(&source),
        1,
        Form::Text,
        [(0, &PageEntry::Text("cached sentinel".into()))],
    );
    assert_eq!(
        run(dir, &["text", &source]).expect("cached receipt")["pages"][0]["text"],
        "cached sentinel"
    );
    assert_eq!(
        run(dir, &["text", &source, "--no-cache"]).expect("cache bypass"),
        live
    );
}

#[test]
fn literal_search_folds_ligatures_in_both_pdf_text_and_the_query() {
    let work = work();
    let dir = work.path();
    let mut pdf = PdfBuilder::new();
    pdf.page(400.0,400.0).text(40.0,50.0,"Heading").raw_content(b"/Span << /ActualText <FEFF00440065FB01006E0069006E006700200074006800650020FB02006F0077> >> BDC BT /helv 12 Tf 40 200 Td (Defining the flow) Tj ET EMC");
    let source = pdf
        .save(dir.join("ligatures.pdf"))
        .to_string_lossy()
        .into_owned();
    let ordinary =
        run(dir, &["search", &source, "Defining", "--no-cache"]).expect("expanded ligature search");
    let ligature = run(dir, &["search", &source, "Deﬁning"]).expect("ligature query search");
    assert_eq!(ordinary["count"], 1);
    assert_eq!(ordinary["hits"], ligature["hits"]);
}

#[test]
fn layout_preserves_unicode_spacing_from_actual_text() {
    let work = work();
    let dir = work.path();
    let mut pdf = PdfBuilder::new();
    pdf.page(400.0,400.0).text(40.0,50.0,"Heading").raw_content(b"/Span << /ActualText <FEFF03B103B220000667FB01> >> BDC BT /helv 12 Tf 40 200 Td (abcde) Tj ET EMC");
    let source = pdf
        .save(dir.join("unicode.pdf"))
        .to_string_lossy()
        .into_owned();
    let value = run(dir, &["text", &source, "--layout"]).expect("Unicode layout text");
    assert_eq!(
        value["pages"][0]["text"],
        "Heading\n\n\n\n\n\nαβ\u{2000}٧ ﬁ"
    );
    assert_eq!(value["pages"][0]["reading_order"][1]["text"], "αβ ٧ ﬁ");
    assert_eq!(value["pages"][0]["word_count"], 4);
}

#[test]
fn a_cache_miss_reconciles_the_actual_page_count_before_returning_results() {
    use goat_common::textcache::{Cache, Form, PageEntry, document_key};
    let work = work();
    let dir = work.path();
    let source = document(dir, "short.pdf", &[&["live text"]]);
    let expected = run(dir, &["text", &source, "--no-cache"]).expect("uncached text");
    let key = document_key(Path::new(&source)).expect("file key");
    Cache::open(dir.join("home/cache.sqlite"), 1 << 20).write(
        &key,
        Path::new(&source),
        2,
        Form::Text,
        [(0, &PageEntry::Text("stale row".into()))],
    );
    assert_eq!(
        run(dir, &["text", &source]).expect("overstated count recovery"),
        expected
    );

    let source = document(
        dir,
        "long.pdf",
        &[&["page one"], &["page two"], &["target"]],
    );
    let expected =
        run(dir, &["search", &source, "target", "--first", "--no-cache"]).expect("last page hit");
    let doc = pdf_core::Document::load(fs::read(&source).expect("PDF bytes")).expect("PDF");
    let first = PageEntry::Words(pdf_text::page_words(&doc, 0).expect("first-page words"));
    let key = document_key(Path::new(&source)).expect("file key");
    Cache::open(dir.join("home/cache.sqlite"), 1 << 20).write(
        &key,
        Path::new(&source),
        2,
        Form::Words,
        [(0, &first)],
    );
    assert_eq!(
        run(dir, &["search", &source, "target", "--first"]).expect("understated count recovery"),
        expected
    );
}
