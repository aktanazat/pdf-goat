//! PDF/A candidates must carry their font programs and may never retain encryption.

use std::{ffi::OsString, fs, path::Path};

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{PdfBuilder, Standard14, TextStyle};
use pdf_core::{Dict, Document, Object, Stream};
use pdf_font::{CMap, CidToGid, Font, ToUnicodeMap};
use serde_json::{Map, Value};

fn convert(home: &Path, source: &Path, out: &Path) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_inspect::register(&mut registry);
    let cli = registry.into_cli(clap::Command::new("pdf-goat"), &["convert"], &[]);
    let args = [
        OsString::from("convert"),
        OsString::from("pdfa"),
        source.as_os_str().to_owned(),
        OsString::from("-o"),
        out.as_os_str().to_owned(),
    ];
    cli.parse(&args)
        .unwrap_or_else(|failure| panic!("{failure:?}"))
        .run(&Ctx::new(home))
}

fn page_font(doc: &Document, page: usize) -> Dict {
    let page = doc.page(page).expect("page");
    let fonts = doc
        .resolve_dict(page.resources.get(b"Font").expect("font resources"))
        .expect("resolve fonts")
        .expect("font dictionary");
    doc.resolve_dict(fonts.iter().next().expect("one font").1)
        .expect("resolve font")
        .expect("font")
}

fn decoded(doc: &Document, owner: &Dict, key: &[u8]) -> Vec<u8> {
    let stream = doc
        .resolve_stream(owner.get(key).expect("required stream"))
        .expect("resolve stream")
        .expect("stream");
    doc.decode_stream(&stream).expect("decode stream").data
}

/// Read the saved font through the public font decoder, not the conversion helpers.
fn read_run(doc: &Document, font: &Dict, text: &[u8]) -> (String, Vec<f64>) {
    let descendants = doc
        .resolve_array(font.get(b"DescendantFonts").expect("CID font"))
        .expect("descendants")
        .expect("array");
    let descendant = doc
        .resolve_dict(&descendants[0])
        .expect("descendant")
        .expect("dictionary");
    let descriptor = doc
        .resolve_dict(descendant.get(b"FontDescriptor").expect("descriptor"))
        .expect("resolve descriptor")
        .expect("dictionary");
    let program = Font::parse(decoded(doc, &descriptor, b"FontFile2"))
        .expect("a real embedded TrueType program");
    let encoding = match doc
        .resolve(font.get(b"Encoding").expect("encoding"))
        .expect("resolve encoding")
    {
        Object::Name(name) => {
            CMap::predefined(name.as_str().expect("name")).expect("predefined CMap")
        }
        Object::Stream(stream) => {
            let cmap = CMap::parse(&doc.decode_stream(&stream).expect("encoding stream").data)
                .expect("CMap");
            let encoded_system = doc
                .resolve_dict(
                    stream
                        .dict
                        .get(b"CIDSystemInfo")
                        .expect("encoding collection"),
                )
                .expect("resolve")
                .expect("dictionary");
            let font_system = doc
                .resolve_dict(descendant.get(b"CIDSystemInfo").expect("font collection"))
                .expect("resolve")
                .expect("dictionary");
            let program_system = cmap.cid_system_info().expect("CMap program collection");
            for (key, expected) in [
                (b"Registry".as_slice(), &program_system.registry),
                (b"Ordering".as_slice(), &program_system.ordering),
            ] {
                assert_eq!(
                    encoded_system
                        .get_string(key)
                        .expect("encoding collection name")
                        .to_text(),
                    *expected
                );
                assert_eq!(
                    font_system
                        .get_string(key)
                        .expect("font collection name")
                        .to_text(),
                    *expected
                );
            }
            assert!(
                font_system.get_i64(b"Supplement").expect("font supplement")
                    <= encoded_system
                        .get_i64(b"Supplement")
                        .expect("encoding supplement")
            );
            cmap
        }
        _ => panic!("font encoding must be a CMap"),
    };
    let unicode = ToUnicodeMap::parse(&decoded(doc, font, b"ToUnicode")).expect("Unicode map");
    let map = decoded(doc, &descendant, b"CIDToGIDMap");
    let widths = doc
        .resolve_array(descendant.get(b"W").expect("widths"))
        .expect("width array")
        .expect("array");
    let mut all_widths = vec![descendant.get_f64(b"DW").unwrap_or(1000.0); 65536];
    let mut i = 0;
    while i < widths.len() {
        let first = widths[i].as_i64().expect("first CID") as usize;
        match &widths[i + 1] {
            Object::Array(values) => {
                for (offset, value) in values.iter().enumerate() {
                    all_widths[first + offset] = value.as_f64().expect("width");
                }
                i += 2;
            }
            last => {
                let last = last.as_i64().expect("last CID") as usize;
                all_widths[first..=last].fill(widths[i + 2].as_f64().expect("width"));
                i += 3;
            }
        }
    }
    let mut chars = String::new();
    let mut advances = Vec::new();
    for (code, cid) in encoding.decode(text) {
        let value = unicode
            .lookup(code.code)
            .expect("Unicode for the shown code");
        let gid = program
            .glyph_for_cid(cid, CidToGid::Map(&map))
            .expect("embedded glyph");
        assert_ne!(gid, 0, "{value:?} must not become .notdef");
        if !value.chars().all(char::is_whitespace) {
            assert!(
                !program.outline(gid).expect("glyph outline").is_empty(),
                "{value:?} must be drawable"
            );
        }
        chars.push_str(&value);
        advances.push(all_widths[cid as usize]);
        let program_width = f64::from(program.advance(gid).expect("font advance")) * 1000.0
            / f64::from(program.units_per_em());
        assert!(
            (program_width - all_widths[cid as usize]).abs() <= 1.0,
            "PDF/A requires matching program and dictionary advances: {program_width} vs {}",
            all_widths[cid as usize]
        );
    }
    (chars, advances)
}

#[test]
fn pdfa_embeds_standard_fonts_with_the_same_characters_and_advances() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let fonts = [
        Standard14::Helvetica,
        Standard14::HelveticaBoldOblique,
        Standard14::TimesRoman,
        Standard14::Courier,
    ];
    for font in fonts {
        builder.page(400.0, 300.0).styled_text(
            40.0,
            100.0,
            "Hello, PDF!",
            TextStyle::new(font, 12.0),
        );
    }
    let source = builder.save(dir.path().join("fonts.pdf"));
    let out = dir.path().join("fonts.pdfa.pdf");
    let result = convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(&out).expect("converted document");
    for (page, original) in fonts.iter().enumerate() {
        let font = page_font(&doc, page);
        let (text, advances) = read_run(&doc, &font, b"Hello, PDF!");
        assert_eq!(text, "Hello, PDF!");
        let expected: Vec<f64> = b"Hello, PDF!"
            .iter()
            .map(|code| f64::from(original.code_width(*code)))
            .collect();
        assert_eq!(advances, expected, "embedding may not reflow the page");
    }
    assert_eq!(result["conformance_validated"], false);
    let catalog = doc.catalog().expect("catalog");
    let intents = doc
        .resolve_array(catalog.get(b"OutputIntents").expect("output intents"))
        .expect("resolve")
        .expect("array");
    let intent = doc
        .resolve_dict(&intents[0])
        .expect("resolve")
        .expect("dictionary");
    let profile = decoded(&doc, &intent, b"DestOutputProfile");
    let word =
        |at: usize| u32::from_be_bytes(profile[at..at + 4].try_into().expect("ICC word")) as usize;
    assert_eq!(
        word(0),
        profile.len(),
        "the ICC header must describe the actual profile size"
    );
    let table_end = 132 + word(128) * 12;
    assert!(
        table_end <= profile.len(),
        "ICC tag table must fit the profile"
    );
    for at in (132..table_end).step_by(12) {
        let offset = word(at + 4);
        let length = word(at + 8);
        assert!(
            offset >= table_end && offset + length <= profile.len(),
            "ICC tag payload must be in bounds"
        );
        assert_eq!(offset % 4, 0, "ICC tags must be aligned");
    }
}

#[test]
fn pdfa_preserves_difference_encodings_and_explicit_widths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let encoding: Dict = [
        ("BaseEncoding".into(), Object::name("WinAnsiEncoding")),
        (
            "Differences".into(),
            Object::Array(vec![
                Object::Integer(65),
                Object::name("eacute"),
                Object::name("A"),
            ]),
        ),
    ]
    .into_iter()
    .collect();
    let font: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("Type1")),
        ("BaseFont".into(), Object::name("Helvetica")),
        ("Encoding".into(), encoding.into()),
        ("FirstChar".into(), Object::Integer(65)),
        ("LastChar".into(), Object::Integer(66)),
        (
            "Widths".into(),
            Object::Array(vec![Object::Real(601.5), Object::Integer(777)]),
        ),
    ]
    .into_iter()
    .collect();
    builder
        .page(200.0, 200.0)
        .resource("Font", "Special", font.into())
        .raw_content(b"BT /Special 10 Tf 20 100 Td (AB) Tj ET");
    let source = builder.save(dir.path().join("differences.pdf"));
    let out = dir.path().join("differences.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(&out).expect("converted document");
    assert_eq!(
        read_run(&doc, &page_font(&doc, 0), b"AB"),
        ("éA".to_owned(), vec![601.5, 777.0])
    );
}

#[test]
fn pdfa_preserves_composite_font_codes_and_widths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let descriptor: Dict = [
        ("Type".into(), Object::name("FontDescriptor")),
        ("FontName".into(), Object::name("Helvetica")),
        ("Flags".into(), Object::Integer(32)),
        ("ItalicAngle".into(), Object::Integer(0)),
        ("Ascent".into(), Object::Integer(800)),
        ("Descent".into(), Object::Integer(-200)),
        ("CapHeight".into(), Object::Integer(700)),
        ("StemV".into(), Object::Integer(80)),
        (
            "FontBBox".into(),
            Object::Array(vec![(-100).into(), (-300).into(), 1200.into(), 1000.into()]),
        ),
    ]
    .into_iter()
    .collect();
    let collection: Dict = [
        ("Registry".into(), Object::text("Adobe")),
        ("Ordering".into(), Object::text("Identity")),
        ("Supplement".into(), Object::Integer(0)),
    ]
    .into_iter()
    .collect();
    let descendant: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("CIDFontType2")),
        ("BaseFont".into(), Object::name("Helvetica")),
        ("DW".into(), Object::Integer(800)),
        ("FontDescriptor".into(), descriptor.into()),
        ("CIDSystemInfo".into(), collection.into()),
        (
            "W".into(),
            Object::Array(vec![
                Object::Integer(7),
                Object::Array(vec![Object::Integer(333), Object::Integer(701)]),
            ]),
        ),
    ]
    .into_iter()
    .collect();
    let unicode = pdf_font::write_to_unicode_cmap(&[(7, "é"), (8, "A")]);
    let font: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("Type0")),
        ("BaseFont".into(), Object::name("Helvetica")),
        ("Encoding".into(), Object::name("Identity-H")),
        (
            "DescendantFonts".into(),
            Object::Array(vec![descendant.into()]),
        ),
        ("ToUnicode".into(), Stream::new(Dict::new(), unicode).into()),
    ]
    .into_iter()
    .collect();
    builder
        .page(200.0, 200.0)
        .resource("Font", "Composite", font.into())
        .raw_content(b"BT /Composite 10 Tf 20 100 Td <00070008> Tj ET");
    let source = builder.save(dir.path().join("composite.pdf"));
    let out = dir.path().join("composite.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(&out).expect("converted document");
    assert_eq!(
        read_run(&doc, &page_font(&doc, 0), &[0, 7, 0, 8]),
        ("éA".to_owned(), vec![333.0, 701.0])
    );
}

#[test]
fn pdfa_rejects_locked_input_without_replacing_the_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder.page(200.0, 200.0).text(20.0, 100.0, "secret");
    let source = dir.path().join("locked.pdf");
    let options = pdf_core::SaveOptions {
        encryption: pdf_core::Encryption::New(pdf_core::NewEncryption {
            method: pdf_core::NewMethod::Aes256,
            user_password: b"secret".to_vec(),
            owner_password: b"owner".to_vec(),
            permissions: -4,
            encrypt_metadata: true,
        }),
        ..pdf_core::SaveOptions::default()
    };
    builder
        .build()
        .save(&source, &options)
        .expect("encrypted fixture");
    let out = dir.path().join("candidate.pdf");
    fs::write(&out, b"keep this output").expect("existing output");
    let error = convert(dir.path(), &source, &out)
        .expect_err("cannot make PDF/A from unauthenticated bytes");
    assert!(error.to_string().contains("password"), "{error}");
    assert_eq!(fs::read(out).expect("output retained"), b"keep this output");
}

#[test]
fn pdfa_removes_owner_only_encryption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    builder.page(200.0, 200.0).text(20.0, 100.0, "open");
    let source = dir.path().join("owner.pdf");
    let options = pdf_core::SaveOptions {
        encryption: pdf_core::Encryption::New(pdf_core::NewEncryption {
            method: pdf_core::NewMethod::Aes256,
            user_password: Vec::new(),
            owner_password: b"owner".to_vec(),
            permissions: -4,
            encrypt_metadata: true,
        }),
        ..pdf_core::SaveOptions::default()
    };
    builder
        .build()
        .save(&source, &options)
        .expect("encrypted fixture");
    let out = dir.path().join("candidate.pdf");
    convert(dir.path(), &source, &out).expect("conversion with empty user password");
    let doc = Document::open(&out).expect("output");
    assert!(!doc.is_encrypted());
    assert!(!doc.trailer().contains_key(b"Encrypt"));
    assert_eq!(read_run(&doc, &page_font(&doc, 0), b"open").0, "open");
}

#[test]
fn pdfa_embeds_fonts_inside_form_xobjects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let font: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("Type1")),
        ("BaseFont".into(), Object::name("Helvetica")),
        ("Encoding".into(), Object::name("WinAnsiEncoding")),
    ]
    .into_iter()
    .collect();
    let fonts: Dict = [("Nested".into(), font.into())].into_iter().collect();
    let resources: Dict = [("Font".into(), fonts.into())].into_iter().collect();
    let form: Dict = [
        ("Type".into(), Object::name("XObject")),
        ("Subtype".into(), Object::name("Form")),
        (
            "BBox".into(),
            Object::Array(vec![0.into(), 0.into(), 200.into(), 200.into()]),
        ),
        ("Resources".into(), resources.into()),
    ]
    .into_iter()
    .collect();
    let mut builder = PdfBuilder::new();
    builder
        .page(200.0, 200.0)
        .resource(
            "XObject",
            "Nested",
            Stream::new(form, b"BT /Nested 12 Tf 20 100 Td (Hello) Tj ET".to_vec()).into(),
        )
        .raw_content(b"q /Nested Do Q");
    let source = builder.save(dir.path().join("form.pdf"));
    let out = dir.path().join("form.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(out).expect("output");
    let page = doc.page(0).expect("page");
    let xobjects = doc
        .resolve_dict(page.resources.get(b"XObject").expect("xobjects"))
        .expect("resolve")
        .expect("dictionary");
    let form = doc
        .resolve_stream(xobjects.get(b"Nested").expect("form"))
        .expect("resolve")
        .expect("stream");
    let resources = doc
        .resolve_dict(form.dict.get(b"Resources").expect("form resources"))
        .expect("resolve")
        .expect("dictionary");
    let fonts = doc
        .resolve_dict(resources.get(b"Font").expect("fonts"))
        .expect("resolve")
        .expect("dictionary");
    let font = doc
        .resolve_dict(fonts.get(b"Nested").expect("nested font"))
        .expect("resolve")
        .expect("dictionary");
    assert_eq!(read_run(&doc, &font, b"Hello").0, "Hello");
}

#[test]
fn pdfa_preserves_symbolic_fonts_with_explicit_latin_extraction_encodings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let originals = [Standard14::Symbol, Standard14::ZapfDingbats];
    let mut builder = PdfBuilder::new();
    for original in originals {
        let font: Dict = [
            ("Type".into(), Object::name("Font")),
            ("Subtype".into(), Object::name("Type1")),
            ("BaseFont".into(), Object::name(original.metrics().name)),
            ("Encoding".into(), Object::name("WinAnsiEncoding")),
        ]
        .into_iter()
        .collect();
        builder
            .page(400.0, 300.0)
            .resource("Font", "Symbol", font.into())
            .raw_content(b"BT /Symbol 12 Tf 40 100 Td (Hello, PDF!) Tj ET");
    }
    let source = builder.save(dir.path().join("symbols.pdf"));
    let out = dir.path().join("symbols.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(out).expect("output");
    for (page, original) in originals.iter().enumerate() {
        let (text, advances) = read_run(&doc, &page_font(&doc, page), b"Hello, PDF!");
        assert_eq!(
            text, "Hello, PDF!",
            "the extraction encoding is independent of the symbolic glyphs"
        );
        let expected: Vec<f64> = b"Hello, PDF!"
            .iter()
            .map(|code| f64::from(original.code_width(*code)))
            .collect();
        assert_eq!(
            advances, expected,
            "symbolic fonts keep their built-in metrics"
        );
    }
}

#[test]
fn pdfa_normalizes_annotation_visibility_and_keeps_unrelated_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let page = builder.page(200.0, 200.0);
    for (subtype, flags) in [
        ("Link", None),
        ("Link", Some(1 | 2 | 8 | 16 | 32 | 64 | 128 | 256)),
        ("Popup", None),
    ] {
        let mut annotation: Dict = [
            ("Type".into(), Object::name("Annot")),
            ("Subtype".into(), Object::name(subtype)),
            (
                "Rect".into(),
                Object::Array(vec![10.into(), 20.into(), 50.into(), 60.into()]),
            ),
        ]
        .into_iter()
        .collect();
        if let Some(flags) = flags {
            annotation.insert("F", Object::Integer(flags));
        }
        page.annotation(annotation);
    }
    let source = builder.save(dir.path().join("annotations.pdf"));
    let out = dir.path().join("annotations.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(out).expect("output");
    let page = doc.page(0).expect("page");
    let annotations = doc
        .resolve_array(page.dict.get(b"Annots").expect("annotations"))
        .expect("resolve")
        .expect("array");
    let flags: Vec<_> = annotations
        .iter()
        .map(|value| {
            doc.resolve_dict(value)
                .expect("resolve")
                .expect("annotation")
                .get_i64(b"F")
        })
        .collect();
    assert_eq!(flags, [Some(4), Some(4 | 8 | 16 | 64 | 128), None]);
}

#[test]
fn pdfa_keeps_distinct_advances_for_two_codes_with_the_same_glyph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let encoding: Dict = [
        ("BaseEncoding".into(), Object::name("WinAnsiEncoding")),
        (
            "Differences".into(),
            Object::Array(vec![
                Object::Integer(65),
                Object::name("eacute"),
                Object::name("eacute"),
            ]),
        ),
    ]
    .into_iter()
    .collect();
    let font: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("Type1")),
        ("BaseFont".into(), Object::name("Helvetica")),
        ("Encoding".into(), encoding.into()),
        ("FirstChar".into(), Object::Integer(65)),
        ("LastChar".into(), Object::Integer(66)),
        (
            "Widths".into(),
            Object::Array(vec![Object::Integer(333), Object::Integer(701)]),
        ),
    ]
    .into_iter()
    .collect();
    builder
        .page(200.0, 200.0)
        .resource("Font", "Aliases", font.into())
        .raw_content(b"BT /Aliases 10 Tf 20 100 Td (AB) Tj ET");
    let source = builder.save(dir.path().join("aliases.pdf"));
    let out = dir.path().join("aliases.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(out).expect("output");
    assert_eq!(
        read_run(&doc, &page_font(&doc, 0), b"AB"),
        ("éé".to_owned(), vec![333.0, 701.0])
    );
}

#[test]
fn pdfa_keeps_the_cid_collection_of_a_retained_encoding_cmap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut builder = PdfBuilder::new();
    let descriptor: Dict = [
        ("Type".into(), Object::name("FontDescriptor")),
        ("FontName".into(), Object::name("Helvetica")),
        ("Flags".into(), Object::Integer(32)),
        ("ItalicAngle".into(), Object::Integer(0)),
        ("Ascent".into(), Object::Integer(800)),
        ("Descent".into(), Object::Integer(-200)),
        ("CapHeight".into(), Object::Integer(700)),
        ("StemV".into(), Object::Integer(80)),
        (
            "FontBBox".into(),
            Object::Array(vec![(-100).into(), (-300).into(), 1200.into(), 1000.into()]),
        ),
    ]
    .into_iter()
    .collect();
    let collection: Dict = [
        ("Registry".into(), Object::text("Adobe")),
        ("Ordering".into(), Object::text("RetainedFixture")),
        ("Supplement".into(), Object::Integer(1)),
    ]
    .into_iter()
    .collect();
    let descendant: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("CIDFontType2")),
        ("BaseFont".into(), Object::name("Helvetica")),
        ("DW".into(), Object::Integer(800)),
        ("FontDescriptor".into(), descriptor.into()),
        ("CIDSystemInfo".into(), collection.clone().into()),
        (
            "W".into(),
            Object::Array(vec![
                Object::Integer(7),
                Object::Integer(7),
                Object::Integer(333),
                Object::Integer(8),
                Object::Array(vec![Object::Integer(701)]),
            ]),
        ),
    ]
    .into_iter()
    .collect();
    let encoding: Dict = [
        ("Type".into(), Object::name("CMap")),
        ("CMapName".into(), Object::name("RetainedFixture")),
        ("CIDSystemInfo".into(), collection.into()),
        ("WMode".into(), Object::Integer(0)),
    ]
    .into_iter()
    .collect();
    let program = b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (RetainedFixture) /Supplement 1 >> def\n/CMapName /RetainedFixture def\n/CMapType 1 def\n/WMode 0 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n1 begincidrange\n<0007> <0008> 7\nendcidrange\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
    let font: Dict = [
        ("Type".into(), Object::name("Font")),
        ("Subtype".into(), Object::name("Type0")),
        ("BaseFont".into(), Object::name("Helvetica")),
        (
            "Encoding".into(),
            Stream::new(encoding, program.to_vec()).into(),
        ),
        (
            "DescendantFonts".into(),
            Object::Array(vec![descendant.into()]),
        ),
        (
            "ToUnicode".into(),
            Stream::new(
                Dict::new(),
                pdf_font::write_to_unicode_cmap(&[(7, "é"), (8, "A")]),
            )
            .into(),
        ),
    ]
    .into_iter()
    .collect();
    builder
        .page(200.0, 200.0)
        .resource("Font", "Retained", font.into())
        .raw_content(b"BT /Retained 10 Tf 20 100 Td <00070008> Tj ET");
    let source = builder.save(dir.path().join("retained.pdf"));
    let out = dir.path().join("retained.pdfa.pdf");
    convert(dir.path(), &source, &out).expect("conversion");
    let doc = Document::open(out).expect("output");
    assert_eq!(
        read_run(&doc, &page_font(&doc, 0), &[0, 7, 0, 8]),
        ("éA".to_owned(), vec![333.0, 701.0])
    );
}
