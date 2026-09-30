use pdf_core::{
    Dict, Document, ObjRef, Object, PdfString, Rect, SaveOptions, Stream, parse_indirect_at,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn dict(entries: impl IntoIterator<Item = (&'static str, Object)>) -> Dict {
    entries
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

fn sample() -> Result<Document, pdf_core::Error> {
    let mut doc = Document::new();
    let font = doc.add(dict([
        ("Type", Object::name("Font")),
        ("Subtype", Object::name("Type1")),
        ("BaseFont", Object::name("Helvetica")),
    ]));
    let resources = doc.add(dict([("Font", dict([("F1", font.into())]).into())]));
    for index in 0..3 {
        let id = doc.insert_blank_page(index, Rect::new(0.0, 0.0, 300.0, 400.0))?;
        let mut page = doc.page(index)?.dict;
        let content = doc.add(Stream::new(
            Dict::new(),
            format!("BT /F1 12 Tf 20 30 Td (Page {index}) Tj ET").into_bytes(),
        ));
        page.insert("Contents", content);
        page.remove(b"MediaBox");
        page.remove(b"Resources");
        doc.set(id, page);
    }
    let root = doc
        .catalog()?
        .get_ref(b"Pages")
        .ok_or_else(|| pdf_core::Error::Invalid("Pages".into()))?;
    let mut pages = doc
        .get(root)?
        .as_dict()
        .cloned()
        .ok_or_else(|| pdf_core::Error::Invalid("pages dict".into()))?;
    pages.insert("MediaBox", Rect::new(0.0, 0.0, 300.0, 400.0).to_object());
    pages.insert("Rotate", 90);
    pages.insert("Resources", resources);
    doc.set(root, pages);
    let binary = doc.add(Stream::new(Dict::new(), (0..=255).collect()));
    let extra = doc.add(vec![
        Object::Null,
        false.into(),
        (-15).into(),
        0.125.into(),
        PdfString::literal(b"a (b) \\ \n\r\0".to_vec()).into(),
        PdfString::hex(vec![0, 128, 255]).into(),
        Object::name("a/#b"),
        dict([("Nested", vec![Object::from(binary)].into())]).into(),
    ]);
    let mut catalog = doc.catalog()?;
    catalog.insert("Extra", extra);
    doc.set(doc.catalog_ref()?, catalog);
    doc.set_info(dict([("Title", Object::text("Original title \u{3bb}"))]));
    Ok(doc)
}

fn comparable(doc: &Document, id: ObjRef) -> Result<Object, pdf_core::Error> {
    let mut object = doc.get(id)?;
    if let Object::Stream(stream) = &mut object {
        stream.set_decoded(doc.decode_stream(stream)?.data);
        stream.dict.remove(b"Length");
    }
    Ok(object)
}

macro_rules! roundtrip {
    ($name:ident, $packed:expr) => {
        #[test]
        fn $name() -> TestResult {
            // A full rewrite preserves every object and loads using its advertised xref format without repair.
            let source = sample()?;
            let bytes = source.save_to_bytes(&SaveOptions {
                object_streams: $packed,
                ..SaveOptions::default()
            })?;
            let loaded = Document::load(bytes.clone())?;
            assert!(!loaded.was_repaired());
            for id in source.object_ids() {
                assert_eq!(comparable(&loaded, id)?, comparable(&source, id)?, "{id}");
            }
            assert_eq!(bytes.windows(5).any(|s| s == b"\nxref"), !$packed);
            assert_eq!(
                loaded.object_ids().into_iter().any(|id| {
                    loaded
                        .get(id)
                        .is_ok_and(|o| o.as_stream().is_some_and(|s| s.dict.has_type(b"ObjStm")))
                }),
                $packed
            );
            Ok(())
        }
    };
}
roundtrip!(classic_xref_roundtrip, false);
roundtrip!(xref_and_object_stream_roundtrip, true);

macro_rules! incremental {
    ($name:ident, $packed:expr) => {
        #[test]
        fn $name() -> TestResult {
            // An incremental save preserves the original bytes and uses its new section to override an existing object.
            let bytes = sample()?.save_to_bytes(&SaveOptions {
                object_streams: $packed,
                ..SaveOptions::default()
            })?;
            let mut doc = Document::load(bytes.clone())?;
            let info_id = doc.trailer().get_ref(b"Info").ok_or("info reference")?;
            doc.set_info(dict([("Title", Object::text("Revised title"))]));
            let update = doc.save_incremental(false)?;
            assert_eq!(update.update_offset, bytes.len());
            assert_eq!(&update.data[..update.update_offset], bytes);
            let offset = update
                .offsets
                .iter()
                .find(|(id, _)| *id == info_id)
                .ok_or("updated info offset")?
                .1;
            let (id, object, _) = parse_indirect_at(&update.data, offset)?;
            assert_eq!((id, object), (info_id, doc.get(info_id)?));
            assert_eq!(
                update.data[update.xref_offset..].starts_with(b"xref"),
                !$packed
            );
            let loaded = Document::load(update.data)?;
            assert!(!loaded.was_repaired());
            assert_eq!(loaded.get(info_id)?, doc.get(info_id)?);
            assert_eq!(loaded.page_count()?, 3);
            Ok(())
        }
    };
}
incremental!(incremental_table_overrides_object, false);
incremental!(incremental_stream_overrides_object, true);

macro_rules! repair {
    ($name:ident, $packed:expr, $replacement:expr) => {
        #[test]
        fn $name() -> TestResult {
            // Reconstructing a damaged startxref recovers the document's indirect objects, including packed ones.
            let source = sample()?;
            let mut bytes = source.save_to_bytes(&SaveOptions {
                object_streams: $packed,
                ..SaveOptions::default()
            })?;
            let start = bytes
                .windows(9)
                .rposition(|s| s == b"startxref")
                .ok_or("startxref")?;
            bytes.truncate(start);
            bytes.extend_from_slice($replacement);
            let loaded = Document::load(bytes)?;
            assert!(loaded.was_repaired());
            assert_eq!(loaded.page_count()?, 3);
            for id in source.object_ids() {
                assert_eq!(comparable(&loaded, id)?, comparable(&source, id)?, "{id}");
            }
            Ok(())
        }
    };
}
repair!(
    repair_offset_past_eof,
    false,
    b"startxref\n999999999\n%%EOF\n"
);
repair!(repair_missing_startxref, false, b"%%EOF\n");
repair!(repair_packed_objects, true, b"startxref\n0\n%%EOF\n");

#[test]
fn linearized_offsets_and_inherited_pages() -> TestResult {
    // Linearization locates the first page and hint stream correctly while preserving inherited page appearance.
    let source = sample()?;
    let bytes = source.save_to_bytes(&SaveOptions {
        linearize: true,
        compress_streams: true,
        ..SaveOptions::default()
    })?;
    let first = bytes
        .split_inclusive(|&b| b == b'\n')
        .take(2)
        .map(<[u8]>::len)
        .sum();
    let (_, object, _) = parse_indirect_at(&bytes, first)?;
    let linear = object.as_dict().ok_or("linearization dictionary")?;
    let loaded = Document::load(bytes.clone())?;
    assert!(!loaded.was_repaired());
    assert_eq!(linear.get_i64(b"Linearized"), Some(1));
    assert_eq!(linear.get_i64(b"L"), Some(bytes.len() as i64));
    assert_eq!(linear.get_i64(b"N"), Some(3));
    assert_eq!(
        linear.get_i64(b"O"),
        Some(i64::from(loaded.page(0)?.id.num))
    );
    let hints = linear.get_array(b"H").ok_or("hint range")?;
    let hint_offset = hints[0].as_i64().ok_or("hint offset")? as usize;
    let (hint_id, hint, _) = parse_indirect_at(&bytes, hint_offset)?;
    assert!(
        hint.as_stream()
            .is_some_and(|s| s.dict.get_i64(b"S").is_some())
    );
    assert_eq!(loaded.get(hint_id)?, hint);
    let mut xref_zero = linear.get_i64(b"T").ok_or("xref offset")? as usize;
    while bytes[xref_zero].is_ascii_whitespace() {
        xref_zero += 1;
    }
    assert!(bytes[xref_zero..].starts_with(b"0000000000 65535 f"));
    for index in 0..3 {
        let before = source.page(index)?;
        let after = loaded.page(index)?;
        assert_eq!(loaded.page_content(&after)?, source.page_content(&before)?);
        assert_eq!(
            (after.media_box(), after.rotation()),
            (before.media_box(), before.rotation())
        );
        let fonts = loaded.resolve_key(&after.resources, b"Font")?;
        let font = loaded.resolve_key(fonts.as_dict().ok_or("font resources")?, b"F1")?;
        assert_eq!(
            font.as_dict().and_then(|d| d.get_name(b"BaseFont")),
            Some(b"Helvetica".as_slice())
        );
    }
    Ok(())
}

#[test]
fn concurrent_reads_share_a_consistent_document() -> TestResult {
    // Concurrent page readers observe the same decoded content through the lazy object cache.
    let doc = Document::load(sample()?.save_to_bytes(&SaveOptions {
        object_streams: true,
        ..SaveOptions::default()
    })?)?;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let doc = &doc;
                scope.spawn(move || {
                    let page = doc.page(index % 3)?;
                    doc.page_content(&page)
                })
            })
            .collect();
        for (index, handle) in handles.into_iter().enumerate() {
            let content = handle.join().map_err(|_| "page reader panicked")??;
            assert_eq!(
                content,
                format!("BT /F1 12 Tf 20 30 Td (Page {}) Tj ET", index % 3).as_bytes()
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

#[test]
fn changing_trailer_root_invalidates_cached_pages() -> TestResult {
    // Replacing the trailer's catalog makes subsequent page queries use the new page tree.
    let mut doc = sample()?;
    let empty = doc.add(dict([
        ("Type", Object::name("Pages")),
        ("Kids", Vec::<Object>::new().into()),
        ("Count", 0.into()),
    ]));
    let catalog = doc.add(dict([
        ("Type", Object::name("Catalog")),
        ("Pages", empty.into()),
    ]));
    assert_eq!(doc.page_count()?, 3);
    doc.trailer_mut().insert("Root", catalog);
    assert_eq!(doc.page_count()?, 0);
    Ok(())
}
