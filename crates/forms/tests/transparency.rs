use std::ffi::OsString;
use std::path::Path;

use goat_common::{Ctx, Registry};
use goat_fixtures::PdfBuilder;
use pdf_core::{Dict, Document, Object, Rect, SaveOptions};
use pdf_render::{RenderOptions, render_page};
use serde_json::{Map, Value, json};

fn flatten(home: &Path, source: &str) -> Map<String, Value> {
    let mut registry = Registry::new();
    pdf_forms::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["form", "annotate", "pages"],
        &[],
    );
    let args: Vec<_> = ["pages", "flatten", source]
        .into_iter()
        .map(OsString::from)
        .collect();
    cli.parse(&args)
        .expect("arguments")
        .run(&Ctx::new(home))
        .expect("flatten")
}

fn contents(doc: &Document, page: usize) -> Vec<u8> {
    doc.page_content(&doc.page(page).expect("page"))
        .expect("content")
}

#[test]
fn transparent_page_content_stays_vector_and_unchanged() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder
        .page(192.0, 256.0)
        .text(30.0, 60.0, "Searchable value")
        .raw_content(b"q /Fade gs 1 0 0 rg 25 35 80 40 re f Q\n");
    let mut doc = builder.build();
    let page = doc.page(0).expect("page");
    let mut resources = page.resources;
    let mut state = Dict::new();
    state.insert("ca", Object::Real(0.5));
    let mut states = Dict::new();
    states.insert("Fade", state);
    resources.insert("ExtGState", states);
    let mut dictionary = page.dict;
    dictionary.insert("Resources", resources);
    doc.set(page.id, dictionary);
    let source = home.path().join("transparency.pdf");
    doc.save(&source, &SaveOptions::default())
        .expect("save fixture");
    let first = doc.page(0).expect("first page");
    let options = RenderOptions {
        dpi: 600.0,
        alpha: false,
        annotations: false,
    };
    let before = render_page(&doc, &first, &options).expect("render source");

    let result = flatten(home.path(), source.to_str().expect("path"));

    assert_eq!(result["flattened"], json!(["annotations", "form_fields"]));
    let output = Document::open(result["outputs"][0].as_str().expect("output")).expect("reopen");
    let page = output.page(0).expect("output page");
    assert_eq!(contents(&output, 0), contents(&doc, 0));
    let states = output
        .resolve_key(&page.resources, b"ExtGState")
        .expect("states");
    let fade = output
        .resolve_dict(
            states
                .as_dict()
                .expect("dictionary")
                .get(b"Fade")
                .expect("Fade"),
        )
        .expect("resolve")
        .expect("dictionary");
    assert_eq!(fade.get(b"ca"), Some(&Object::Real(0.5)));
    assert!(!page.resources.contains_key(b"XObject"));
    let after = render_page(&output, &page, &options).expect("render output");
    assert!(before.data() == after.data(), "high-zoom render changed");
}

#[test]
fn flatten_keeps_inactive_link_appearances_and_actions() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder
        .page(96.0, 96.0)
        .link_uri([20.0, 20.0, 70.0, 50.0], "https://example.com/active");
    let mut doc = builder.build();
    let page = doc.page(0).expect("page");
    let annotations = doc.resolve_key(&page.dict, b"Annots").expect("annotations");
    let id = annotations.as_array().expect("array")[0]
        .as_reference()
        .expect("link");
    let mut link = doc
        .get(id)
        .expect("link")
        .as_dict()
        .expect("dictionary")
        .clone();
    let original_rect = link.get(b"Rect").expect("rectangle").clone();
    let mut state = Dict::new();
    state.insert("ca", Object::Real(0.5));
    let mut states = Dict::new();
    states.insert("Fade", state);
    let mut resources = Dict::new();
    resources.insert("ExtGState", states);
    let mut form = Dict::new();
    form.insert("Type", Object::name("XObject"));
    form.insert("Subtype", Object::name("Form"));
    form.insert("BBox", Rect::new(0.0, 0.0, 10.0, 10.0).to_object());
    form.insert("Resources", resources);
    let form = doc.add(pdf_core::Stream::new(
        form,
        b"/Fade gs 0 0 1 rg 0 0 10 10 re f".to_vec(),
    ));
    let mut appearance = Dict::new();
    appearance.insert("N", form);
    link.insert("AP", appearance);
    link.insert("F", 4_i64);
    doc.set(id, link);
    let source = home.path().join("link.pdf");
    doc.save(&source, &SaveOptions::default()).expect("save");
    let before = render_page(
        &doc,
        &page,
        &RenderOptions {
            dpi: 144.0,
            alpha: false,
            annotations: true,
        },
    )
    .expect("source render");
    let result = flatten(home.path(), source.to_str().expect("path"));
    let output = Document::open(result["outputs"][0].as_str().expect("output")).expect("reopen");
    let page = output.page(0).expect("page");
    let annotations = output
        .resolve_key(&page.dict, b"Annots")
        .expect("annotations");
    let link = output
        .resolve_dict(&annotations.as_array().expect("array")[0])
        .expect("resolve")
        .expect("link");
    assert_eq!(link.get(b"Rect"), Some(&original_rect));
    let action = output
        .resolve_dict(link.get(b"A").expect("action"))
        .expect("resolve")
        .expect("action");
    assert_eq!(
        action.get_string(b"URI").expect("URI").to_text(),
        "https://example.com/active"
    );
    let after = render_page(
        &output,
        &page,
        &RenderOptions {
            dpi: 144.0,
            alpha: false,
            annotations: true,
        },
    )
    .expect("output render");
    assert!(
        before.data() == after.data(),
        "flattening must not paint a normally hidden link appearance"
    );
    assert_eq!(result["flattened"], json!(["annotations", "form_fields"]));
}
