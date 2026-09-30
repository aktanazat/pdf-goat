use goat_fixtures::PdfBuilder;
use pdf_core::{Dict, Object, Rect, Stream};
use pdf_interp::{Device, FillEvent, Glyph, Paint, Path, RunOptions, TextRun, run_page_contents};

fn numbers(values: &[f64]) -> Object {
    Object::Array(values.iter().copied().map(Object::from).collect())
}
#[derive(Default)]
struct Text {
    glyphs: Vec<Glyph>,
    fills: Vec<([f32; 3], Rect)>,
}
impl Device for Text {
    fn text(&mut self, run: &TextRun<'_>) {
        self.glyphs.extend_from_slice(run.glyphs);
    }
    fn wants_type3_procs(&self) -> bool {
        true
    }
    fn fill_path(&mut self, path: &Path, event: &FillEvent<'_>) {
        if let Paint::Color(c) = event.brush.paint {
            self.fills
                .push((c, path.bounds(&event.ctm).expect("glyph bounds")));
        }
    }
}
fn collect(font: Dict, content: &[u8]) -> Text {
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("Font", "F", Object::Dict(font))
        .raw_content(content);
    let doc = b.build();
    let mut text = Text::default();
    run_page_contents(
        &doc,
        &doc.page(0).expect("page"),
        &mut text,
        &RunOptions::default(),
    )
    .expect("run text");
    text
}
fn composite(encoding: &str) -> Dict {
    let mut system = Dict::new();
    system.insert("Registry", Object::string(b"Adobe".to_vec()));
    system.insert("Ordering", Object::string(b"Identity".to_vec()));
    system.insert("Supplement", 0);
    let mut cid = Dict::new();
    cid.insert("Subtype", Object::name("CIDFontType2"));
    cid.insert("BaseFont", Object::name("Helvetica"));
    cid.insert("CIDSystemInfo", system);
    cid.insert("DW", 500);
    let mut descriptor = Dict::new();
    descriptor.insert("Type", Object::name("FontDescriptor"));
    descriptor.insert("FontName", Object::name("Helvetica"));
    descriptor.insert("Flags", 32);
    descriptor.insert("FontBBox", numbers(&[-200.0, -300.0, 1200.0, 1000.0]));
    descriptor.insert("Ascent", 800);
    descriptor.insert("Descent", -200);
    descriptor.insert("CapHeight", 700);
    descriptor.insert("ItalicAngle", 0);
    descriptor.insert("StemV", 80);
    cid.insert("FontDescriptor", descriptor);
    let mut font = Dict::new();
    font.insert("Subtype", Object::name("Type0"));
    font.insert("BaseFont", Object::name("Helvetica"));
    font.insert("Encoding", Object::name(encoding));
    font.insert("DescendantFonts", Object::Array(vec![Object::Dict(cid)]));
    font.insert("ToUnicode",Object::Stream(Stream::new(Dict::new(),b"begincmap 1 begincodespacerange <0000> <ffff> endcodespacerange 2 beginbfchar <0020> <0020> <0041> <0041> endbfchar endcmap".to_vec())));
    font
}

#[test]
fn two_byte_space_does_not_receive_single_byte_word_spacing() {
    let text = collect(
        composite("Identity-H"),
        b"BT /F 10 Tf 8 Tw 0 80 Td <00200041> Tj ET",
    );
    assert_eq!(
        text.glyphs
            .iter()
            .map(|g| (
                g.unicode.first(),
                g.trm.e,
                g.trm.f,
                g.word_space,
                g.pos.byte_len
            ))
            .collect::<Vec<_>>(),
        vec![(' ', 0.0, 20.0, false, 2), ('A', 5.0, 20.0, false, 2)]
    );
}

#[test]
fn vertical_text_uses_vertical_origin_and_advance_not_horizontal_width() {
    let text = collect(
        composite("Identity-V"),
        b"BT /F 10 Tf 30 80 Td <00410041> Tj ET",
    );
    assert_eq!(text.glyphs.len(), 2);
    for (glyph, y) in text
        .glyphs
        .iter()
        .zip([28.800003051757812, 38.79999923706055])
    {
        assert_eq!((glyph.trm.e, glyph.width), (27.5, -1.0));
        assert!((glyph.trm.f - y).abs() < 1e-12);
        let rect = glyph.quad.rect();
        assert_eq!((rect.x0, rect.x1), (27.5, 37.5));
        assert!((rect.y0 - y).abs() < 1e-12 && (rect.y1 - y - 10.0).abs() < 1e-12);
    }
}

#[test]
fn uncolored_type3_procedure_keeps_inherited_ink_and_advances_each_glyph() {
    let mut encoding = Dict::new();
    encoding.insert(
        "Differences",
        Object::Array(vec![65.into(), Object::name("A")]),
    );
    let mut procs = Dict::new();
    procs.insert(
        "A",
        Object::Stream(Stream::new(
            Dict::new(),
            b"500 0 0 0 500 700 d1 1 0 0 rg 0 0 500 700 re f".to_vec(),
        )),
    );
    let mut font = Dict::new();
    font.insert("Subtype", Object::name("Type3"));
    font.insert("FontMatrix", numbers(&[0.001, 0.0, 0.0, 0.001, 0.0, 0.0]));
    font.insert("FontBBox", numbers(&[0.0, 0.0, 500.0, 700.0]));
    font.insert("FirstChar", 65);
    font.insert("LastChar", 65);
    font.insert("Widths", numbers(&[500.0]));
    font.insert("Encoding", encoding);
    font.insert("CharProcs", procs);
    let text = collect(font, b"0 1 0 rg BT /F 10 Tf 10 20 Td (AA) Tj ET");
    assert_eq!(
        text.glyphs
            .iter()
            .map(|g| (g.trm.e, g.trm.f, g.width))
            .collect::<Vec<_>>(),
        vec![(10.0, 80.0, 0.5), (15.0, 80.0, 0.5)]
    );
    assert_eq!(
        text.fills,
        vec![
            ([0.0, 1.0, 0.0], Rect::new(10.0, 73.0, 15.0, 80.0)),
            ([0.0, 1.0, 0.0], Rect::new(15.0, 73.0, 20.0, 80.0))
        ]
    );
}
