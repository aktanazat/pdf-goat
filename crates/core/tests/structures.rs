use pdf_core::{
    Dict, Document, Error, ObjRef, Object, Operation, Rect, SaveOptions, Stream, parse_content,
    parse_indirect_at, parse_object_at, write_content,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn dict(entries: impl IntoIterator<Item = (&'static str, Object)>) -> Dict {
    entries
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

fn page_tree() -> Result<Document, pdf_core::Error> {
    let mut doc = Document::new();
    let font = doc.add(dict([
        ("Type", Object::name("Font")),
        ("BaseFont", Object::name("Helvetica")),
    ]));
    let resources = doc.add(dict([("Font", dict([("F1", font.into())]).into())]));
    let content = doc.add(Stream::new(
        Dict::new(),
        b"BT /F1 12 Tf (shared text) Tj ET".to_vec(),
    ));
    let inner = doc.reserve();
    let root = ObjRef::new(2, 0);
    let p0 = doc.add(dict([
        ("Type", Object::name("Page")),
        ("Parent", inner.into()),
        ("Contents", content.into()),
    ]));
    let p1 = doc.add(dict([
        ("Type", Object::name("Page")),
        ("Parent", inner.into()),
        ("Contents", content.into()),
        ("MediaBox", Rect::new(0.0, 0.0, 200.0, 300.0).to_object()),
        ("Rotate", 270.into()),
    ]));
    let link = doc.add(dict([
        ("Subtype", Object::name("Link")),
        ("Dest", vec![p1.into(), Object::name("Fit")].into()),
    ]));
    let mut first = doc
        .get(p0)?
        .as_dict()
        .cloned()
        .ok_or_else(|| Error::Invalid("page".into()))?;
    first.insert("Annots", vec![link.into()]);
    doc.set(p0, first);
    doc.set(
        inner,
        dict([
            ("Type", Object::name("Pages")),
            ("Parent", root.into()),
            ("Kids", vec![p0.into(), p1.into()].into()),
            ("Count", 2.into()),
            ("CropBox", Rect::new(10.0, 10.0, 490.0, 590.0).to_object()),
            ("Rotate", 90.into()),
        ]),
    );
    doc.set(
        root,
        dict([
            ("Type", Object::name("Pages")),
            ("Kids", vec![inner.into()].into()),
            ("Count", 2.into()),
            ("MediaBox", Rect::new(0.0, 0.0, 500.0, 600.0).to_object()),
            ("Resources", resources.into()),
            ("Rotate", 180.into()),
        ]),
    );
    Ok(doc)
}

macro_rules! inheritance {
    ($name:ident, $index:expr, $media:expr, $crop:expr, $rotation:expr) => {
        #[test]
        fn $name() -> TestResult {
            // A page's attributes override the nearest ancestor's and inherited crop boxes are clipped to its media box.
            let doc = page_tree()?;
            let page = doc.page($index)?;
            assert_eq!(
                (page.media_box(), page.crop_box(), page.rotation()),
                ($media, $crop, $rotation)
            );
            let fonts = doc.resolve_key(&page.resources, b"Font")?;
            let font = doc.resolve_key(fonts.as_dict().ok_or("fonts")?, b"F1")?;
            assert_eq!(
                font.as_dict().and_then(|d| d.get_name(b"BaseFont")),
                Some(b"Helvetica".as_slice())
            );
            Ok(())
        }
    };
}
inheritance!(
    ancestor_attributes,
    0,
    Rect::new(0.0, 0.0, 500.0, 600.0),
    Rect::new(10.0, 10.0, 490.0, 590.0),
    90
);
inheritance!(
    page_overrides,
    1,
    Rect::new(0.0, 0.0, 200.0, 300.0),
    Rect::new(10.0, 10.0, 200.0, 300.0),
    270
);

macro_rules! page_import {
    ($name:ident, $indices:expr, $copy_dest:expr) => {
        #[test]
        fn $name() -> TestResult {
            // Import keeps content and inherited resources, shares copied objects, and remaps only destinations among imported pages.
            let source = page_tree()?;
            let mut target = Document::new();
            target.insert_blank_page(0, Rect::new(0.0, 0.0, 10.0, 20.0))?;
            target.import_pages(&source, $indices, 1)?;
            let target = Document::load(target.save_to_bytes(&SaveOptions::default())?)?;
            assert_eq!(target.page_count()?, $indices.len() + 1);
            let page = target.page(1)?;
            assert_eq!(
                target.page_content(&page)?,
                b"BT /F1 12 Tf (shared text) Tj ET"
            );
            assert_eq!(
                (page.media_box(), page.crop_box(), page.rotation()),
                (
                    Rect::new(0.0, 0.0, 500.0, 600.0),
                    Rect::new(10.0, 10.0, 490.0, 590.0),
                    90
                )
            );
            let fonts = target.resolve_key(&page.resources, b"Font")?;
            let font = target.resolve_key(fonts.as_dict().ok_or("fonts")?, b"F1")?;
            assert_eq!(
                font.as_dict().and_then(|d| d.get_name(b"BaseFont")),
                Some(b"Helvetica".as_slice())
            );
            let annots = target.resolve_key(&page.dict, b"Annots")?;
            let annot = target.resolve(&annots.as_array().ok_or("annotations")?[0])?;
            let dest = annot
                .as_dict()
                .and_then(|d| d.get_array(b"Dest"))
                .ok_or("destination")?;
            if $copy_dest {
                let second = target.page(2)?;
                assert_eq!(dest[0].as_reference(), Some(second.id));
                assert_eq!(page.content_refs(), second.content_refs());
                assert_eq!(page.resources, second.resources);
            } else {
                assert_eq!(dest[0], Object::Null);
            }
            Ok(())
        }
    };
}
page_import!(import_single_page, &[0_usize], false);
page_import!(import_shared_pages, &[0_usize, 1], true);

#[test]
fn blank_page_does_not_inherit_rotation_or_resources() -> TestResult {
    // A blank page inserted into a rotated subtree stays upright with no inherited drawing resources.
    let mut doc = page_tree()?;
    doc.insert_blank_page(0, Rect::new(0.0, 0.0, 100.0, 200.0))?;
    let page = doc.page(0)?;
    assert_eq!(
        (page.rotation(), page.crop_box(), page.resources),
        (0, Rect::new(0.0, 0.0, 100.0, 200.0), Dict::new())
    );
    assert_eq!(doc.page_count()?, 3);
    Ok(())
}

#[test]
fn content_roundtrip_with_inline_images() -> TestResult {
    // Inline-image data containing EI and encoded image data both survive content parsing and rewriting without losing surrounding operators.
    let raw = b"q 2 0 0 2 10 20 cm BI /W 4 /H 1 /BPC 8 /CS /G ID  EI \nEI Q BI /F /AHx ID 00ff>\nEI [(a\\(b\\)) -20 <4344>] TJ";
    let expected = vec![
        Operation::new(b"q", vec![]),
        Operation::new(
            b"cm",
            vec![2.into(), 0.into(), 0.into(), 2.into(), 10.into(), 20.into()],
        ),
        Operation::new(
            b"BI",
            vec![
                Stream::new(
                    dict([
                        ("W", 4.into()),
                        ("H", 1.into()),
                        ("BPC", 8.into()),
                        ("CS", Object::name("G")),
                    ]),
                    b" EI ".to_vec(),
                )
                .into(),
            ],
        ),
        Operation::new(b"Q", vec![]),
        Operation::new(
            b"BI",
            vec![Stream::new(dict([("F", Object::name("AHx"))]), b"00ff>".to_vec()).into()],
        ),
        Operation::new(
            b"TJ",
            vec![vec![Object::string(b"a(b)"), (-20).into(), Object::string(b"CD")].into()],
        ),
    ];
    assert_eq!(parse_content(raw)?, expected);
    assert_eq!(parse_content(&write_content(&expected))?, expected);
    Ok(())
}

#[test]
fn name_tree_reads_kids_and_writes_sorted_entries() -> TestResult {
    // Name-tree entries cross indirect Kids on read and survive replacement in byte-sorted order without changing other trees.
    let mut doc = Document::new();
    let leaf1 = doc.add(dict([(
        "Names",
        vec![
            Object::string(b"A"),
            1.into(),
            Object::string(b"B"),
            2.into(),
        ]
        .into(),
    )]));
    let leaf2 = doc.add(dict([(
        "Names",
        vec![Object::string(b"C"), 3.into()].into(),
    )]));
    let root = doc.add(dict([("Kids", vec![leaf1.into(), leaf2.into()].into())]));
    let names = doc.add(dict([("Dests", root.into())]));
    let mut catalog = doc.catalog()?;
    catalog.insert("Names", names);
    doc.set(doc.catalog_ref()?, catalog);
    assert_eq!(
        doc.names(b"Dests")?,
        vec![
            (b"A".to_vec(), 1.into()),
            (b"B".to_vec(), 2.into()),
            (b"C".to_vec(), 3.into())
        ]
    );
    doc.set_names(b"EmbeddedFiles", vec![(b"kept".to_vec(), 99.into())])?;
    doc.set_names(
        b"Dests",
        vec![
            (b"z".to_vec(), 4.into()),
            (b"A".to_vec(), 5.into()),
            (b"z".to_vec(), 6.into()),
        ],
    )?;
    let mut loaded = Document::load(doc.save_to_bytes(&SaveOptions::default())?)?;
    assert_eq!(
        loaded.names(b"Dests")?,
        vec![(b"A".to_vec(), 5.into()), (b"z".to_vec(), 6.into())]
    );
    loaded.set_names(b"Dests", vec![])?;
    assert_eq!(loaded.names(b"Dests")?, vec![]);
    assert_eq!(
        loaded.names(b"EmbeddedFiles")?,
        vec![(b"kept".to_vec(), 99.into())]
    );
    Ok(())
}

macro_rules! stream_length {
    ($name:ident, $length:literal) => {
        #[test]
        fn $name() -> TestResult {
            // A wrong or indirect stream Length is recovered from endstream without dropping payload bytes.
            let bytes = concat!(
                "42 0 obj\n<< /Length ",
                $length,
                " >>\nstream\nabc\nendstream\nendobj"
            );
            let (id, object, end) = parse_indirect_at(bytes.as_bytes(), 0)?;
            assert_eq!(id, ObjRef::new(42, 0));
            assert_eq!(object.as_stream().ok_or("stream")?.raw(), b"abc");
            assert_eq!(end, bytes.len());
            Ok(())
        }
    };
}
stream_length!(short_stream_length, "1");
stream_length!(long_stream_length, "999");
stream_length!(indirect_stream_length, "55 0 R");

#[test]
fn deeply_nested_input_is_bounded() {
    // Adversarially nested direct objects return a depth error instead of exhausting the stack.
    let mut data = vec![b'['; 1024];
    data.extend_from_slice(&vec![b']'; 1024]);
    assert!(matches!(
        parse_object_at(&data, 0),
        Err(Error::LimitExceeded(_))
    ));
}
