//! Composition contracts observed through the registered commands and reopened output PDFs.
use std::ffi::OsString;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use pdf_codec::{JpegColor, PngColor, encode_jpeg, encode_png};
use pdf_core::{Document, Object, Rect, Stream};
use serde_json::{Map, Value};

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_compose::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["from-images", "from-html", "from-md"],
        &[],
    );
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    cli.parse(&args)
        .unwrap_or_else(|failure| panic!("{failure:?}"))
        .run(&Ctx::new(home))
}

fn image(doc: &Document, page: usize) -> Stream {
    let page = doc.page(page).unwrap_or_else(|e| panic!("{e}"));
    let objects = doc
        .resolve_dict(page.resources.get(b"XObject").unwrap_or(&Object::Null))
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_default();
    doc.resolve_stream(objects.get(b"Im0").unwrap_or(&Object::Null))
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("image is missing"))
}

#[test]
fn images_keep_input_order_and_use_each_images_resolution() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let png = home.path().join("wide.png");
    let jpg = home.path().join("tall.jpg");
    let out = home.path().join("out.pdf");
    let first = encode_png(
        &vec![40; 120 * 80 * 3],
        120,
        80,
        PngColor::Rgb,
        Some((150.0, 100.0)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let second = encode_jpeg(
        &vec![90; 60 * 90 * 3],
        60,
        90,
        JpegColor::Rgb,
        85,
        Some((300.0, 150.0)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&png, first).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&jpg, &second).unwrap_or_else(|e| panic!("{e}"));
    let result = run(
        home.path(),
        &[
            "from-images",
            &png.to_string_lossy(),
            &jpg.to_string_lossy(),
            "-o",
            &out.to_string_lossy(),
        ],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let doc = Document::open(out).unwrap_or_else(|e| panic!("{e}"));
    let sizes: Vec<Rect> = doc
        .pages()
        .unwrap_or_else(|e| panic!("{e}"))
        .iter()
        .map(|p| p.media_box())
        .collect();
    assert_eq!(
        sizes,
        vec![
            Rect::new(0.0, 0.0, 57.6, 57.6),
            Rect::new(0.0, 0.0, 14.4, 43.2)
        ]
    );
    assert_eq!(result["image_count"], 2);
    assert_eq!(
        image(&doc, 1).raw(),
        second.as_slice(),
        "JPEG must not be recompressed"
    );
}

#[test]
fn alpha_images_are_flattened_on_white_and_lose_the_source_dpi() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let input = home.path().join("alpha.png");
    let out = home.path().join("out.pdf");
    let png = encode_png(
        &[255, 0, 0, 128, 0, 0, 255, 0],
        2,
        1,
        PngColor::Rgba,
        Some((300.0, 300.0)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&input, png).unwrap_or_else(|e| panic!("{e}"));
    run(
        home.path(),
        &[
            "from-images",
            &input.to_string_lossy(),
            "-o",
            &out.to_string_lossy(),
        ],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let doc = Document::open(out).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        doc.page(0).unwrap_or_else(|e| panic!("{e}")).media_box(),
        Rect::new(0.0, 0.0, 1.5, 0.75)
    );
    let decoded = doc
        .decode_stream(&image(&doc, 0))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(decoded.data, [255, 127, 127, 255, 255, 255]);
}

#[test]
fn invalid_later_image_does_not_destroy_an_existing_destination() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let input = home.path().join("first.png");
    let invalid = home.path().join("second.png");
    let out = home.path().join("out.pdf");
    let png =
        encode_png(&[10, 20, 30], 1, 1, PngColor::Rgb, None).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&input, png).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&invalid, b"not an image").unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&out, b"preserve this output").unwrap_or_else(|e| panic!("{e}"));
    assert!(
        run(
            home.path(),
            &[
                "from-images",
                &input.to_string_lossy(),
                &invalid.to_string_lossy(),
                "-o",
                &out.to_string_lossy()
            ]
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(out).unwrap_or_else(|e| panic!("{e}")),
        b"preserve this output"
    );
}

#[test]
fn forced_page_breaks_and_custom_page_size_survive_pdf_reopening() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let input = home.path().join("pages.html");
    let out = home.path().join("pages.pdf");
    std::fs::write(&input, "<style>@page {size: 200pt 300pt; margin: 20pt} h1 {page-break-before:always} p {margin:0}</style><h1>First</h1><p>Opening.</p><h1>Second</h1><p>Middle.</p><h1>Third</h1><p>End.</p>").unwrap_or_else(|e| panic!("{e}"));
    run(
        home.path(),
        &[
            "from-html",
            &input.to_string_lossy(),
            "-o",
            &out.to_string_lossy(),
        ],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let doc = Document::open(out).unwrap_or_else(|e| panic!("{e}"));
    let pages = doc.pages().unwrap_or_else(|e| panic!("{e}"));
    let sizes: Vec<(i64, i64)> = pages
        .iter()
        .map(|page| {
            let size = page.media_box();
            (
                (size.width() * 1000.0).round() as i64,
                (size.height() * 1000.0).round() as i64,
            )
        })
        .collect();
    assert_eq!(
        sizes,
        vec![(200_000, 300_000); 3],
        "page dimensions in thousandths of a point"
    );
    let tops: Vec<_> = (0..3)
        .map(|index| {
            let text = pdf_text::extract_page(&doc, index, pdf_text::TextFlags::WORDS)
                .unwrap_or_else(|e| panic!("{e}"));
            text.words()
                .first()
                .map(|word| word.rect.y0)
                .unwrap_or_else(|| panic!("empty page {index}"))
        })
        .collect();
    assert!(
        tops.iter().all(|top| (top - tops[0]).abs() < 0.1),
        "forced breaks retain the following heading's top margin: {tops:?}"
    );
}

#[test]
fn percent_encoded_image_bytes_survive_html_embedding() {
    use std::fmt::Write;
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let input = home.path().join("image.html");
    let output = home.path().join("image.pdf");
    let pixels = [255, 0, 0, 0, 128, 255];
    let png = encode_png(&pixels, 2, 1, PngColor::Rgb, None).unwrap_or_else(|e| panic!("{e}"));
    let mut uri = String::from("data:image/png,");
    for byte in png {
        write!(&mut uri, "%{byte:02X}").unwrap_or_else(|e| panic!("{e}"));
    }
    std::fs::write(&input, format!("<img src=\"{uri}\">")).unwrap_or_else(|e| panic!("{e}"));
    run(
        home.path(),
        &[
            "from-html",
            &input.to_string_lossy(),
            "-o",
            &output.to_string_lossy(),
        ],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let doc = Document::open(output).unwrap_or_else(|e| panic!("{e}"));
    let decoded = doc
        .decode_stream(&image(&doc, 0))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(decoded.data, pixels);
}

fn html_document(home: &Path, html: &str) -> Document {
    let input = home.join("document.html");
    let output = home.join("document.pdf");
    std::fs::write(&input, html).unwrap_or_else(|e| panic!("{e}"));
    run(
        home,
        &[
            "from-html",
            &input.to_string_lossy(),
            "-o",
            &output.to_string_lossy(),
        ],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    Document::open(output).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn a_row_taller_than_a_page_keeps_every_paragraph_and_repeats_the_header() {
    use std::fmt::Write;
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut html = String::from(
        "<style>@page{size:220pt 200pt;margin:15pt} body{font:12px Arial;margin:0} table{width:100%;border-collapse:collapse} td,th{padding:4px;border:1px solid black} p{margin:5px 0}</style><table><thead><tr><th>Repeated heading</th></tr></thead><tbody><tr><td>",
    );
    for n in 0..30 {
        write!(
            html,
            "<p>Entry{n:02} must survive across the page boundary.</p>"
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }
    html.push_str("</td></tr></tbody></table><p>After table.</p>");
    let doc = html_document(home.path(), &html);
    let pages = doc.pages().unwrap_or_else(|e| panic!("{e}"));
    assert!(
        pages.len() >= 3,
        "the row must actually cross multiple page boundaries"
    );
    let mut all = String::new();
    for index in 0..pages.len() {
        let page = pdf_text::extract_page(&doc, index, pdf_text::TextFlags::TEXT)
            .unwrap_or_else(|e| panic!("{e}"));
        let text = page.text().split_whitespace().collect::<Vec<_>>().join(" ");
        if text.contains("Entry") {
            assert_eq!(
                text.matches("Repeated heading").count(),
                1,
                "page {index} has its header once"
            );
        }
        all.push_str(&text);
    }
    for n in 0..30 {
        assert_eq!(
            all.matches(&format!("Entry{n:02}")).count(),
            1,
            "paragraph {n} is neither clipped nor duplicated"
        );
    }
    assert_eq!(all.matches("After table.").count(), 1);
}

#[test]
fn flex_measurement_uses_content_width_before_placing_the_following_block() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let doc = html_document(
        home.path(),
        "<style>body{font:14px Arial}.row{display:flex;width:420px;gap:12px}.row>div{padding:10px;border:1px solid black}.first{flex:1}.second{flex:2}</style><div class=row><div class=first>First flexible card wraps its own words. Terminal</div><div class=second>Second flexible card has twice the growing space.</div></div><div>Following</div>",
    );
    let page = pdf_text::extract_page(&doc, 0, pdf_text::TextFlags::WORDS)
        .unwrap_or_else(|e| panic!("{e}"));
    let words = page.words();
    let word = |text| {
        words
            .iter()
            .find(|word| word.text == text)
            .unwrap_or_else(|| panic!("missing word {text}"))
    };
    let first = word("First");
    let second = word("Second");
    let terminal = word("Terminal");
    let following = word("Following");
    assert!(
        (first.rect.y0 - second.rect.y0).abs() < 0.1,
        "flex items start on the same line"
    );
    assert!(
        second.rect.x0 > first.rect.x1,
        "the second card is beside the first"
    );
    assert!(
        terminal.rect.y0 > first.rect.y1,
        "the narrow card really wraps"
    );
    assert!(
        following.rect.y0 >= terminal.rect.y1,
        "a wrapped line cannot escape into the following block"
    );
}

#[test]
fn fixed_content_repeats_without_moving_absolute_content_out_of_its_container() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let doc = html_document(
        home.path(),
        "<style>@page{size:300pt 300pt;margin:20pt}body{font:12px Arial;margin:0}p{margin:0}</style><div style='position:relative;width:200px;height:100px'><div style='position:absolute;right:10px;top:12px;width:50px'>Corner</div><p>Origin <span style='position:relative;left:8px;top:4px'>Shifted</span></p></div><div style='position:fixed;right:0;bottom:0'>Footer</div><p style='break-before:page'>Next page.</p>",
    );
    assert_eq!(doc.pages().unwrap_or_else(|e| panic!("{e}")).len(), 2);
    let pages: Vec<_> = (0..2)
        .map(|index| {
            pdf_text::extract_page(&doc, index, pdf_text::TextFlags::WORDS)
                .unwrap_or_else(|e| panic!("{e}"))
        })
        .collect();
    let words: Vec<_> = pages.iter().map(|page| page.words()).collect();
    let word = |page: usize, text| {
        words[page]
            .iter()
            .find(|word| word.text == text)
            .unwrap_or_else(|| panic!("missing {text} on page {page}"))
    };
    let origin = word(0, "Origin");
    let corner = word(0, "Corner");
    assert!(
        (corner.rect.x0 - origin.rect.x0 - 105.0).abs() < 0.1,
        "right inset uses the 200px containing block"
    );
    assert!((corner.rect.y0 - origin.rect.y0 - 9.0).abs() < 0.1);
    assert!(
        (word(0, "Shifted").rect.y0 - origin.rect.y0 - 3.0).abs() < 0.1,
        "relative inline content moves without changing the line box"
    );
    let first_footer = word(0, "Footer");
    let second_footer = word(1, "Footer");
    assert_eq!(
        first_footer.rect, second_footer.rect,
        "fixed text remains at the same page coordinates"
    );
    assert_eq!(pages[0].text().matches("Footer").count(), 1);
    assert_eq!(pages[1].text().matches("Footer").count(), 1);
    assert!(
        !pages[1].text().contains("Corner"),
        "absolute content is not fixed content"
    );
}

#[test]
fn horizontal_kerning_keeps_one_text_matrix_per_run() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let sentence = "Careful research depends on clear evidence and accurate records. ";
    let html = format!(
        "<style>body{{font:16px 'Helvetica Neue';line-height:1.5}}</style><p>{}</p>",
        sentence.repeat(30)
    );
    let doc = html_document(home.path(), &html);
    let (mut matrices, mut runs) = (0, 0);
    let mut text = String::new();
    for (index, page) in doc
        .pages()
        .unwrap_or_else(|e| panic!("{e}"))
        .iter()
        .enumerate()
    {
        let content = doc.page_content(page).unwrap_or_else(|e| panic!("{e}"));
        let operations = pdf_core::parse_content(&content).unwrap_or_else(|e| panic!("{e}"));
        matrices += operations.iter().filter(|op| op.operator == b"Tm").count();
        runs += operations.iter().filter(|op| op.operator == b"BT").count();
        text.push_str(
            &pdf_text::extract_page(&doc, index, pdf_text::TextFlags::TEXT)
                .unwrap_or_else(|e| panic!("{e}"))
                .text(),
        );
    }
    assert_eq!(
        text.split_whitespace().collect::<Vec<_>>().join(" "),
        sentence.repeat(30).trim()
    );
    assert_eq!(
        matrices, runs,
        "horizontal kerning must use text advances, not a matrix reset for each glyph"
    );
}

#[test]
fn svg_labels_remain_searchable_at_their_scaled_positions_without_hidden_text() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100" viewBox="0 0 200 100" font-family="Arial" font-size="16">
        <defs>
            <clipPath id="corner"><rect width="3" height="3"/></clipPath>
            <mask id="black"><rect width="200" height="100" fill="black"/></mask>
            <text>Definition</text>
        </defs>
        <g transform="translate(10 5) scale(1.5)">
            <text x="10" y="20">Visible</text><text x="10" y="42">Another</text>
        </g>
        <text x="10" y="80" visibility="hidden">Hidden</text>
        <text x="10" y="80" display="none">Removed</text>
        <g opacity="0"><text x="10" y="80">Transparent</text></g>
        <text x="10" y="80" fill="none">Unpainted</text>
        <text x="10" y="80" fill-opacity="0">Faded</text>
        <g clip-path="url(#corner)"><text x="10" y="80">Clipped</text></g>
        <g mask="url(#black)"><text x="10" y="80">Masked</text></g>
    </svg>"#;
    std::fs::write(home.path().join("labels.svg"), svg).unwrap_or_else(|e| panic!("{e}"));
    let html = format!(
        "<style>@page{{size:400pt 400pt;margin:0}}body{{margin:0}}img,svg{{display:block}}svg{{width:400px;height:200px}}</style><img src='labels.svg' width='200' height='100'>{svg}"
    );
    let doc = html_document(home.path(), &html);
    let page = pdf_text::extract_page(&doc, 0, pdf_text::TextFlags::WORDS)
        .unwrap_or_else(|e| panic!("{e}"));
    let words = page.words();
    assert_eq!(
        words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        ["Visible", "Another", "Visible", "Another"]
    );
    for (first, second) in words[..2].iter().zip(&words[2..]) {
        assert!((second.rect.x0 - first.rect.x0 * 2.0).abs() < 0.02);
        assert!((second.rect.x1 - first.rect.x1 * 2.0).abs() < 0.02);
        assert!((second.rect.y0 - first.rect.y0 * 2.0 - 75.0).abs() < 0.02);
        assert!((second.rect.y1 - first.rect.y1 * 2.0 - 75.0).abs() < 0.02);
    }
    assert!(
        (words[0].rect.x0 - 18.75).abs() < 0.02,
        "SVG translation and scale determine the label origin"
    );
    assert!(
        (words[1].rect.y0 - words[0].rect.y0 - 24.75).abs() < 0.02,
        "SVG baselines determine label separation"
    );
}
