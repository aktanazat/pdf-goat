//! `recognize` contract: malformed bitmaps are rejected as values, blank
//! images read as no lines, and boxes come back in top-left pixel space.
//! Text is drawn with a 5x7 block font scaled up, so no font rasterizer is
//! needed.

use pdf_ocr::{Bitmap, OcrError, OcrOptions, PixelFormat};

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    /// 5x7 glyphs, one row per string, `#` inked.
    fn glyph(c: char) -> [&'static str; 7] {
        match c {
            'H' => [
                "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
            ],
            'E' => [
                "#####", "#....", "#....", "####.", "#....", "#....", "#####",
            ],
            'L' => [
                "#....", "#....", "#....", "#....", "#....", "#....", "#####",
            ],
            'O' => [
                ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
            ],
            'R' => [
                "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#",
            ],
            'D' => [
                "####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####.",
            ],
            _ => ["....."; 7],
        }
    }

    const DOT: usize = 8;
    const WIDTH: usize = 900;
    const HEIGHT: usize = 400;

    /// White gray page with `text` inked starting at pixel (`left`, `top`).
    fn page(text: &str, left: usize, top: usize) -> Vec<u8> {
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        for (index, c) in text.chars().enumerate() {
            let x0 = left + index * 6 * DOT;
            for (row, bits) in glyph(c).iter().enumerate() {
                for (col, bit) in bits.chars().enumerate() {
                    if bit != '#' {
                        continue;
                    }
                    for y in 0..DOT {
                        let start = (top + row * DOT + y) * WIDTH + x0 + col * DOT;
                        pixels[start..start + DOT].fill(0);
                    }
                }
            }
        }
        pixels
    }

    fn gray(pixels: &[u8]) -> Bitmap<'_> {
        Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: pixels,
        }
    }

    #[test]
    fn block_text_is_read_with_top_left_pixel_boxes() {
        // Text in the top part of the page: a y-flip would put it near the
        // bottom instead.
        let (left, top) = (60, 40);
        let pixels = page("HELLO OLDER", left, top);
        let lines = pdf_ocr::recognize(&gray(&pixels), &OcrOptions::default()).unwrap();

        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert_eq!(line.text, "HELLO OLDER");
        let words: Vec<&str> = line.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(words, ["HELLO", "OLDER"]);

        let (ink_right, ink_bottom) = (left + (11 * 6 - 1) * DOT, top + 7 * DOT);
        let near =
            |actual: f64, expected: usize| (actual - expected as f64).abs() < 3.0 * DOT as f64;
        let b = line.bbox;
        assert!(near(b.x, left) && near(b.y, top), "line box {b:?}");
        assert!(
            near(b.x + b.width, ink_right) && near(b.y + b.height, ink_bottom),
            "line box {b:?}"
        );

        let hello = line.words[0].bbox.unwrap();
        let older = line.words[1].bbox.unwrap();
        assert!(near(hello.x, left), "HELLO box {hello:?}");
        assert!(near(older.x, left + 6 * 6 * DOT), "OLDER box {older:?}");
        assert!(
            hello.x + hello.width <= older.x + DOT as f64,
            "{hello:?} overlaps {older:?}"
        );
    }

    #[test]
    fn transparent_rgba_background_reads_as_white_paper() {
        // Black ink on a fully transparent background, the way a page
        // rendered without a paper fill arrives.
        let gray_pixels = page("HELLO", 60, 40);
        let rgba: Vec<u8> = gray_pixels
            .iter()
            .flat_map(|&v| if v == 0 { [0, 0, 0, 255] } else { [0, 0, 0, 0] })
            .collect();
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH * 4,
            format: PixelFormat::Rgba8Premultiplied,
            data: &rgba,
        };
        let lines = pdf_ocr::recognize(&bitmap, &OcrOptions::default()).unwrap();
        let text: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(text, ["HELLO"]);
    }

    #[test]
    fn blank_page_has_no_lines() {
        let pixels = vec![255u8; WIDTH * HEIGHT];
        let lines = pdf_ocr::recognize(&gray(&pixels), &OcrOptions::default()).unwrap();
        assert_eq!(lines, []);
    }

    #[test]
    fn zero_sized_image_has_no_lines() {
        let bitmap = Bitmap {
            width: 0,
            height: 10,
            stride: 0,
            format: PixelFormat::Gray8,
            data: &[],
        };
        assert_eq!(
            pdf_ocr::recognize(&bitmap, &OcrOptions::default()),
            Ok(Vec::new())
        );
    }

    #[test]
    fn buffer_shorter_than_stride_times_height_is_rejected() {
        let pixels = vec![255u8; 10 * 10 - 1];
        let bitmap = Bitmap {
            width: 10,
            height: 10,
            stride: 10,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let error = pdf_ocr::recognize(&bitmap, &OcrOptions::default()).unwrap_err();
        assert!(matches!(error, OcrError::InvalidBitmap(_)), "{error:?}");
    }

    #[test]
    fn stride_shorter_than_a_row_is_rejected() {
        let pixels = vec![255u8; 4 * 10 * 10];
        let bitmap = Bitmap {
            width: 10,
            height: 10,
            stride: 39,
            format: PixelFormat::Rgba8,
            data: &pixels,
        };
        let error = pdf_ocr::recognize(&bitmap, &OcrOptions::default()).unwrap_err();
        assert!(matches!(error, OcrError::InvalidBitmap(_)), "{error:?}");
    }

    #[test]
    fn unknown_recognition_language_is_refused_with_the_supported_list() {
        let pixels = page("HELLO", 60, 40);
        let options = OcrOptions {
            languages: vec!["en-US".into(), "xx-NOT-A-LANGUAGE".into()],
            ..OcrOptions::default()
        };
        let error = pdf_ocr::recognize(&gray(&pixels), &options).unwrap_err();
        let OcrError::UnsupportedLanguage {
            language,
            supported,
        } = error
        else {
            panic!("expected UnsupportedLanguage, got {error:?}");
        };
        assert_eq!(language, "xx-NOT-A-LANGUAGE");
        assert_eq!(supported, pdf_ocr::supported_languages().unwrap());
        assert!(
            supported.iter().any(|code| code == "en-US"),
            "{supported:?}"
        );
    }

    #[test]
    fn listed_language_is_accepted() {
        let pixels = page("HELLO", 60, 40);
        let options = OcrOptions {
            languages: vec!["fr-FR".into()],
            detect_language: false,
        };
        let lines = pdf_ocr::recognize(&gray(&pixels), &options).unwrap();
        let text: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(text, ["HELLO"]);
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn other_targets_report_unsupported() {
    let bitmap = Bitmap {
        width: 1,
        height: 1,
        stride: 1,
        format: PixelFormat::Gray8,
        data: &[255],
    };
    assert_eq!(
        pdf_ocr::recognize(&bitmap, &OcrOptions::default()),
        Err(OcrError::Unsupported)
    );
}
