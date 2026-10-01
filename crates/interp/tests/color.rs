//! Colour management guards: rendering intents, black point compensation,
//! image intents and default colour spaces, pinned to what PyMuPDF 1.27
//! paints at 72 dpi (within one level).

use goat_fixtures::PdfBuilder;
use pdf_core::{Dict, Object, Stream};
use pdf_interp::{Device, FillEvent, ImageEvent, Paint, Path, RunOptions, run_page_contents};

#[derive(Default)]
struct Colors {
    fills: Vec<[u8; 3]>,
    images: Vec<[u8; 3]>,
}
impl Device for Colors {
    fn fill_path(&mut self, _path: &Path, event: &FillEvent<'_>) {
        if let Paint::Color(color) = event.brush.paint {
            self.fills.push(color.map(|v| (v * 255.0 + 0.5) as u8));
        }
    }
    fn fill_image(&mut self, event: &ImageEvent<'_>) {
        let pixels = event.image.decode_rgb().expect("fixture image decodes");
        self.images
            .push([pixels.rgb[0], pixels.rgb[1], pixels.rgb[2]]);
    }
}

fn colors(builder: &PdfBuilder) -> Colors {
    let doc = builder.build();
    let mut device = Colors::default();
    run_page_contents(
        &doc,
        &doc.page(0).expect("page"),
        &mut device,
        &RunOptions::default(),
    )
    .expect("interpret fixture");
    device
}

#[track_caller]
fn check(actual: [u8; 3], pymupdf: [u8; 3], what: &str) {
    assert!(
        actual.iter().zip(pymupdf).all(|(a, b)| a.abs_diff(b) <= 1),
        "{what}: ours {actual:?}, PyMuPDF {pymupdf:?}"
    );
}

fn image(space: Object, samples: &[u8], intent: Option<&str>) -> Stream {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Image"));
    dict.insert("Width", 1);
    dict.insert("Height", 1);
    dict.insert("BitsPerComponent", 8);
    dict.insert("ColorSpace", space);
    if let Some(intent) = intent {
        dict.insert("Intent", Object::name(intent));
    }
    Stream::new(dict, samples.to_vec())
}

fn gs(key: &str, name: &str) -> Object {
    let mut dict = Dict::new();
    dict.insert(key, Object::name(name));
    Object::Dict(dict)
}

fn reals(values: &[f64]) -> Object {
    Object::Array(values.iter().map(|v| Object::from(*v)).collect())
}

/// CalRGB with sRGB primaries and gamma 1.8.
fn cal_rgb() -> Object {
    let mut dict = Dict::new();
    dict.insert("WhitePoint", reals(&[0.9505, 1.0, 1.089]));
    dict.insert("Gamma", reals(&[1.8, 1.8, 1.8]));
    dict.insert(
        "Matrix",
        reals(&[
            0.4124, 0.2126, 0.0193, 0.3576, 0.7152, 0.1192, 0.1805, 0.0722, 0.9505,
        ]),
    );
    Object::Array(vec![Object::name("CalRGB"), Object::Dict(dict)])
}

/// A form XObject painting `0.78 0.39 0.12 rg` over its bbox, with the
/// given Resources.
fn form(resources: Option<Dict>) -> Object {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Form"));
    dict.insert("BBox", reals(&[0.0, 0.0, 10.0, 10.0]));
    if let Some(resources) = resources {
        dict.insert("Resources", Object::Dict(resources));
    }
    Object::Stream(Stream::new(
        dict,
        b"0.78 0.39 0.12 rg 0 0 10 10 re f".to_vec(),
    ))
}

fn s15(x: f64) -> [u8; 4] {
    ((x * 65536.0).round() as i32).to_be_bytes()
}

fn xyz_tag(v: [f64; 3]) -> Vec<u8> {
    let mut tag = b"XYZ \0\0\0\0".to_vec();
    for c in v {
        tag.extend_from_slice(&s15(c));
    }
    tag
}

/// A v2 matrix/TRC RGB profile with sRGB primaries and linear curves:
/// mid-grey through it is far lighter than through sRGB.
fn linear_rgb_profile() -> Vec<u8> {
    let bodies: [(&[u8; 4], Vec<u8>); 5] = [
        (b"rXYZ", xyz_tag([0.4361, 0.2225, 0.0139])),
        (b"gXYZ", xyz_tag([0.3851, 0.7169, 0.0971])),
        (b"bXYZ", xyz_tag([0.1431, 0.0606, 0.7141])),
        (b"wtpt", xyz_tag([0.9642, 1.0, 0.8249])),
        (b"rTRC", b"curv\0\0\0\0\0\0\0\0".to_vec()),
    ];
    let entries: [&[u8; 4]; 7] = [
        b"rXYZ", b"gXYZ", b"bXYZ", b"wtpt", b"rTRC", b"gTRC", b"bTRC",
    ];
    let base = 128 + 4 + 12 * entries.len();
    let mut data = Vec::new();
    let mut located = Vec::new();
    for (sig, body) in &bodies {
        located.push((*sig, base + data.len(), body.len()));
        data.extend_from_slice(body);
    }
    let mut table = Vec::new();
    for sig in entries {
        let (_, offset, size) = located
            .iter()
            .find(|(s, _, _)| *s == sig)
            .unwrap_or(&located[4]);
        table.extend_from_slice(sig);
        table.extend_from_slice(&(*offset as u32).to_be_bytes());
        table.extend_from_slice(&(*size as u32).to_be_bytes());
    }
    let size = base + data.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&0x0220_0000u32.to_be_bytes());
    out.extend_from_slice(b"mntrRGB XYZ ");
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(b"acsp");
    out.extend_from_slice(&[0; 28]);
    for c in [0.9642, 1.0, 0.8249] {
        out.extend_from_slice(&s15(c));
    }
    out.extend_from_slice(&[0; 48]);
    assert_eq!(out.len(), 128);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&data);
    out
}

fn icc_based(profile: Vec<u8>, n: i64) -> Object {
    let mut dict = Dict::new();
    dict.insert("N", n);
    Object::Array(vec![
        Object::name("ICCBased"),
        Object::Stream(Stream::new(dict, profile)),
    ])
}

#[test]
fn rendering_intent_operator_changes_cmyk_fills() {
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0).raw_content(
        b"0 0 0 1 k 0 0 10 10 re f \
          /Perceptual ri 0 0 0 1 k 0 0 10 10 re f \
          /AbsoluteColorimetric ri 0 0 0 1 k 0 0 10 10 re f \
          /Saturation ri 0.3 0.6 0.1 0.2 k 0 0 10 10 re f \
          /Bogus ri 0 0 0 1 k 0 0 10 10 re f \
          0.2 g 0 0 10 10 re f",
    );
    let c = colors(&b);
    check(c.fills[0], [34, 31, 31], "relative colorimetric K");
    check(c.fills[1], [43, 40, 41], "perceptual K");
    check(c.fills[2], [47, 44, 43], "absolute colorimetric K");
    check(c.fills[3], [151, 104, 141], "saturation CMYK");
    check(
        c.fills[4],
        [34, 31, 31],
        "unknown intent is relative colorimetric",
    );
    check(c.fills[5], [51, 51, 50], "gray is intent-independent");
}

#[test]
fn extgstate_intent_and_black_point_compensation() {
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("ExtGState", "Off", gs("UseBlackPtComp", "OFF"))
        .resource("ExtGState", "Dflt", gs("UseBlackPtComp", "Default"))
        .resource("ExtGState", "On", gs("UseBlackPtComp", "ON"))
        .resource("ExtGState", "Perc", gs("RI", "Perceptual"))
        .raw_content(
            b"/Off gs 0 0 0 1 k 0 0 10 10 re f \
              /Dflt gs 0.8 0.7 0.7 0.9 k 0 0 10 10 re f \
              /On gs 0 0 0 1 k 0 0 10 10 re f \
              /Perc gs 0 0 0 1 k 0 0 10 10 re f \
              /Off gs 0 0 0 1 k 0 0 10 10 re f \
              /RelativeColorimetric ri 0 0 0 1 K 0 0 10 10 re f",
        );
    let c = colors(&b);
    check(c.fills[0], [55, 52, 53], "compensation off");
    check(c.fills[1], [38, 40, 40], "Default is off");
    check(c.fills[2], [34, 31, 31], "compensation on");
    check(c.fills[3], [43, 40, 41], "/RI Perceptual");
    check(c.fills[4], [55, 52, 53], "perceptual without compensation");
    check(c.fills[5], [55, 52, 53], "ri keeps compensation off");
}

#[test]
fn image_intent_overrides_the_fill_intent() {
    let cmyk = || Object::name("DeviceCMYK");
    let mut lab = Dict::new();
    lab.insert("WhitePoint", reals(&[0.9642, 1.0, 0.8249]));
    lab.insert("Range", reals(&[-100.0, 100.0, -100.0, 100.0]));
    let lab = Object::Array(vec![Object::name("Lab"), Object::Dict(lab)]);
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .image_xobject([0.0, 0.0, 10.0, 10.0], image(cmyk(), &[0, 0, 0, 255], None))
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(cmyk(), &[0, 0, 0, 255], Some("Perceptual")),
        )
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(cmyk(), &[77, 153, 26, 51], Some("AbsoluteColorimetric")),
        )
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(cmyk(), &[0, 0, 0, 255], Some("Bogus")),
        )
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(Object::name("DeviceGray"), &[51], None),
        )
        .raw_content(b"/Perceptual ri")
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(cmyk(), &[204, 179, 179, 230], None),
        )
        .image_xobject([0.0, 0.0, 10.0, 10.0], image(lab, &[128, 148, 98], None));
    let c = colors(&b);
    check(c.images[0], [35, 31, 32], "CMYK image, default intent");
    check(c.images[1], [44, 41, 42], "/Intent /Perceptual");
    check(c.images[2], [135, 94, 121], "/Intent /AbsoluteColorimetric");
    check(
        c.images[3],
        [35, 31, 32],
        "unknown /Intent keeps the fill's",
    );
    check(c.images[4], [51, 51, 51], "gray image byte");
    check(
        c.images[5],
        [19, 22, 21],
        "fill intent applies to the image",
    );
    check(
        c.images[6],
        [132, 108, 171],
        "Lab image bytes are the 8-bit encoding",
    );
}

#[test]
fn page_default_rgb_replaces_device_rgb_until_a_form_overrides_it() {
    let indexed = Object::Array(vec![
        Object::name("Indexed"),
        Object::name("DeviceRGB"),
        Object::Integer(1),
        Object::String(pdf_core::PdfString::hex(vec![
            0xc8, 0x64, 0x1e, 0x32, 0x64, 0xc8,
        ])),
    ]);
    let mut override_rgb = Dict::new();
    let mut device_rgb = Dict::new();
    device_rgb.insert("DefaultRGB", Object::name("DeviceRGB"));
    override_rgb.insert("ColorSpace", Object::Dict(device_rgb));
    let mut other = Dict::new();
    let mut foo = Dict::new();
    foo.insert("Foo", Object::name("DeviceGray"));
    other.insert("ColorSpace", Object::Dict(foo));
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("ColorSpace", "DefaultRGB", cal_rgb())
        .resource("ColorSpace", "Idx", indexed.clone())
        .resource("XObject", "Finh", form(None))
        .resource("XObject", "Fdev", form(Some(override_rgb)))
        .resource("XObject", "Foth", form(Some(other)))
        .raw_content(b"0.78 0.39 0.12 rg 0 0 10 10 re f /Idx cs 1 sc 0 0 10 10 re f")
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(Object::name("DeviceRGB"), &[200, 100, 30], None),
        )
        .image_xobject([0.0, 0.0, 10.0, 10.0], image(indexed, &[1], None))
        .raw_content(b"/Finh Do /Fdev Do /Foth Do 0.78 0.39 0.12 rg 0 0 10 10 re f");
    let c = colors(&b);
    check(c.fills[0], [209, 118, 40], "rg through DefaultRGB");
    check(c.fills[1], [50, 100, 200], "Indexed over DeviceRGB stays");
    check(
        c.images[0],
        [210, 119, 40],
        "DeviceRGB image through DefaultRGB",
    );
    check(
        c.images[1],
        [65, 119, 210],
        "Indexed image through DefaultRGB",
    );
    check(c.fills[2], [209, 118, 40], "form inherits the page default");
    check(c.fills[3], [198, 99, 30], "form's DefaultRGB /DeviceRGB");
    check(
        c.fills[4],
        [209, 118, 40],
        "form with other ColorSpace entries",
    );
    check(
        c.fills[5],
        [209, 118, 40],
        "page default restored after forms",
    );
}

#[test]
fn mismatched_defaults_are_ignored() {
    let mut gray = Dict::new();
    gray.insert("WhitePoint", reals(&[0.9505, 1.0, 1.089]));
    gray.insert("Gamma", 1.8);
    let cal_gray = Object::Array(vec![Object::name("CalGray"), Object::Dict(gray)]);
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource("ColorSpace", "DefaultRGB", cal_gray)
        .resource("ColorSpace", "DefaultGray", Object::name("DeviceRGB"))
        .raw_content(b"0.78 0.39 0.12 rg 0 0 10 10 re f 0.2 g 0 0 10 10 re f");
    let c = colors(&b);
    check(
        c.fills[0],
        [198, 99, 30],
        "one-component DefaultRGB ignored",
    );
    check(
        c.fills[1],
        [51, 51, 50],
        "three-component DefaultGray ignored",
    );
}

#[test]
fn output_intent_stands_in_for_device_rgb() {
    let mut intent = Dict::new();
    intent.insert("Type", Object::name("OutputIntent"));
    intent.insert("S", Object::name("GTS_PDFA1"));
    let mut n = Dict::new();
    n.insert("N", 3);
    intent.insert(
        "DestOutputProfile",
        Object::Stream(Stream::new(n, linear_rgb_profile())),
    );
    let mut b = PdfBuilder::new();
    b.catalog_entry("OutputIntents", Object::Array(vec![Object::Dict(intent)]));
    b.page(100.0, 100.0)
        .resource("ColorSpace", "DefaultRGB", Object::name("DeviceRGB"))
        .raw_content(b"0.5 0.5 0.5 rg 0 0 10 10 re f 0.78 0.39 0.12 rg 0 0 10 10 re f")
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(Object::name("DeviceRGB"), &[128, 128, 128], None),
        )
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(Object::name("DeviceRGB"), &[200, 100, 30], None),
        );
    let c = colors(&b);
    check(c.fills[0], [187, 187, 187], "mid-grey fill");
    check(c.fills[1], [228, 167, 97], "fill");
    check(c.images[0], [188, 188, 188], "mid-grey image");
    check(c.images[1], [229, 168, 96], "image");
}

#[test]
fn icc_default_rgb_converts_fills_and_images() {
    let mut b = PdfBuilder::new();
    b.page(100.0, 100.0)
        .resource(
            "ColorSpace",
            "DefaultRGB",
            icc_based(linear_rgb_profile(), 3),
        )
        .raw_content(b"0.5 0.5 0.5 rg 0 0 10 10 re f")
        .image_xobject(
            [0.0, 0.0, 10.0, 10.0],
            image(Object::name("DeviceRGB"), &[200, 100, 30], None),
        );
    let c = colors(&b);
    check(c.fills[0], [187, 187, 187], "mid-grey fill");
    check(c.images[0], [229, 168, 96], "image");
}
