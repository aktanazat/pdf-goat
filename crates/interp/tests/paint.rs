use goat_fixtures::PdfBuilder;
use pdf_core::{Dict, Matrix, Object, Point, Rect, Stream};
use pdf_interp::{
    Device, FillEvent, GroupEvent, Paint, Path, RunOptions, ShadingEvent, run_page_contents,
};

fn array(values: &[f64]) -> Object {
    Object::Array(values.iter().copied().map(Object::from).collect())
}
fn form(content: &[u8]) -> Stream {
    let mut d = Dict::new();
    d.insert("Subtype", Object::name("Form"));
    d.insert("BBox", array(&[0.0, 0.0, 10.0, 10.0]));
    Stream::new(d, content.to_vec())
}
fn run(builder: PdfBuilder, device: &mut dyn Device) {
    let doc = builder.build();
    run_page_contents(
        &doc,
        &doc.page(0).expect("page"),
        device,
        &RunOptions::default(),
    )
    .expect("interpret fixture");
}

#[derive(Default)]
struct Paints {
    colors: Vec<([f32; 3], Rect)>,
    shapes: Vec<(bool, f32)>,
    groups: Vec<(bool, bool)>,
    masks: Vec<(bool, Rect, u8)>,
    gradients: Vec<[Option<[f32; 3]>; 3]>,
}
impl Device for Paints {
    fn fill_path(&mut self, path: &Path, event: &FillEvent<'_>) {
        self.shapes
            .push((event.brush.alpha_is_shape, event.brush.alpha));
        if let Some(mask) = event.brush.soft_mask {
            self.masks.push((
                mask.luminosity,
                mask.bbox,
                mask.transfer.as_ref().map_or(127, |t| t[127]),
            ));
            mask.run(self).expect("mask replay");
        }
        match event.brush.paint {
            Paint::Color(color) => self
                .colors
                .push((color, path.bounds(&event.ctm).expect("path bounds"))),
            Paint::Tiling(tile) => tile.run_cell(self).expect("pattern replay"),
            Paint::Shading(_) => panic!("fixture uses direct shading"),
        }
    }
    fn begin_group(&mut self, event: &GroupEvent<'_>) {
        self.groups.push((event.isolated, event.alpha_is_shape));
    }
    fn fill_shading(&mut self, event: &ShadingEvent<'_>) {
        self.gradients.push(
            [
                Point::new(-1.0, 0.0),
                Point::new(5.0, 0.0),
                Point::new(11.0, 0.0),
            ]
            .map(|p| event.shading.sample(p)),
        );
    }
}

#[test]
fn alpha_shape_and_opacity_restore_independently_across_a_group() {
    let mut state = Dict::new();
    state.insert("AIS", true);
    state.insert("ca", 0.25);
    let mut group = Dict::new();
    group.insert("S", Object::name("Transparency"));
    group.insert("I", false);
    let mut f = form(b"0 0 2 2 re f");
    f.dict.insert("Group", group);
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("ExtGState", "Shape", Object::Dict(state))
        .resource("XObject", "G", Object::Stream(f))
        .raw_content(b"q /Shape gs 0 0 1 1 re f /G Do Q 0 0 1 1 re f");
    let mut paints = Paints::default();
    run(b, &mut paints);
    assert_eq!(paints.groups, vec![(false, true)]);
    assert_eq!(paints.shapes, vec![(true, 0.25), (true, 1.0), (false, 1.0)]);
}

#[test]
fn uncolored_pattern_ignores_cell_colors_and_anchors_before_caller_transform() {
    let mut tile = form(b"1 0 0 rg 0 0 2 3 re f");
    tile.dict.insert("PatternType", 1);
    tile.dict.insert("PaintType", 2);
    tile.dict.insert("XStep", 4);
    tile.dict.insert("YStep", 5);
    tile.dict
        .insert("Matrix", Matrix::translate(7.0, 11.0).to_object());
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource(
            "ColorSpace",
            "P",
            Object::Array(vec![Object::name("Pattern"), Object::name("DeviceRGB")]),
        )
        .resource("Pattern", "Tile", Object::Stream(tile))
        .raw_content(b"1 0 0 1 20 30 cm /P cs 0 1 0 /Tile scn 0 0 10 10 re f");
    let mut paints = Paints::default();
    run(b, &mut paints);
    assert_eq!(
        paints.colors,
        vec![([0.0, 1.0, 0.0], Rect::new(7.0, 86.0, 9.0, 89.0))]
    );
}

#[test]
fn soft_mask_keeps_definition_transform_and_replays_without_itself() {
    let mut group = Dict::new();
    group.insert("S", Object::name("Transparency"));
    group.insert("CS", Object::name("DeviceGray"));
    let mut f = form(b"0.5 g 0 0 10 10 re f");
    f.dict.insert("Group", group);
    let mut transfer = Dict::new();
    transfer.insert("FunctionType", 2);
    transfer.insert("Domain", array(&[0.0, 1.0]));
    transfer.insert("C0", array(&[1.0]));
    transfer.insert("C1", array(&[0.0]));
    transfer.insert("N", 1);
    let mut mask = Dict::new();
    mask.insert("S", Object::name("Luminosity"));
    mask.insert("G", Object::Stream(f));
    mask.insert("TR", transfer);
    let mut state = Dict::new();
    state.insert("SMask", mask);
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("ExtGState", "M", Object::Dict(state))
        .raw_content(b"1 0 0 1 5 6 cm /M gs 1 0 0 1 20 30 cm 1 0 0 rg 0 0 2 2 re f");
    let mut paints = Paints::default();
    run(b, &mut paints);
    assert_eq!(
        paints.masks,
        vec![(true, Rect::new(5.0, 84.0, 15.0, 94.0), 128)]
    );
    assert_eq!(paints.groups, vec![(true, false)]);
    assert_eq!(
        paints.colors,
        vec![
            ([0.5, 0.5, 0.5], Rect::new(5.0, 84.0, 15.0, 94.0)),
            ([1.0, 0.0, 0.0], Rect::new(25.0, 62.0, 27.0, 64.0))
        ]
    );
}

#[test]
fn axial_shading_has_unpainted_ends_without_extension() {
    let mut f = Dict::new();
    f.insert("FunctionType", 2);
    f.insert("Domain", array(&[0.0, 1.0]));
    f.insert("C0", array(&[1.0, 0.0, 0.0]));
    f.insert("C1", array(&[0.0, 0.0, 1.0]));
    f.insert("N", 1);
    let mut s = Dict::new();
    s.insert("ShadingType", 2);
    s.insert("ColorSpace", Object::name("DeviceRGB"));
    s.insert("Coords", array(&[0.0, 0.0, 10.0, 0.0]));
    s.insert("Function", f);
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("Shading", "S", Object::Dict(s))
        .raw_content(b"/S sh");
    let mut paints = Paints::default();
    run(b, &mut paints);
    assert_eq!(paints.gradients, vec![[None, Some([0.5, 0.0, 0.5]), None]]);
}

#[test]
fn operation_limit_stops_cyclic_pattern_replay() {
    struct Recur {
        errors: Vec<pdf_interp::InterpError>,
        fills: usize,
    }
    impl Device for Recur {
        fn fill_path(&mut self, _: &Path, event: &FillEvent<'_>) {
            self.fills += 1;
            if let Paint::Tiling(tile) = event.brush.paint
                && let Err(e) = tile.run_cell(self)
            {
                self.errors.push(e);
            }
        }
    }
    let mut tile = form(b"/Pattern cs /P scn 0 0 1 1 re f");
    tile.dict.insert("PatternType", 1);
    tile.dict.insert("PaintType", 1);
    tile.dict.insert("XStep", 1);
    tile.dict.insert("YStep", 1);
    let mut b = PdfBuilder::new();
    b.page(10.0, 10.0)
        .resource("Pattern", "P", Object::Stream(tile))
        .raw_content(b"/Pattern cs /P scn 0 0 1 1 re f");
    let doc = b.build();
    let mut device = Recur {
        errors: Vec::new(),
        fills: 0,
    };
    run_page_contents(
        &doc,
        &doc.page(0).expect("page"),
        &mut device,
        &RunOptions {
            max_operations: 12,
            ..RunOptions::default()
        },
    )
    .expect("outer call completes");
    assert_eq!(device.fills, 3);
    assert!(matches!(
        device.errors.as_slice(),
        [pdf_interp::InterpError::Limit(_)]
    ));
}
