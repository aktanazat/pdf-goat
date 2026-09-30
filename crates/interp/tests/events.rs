use goat_fixtures::PdfBuilder;
use pdf_core::{Dict, Document, Matrix, Object, Rect, Stream};
use pdf_interp::{
    ContentSource, Device, FillEvent, Glyph, ImageEvent, Paint, Path, Provenance, RunOptions,
    TextRun, run_page_contents,
};

#[derive(Default)]
struct Events {
    glyphs: Vec<(Glyph, Provenance, bool)>,
    fills: Vec<([f32; 3], Rect)>,
    images: Vec<pdf_interp::ImagePixels>,
}
impl Device for Events {
    fn text(&mut self, run: &TextRun<'_>) {
        for glyph in run.glyphs {
            self.glyphs.push((
                *glyph,
                run.glyph_source(glyph),
                run.fill.is_some() || run.stroke.is_some(),
            ));
        }
    }
    fn fill_path(&mut self, path: &Path, event: &FillEvent<'_>) {
        if let Paint::Color(color) = event.brush.paint {
            self.fills.push((
                color,
                path.bounds(&event.ctm)
                    .expect("a painted fixture path has bounds"),
            ));
        }
    }
    fn fill_image(&mut self, event: &ImageEvent<'_>) {
        self.images
            .push(event.image.decode_rgb().expect("fixture image decodes"));
    }
}
fn font() -> Object {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Font"));
    dict.insert("Subtype", Object::name("Type1"));
    dict.insert("BaseFont", Object::name("Helvetica"));
    dict.insert("FirstChar", 32);
    dict.insert("LastChar", 90);
    dict.insert("Widths", vec![Object::Integer(500); 59]);
    Object::Dict(dict)
}
fn document(content: &[u8]) -> Document {
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("Font", "F", font())
        .raw_content(content);
    b.build()
}
fn events(doc: &Document) -> Events {
    let mut e = Events::default();
    run_page_contents(
        doc,
        &doc.page(0).expect("page"),
        &mut e,
        &RunOptions::default(),
    )
    .expect("interpret fixture");
    e
}

#[test]
fn text_positions_include_kerning_horizontal_scale_character_and_word_spacing() {
    let e = events(&document(
        b"BT /F 10 Tf 50 Tz 2 Tc 3 Tw 20 70 Td [(A) 100 (B C)] TJ ET",
    ));
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, _, _)| g.unicode.first())
            .collect::<String>(),
        "AB C"
    );
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, _, _)| (g.trm.e, g.trm.f))
            .collect::<Vec<_>>(),
        vec![(20.0, 30.0), (23.0, 30.0), (26.5, 30.0), (31.5, 30.0)]
    );
    assert_eq!(
        e.glyphs[0].0.quad.rect(),
        Rect::new(20.0, 19.25, 22.5, 32.9900016784668)
    );
}

#[test]
fn graphics_restore_recovers_transform_and_fill_color() {
    let e = events(&document(
        b"1 0 0 rg q 2 0 0 2 10 20 cm 0 0 1 rg 0 0 10 10 re f Q 0 0 10 10 re f",
    ));
    assert_eq!(
        e.fills,
        vec![
            ([0.0, 0.0, 1.0], Rect::new(10.0, 60.0, 30.0, 80.0)),
            ([1.0, 0.0, 0.0], Rect::new(0.0, 90.0, 10.0, 100.0))
        ]
    );
}

#[test]
fn form_matrix_precedes_calling_transform_and_resources_inherit() {
    let mut form = Dict::new();
    form.insert("Subtype", Object::name("Form"));
    form.insert("BBox", Rect::new(0.0, 0.0, 20.0, 20.0).to_object());
    form.insert(
        "Matrix",
        Matrix::new(2.0, 0.0, 0.0, 3.0, 10.0, 5.0).to_object(),
    );
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("Font", "F", font())
        .resource(
            "XObject",
            "Fm",
            Object::Stream(Stream::new(form, b"BT /F 10 Tf 1 2 Td (A) Tj ET".to_vec())),
        )
        .raw_content(b"1 0 0 1 20 0 cm /Fm Do BT /F 10 Tf 1 2 Td (B) Tj ET");
    let e = events(&b.build());
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, _, _)| (g.trm.e, g.trm.f))
            .collect::<Vec<_>>(),
        vec![(32.0, 89.0), (21.0, 98.0)]
    );
    assert!(matches!(e.glyphs[0].1.source, ContentSource::Form(_)));
    assert_eq!(e.glyphs[0].1.forms[0].op, 1);
}

#[test]
fn hidden_layers_preserve_editable_text_without_painting_it() {
    let mut b = PdfBuilder::new();
    let layer = b.layer("secret", false);
    b.page(100.0, 100.0)
        .resource("Font", "F", font())
        .begin_layer(layer)
        .raw_content(b"BT /F 10 Tf 10 50 Td (A) Tj ET 0 0 5 5 re f")
        .end_layer()
        .raw_content(b"BT /F 10 Tf 20 50 Td (B) Tj ET 10 0 5 5 re f");
    let e = events(&b.build());
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, _, paint)| (g.unicode.first(), *paint))
            .collect::<Vec<_>>(),
        vec![('A', false), ('B', true)]
    );
    assert_eq!(
        e.fills,
        vec![([0.0, 0.0, 0.0], Rect::new(10.0, 95.0, 15.0, 100.0))]
    );
}

#[test]
fn glyph_provenance_identifies_array_elements_and_byte_offsets() {
    let e = events(&document(b"BT /F 10 Tf [(AB) 10 (C)] TJ ET"));
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, p, _)| (
                p.op,
                g.pos.index,
                g.pos.element,
                g.pos.byte_offset,
                g.pos.byte_len
            ))
            .collect::<Vec<_>>(),
        vec![(2, 0, 0, 0, 1), (2, 1, 0, 1, 1), (2, 2, 2, 0, 1)]
    );
    assert_eq!(e.glyphs[2].1.glyph, Some(e.glyphs[2].0.pos));
}

#[test]
fn image_decode_reverses_samples_and_applies_color_key_mask() {
    let mut image = Dict::new();
    image.insert("Subtype", Object::name("Image"));
    image.insert("Width", 2);
    image.insert("Height", 1);
    image.insert("BitsPerComponent", 8);
    image.insert("ColorSpace", Object::name("DeviceRGB"));
    image.insert(
        "Decode",
        Object::Array([1, 0, 1, 0, 1, 0].map(Object::from).to_vec()),
    );
    image.insert(
        "Mask",
        Object::Array([255, 255, 0, 0, 0, 0].map(Object::from).to_vec()),
    );
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource(
            "XObject",
            "Im",
            Object::Stream(Stream::new(image, vec![255, 0, 0, 0, 255, 0])),
        )
        .raw_content(b"/Im Do");
    let e = events(&b.build());
    assert_eq!(e.images[0].rgb, vec![0, 255, 255, 255, 0, 255]);
    assert_eq!(
        e.images[0].alpha.as_ref().expect("color key opacity").data,
        vec![0, 255]
    );
}

#[test]
fn removal_mode_includes_hidden_form_paths_text_and_both_image_kinds() {
    let mut form = Dict::new();
    form.insert("Subtype", Object::name("Form"));
    form.insert("BBox", Rect::new(0.0, 0.0, 20.0, 20.0).to_object());
    let mut image = Dict::new();
    image.insert("Subtype", Object::name("Image"));
    image.insert("Width", 1);
    image.insert("Height", 1);
    image.insert("BitsPerComponent", 8);
    image.insert("ColorSpace", Object::name("DeviceRGB"));
    let mut b = PdfBuilder::new();
    let hidden = b.layer("hidden", false);
    b.page(100.0, 100.0)
        .resource("Font", "F", font())
        .resource(
            "XObject",
            "Fm",
            Object::Stream(Stream::new(
                form,
                b"BT /F 10 Tf (A) Tj ET 0 0 5 5 re f".to_vec(),
            )),
        )
        .resource(
            "XObject",
            "Im",
            Object::Stream(Stream::new(image, vec![0, 255, 0])),
        )
        .begin_layer(hidden)
        .raw_content(b"/Fm Do /Im Do BI /W 1 /H 1 /CS /RGB /BPC 8 ID \xff\x00\x00 EI")
        .end_layer();
    let doc = b.build();
    let visible = events(&doc);
    assert_eq!(
        visible
            .glyphs
            .iter()
            .map(|(g, _, paint)| (g.unicode.first(), *paint))
            .collect::<Vec<_>>(),
        vec![('A', false)]
    );
    assert!(visible.fills.is_empty() && visible.images.is_empty());
    let mut all = Events::default();
    run_page_contents(
        &doc,
        &doc.page(0).expect("page"),
        &mut all,
        &RunOptions {
            include_hidden_content: true,
            ..RunOptions::default()
        },
    )
    .expect("removal traversal");
    assert_eq!(
        all.glyphs
            .iter()
            .map(|(g, _, paint)| (g.unicode.first(), *paint))
            .collect::<Vec<_>>(),
        vec![('A', true)]
    );
    assert_eq!(
        all.fills,
        vec![([0.0, 0.0, 0.0], Rect::new(0.0, 95.0, 5.0, 100.0))]
    );
    assert_eq!(
        all.images
            .iter()
            .map(|p| p.rgb.as_slice())
            .collect::<Vec<_>>(),
        vec![&[0, 255, 0][..], &[255, 0, 0][..]]
    );
}

fn proportional_document(content: &[u8]) -> Document {
    let mut font = Dict::new();
    font.insert("Subtype", Object::name("Type1"));
    font.insert("BaseFont", Object::name("Helvetica"));
    let mut b = PdfBuilder::new();
    b.page(300.0, 300.0)
        .resource("Font", "F", Object::Dict(font))
        .raw_content(content);
    b.build()
}

#[test]
fn proportional_advances_preserve_mupdf_rounding_at_a_word_boundary() {
    let e = events(&proportional_document(
        b"BT /F 12 Tf 40 200 Td (abcde) Tj ET",
    ));
    assert_eq!(
        e.glyphs.iter().map(|(g, _, _)| g.trm.e).collect::<Vec<_>>(),
        vec![
            40.0,
            46.672000885009766,
            53.34400177001953,
            59.34400177001953,
            66.01600646972656
        ]
    );
    assert_eq!(e.glyphs[3].0.quad.rect().x1, 66.01599884033203);
    assert!(e.glyphs[3].0.quad.rect().x1 < e.glyphs[4].0.trm.e);
}

#[test]
fn text_matrix_spacing_and_kerning_keep_float32_operation_order() {
    let e = events(&proportional_document(
        b"BT /F 12.3 Tf 87.5 Tz 0.1 Tc 0.2 Tw 1 0.3 -0.2 1 40.2 200.4 Tm [(ab) 37 ( c)] TJ ET",
    ));
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, _, _)| (g.trm.e, g.trm.f))
            .collect::<Vec<_>>(),
        vec![
            (40.20000076293945, 99.60000610351562),
            (46.27145004272461, 97.778564453125),
            (51.94468688964844, 96.07658386230469),
            (55.199161529541016, 95.10023498535156)
        ]
    );
    assert_eq!(
        e.glyphs[0].0.quad.rect(),
        Rect::new(
            37.55550003051758,
            84.58231353759766,
            46.919490814208984,
            103.2777099609375
        )
    );
}

#[test]
fn combined_graphics_and_text_matrices_keep_native_rounding() {
    let e = events(&proportional_document(b"0.731 -0.287 0.142 1.139 13.437 27.291 cm BT /F 12.3 Tf 87.5 Tz 0.1 Tc 0.2 Tw 1.123 0.337 -0.217 0.913 40.213 200.427 Tm [(ab) 37 ( c)] TJ ET"));
    assert_eq!(
        e.glyphs
            .iter()
            .map(|(g, _, _)| (g.trm.e, g.trm.f))
            .collect::<Vec<_>>(),
        vec![
            (71.2933349609375, 55.96376037597656),
            (76.5680160522461, 55.59010314941406),
            (81.49673461914062, 55.240966796875),
            (84.32412719726562, 55.040679931640625)
        ]
    );
    assert_eq!(
        e.glyphs[0].0.quad.rect(),
        Rect::new(
            70.9101333618164,
            41.02184295654297,
            76.59857940673828,
            60.017269134521484
        )
    );
}
