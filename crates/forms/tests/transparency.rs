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
fn transparent_pages_become_opaque_searchable_and_keep_geometry_links_metadata() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder.info("Title", "Preserved transparency");
    builder
        .page(192.0, 256.0)
        .rotate(90)
        .entry("CropBox", Rect::new(10.0, 20.0, 182.0, 236.0).to_object())
        .entry("UserUnit", Object::Real(1.25))
        .text(30.0, 60.0, "Searchable value")
        .link_uri([30.0, 70.0, 130.0, 90.0], "https://example.com/kept")
        .raw_content(b"q /Fade gs 1 0 0 rg 25 35 80 40 re f Q\n");
    builder
        .page(192.0, 256.0)
        .text(30.0, 60.0, "Untouched page");
    let mut doc = builder.build();
    for index in 0..2 {
        let page = doc.page(index).expect("page");
        let mut resource = page.resources;
        let mut states = Dict::new();
        let mut state = Dict::new();
        state.insert("ca", Object::Real(0.5));
        states.insert("Fade", state);
        resource.insert("ExtGState", states);
        let mut dictionary = page.dict;
        dictionary.insert("Resources", resource);
        doc.set(page.id, dictionary);
    }
    let source = home.path().join("transparency.pdf");
    doc.save(&source, &SaveOptions::default())
        .expect("save fixture");
    let first = doc.page(0).expect("first page");
    let before = render_page(
        &doc,
        &first,
        &RenderOptions {
            dpi: 144.0,
            alpha: false,
            annotations: false,
        },
    )
    .expect("render source");
    assert!(
        before
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[0] == 255
                && (126..=129).contains(&pixel[1])
                && pixel[1] == pixel[2])
    );
    let text = pdf_text::extract_page(&doc, 0, pdf_text::TextFlags::TEXT)
        .expect("source text")
        .text();
    let untouched = contents(&doc, 1);
    let result = flatten(home.path(), source.to_str().expect("path"));
    assert_eq!(
        result["flattened"],
        json!(["annotations", "form_fields", "transparency"])
    );
    let path = result["outputs"][0].as_str().expect("output");
    let output = Document::open(path).expect("reopen");
    let page = output.page(0).expect("output page");
    for key in [b"MediaBox".as_slice(), b"CropBox", b"Rotate", b"UserUnit"] {
        assert_eq!(page.dict.get(key), first.dict.get(key));
    }
    assert_eq!(
        output
            .info()
            .expect("info")
            .expect("dictionary")
            .get_string(b"Title")
            .expect("title")
            .to_text(),
        "Preserved transparency"
    );
    let annots = output.resolve_key(&page.dict, b"Annots").expect("links");
    let link = output
        .resolve_dict(&annots.as_array().expect("array")[0])
        .expect("resolve")
        .expect("dictionary");
    assert_eq!(link.get_name(b"Subtype"), Some(b"Link".as_slice()));
    let action = output
        .resolve_dict(link.get(b"A").expect("action"))
        .expect("resolve")
        .expect("dictionary");
    assert_eq!(
        action.get_string(b"URI").expect("URI").to_text(),
        "https://example.com/kept"
    );
    assert!(!page.dict.contains_key(b"Group"));
    assert!(!page.resources.contains_key(b"ExtGState"));
    let images = output
        .resolve_key(&page.resources, b"XObject")
        .expect("images");
    for (_, object) in images.as_dict().expect("dictionary").iter() {
        let image = output
            .resolve_stream(object)
            .expect("resolve")
            .expect("image");
        assert_eq!(image.dict.get_name(b"Subtype"), Some(b"Image".as_slice()));
        assert_eq!(
            image.dict.get_name(b"ColorSpace"),
            Some(b"DeviceRGB".as_slice())
        );
        assert!(!image.dict.contains_key(b"SMask"));
        assert!(!image.dict.contains_key(b"Mask"));
    }
    let after = render_page(
        &output,
        &page,
        &RenderOptions {
            dpi: 144.0,
            alpha: false,
            annotations: false,
        },
    )
    .expect("render output");
    assert_eq!(
        (before.width(), before.height()),
        (after.width(), after.height())
    );
    let max_difference = before
        .data()
        .iter()
        .zip(after.data())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .expect("pixels");
    assert!(
        max_difference <= 1,
        "raster appearance changed by {max_difference} channel levels"
    );
    assert_eq!(
        pdf_text::extract_page(&output, 0, pdf_text::TextFlags::TEXT)
            .expect("output text")
            .text(),
        text
    );
    assert_eq!(contents(&output, 1), untouched);
    let second = flatten(home.path(), path);
    assert_eq!(second["flattened"], json!(["annotations", "form_fields"]));
}

#[test]
fn intrinsic_image_masks_are_flattened_without_inventing_text() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder
        .page(120.0, 80.0)
        .raw_content(b"q 100 0 0 40 10 20 cm /Masked Do Q\n");
    let mut doc = builder.build();
    let mut mask = Dict::new();
    mask.insert("Type", Object::name("XObject"));
    mask.insert("Subtype", Object::name("Image"));
    mask.insert("Width", 2_i64);
    mask.insert("Height", 1_i64);
    mask.insert("BitsPerComponent", 8_i64);
    mask.insert("ColorSpace", Object::name("DeviceGray"));
    let alpha = doc.add(pdf_core::Stream::new(mask.clone(), vec![128, 255]));
    mask.insert("ColorSpace", Object::name("DeviceRGB"));
    mask.insert("SMask", alpha);
    let image = doc.add(pdf_core::Stream::new(mask, vec![255, 0, 0, 0, 0, 255]));
    let mut page = doc.page(0).expect("page");
    let mut objects = Dict::new();
    objects.insert("Masked", image);
    page.resources.insert("XObject", objects);
    page.dict.insert("Resources", page.resources.clone());
    doc.set(page.id, page.dict.clone());
    let source = home.path().join("masked.pdf");
    doc.save(&source, &SaveOptions::default()).expect("save");
    let before = render_page(
        &doc,
        &page,
        &RenderOptions {
            dpi: 144.0,
            alpha: false,
            annotations: false,
        },
    )
    .expect("source render");
    let result = flatten(home.path(), source.to_str().expect("path"));
    assert_eq!(
        result["flattened"],
        json!(["annotations", "form_fields", "transparency"])
    );
    let output = Document::open(result["outputs"][0].as_str().expect("output")).expect("reopen");
    let page = output.page(0).expect("page");
    let after = render_page(
        &output,
        &page,
        &RenderOptions {
            dpi: 144.0,
            alpha: false,
            annotations: false,
        },
    )
    .expect("output render");
    assert_eq!(before.data(), after.data());
    let objects = output
        .resolve_key(&page.resources, b"XObject")
        .expect("objects");
    for (_, object) in objects.as_dict().expect("dictionary").iter() {
        let image = output
            .resolve_stream(object)
            .expect("resolve")
            .expect("image");
        assert!(!image.dict.contains_key(b"SMask"));
        assert!(!image.dict.contains_key(b"Mask"));
    }
    assert_eq!(
        pdf_text::extract_page(&output, 0, pdf_text::TextFlags::TEXT)
            .expect("text")
            .text(),
        ""
    );
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
