use std::ffi::OsString;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::PdfBuilder;
use pdf_core::{Document, Object};
use serde_json::{Map, Value, json};

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_forms::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["form", "annotate", "pages"],
        &[],
    );
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    cli.parse(&args)
        .unwrap_or_else(|e| panic!("{e:?}"))
        .run(&Ctx::new(home))
}
fn source(home: &Path) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(595.0, 842.0)
        .text(72.0, 72.0, "Hello World")
        .link_uri([72.0, 80.0, 120.0, 100.0], "https://example.com");
    builder.page(595.0, 842.0).text(72.0, 72.0, "hello again");
    builder
        .save(home.join("base.pdf"))
        .to_string_lossy()
        .into_owned()
}
fn output(result: &Map<String, Value>) -> &str {
    result["outputs"][0].as_str().expect("output")
}

#[test]
fn markup_search_respects_page_selection_and_delete_keeps_links() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = source(home.path());
    let marked = run(
        home.path(),
        &[
            "annotate",
            "highlight",
            &source,
            "--find",
            "hello",
            "--pages",
            "2",
        ],
    )
    .expect("highlight");
    assert_eq!(marked["marks"], 1);
    let listed = run(home.path(), &["annotate", "list", output(&marked)]).expect("list");
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["annotations"][0]["page"], 2);
    assert_eq!(listed["annotations"][0]["type"], "Highlight");
    let removed = run(
        home.path(),
        &[
            "annotate",
            "delete",
            output(&marked),
            "--type",
            "highlight",
            "--pages",
            "2",
        ],
    )
    .expect("delete");
    assert_eq!(removed["removed"], 1);
    let doc = Document::open(output(&removed)).expect("output");
    let page = doc.page(0).expect("page");
    let annots = doc.resolve_key(&page.dict, b"Annots").expect("annotations");
    let links = annots
        .as_array()
        .expect("array")
        .iter()
        .filter(|a| {
            doc.resolve_dict(a)
                .expect("resolve")
                .is_some_and(|d| d.get_name(b"Subtype") == Some(b"Link"))
        })
        .count();
    assert_eq!(links, 1);
}

#[test]
fn note_delete_removes_its_popup_but_not_other_annotations() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = source(home.path());
    let note = run(
        home.path(),
        &[
            "annotate",
            "note",
            &source,
            "--at",
            "100,200",
            "--text",
            "Keep the source",
        ],
    )
    .expect("note");
    let rectangle = run(
        home.path(),
        &[
            "annotate",
            "rect",
            output(&note),
            "--rect",
            "100,400,200,450",
        ],
    )
    .expect("rect");
    let deleted = run(
        home.path(),
        &["annotate", "delete", output(&rectangle), "--type", "Text"],
    )
    .expect("delete");
    assert_eq!(deleted["removed"], 1);
    let list = run(home.path(), &["annotate", "list", output(&deleted)]).expect("list");
    assert_eq!(list["count"], 1);
    assert_eq!(list["annotations"][0]["type"], "Square");
    let doc = Document::open(output(&deleted)).expect("pdf");
    let page = doc.page(0).expect("page");
    let annots = doc.resolve_key(&page.dict, b"Annots").expect("annotations");
    assert!(annots.as_array().expect("array").iter().all(|a| {
        doc.resolve_dict(a)
            .expect("resolve")
            .is_none_or(|d| !matches!(d.get_name(b"Subtype"), Some(b"Popup" | b"Text")))
    }));
}

#[test]
fn area_highlight_keeps_opacity_color_and_exact_visible_bounds() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = source(home.path());
    let marked = run(
        home.path(),
        &[
            "annotate",
            "area-highlight",
            &source,
            "--rect",
            "50,700,250,750",
            "--color",
            "0,1,0",
            "--opacity",
            "0.4",
        ],
    )
    .expect("area");
    let listed = run(home.path(), &["annotate", "list", output(&marked)]).expect("list");
    assert_eq!(
        listed["annotations"][0]["rect"],
        json!([49.0, 699.0, 251.0, 751.0])
    );
    let doc = Document::open(output(&marked)).expect("pdf");
    let page = doc.page(0).expect("page");
    let annots = doc.resolve_key(&page.dict, b"Annots").expect("annotations");
    let area = annots
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|o| doc.resolve_dict(o).expect("resolve"))
        .find(|d| d.get_name(b"Subtype") == Some(b"Square"))
        .expect("area");
    assert_eq!(area.get_f64(b"CA"), Some(0.4));
    assert_eq!(
        area.get_array(b"IC")
            .expect("color")
            .iter()
            .filter_map(Object::as_f64)
            .collect::<Vec<_>>(),
        vec![0.0, 1.0, 0.0]
    );
    let invalid = run(
        home.path(),
        &[
            "annotate",
            "area-highlight",
            &source,
            "--rect",
            "50,700,250,750",
            "--opacity",
            "2",
        ],
    )
    .expect_err("opacity rejected");
    assert_eq!(invalid.to_string(), "--opacity must be between 0 and 1");
}

#[test]
fn flatten_preserves_textbox_text_and_existing_page_text() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = source(home.path());
    let textbox = run(
        home.path(),
        &[
            "annotate",
            "textbox",
            &source,
            "--rect",
            "100,300,300,360",
            "--text",
            "Retained note",
        ],
    )
    .expect("textbox");
    let flattened = run(home.path(), &["annotate", "flatten", output(&textbox)]).expect("flatten");
    let listed = run(home.path(), &["annotate", "list", output(&flattened)]).expect("list");
    assert_eq!(listed["count"], 0);
    let doc = Document::open(output(&flattened)).expect("pdf");
    let text = pdf_text::extract_page(&doc, 0, pdf_text::TextFlags::TEXT)
        .expect("extract")
        .text();
    assert!(text.contains("Hello World"), "{text}");
    assert!(text.contains("Retained note"), "{text}");
}

#[test]
fn rotated_pages_use_unrotated_annotation_coordinates() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder
        .page(595.0, 842.0)
        .rotate(90)
        .text(72.0, 72.0, "Hello World");
    let source = builder.save(home.path().join("rotated.pdf"));
    let source = source.to_str().expect("path");
    let rectangle = run(
        home.path(),
        &["annotate", "rect", source, "--rect", "100,200,250,250"],
    )
    .expect("rectangle");
    let listed = run(home.path(), &["annotate", "list", output(&rectangle)]).expect("list");
    assert_eq!(
        listed["annotations"][0]["rect"],
        json!([99.0, 199.0, 251.0, 251.0])
    );
    let note = run(
        home.path(),
        &[
            "annotate",
            "note",
            source,
            "--at",
            "100,200",
            "--text",
            "Rotated note",
        ],
    )
    .expect("note");
    let listed = run(home.path(), &["annotate", "list", output(&note)]).expect("list");
    assert_eq!(
        listed["annotations"][0]["rect"],
        json!([100.0, 184.0, 116.0, 200.0])
    );
    let doc = Document::open(output(&note)).expect("note PDF");
    assert_eq!(doc.page(0).expect("page").rotation(), 90);
}
