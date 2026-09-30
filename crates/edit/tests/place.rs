//! `edit add-text` and `edit add-image` draw into page content in the `search`
//! frame, keep the existing drawing and its graphics state apart, and report
//! the box `search` finds.

use std::ffi::OsString;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{Paint, PdfBuilder};
use pdf_codec::{JpegColor, PngColor, encode_jpeg, encode_png};
use pdf_core::{Dict, Document, Object, SaveOptions, Stream};
use pdf_text::{Block, TextFlags};
use serde_json::{Map, Value, json};

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

fn save(dir: &Path, name: &str, builder: &PdfBuilder) -> String {
    builder.save(dir.join(name)).to_string_lossy().into_owned()
}

fn output(result: &Map<String, Value>) -> Document {
    Document::open(result["outputs"][0].as_str().expect("one output")).expect("open output")
}

fn rounded(values: [f64; 4]) -> Value {
    json!(values.map(|v| format!("{v:.1}").parse::<f64>().expect("a number")))
}

/// Rectangles `search` reports for `query` on the first page.
fn search(doc: &Document, query: &str) -> Vec<Value> {
    let words = pdf_text::page_words(doc, 0).expect("page words");
    pdf_text::page_hits(&words, &pdf_text::hit_pattern(query).expect("pattern"))
        .expect("hits")
        .into_iter()
        .map(|r| rounded([r.x0, r.y0, r.x1, r.y1]))
        .collect()
}

fn page_text(doc: &Document) -> String {
    pdf_text::extract_page(doc, 0, TextFlags::TEXT)
        .expect("extract")
        .text()
}

/// Opaque RGB of the first page rendered at 72 dpi, one per point.
fn colors(doc: &Document, points: &[(u32, u32)]) -> Vec<[u8; 3]> {
    let page = doc.page(0).expect("page");
    let pixmap = pdf_render::render_page(
        doc,
        &page,
        &pdf_render::RenderOptions {
            dpi: 72.0,
            alpha: false,
            annotations: false,
        },
    )
    .expect("render");
    points
        .iter()
        .map(|&(x, y)| {
            let [r, g, b, _] = pixmap.pixel(x, y).expect("inside the page");
            [r, g, b]
        })
        .collect()
}

fn red(c: [u8; 3]) -> bool {
    c[0] > 190 && c[1] < 70 && c[2] < 70
}
fn blue(c: [u8; 3]) -> bool {
    c[2] > 190 && c[0] < 70 && c[1] < 70
}
fn black(c: [u8; 3]) -> bool {
    c.iter().all(|&v| v < 60)
}

#[test]
fn text_box_is_the_search_rectangle_on_an_offset_rotated_crop() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder
        .page(400.0, 400.0)
        .set_box("CropBox", [20.0, 30.0, 380.0, 350.0])
        .rotate(90);
    let source = save(dir, "form.pdf", &builder);
    let result = run(
        dir,
        &[
            "edit", "add-text", &source, "--text", "needle", "--at", "20,40", "--size", "11",
        ],
    )
    .expect("add text");
    // `search` reports this rectangle for Helvetica 11 "needle" with its
    // baseline at (20, 40) of this crop box (see the text crate's tests).
    let expected = json!([20.0, 28.2, 53.0, 43.3]);
    assert_eq!(result["bbox"], expected);
    assert_eq!(result["page"], 1);
    assert_eq!(search(&output(&result), "needle"), vec![expected]);
}

#[test]
fn installed_and_file_fonts_are_embedded_and_read_back() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder.page(300.0, 300.0);
    let source = save(dir, "form.pdf", &builder);
    for (font, name) in [
        ("Helvetica-Light", "Helvetica-Light"),
        ("/System/Library/Fonts/Helvetica.ttc", "Helvetica"),
    ] {
        let result = run(
            dir,
            &[
                "edit",
                "add-text",
                &source,
                "--text",
                "Jane Q. Member",
                "--at",
                "40,100",
                "--font",
                font,
                "-o",
                &dir.join("out.pdf").to_string_lossy(),
            ],
        )
        .expect("add text");
        assert_eq!(result["font"], name, "{font}");
        let doc = output(&result);
        assert_eq!(page_text(&doc).trim(), "Jane Q. Member", "{font}");
        let page = doc.page(0).expect("page");
        let fonts = doc.resolve_key(&page.resources, b"Font").expect("fonts");
        let dict = doc
            .resolve(
                fonts
                    .as_dict()
                    .expect("font dict")
                    .get(b"GoatF1")
                    .expect("GoatF1"),
            )
            .expect("font");
        let dict = dict.as_dict().expect("font dictionary");
        assert_eq!(
            dict.get_name(b"Subtype"),
            Some(b"Type0".as_slice()),
            "{font}"
        );
    }
}

#[test]
fn zapf_dingbats_draws_check_marks_from_unicode_and_ascii_codes() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder.page(200.0, 200.0);
    let source = save(dir, "form.pdf", &builder);
    let result = run(
        dir,
        &[
            "edit",
            "add-text",
            &source,
            "--text",
            "✔4✗",
            "--at",
            "40,100",
            "--font",
            "ZapfDingbats",
        ],
    )
    .expect("add check marks");
    assert_eq!(page_text(&output(&result)).trim(), "✔✔✗");
}

#[test]
fn text_the_font_cannot_encode_names_each_character_and_writes_nothing() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder.page(200.0, 200.0);
    let source = save(dir, "form.pdf", &builder);
    let out = dir.join("out.pdf");
    let error = run(
        dir,
        &[
            "edit",
            "add-text",
            &source,
            "--text",
            "ok ✔ ✗ ✔",
            "--at",
            "40,100",
            "-o",
            &out.to_string_lossy(),
        ],
    )
    .expect_err("Helvetica has no check marks");
    let message = error.to_string();
    assert!(
        message.contains("Helvetica cannot encode ✔ (U+2714), ✗ (U+2717);"),
        "{message}"
    );
    assert!(!out.exists());
}

#[test]
fn leaked_page_state_and_inherited_resources_do_not_reach_the_new_text() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder.page(300.0, 300.0).text(40.0, 200.0, "keep");
    let mut doc = builder.build();
    let page = doc.page(0).expect("page");
    let mut dict = page.dict.clone();
    let resources = dict.remove(b"Resources").expect("page resources");
    let parent = dict.get_ref(b"Parent").expect("parent");
    let mut pages = doc
        .get(parent)
        .expect("pages")
        .as_dict()
        .cloned()
        .expect("pages dict");
    pages.insert("Resources", resources);
    doc.set(parent, pages);
    // Two q left open with a scale between them: popping one q is not enough.
    let leak = doc.add(Stream::new(
        Dict::new(),
        b"q 2 0 0 2 0 0 cm q 0 0 1 rg".to_vec(),
    ));
    let original = dict.get(b"Contents").cloned().expect("contents");
    dict.insert("Contents", vec![original, Object::Reference(leak)]);
    doc.set(page.id, dict);
    let source = dir.join("form.pdf");
    doc.save(&source, &SaveOptions::default()).expect("save");
    let result = run(
        dir,
        &[
            "edit",
            "add-text",
            &source.to_string_lossy(),
            "--text",
            "needle",
            "--at",
            "20,40",
            "--size",
            "11",
        ],
    )
    .expect("add text");
    assert_eq!(result["bbox"], json!([20.0, 28.2, 53.0, 43.3]));
    let doc = output(&result);
    let text = page_text(&doc);
    assert!(text.contains("keep") && text.contains("needle"), "{text}");
}

fn two_tone_png(transparent_left: bool) -> Vec<u8> {
    let mut rgba = Vec::new();
    for _ in 0..10 {
        for x in 0..20 {
            let alpha = if transparent_left && x < 10 { 0 } else { 255 };
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    encode_png(&rgba, 20, 10, PngColor::Rgba, None).expect("png")
}

#[test]
fn png_transparency_leaves_the_page_visible() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder
        .page(200.0, 200.0)
        .rect([0.0, 0.0, 200.0, 200.0], Paint::fill([1.0, 0.0, 0.0]));
    let source = save(dir, "form.pdf", &builder);
    let image = dir.join("signature.png");
    std::fs::write(&image, two_tone_png(true)).expect("write png");
    let result = run(
        dir,
        &[
            "edit",
            "add-image",
            &source,
            "--image",
            &image.to_string_lossy(),
            "--rect",
            "50,50,150,100",
        ],
    )
    .expect("add image");
    let [clear, ink] = colors(&output(&result), &[(75, 75), (125, 75)])[..] else {
        panic!("two samples");
    };
    assert!(red(clear), "{clear:?}");
    assert!(black(ink), "{ink:?}");
}

#[test]
fn image_keeps_its_aspect_ratio_centred_in_the_rect() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder
        .page(400.0, 400.0)
        .set_box("CropBox", [20.0, 30.0, 380.0, 350.0])
        .rotate(90);
    let source = save(dir, "form.pdf", &builder);
    let image = dir.join("signature.png");
    std::fs::write(&image, two_tone_png(false)).expect("write png");
    let result = run(
        dir,
        &[
            "edit",
            "add-image",
            &source,
            "--image",
            &image.to_string_lossy(),
            "--rect",
            "50,50,150,150",
        ],
    )
    .expect("add image");
    let expected = json!([50.0, 75.0, 150.0, 125.0]);
    assert_eq!(result["bbox"], expected);
    let page = pdf_text::extract_page(&output(&result), 0, TextFlags::DICT).expect("extract");
    let drawn: Vec<Value> = page
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Image(image) => Some(rounded([
                image.bbox.x0,
                image.bbox.y0,
                image.bbox.x1,
                image.bbox.y1,
            ])),
            Block::Text(_) => None,
        })
        .collect();
    assert_eq!(drawn, vec![expected]);
}

/// A 20x10 JPEG, red on the left and blue on the right, with an EXIF
/// orientation tag when `orientation` is given.
fn two_tone_jpeg(orientation: Option<u8>) -> Vec<u8> {
    let mut rgb = Vec::new();
    for _ in 0..10 {
        for x in 0..20 {
            rgb.extend_from_slice(if x < 10 { &[255, 0, 0] } else { &[0, 0, 255] });
        }
    }
    let jpeg = encode_jpeg(&rgb, 20, 10, JpegColor::Rgb, 95, None).expect("jpeg");
    let Some(orientation) = orientation else {
        return jpeg;
    };
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x22]);
    out.extend_from_slice(b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0");
    out.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn jpeg_is_embedded_as_the_original_dct_bytes() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder.page(300.0, 300.0);
    let source = save(dir, "form.pdf", &builder);
    let jpeg = two_tone_jpeg(None);
    let image = dir.join("signature.jpg");
    std::fs::write(&image, &jpeg).expect("write jpeg");
    let result = run(
        dir,
        &[
            "edit",
            "add-image",
            &source,
            "--image",
            &image.to_string_lossy(),
            "--rect",
            "100,100,200,200",
        ],
    )
    .expect("add image");
    let doc = output(&result);
    let page = doc.page(0).expect("page");
    let xobjects = doc
        .resolve_key(&page.resources, b"XObject")
        .expect("xobjects");
    let stream = doc
        .resolve(
            xobjects
                .as_dict()
                .expect("dict")
                .get(b"GoatIm1")
                .expect("GoatIm1"),
        )
        .expect("image");
    let stream = stream.as_stream().expect("image stream");
    assert_eq!(
        stream.dict.get_name(b"Filter"),
        Some(b"DCTDecode".as_slice())
    );
    assert_eq!(stream.raw(), jpeg.as_slice());
}

#[test]
fn jpeg_exif_orientation_is_drawn_upright() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let mut builder = PdfBuilder::new();
    builder.page(300.0, 300.0);
    let source = save(dir, "form.pdf", &builder);
    let image = dir.join("photo.jpg");
    // Orientation 6: the stored left edge is the displayed top.
    std::fs::write(&image, two_tone_jpeg(Some(6))).expect("write jpeg");
    let result = run(
        dir,
        &[
            "edit",
            "add-image",
            &source,
            "--image",
            &image.to_string_lossy(),
            "--rect",
            "100,100,200,200",
        ],
    )
    .expect("add image");
    assert_eq!(result["bbox"], json!([125.0, 100.0, 175.0, 200.0]));
    let [top, bottom] = colors(&output(&result), &[(150, 120), (150, 180)])[..] else {
        panic!("two samples");
    };
    assert!(red(top), "{top:?}");
    assert!(blue(bottom), "{bottom:?}");
}
