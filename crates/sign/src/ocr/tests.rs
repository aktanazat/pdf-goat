//! OCR's deterministic post-recognition boundary: real PDF fonts, content and reader.
#![cfg(target_os = "macos")]

use super::*;
use goat_fixtures::PdfBuilder;
use pdf_core::SaveOptions;

fn recognized(text: &str) -> Vec<pdf_ocr::Line> {
    let point = |x, y| pdf_ocr::Point { x, y };
    let quad = pdf_ocr::Quad {
        top_left: point(200.0, 300.0),
        top_right: point(800.0, 300.0),
        bottom_right: point(800.0, 380.0),
        bottom_left: point(200.0, 380.0),
    };
    vec![pdf_ocr::Line {
        text: text.to_owned(),
        confidence: 1.0,
        quad,
        words: vec![pdf_ocr::Word {
            text: text.to_owned(),
            quad: Some(quad),
        }],
    }]
}

fn reopen(doc: &Document) -> Document {
    Document::load(
        doc.save_to_bytes(&SaveOptions {
            garbage_collect: true,
            ..Default::default()
        })
        .expect("save OCR result"),
    )
    .expect("reopen OCR result")
}

#[test]
fn forced_ocr_replaces_existing_text_instead_of_keeping_two_layers() {
    let mut fixture = PdfBuilder::new();
    fixture.page(300.0, 400.0).text(30.0, 200.0, "OBSOLETE");
    let mut doc = fixture.build();
    let page = doc.page(0).expect("page");
    let raster = render_page(
        &doc,
        &page,
        &RenderOptions {
            dpi: DPI,
            alpha: false,
            annotations: false,
        },
    )
    .expect("raster");
    let gray = raster.to_gray8(255);
    let scan = Scan {
        pixmap: &raster,
        gray: &gray,
        dpi: DPI,
    };
    apply_page(
        &mut doc,
        &page,
        &scan,
        &recognized("replacement"),
        true,
        &mut font::Fonts::default(),
    )
    .expect("apply OCR");
    let doc = reopen(&doc);
    let (text, _) = pdf_text::page_text_and_words(&doc, 0).expect("extract text");
    assert_eq!(text.trim(), "replacement");
}

#[test]
fn unicode_ocr_stays_over_rotated_cropped_userunit_pages() {
    for rotation in [0, 90, 180, 270] {
        let mut fixture = PdfBuilder::new();
        fixture
            .page(420.0, 540.0)
            .rotate(rotation)
            .set_box("CropBox", [30.0, 50.0, 330.0, 450.0]);
        let mut doc = fixture.build();
        let mut page = doc.page(0).expect("page");
        page.dict.insert("UserUnit", 2);
        doc.set(page.id, page.dict);
        let page = doc.page(0).expect("updated page");
        let bounds = pdf_interp::page_bounds(&page);
        let raster = Pixmap::new(
            (bounds.width() * DPI / 72.0).ceil() as u32,
            (bounds.height() * DPI / 72.0).ceil() as u32,
        )
        .expect("bitmap");
        let gray = raster.to_gray8(255);
        let scan = Scan {
            pixmap: &raster,
            gray: &gray,
            dpi: DPI,
        };
        apply_page(
            &mut doc,
            &page,
            &scan,
            &recognized("caféΩЖ"),
            false,
            &mut font::Fonts::default(),
        )
        .expect("apply Unicode OCR");
        let doc = reopen(&doc);
        let (text, words) = pdf_text::page_text_and_words(&doc, 0).expect("extract text");
        assert_eq!(text.trim(), "caféΩЖ", "rotation {rotation}");
        let word = words
            .iter()
            .find(|word| word.text == "caféΩЖ")
            .expect("recognized word");
        // Text extraction clears /Rotate; map its result back to the displayed pixels.
        let mut unrotated = page.clone();
        unrotated.dict.insert("Rotate", 0);
        let extracted_to_pixels = pdf_interp::page_transform(&unrotated)
            .invert()
            .expect("inverse")
            .concat(&pdf_interp::page_transform(&page))
            .concat(&Matrix::scale(DPI / 72.0, DPI / 72.0));
        let rect = word.rect.transform(&extracted_to_pixels);
        for (actual, expected) in [
            (rect.x0, 200.0),
            (rect.y0, 300.0),
            (rect.x1, 800.0),
            (rect.y1, 380.0),
        ] {
            assert!(
                (actual - expected).abs() < 1.0,
                "rotation {rotation}: {rect:?}, expected {expected}"
            );
        }
    }
}
