use pdf_core::{Dict, Document, Name, Object, Rect, Stream};
use pdf_render::{RenderOptions, render_page, render_page_graphics};

fn name(value: &str) -> Object {
    Name::from(value).into()
}

fn document(content: &[u8], resources: Dict) -> Document {
    let mut doc = Document::new();
    let page = doc
        .insert_blank_page(0, Rect::new(0.0, 0.0, 40.0, 40.0))
        .unwrap();
    let content = doc.add(Stream::new(Dict::new(), content.to_vec()));
    let mut dict = doc.get(page).unwrap().as_dict().unwrap().clone();
    dict.insert("Contents", content);
    dict.insert("Resources", resources);
    doc.set(page, dict);
    doc
}

fn set_resources(doc: &mut Document, resources: Dict) {
    let page = doc.page(0).unwrap();
    let mut dict = page.dict;
    dict.insert("Resources", resources);
    doc.set(page.id, dict);
}

fn pixels(doc: &Document) -> pdf_raster::Pixmap {
    render_page(doc, &doc.page(0).unwrap(), &RenderOptions::default()).unwrap()
}

#[test]
fn nested_clip_is_restored_before_the_next_paint() {
    let doc = document(
        b"q 0 0 20 40 re W n 1 0 0 rg 0 0 40 40 re f Q 0 0 1 rg 30 0 10 40 re f",
        Dict::new(),
    );
    let image = pixels(&doc);
    assert_eq!(image.pixel(5, 20), Some([255, 0, 0, 255]));
    assert_eq!(image.pixel(25, 20), Some([255, 255, 255, 255]));
    assert_eq!(image.pixel(35, 20), Some([0, 0, 255, 255]));
}

#[test]
fn crop_rotation_and_user_unit_move_content_and_dimensions_together() {
    let mut doc = document(b"1 0 0 rg 10 5 10 10 re f", Dict::new());
    let page = doc.page(0).unwrap();
    let mut dict = page.dict;
    dict.insert("CropBox", Rect::new(10.0, 5.0, 30.0, 35.0).to_object());
    dict.insert("Rotate", 90);
    dict.insert("UserUnit", 2);
    doc.set(page.id, dict);
    let image = pixels(&doc);
    assert_eq!((image.width(), image.height()), (60, 40));
    assert_eq!(image.pixel(5, 5), Some([255, 0, 0, 255]));
    assert_eq!(image.pixel(45, 25), Some([255, 255, 255, 255]));
}

#[test]
fn image_rows_and_soft_mask_use_the_same_top_edge() {
    let mut doc = document(b"q 20 0 0 20 10 10 cm /Im Do Q", Dict::new());
    let mut alpha = Dict::new();
    alpha.insert("Type", name("XObject"));
    alpha.insert("Subtype", name("Image"));
    alpha.insert("Width", 1);
    alpha.insert("Height", 2);
    alpha.insert("BitsPerComponent", 8);
    alpha.insert("ColorSpace", name("DeviceGray"));
    let mask = doc.add(Stream::new(alpha.clone(), vec![128, 255]));
    alpha.insert("ColorSpace", name("DeviceRGB"));
    alpha.insert("SMask", mask);
    let image = doc.add(Stream::new(alpha, vec![255, 0, 0, 0, 0, 255]));
    let mut xobjects = Dict::new();
    xobjects.insert("Im", image);
    let mut resources = Dict::new();
    resources.insert("XObject", xobjects);
    set_resources(&mut doc, resources);
    let image = pixels(&doc);
    assert_eq!(image.pixel(15, 15), Some([255, 127, 127, 255]));
    assert_eq!(image.pixel(15, 25), Some([0, 0, 255, 255]));
    assert_eq!(image.pixel(5, 5), Some([255, 255, 255, 255]));
}

#[test]
fn alpha_output_keeps_unpainted_pixels_transparent() {
    let doc = document(b"1 0 0 rg 0 0 20 40 re f", Dict::new());
    let image = render_page(
        &doc,
        &doc.page(0).unwrap(),
        &RenderOptions {
            alpha: true,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(image.pixel(5, 20), Some([255, 0, 0, 255]));
    assert_eq!(image.pixel(25, 20), Some([0, 0, 0, 0]));
}

#[test]
fn type3_glyphs_paint_but_invisible_text_does_not() {
    let mut doc = document(
        b"BT /F 10 Tf 1 0 0 1 5 5 Tm (A) Tj 3 Tr 1 0 0 1 20 5 Tm (A) Tj ET",
        Dict::new(),
    );
    let procedure = doc.add(Stream::new(
        Dict::new(),
        b"1000 0 d0 0 0 1000 1000 re f".to_vec(),
    ));
    let mut procedures = Dict::new();
    procedures.insert("A", procedure);
    let mut encoding = Dict::new();
    encoding.insert("Differences", Object::Array(vec![65.into(), name("A")]));
    let mut font = Dict::new();
    font.insert("Type", name("Font"));
    font.insert("Subtype", name("Type3"));
    font.insert("FontBBox", Rect::new(0.0, 0.0, 1000.0, 1000.0).to_object());
    font.insert(
        "FontMatrix",
        Object::Array(vec![
            0.001.into(),
            0.into(),
            0.into(),
            0.001.into(),
            0.into(),
            0.into(),
        ]),
    );
    font.insert("FirstChar", 65);
    font.insert("LastChar", 65);
    font.insert("Widths", Object::Array(vec![1000.into()]));
    font.insert("Encoding", encoding);
    font.insert("CharProcs", procedures);
    let mut fonts = Dict::new();
    fonts.insert("F", doc.add(font));
    let mut resources = Dict::new();
    resources.insert("Font", fonts);
    set_resources(&mut doc, resources);
    let image = pixels(&doc);
    assert_eq!(image.pixel(10, 30), Some([0, 0, 0, 255]));
    assert_eq!(image.pixel(25, 30), Some([255, 255, 255, 255]));
    let graphics =
        render_page_graphics(&doc, &doc.page(0).unwrap(), &RenderOptions::default()).unwrap();
    assert_eq!(graphics.pixel(10, 30), Some([255, 255, 255, 255]));
    assert_eq!(graphics.pixel(25, 30), Some([255, 255, 255, 255]));
}

#[test]
fn axial_shading_evaluates_its_function_across_the_page() {
    let mut function = Dict::new();
    function.insert("FunctionType", 2);
    function.insert("Domain", Object::Array(vec![0.into(), 1.into()]));
    function.insert("C0", Object::Array(vec![1.into(), 0.into(), 0.into()]));
    function.insert("C1", Object::Array(vec![0.into(), 0.into(), 1.into()]));
    function.insert("N", 1);
    let mut shading = Dict::new();
    shading.insert("ShadingType", 2);
    shading.insert("ColorSpace", name("DeviceRGB"));
    shading.insert(
        "Coords",
        Object::Array(vec![0.into(), 0.into(), 40.into(), 0.into()]),
    );
    shading.insert("Function", function);
    shading.insert("Extend", Object::Array(vec![true.into(), true.into()]));
    let mut shadings = Dict::new();
    shadings.insert("S", shading);
    let mut resources = Dict::new();
    resources.insert("Shading", shadings);
    let image = pixels(&document(b"/S sh", resources));
    assert_eq!(image.pixel(0, 20), Some([252, 0, 3, 255]));
    assert_eq!(image.pixel(39, 20), Some([3, 0, 252, 255]));
}

#[test]
fn tiling_pattern_preserves_the_gap_between_cells() {
    let mut doc = document(b"/Pattern cs /P scn 0 0 40 40 re f", Dict::new());
    let mut tile = Dict::new();
    tile.insert("Type", name("Pattern"));
    tile.insert("PatternType", 1);
    tile.insert("PaintType", 1);
    tile.insert("TilingType", 1);
    tile.insert("BBox", Rect::new(0.0, 0.0, 8.0, 8.0).to_object());
    tile.insert("XStep", 12);
    tile.insert("YStep", 12);
    tile.insert("Resources", Dict::new());
    let pattern = doc.add(Stream::new(tile, b"1 0 0 rg 0 0 8 8 re f".to_vec()));
    let mut patterns = Dict::new();
    patterns.insert("P", pattern);
    let mut resources = Dict::new();
    resources.insert("Pattern", patterns);
    set_resources(&mut doc, resources);
    let image = pixels(&doc);
    assert_eq!(image.pixel(5, 37), Some([255, 0, 0, 255]));
    assert_eq!(image.pixel(10, 37), Some([255, 255, 255, 255]));
    assert_eq!(image.pixel(17, 37), Some([255, 0, 0, 255]));
}

#[test]
fn luminosity_mask_is_applied_to_the_paint_not_drawn_as_content() {
    let mut doc = document(b"/GS gs 1 0 0 rg 0 0 40 40 re f", Dict::new());
    let mut group = Dict::new();
    group.insert("S", name("Transparency"));
    group.insert("CS", name("DeviceRGB"));
    group.insert("I", true);
    let mut form = Dict::new();
    form.insert("Type", name("XObject"));
    form.insert("Subtype", name("Form"));
    form.insert("BBox", Rect::new(0.0, 0.0, 40.0, 40.0).to_object());
    form.insert("Group", group);
    form.insert("Resources", Dict::new());
    let mask = doc.add(Stream::new(
        form,
        b"0 0 0 rg 0 0 20 40 re f 1 1 1 rg 20 0 20 40 re f".to_vec(),
    ));
    let mut soft_mask = Dict::new();
    soft_mask.insert("S", name("Luminosity"));
    soft_mask.insert("G", mask);
    let mut state = Dict::new();
    state.insert("SMask", soft_mask);
    let mut states = Dict::new();
    states.insert("GS", state);
    let mut resources = Dict::new();
    resources.insert("ExtGState", states);
    set_resources(&mut doc, resources);
    let image = pixels(&doc);
    assert_eq!(image.pixel(10, 20), Some([255, 255, 255, 255]));
    assert_eq!(image.pixel(30, 20), Some([255, 0, 0, 255]));
}

#[test]
fn adjoining_mesh_triangles_do_not_leave_a_translucent_seam() {
    let mut doc = document(b"/S sh", Dict::new());
    let mut data = Vec::new();
    for [x, y] in [[0, 0], [255, 0], [0, 255], [255, 0], [255, 255], [0, 255]] {
        data.extend_from_slice(&[0, x, y, 255, 0, 0]);
    }
    let mut mesh = Dict::new();
    mesh.insert("ShadingType", 4);
    mesh.insert("ColorSpace", name("DeviceRGB"));
    mesh.insert("BitsPerCoordinate", 8);
    mesh.insert("BitsPerComponent", 8);
    mesh.insert("BitsPerFlag", 8);
    mesh.insert(
        "Decode",
        Object::Array([0, 40, 0, 40, 0, 1, 0, 1, 0, 1].map(Object::from).to_vec()),
    );
    let shading = doc.add(Stream::new(mesh, data));
    let mut shadings = Dict::new();
    shadings.insert("S", shading);
    let mut resources = Dict::new();
    resources.insert("Shading", shadings);
    set_resources(&mut doc, resources);
    let image = pixels(&doc);
    for pixel in image.data().as_chunks::<4>().0 {
        assert_eq!(*pixel, [255, 0, 0, 255]);
    }
}

#[test]
fn graphics_only_omits_text_paint_but_preserves_text_clipping() {
    let mut font = Dict::new();
    font.insert("Type", name("Font"));
    font.insert("Subtype", name("Type1"));
    font.insert("BaseFont", name("Helvetica"));
    let mut fonts = Dict::new();
    fonts.insert("F", font);
    let mut resources = Dict::new();
    resources.insert("Font", fonts);
    let doc = document(
        b"0 0 1 rg 0 0 40 40 re f \
          0 0 0 rg BT /F 10 Tf 1 0 0 1 5 25 Tm (H) Tj ET \
          q BT /F 20 Tf 7 Tr 1 0 0 1 5 1 Tm (H) Tj ET \
          1 0 0 rg 0 0 40 40 re f Q",
        resources,
    );
    assert!(pixels(&doc).pixel(6, 10).unwrap()[2] < 255);
    let image =
        render_page_graphics(&doc, &doc.page(0).unwrap(), &RenderOptions::default()).unwrap();
    for y in 7..15 {
        for x in 5..13 {
            assert_eq!(image.pixel(x, y), Some([0, 0, 255, 255]));
        }
    }
    assert_eq!(image.pixel(7, 30), Some([255, 0, 0, 255]));
    assert_eq!(image.pixel(12, 27), Some([0, 0, 255, 255]));
    assert_eq!(image.pixel(2, 30), Some([0, 0, 255, 255]));
}
