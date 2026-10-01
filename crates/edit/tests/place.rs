//! `edit add-text` and `edit add-image` draw into page content in the `search`
//! frame, keep the existing drawing and its graphics state apart, and report
//! the box `search` finds.

use std::ffi::OsString;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{Paint, PdfBuilder};
use pdf_codec::{JpegColor, PngColor, encode_jpeg, encode_png};
use pdf_core::{Dict, Document, Object, SaveOptions, Stream};
use pdf_font::Font;
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

const SNELL: &str = "/System/Library/Fonts/Supplemental/SnellRoundhand.ttc";
const STIX: &str = "/System/Library/Fonts/Supplemental/STIXGeneral.otf";

/// The font resource `GoatF1` of the first page.
fn goat_font(doc: &Document) -> Dict {
    let page = doc.page(0).expect("page");
    let fonts = doc.resolve_key(&page.resources, b"Font").expect("fonts");
    let font = fonts
        .as_dict()
        .expect("font resources")
        .get(b"GoatF1")
        .cloned()
        .expect("GoatF1");
    doc.resolve(&font)
        .expect("font")
        .as_dict()
        .cloned()
        .expect("font dictionary")
}

fn entry(doc: &Document, dict: &Dict, key: &[u8]) -> Object {
    doc.resolve(dict.get(key).expect("key present"))
        .expect("resolve")
}

/// Pixels darker than mid-gray inside `area` (points) of the first page at 72 dpi.
fn ink(doc: &Document, [x0, y0, x1, y1]: [u32; 4]) -> usize {
    let points: Vec<(u32, u32)> = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .collect();
    colors(doc, &points)
        .into_iter()
        .filter(|c| c.iter().all(|&v| v < 128))
        .count()
}

fn blank_pdf(dir: &Path, pages: usize) -> String {
    let mut builder = PdfBuilder::new();
    for _ in 0..pages {
        builder.page(300.0, 300.0);
    }
    save(dir, "form.pdf", &builder)
}

fn out_arg(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

#[test]
fn cff_fonts_embed_a_glyph_id_preserving_subset_that_reads_back_and_draws() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let result = run(
        dir,
        &[
            "edit", "add-text", &source, "--text", "Jane", "--at", "40,100", "--size", "40",
            "--font", STIX,
        ],
    )
    .expect("add text");
    let doc = output(&result);
    assert_eq!(page_text(&doc).trim(), "Jane");
    let Object::Array(descendants) = entry(&doc, &goat_font(&doc), b"DescendantFonts") else {
        panic!("descendant fonts");
    };
    let descendant = doc
        .resolve(&descendants[0])
        .expect("descendant")
        .as_dict()
        .cloned()
        .expect("descendant dictionary");
    assert_eq!(
        descendant.get_name(b"Subtype"),
        Some(b"CIDFontType0".as_slice())
    );
    let descriptor = entry(&doc, &descendant, b"FontDescriptor")
        .as_dict()
        .cloned()
        .expect("descriptor");
    let file = entry(&doc, &descriptor, b"FontFile3");
    let file = file.as_stream().expect("font file");
    assert_eq!(
        file.dict.get_name(b"Subtype"),
        Some(b"CIDFontType0C".as_slice())
    );
    let subset = Font::parse(doc.decode_stream(file).expect("decode").data).expect("subset parses");
    let original = Font::parse(std::fs::read(STIX).expect("read STIX")).expect("STIX");
    for ch in ['J', 'a', 'n', 'e', 'Q', 'z'] {
        let glyph = original.glyph_for_char(ch).expect("STIX glyph");
        let kept = subset.glyph_bounds(glyph).expect("subset bounds");
        if "Jane".contains(ch) {
            let full = original.glyph_bounds(glyph).expect("bounds");
            assert_eq!(kept, full, "{ch} keeps its outline");
        } else {
            assert_eq!(kept, None, "{ch} is dropped");
        }
    }
    assert!(ink(&doc, [40, 70, 130, 100]) > 100, "the subset draws");
}

#[test]
fn script_fonts_resolve_by_name_and_collection_faces_by_face() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let out = out_arg(dir, "out.pdf");
    for (font, face, name) in [
        ("Snell Roundhand", None, "SnellRoundhand"),
        ("Zapfino", None, "Zapfino"),
        (SNELL, Some("1"), "SnellRoundhand-Bold"),
        (SNELL, Some("snellroundhand-black"), "SnellRoundhand-Black"),
    ] {
        let mut args = vec![
            "edit", "add-text", &source, "--text", "Jane", "--at", "40,150", "--font", font, "-o",
            &out,
        ];
        if let Some(face) = face {
            args.extend(["--face", face]);
        }
        let result = run(dir, &args).expect("add text");
        assert_eq!(result["font"], name, "{font} {face:?}");
        assert_eq!(
            page_text(&output(&result)).trim(),
            "Jane",
            "{font} {face:?}"
        );
    }
    let error = run(
        dir,
        &[
            "edit",
            "add-text",
            &source,
            "--text",
            "Jane",
            "--at",
            "40,150",
            "--font",
            SNELL,
            "--face",
            "Snell Roundhand Oblique",
            "-o",
            &out,
        ],
    )
    .expect_err("no such face");
    assert!(
        error
            .to_string()
            .contains("0 SnellRoundhand, 1 SnellRoundhand-Bold"),
        "{error}"
    );
}

#[test]
fn embedded_fonts_are_kerned_by_their_pair_tables() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let path = "/System/Library/Fonts/Supplemental/Brush Script.ttf";
    let text = "ToWaAVTyYo";
    let result = run(
        dir,
        &[
            "edit", "add-text", &source, "--text", text, "--at", "10,150", "--size", "20",
            "--font", path,
        ],
    )
    .expect("add text");
    let font = Font::parse(std::fs::read(path).expect("read font")).expect("font");
    let em = f64::from(font.units_per_em());
    let unkerned: f64 = text
        .chars()
        .map(|ch| {
            let glyph = font.glyph_for_char(ch).expect("glyph");
            f64::from(font.advance(glyph).expect("advance")) / em * 20.0
        })
        .sum();
    let bbox = &result["bbox"];
    let width = bbox[2].as_f64().expect("x1") - bbox[0].as_f64().expect("x0");
    assert!(width < unkerned - 1.0, "{width} vs unkerned {unkerned}");
    assert_eq!(page_text(&output(&result)).trim(), text);
}

#[test]
fn lines_break_at_newlines_and_wrap_inside_width_with_alignment() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let result = run(
        dir,
        &[
            "edit",
            "add-text",
            &source,
            "--text",
            "alpha beta gamma\\ndelta",
            "--width",
            "80",
            "--align",
            "right",
            "--at",
            "100,100",
        ],
    )
    .expect("add text");
    let doc = output(&result);
    let text = page_text(&doc);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines, ["alpha beta", "gamma", "delta"]);
    // Right-aligned inside 80 pt from x 100; Helvetica 12 lines are 14.4 pt apart.
    let mut bottoms = Vec::new();
    for word in ["beta", "gamma", "delta"] {
        let [rect] = &search(&doc, word)[..] else {
            panic!("one {word}");
        };
        assert_eq!(rect[2], json!(180.0), "{word} ends at the right edge");
        bottoms.push(rect[3].as_f64().expect("y1"));
    }
    for pair in bottoms.windows(2) {
        assert!((pair[1] - pair[0] - 14.4).abs() < 0.11, "{bottoms:?}");
    }
}

#[test]
fn rotated_text_runs_counter_clockwise_from_the_point() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let result = run(
        dir,
        &[
            "edit", "add-text", &source, "--text", "needle", "--at", "100,200", "--size", "11",
            "--rotate", "90",
        ],
    )
    .expect("add text");
    // "needle" is 33 pt long at 11 pt; turned 90° it climbs from y 200 to y 167.
    let bbox = &result["bbox"];
    assert_eq!(
        (&bbox[1], &bbox[3]),
        (&json!(167.0), &json!(200.0)),
        "{bbox}"
    );
    assert!(bbox[2].as_f64().expect("x1") <= 100.0 + 3.4, "{bbox}");
}

#[test]
fn cmyk_color_and_opacity_blend_over_the_page() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    // ZapfDingbats "n" is a solid square: at 100 pt it covers (60..120, 85..145).
    let result = run(
        dir,
        &[
            "edit",
            "add-text",
            &source,
            "--text",
            "n",
            "--font",
            "ZapfDingbats",
            "--size",
            "100",
            "--at",
            "50,150",
            "--color",
            "0,1,1,0",
            "--opacity",
            "0.5",
        ],
    )
    .expect("add text");
    let [c] = colors(&output(&result), &[(88, 115)])[..] else {
        panic!("one sample");
    };
    assert!(
        c[0] > 200 && (90..190).contains(&c[1]) && (90..190).contains(&c[2]),
        "half-transparent red: {c:?}"
    );
}

#[test]
fn fit_draws_at_the_largest_size_the_box_holds() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let out = out_arg(dir, "out.pdf");
    let fit = |text: &str, rect: &str, extra: &[&str]| {
        let mut args = vec![
            "edit", "add-text", &source, "--text", text, "--fit", "--rect", rect, "-o", &out,
        ];
        args.extend(extra);
        run(dir, &args).expect("fit text")
    };
    // Width-bound: Helvetica "Jane Q. Member" is 7.447 em wide, so 200 pt holds 26.86 pt.
    let wide = fit("Jane Q. Member", "50,50,250,90", &[]);
    assert_eq!(wide["size"], json!(26.86));
    assert_eq!(
        (&wide["bbox"][0], &wide["bbox"][2]),
        (&json!(50.0), &json!(250.0))
    );
    // Height-bound: ascender to descender is 0.925 em, so 20 pt holds 21.62 pt.
    let tall = fit("Hi", "50,50,250,70", &[]);
    assert_eq!(tall["size"], json!(21.62));
    assert_eq!(tall["bbox"][0], json!(50.0));
    // An explicit --size caps the fit.
    let capped = fit("Hi", "50,50,250,70", &["--size", "10"]);
    assert_eq!(capped["size"], json!(10.0));
}

#[test]
fn pages_draws_on_each_listed_page_and_reports_each_box() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 3);
    let result = run(
        dir,
        &[
            "edit", "add-text", &source, "--text", "needle", "--at", "20,40", "--size", "11",
            "--pages", "3,1",
        ],
    )
    .expect("add text");
    let bbox = json!([20.0, 28.2, 53.0, 43.3]);
    assert_eq!(
        result["placements"],
        json!([{"page": 3, "bbox": bbox}, {"page": 1, "bbox": bbox}])
    );
    assert_eq!(result["page"], 3);
    let doc = output(&result);
    let texts: Vec<String> = (0..3)
        .map(|index| {
            pdf_text::extract_page(&doc, index, TextFlags::TEXT)
                .expect("extract")
                .text()
                .trim()
                .to_owned()
        })
        .collect();
    assert_eq!(texts, ["needle", "", "needle"]);
}

#[test]
fn image_turns_counter_clockwise_fades_and_stretches_in_the_rect() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    let out = out_arg(dir, "out.pdf");
    let jpeg = dir.join("photo.jpg");
    std::fs::write(&jpeg, two_tone_jpeg(None)).expect("write jpeg");
    let png = dir.join("signature.png");
    std::fs::write(&png, two_tone_png(false)).expect("write png");
    let place = |image: &Path, extra: &[&str]| {
        let image = image.to_string_lossy();
        let mut args = vec!["edit", "add-image", &source, "--image", &image, "-o", &out];
        args.extend(extra);
        run(dir, &args).expect("add image")
    };
    // Red left, blue right; a quarter turn counter-clockwise puts blue on top.
    let turned = place(&jpeg, &["--rect", "100,100,200,200", "--rotate", "90"]);
    assert_eq!(turned["bbox"], json!([125.0, 100.0, 175.0, 200.0]));
    let [top, bottom] = colors(&output(&turned), &[(150, 120), (150, 180)])[..] else {
        panic!("two samples");
    };
    assert!(blue(top), "{top:?}");
    assert!(red(bottom), "{bottom:?}");
    let stretched = place(&png, &["--rect", "50,50,150,150", "--stretch"]);
    assert_eq!(stretched["bbox"], json!([50.0, 50.0, 150.0, 150.0]));
    let [corner] = colors(&output(&stretched), &[(55, 55)])[..] else {
        panic!("one sample");
    };
    assert!(black(corner), "{corner:?}");
    let faded = place(&png, &["--rect", "50,50,150,150", "--opacity", "0.5"]);
    let [middle] = colors(&output(&faded), &[(100, 100)])[..] else {
        panic!("one sample");
    };
    assert!(middle.iter().all(|v| (100..160).contains(v)), "{middle:?}");
}

#[test]
fn adobe_cmyk_jpeg_gets_an_inverting_decode_and_plain_cmyk_none() {
    let work = tempfile::tempdir().expect("temp dir");
    let dir = work.path();
    let source = blank_pdf(dir, 1);
    // Full cyan ink; the encoder writes Adobe's inverted CMYK with an APP14 marker.
    let adobe = encode_jpeg(
        &[255, 0, 0, 0].repeat(16 * 16),
        16,
        16,
        JpegColor::Cmyk,
        95,
        None,
    )
    .expect("jpeg");
    let at = adobe
        .windows(2)
        .position(|w| w == [0xFF, 0xEE])
        .expect("APP14");
    let len = usize::from(u16::from_be_bytes([adobe[at + 2], adobe[at + 3]]));
    let plain = [&adobe[..at], &adobe[at + 2 + len..]].concat();
    for (name, jpeg, inverted) in [("adobe.jpg", adobe, true), ("plain.jpg", plain, false)] {
        let image = dir.join(name);
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
                "50,50,150,150",
                "-o",
                &out_arg(dir, "out.pdf"),
            ],
        )
        .expect("add image");
        let doc = output(&result);
        let page = doc.page(0).expect("page");
        let xobjects = doc
            .resolve_key(&page.resources, b"XObject")
            .expect("xobjects");
        let image = entry(&doc, xobjects.as_dict().expect("dict"), b"GoatIm1");
        let decode = image
            .as_stream()
            .expect("image stream")
            .dict
            .get(b"Decode")
            .is_some();
        assert_eq!(decode, inverted, "{name}");
        if inverted {
            let [c] = colors(&doc, &[(100, 100)])[..] else {
                panic!("one sample");
            };
            assert!(c[0] < 80 && c[1] > 150 && c[2] > 150, "cyan: {c:?}");
        }
    }
}
