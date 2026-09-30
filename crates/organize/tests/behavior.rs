use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};
use goat_common::{Cli, Ctx, Registry};
use goat_fixtures::{PdfBuilder, text_pages};
use pdf_core::{Dict, Document, Object, Rect, parse_content};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Case {
    directory: TempDir,
    cli: Cli,
    ctx: Ctx,
}

impl Case {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("isolated test directory");
        let ctx = Ctx::new(directory.path().join("home"));
        let mut registry = Registry::new();
        pdf_organize::register(&mut registry);
        let root = Command::new("pdf-goat")
            .arg(Arg::new("agent").long("agent").action(ArgAction::SetTrue));
        let cli = registry.into_cli(
            root,
            &[],
            &[
                ("pages", "layout"),
                ("get", "assets"),
                ("bookmarks", "outline"),
                ("links", "links"),
            ],
        );
        Self {
            directory,
            cli,
            ctx,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }
    fn text(&self, name: &str, labels: &[&str]) -> String {
        let path = self.path(name);
        std::fs::write(&path, text_pages(labels)).expect("write input");
        path.to_string_lossy().into_owned()
    }
    fn run(&self, args: &[&str]) -> Result<Value, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let call = self
            .cli
            .parse(&args)
            .map_err(|error| format!("{error:?}"))?;
        call.run(&self.ctx)
            .map(Value::Object)
            .map_err(|error| error.envelope_text())
    }
    fn output(&self, result: &Value) -> Document {
        let path = result["outputs"][0].as_str().expect("output path");
        load(Path::new(path))
    }
}

fn load(path: &Path) -> Document {
    Document::load(std::fs::read(path).expect("read PDF")).expect("valid output PDF")
}

/// Decode actual text-showing operands, including placed Form XObjects, rather than
/// comparing serialization or depending on text extraction's in-flight implementation.
fn shown_text(doc: &Document, content: &[u8], resources: &Dict, depth: usize) -> String {
    assert!(depth < 16, "acyclic fixture Forms");
    let mut text = String::new();
    for op in parse_content(content).expect("PDF content") {
        match op.operator.as_slice() {
            b"Tj" => {
                if let Some(value) = op.operands.first().and_then(Object::as_string) {
                    text.push_str(&value.to_text());
                }
            }
            b"TJ" => {
                for value in op
                    .operands
                    .first()
                    .and_then(Object::as_array)
                    .unwrap_or_default()
                {
                    if let Some(value) = value.as_string() {
                        text.push_str(&value.to_text());
                    }
                }
            }
            b"Do" => {
                let name = op
                    .operands
                    .first()
                    .and_then(Object::as_name)
                    .expect("XObject name");
                if let Some(objects) = resources
                    .get(b"XObject")
                    .and_then(|value| doc.resolve_dict(value).expect("resources"))
                    && let Some(object) = objects.get(name)
                {
                    let value = doc.resolve(object).expect("XObject");
                    if let Some(stream) = value.as_stream()
                        && stream.dict.get_name(b"Subtype") == Some(b"Form".as_slice())
                    {
                        let own = stream
                            .dict
                            .get(b"Resources")
                            .and_then(|value| doc.resolve_dict(value).expect("form resources"))
                            .unwrap_or_default();
                        text.push_str(&shown_text(
                            doc,
                            &doc.decode_stream(stream).expect("decode form").data,
                            &own,
                            depth + 1,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    text
}

fn labels(doc: &Document) -> Vec<String> {
    doc.pages()
        .expect("pages")
        .iter()
        .map(|page| {
            shown_text(
                doc,
                &doc.page_content(page).expect("content"),
                &page.resources,
                0,
            )
        })
        .collect()
}

fn linked(case: &Case) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text(72.0, 72.0, "alpha")
        .link_uri([72.0, 100.0, 200.0, 120.0], "https://example.org")
        .link_goto([72.0, 130.0, 200.0, 150.0], 2);
    builder
        .page(612.0, 792.0)
        .text(72.0, 72.0, "beta")
        .link_goto([72.0, 130.0, 200.0, 150.0], 0);
    builder.page(612.0, 792.0).text(72.0, 72.0, "gamma");
    builder
        .outline(1, "Alpha", 1)
        .outline(2, "Beta", 2)
        .outline(1, "Gamma", 3);
    builder
        .save(case.path("linked.pdf"))
        .to_string_lossy()
        .into_owned()
}

#[test]
fn extract_selection_is_required_in_capabilities() {
    let case = Case::new();
    let map = case.cli.capabilities(Some("extract")).expect("schema");
    let pages = map["schemas"]["extract"]["arguments"]
        .as_array()
        .expect("arguments")
        .iter()
        .find(|arg| arg["name"] == "pages")
        .expect("page selector");
    assert_eq!(pages["flags"], json!(["--pages"]));
    assert_eq!(pages["required"], true);
    assert_eq!(pages["type"], "string");
}

#[test]
fn blank_rejects_out_of_range_counts_without_output() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one"]);
    let out = case.path("blank.pdf");
    for count in ["0", "101"] {
        let error = case
            .run(&[
                "pages",
                "blank",
                &input,
                "--count",
                count,
                "-o",
                out.to_str().expect("path"),
            ])
            .expect_err("invalid count");
        assert_eq!(error, "--count must be between 1 and 100");
        assert!(!out.exists());
    }
}

#[test]
fn extraction_keeps_order_repeats_links_and_source_bytes() {
    let case = Case::new();
    let input = linked(&case);
    let original = std::fs::read(&input).expect("source bytes");
    let result = case
        .run(&["extract", &input, "--pages", "1,1,3"])
        .expect("extract");
    let out = case.output(&result);
    assert_eq!(labels(&out), ["alpha", "alpha", "gamma"]);
    assert_eq!(result["pages"], json!([1, 1, 3]));
    assert_eq!(out.version(), (1, 3));
    assert_eq!(std::fs::read(&input).expect("source after"), original);
    let output = result["outputs"][0].as_str().expect("path");
    let links = case.run(&["get", "links", output]).expect("links");
    assert_eq!(links["count"], 4);
    assert_eq!(links["links"][1]["target_page"], 2);
    assert_eq!(links["links"][3]["target_page"], 2);
}

#[test]
fn failed_extract_removes_partial_file() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one", "two"]);
    let blocked = case.path("out.pdf");
    std::fs::create_dir(&blocked).expect("blocking directory");
    let error = case
        .run(&[
            "extract",
            &input,
            "--pages",
            "1",
            "-o",
            blocked.to_str().expect("path"),
        ])
        .expect_err("rename refused");
    assert!(error.starts_with("IsADirectoryError:"), "{error}");
    assert!(blocked.is_dir());
    assert!(!case.path("out.pdf.part").exists());
}

#[test]
fn deletion_preserves_remaining_order_and_source() {
    let case = Case::new();
    let input = case.text(
        "source.pdf",
        &["page one", "page two", "page three", "page four"],
    );
    let original = std::fs::read(&input).expect("source");
    let result = case
        .run(&["delete", &input, "--pages", "2"])
        .expect("delete");
    assert_eq!(result["deleted_pages"], json!([2]));
    assert_eq!(result["remaining_pages"], 3);
    assert_eq!(
        labels(&case.output(&result)),
        ["page one", "page three", "page four"]
    );
    assert_eq!(std::fs::read(input).expect("source after"), original);
}

#[test]
fn deletion_disables_removed_outline_targets_and_keeps_surviving_links() {
    let case = Case::new();
    let input = linked(&case);
    let result = case
        .run(&["delete", &input, "--pages", "2"])
        .expect("delete");
    let output = result["outputs"][0].as_str().expect("path");
    let bookmarks = case.run(&["get", "bookmarks", output]).expect("bookmarks");
    assert_eq!(
        bookmarks["bookmarks"],
        json!([
            {"level":1,"title":"Alpha","page":1}, {"level":2,"title":"Beta","page":-1}, {"level":1,"title":"Gamma","page":2}
        ])
    );
    let links = case.run(&["get", "links", output]).expect("links");
    assert_eq!(links["count"], 2);
    assert_eq!(links["links"][1]["target_page"], 1);
}

#[test]
fn partial_reorder_retains_unmentioned_pages_and_source() {
    let case = Case::new();
    let input = case.text(
        "source.pdf",
        &["page one", "page two", "page three", "page four"],
    );
    let original = std::fs::read(&input).expect("source");
    let result = case
        .run(&["reorder", &input, "--order", "3,1"])
        .expect("reorder");
    assert_eq!(result["order"], json!([3, 1, 2, 4]));
    assert_eq!(
        labels(&case.output(&result)),
        ["page three", "page one", "page two", "page four"]
    );
    assert_eq!(std::fs::read(input).expect("source after"), original);
}

#[test]
fn invalid_reorder_specs_do_not_create_output() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one", "two"]);
    let out = case.path("out.pdf");
    for (spec, expected) in [
        ("1,1", "--order must not name the same page twice"),
        ("", "--order must name at least one page"),
        (" ", "--order must name at least one page"),
        (",", "--order must name at least one page"),
    ] {
        assert_eq!(
            case.run(&[
                "reorder",
                &input,
                "--order",
                spec,
                "-o",
                out.to_str().expect("path")
            ])
            .expect_err("invalid"),
            expected
        );
        assert!(!out.exists());
    }
}

#[test]
fn malformed_page_ranges_are_rejected() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one", "two"]);
    for (verb, flag, spec) in [
        ("reorder", "--order", "2-"),
        ("delete", "--pages", "x"),
        ("extract", "--pages", "-2"),
        ("reorder", "--order", "1-2-3"),
    ] {
        assert_eq!(
            case.run(&[verb, &input, flag, spec]).expect_err("bad spec"),
            format!("'{spec}' is not a page number or range")
        );
    }
}

#[test]
fn duplicate_omits_links_to_pages_not_copied_but_keeps_originals() {
    let case = Case::new();
    let input = linked(&case);
    let result = case
        .run(&["pages", "duplicate", &input, "--pages", "1,2,1"])
        .expect("duplicate");
    assert_eq!(
        labels(&case.output(&result)),
        ["alpha", "alpha", "beta", "beta", "gamma"]
    );
    let output = result["outputs"][0].as_str().expect("path");
    let links = case.run(&["get", "links", output]).expect("links");
    assert_eq!(links["count"], 4);
    assert_eq!(links["links"][1]["target_page"], 4);
    assert_eq!(links["links"][2]["uri"], "https://example.org");
}

#[test]
fn crop_converts_top_left_coordinates_and_rotation_preserves_content() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one", "two"]);
    let crop = case
        .run(&[
            "pages",
            "crop",
            &input,
            "--box",
            "10,20,400,500",
            "--pages",
            "2",
        ])
        .expect("crop");
    let doc = case.output(&crop);
    assert_eq!(
        doc.page(0).expect("page").crop_box(),
        Rect::new(0.0, 0.0, 595.0, 842.0)
    );
    assert_eq!(
        doc.page(1).expect("page").crop_box(),
        Rect::new(10.0, 342.0, 400.0, 822.0)
    );
    let rotate = case
        .run(&["rotate", &input, "--pages", "2,2", "--deg", "-450"])
        .expect("rotate");
    let doc = case.output(&rotate);
    assert_eq!(doc.page(1).expect("page").rotation(), 270);
    assert_eq!(labels(&doc), ["one", "two"]);
}

#[test]
fn booklet_pads_without_losing_page_order() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one", "two", "three", "four", "five"]);
    let result = case.run(&["pages", "booklet", &input]).expect("booklet");
    let doc = case.output(&result);
    assert_eq!(result["page_order"], json!([8, 1, 2, 7, 6, 3, 4, 5]));
    assert_eq!(labels(&doc), ["one", "two", "three", "fourfive"]);
    assert_eq!(
        doc.page(0).expect("sheet").media_box(),
        Rect::new(0.0, 0.0, 1190.0, 842.0)
    );
}

#[test]
fn outline_replacement_and_clear_are_persistent() {
    let case = Case::new();
    let input = linked(&case);
    let data = case.path("outline.json");
    std::fs::write(
        &data,
        r#"[{"level":1,"title":"café 中文","page":2},{"level":2,"title":"last","page":3}]"#,
    )
    .expect("outline JSON");
    let set = case
        .run(&[
            "bookmarks",
            "set",
            &input,
            "--data",
            data.to_str().expect("path"),
        ])
        .expect("set");
    let output = set["outputs"][0].as_str().expect("path");
    let toc = case.run(&["get", "bookmarks", output]).expect("read");
    assert_eq!(
        toc["bookmarks"][0],
        json!({"level":1,"title":"café 中文","page":2})
    );
    let cleared = case.run(&["bookmarks", "clear", output]).expect("clear");
    assert_eq!(cleared["removed"], 2);
    assert!(
        !case
            .output(&cleared)
            .catalog()
            .expect("catalog")
            .contains_key(b"Outlines")
    );
}

#[test]
fn external_link_removal_leaves_page_navigation() {
    let case = Case::new();
    let input = linked(&case);
    let removed = case
        .run(&["links", "remove", &input, "--external-only"])
        .expect("remove external");
    assert_eq!(removed["removed"], 1);
    let output = removed["outputs"][0].as_str().expect("path");
    let links = case.run(&["get", "links", output]).expect("get");
    assert_eq!(
        links["links"]
            .as_array()
            .expect("links")
            .iter()
            .map(|link| link["target_page"].clone())
            .collect::<Vec<_>>(),
        vec![json!(2), json!(0)]
    );
}

#[test]
fn header_templates_and_bates_negative_numbers_survive_save() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one", "two"]);
    let header = case
        .run(&[
            "pages",
            "header",
            &input,
            "--text",
            "Page {page} of {pages}",
        ])
        .expect("header");
    assert_eq!(
        labels(&case.output(&header)),
        ["onePage 1 of 2", "twoPage 2 of 2"]
    );
    let bates = case
        .run(&[
            "pages", "bates", &input, "--prefix", "ABC-", "--start", "-2", "--digits", "4",
        ])
        .expect("bates");
    assert_eq!(bates["first"], "ABC--002");
    assert_eq!(labels(&case.output(&bates)), ["oneABC--002", "twoABC--001"]);
}

#[test]
fn link_rectangles_follow_clockwise_page_rotation() {
    let case = Case::new();
    let mut builder = PdfBuilder::new();
    let mut action = Dict::new();
    action.insert("S", Object::name("URI"));
    action.insert("URI", Object::text("https://example.org"));
    let mut annot = Dict::new();
    annot.insert("Subtype", Object::name("Link"));
    annot.insert("Rect", Rect::new(100.0, 100.0, 200.0, 150.0).to_object());
    annot.insert("A", action);
    builder.page(612.0, 792.0).rotate(90).annotation(annot);
    let input = builder.save(case.path("rotated.pdf"));
    let links = case
        .run(&["get", "links", input.to_str().expect("path")])
        .expect("links");
    assert_eq!(
        links["links"][0]["rect"],
        json!([100.0, 100.0, 150.0, 200.0])
    );
}

#[test]
fn watermark_treats_line_breaks_as_spaces_not_text_lines() {
    let case = Case::new();
    let input = case.text("source.pdf", &["one"]);
    let result = case
        .run(&[
            "watermark",
            &input,
            "--text",
            "a\nb\nc",
            "--size",
            "24",
            "--angle",
            "0",
        ])
        .expect("watermark");
    assert_eq!(result["text"], "a\nb\nc");
    assert_eq!(labels(&case.output(&result)), ["onea b c"]);
}
