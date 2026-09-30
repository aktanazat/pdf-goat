use goat_fixtures::PdfBuilder;
use pdf_core::{Dict, Document, Object, Point};
use pdf_text::{Block, TextFlags, extract_page};

#[test]
fn shaded_image_rows_map_from_the_page_top_downward() {
    let numbers = |values: &[i64]| {
        values
            .iter()
            .copied()
            .map(Object::Integer)
            .collect::<Vec<_>>()
    };
    let mut function = Dict::new();
    function.insert("FunctionType", 2);
    function.insert("Domain", numbers(&[0, 1]));
    function.insert("C0", numbers(&[1, 0, 0]));
    function.insert("C1", numbers(&[0, 0, 1]));
    function.insert("N", 1);
    let mut shading = Dict::new();
    shading.insert("ShadingType", 2);
    shading.insert("ColorSpace", Object::name("DeviceRGB"));
    shading.insert("Coords", numbers(&[0, 0, 0, 100]));
    shading.insert("Domain", numbers(&[0, 1]));
    shading.insert("Function", function);
    shading.insert("Extend", vec![Object::Bool(true), Object::Bool(true)]);
    shading.insert("BBox", numbers(&[0, 0, 100, 100]));
    let mut pdf = PdfBuilder::new();
    pdf.page(100.0, 100.0)
        .resource("Shading", "G", Object::Dict(shading))
        .raw_content(b"/G sh");
    let doc = Document::load(pdf.to_bytes()).expect("gradient document");
    let page = extract_page(&doc, 0, TextFlags::HTML).expect("gradient extraction");
    let image = page
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Image(image) => Some(image),
            _ => None,
        })
        .expect("preserved shading image");
    assert_eq!(
        Point::new(0.0, 0.0).transform(&image.transform),
        Point::new(0.0, 0.0)
    );
    assert_eq!(
        Point::new(1.0, 1.0).transform(&image.transform),
        Point::new(100.0, 100.0)
    );
}
