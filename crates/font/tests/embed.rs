//! Character codes, Unicode values and embedded glyph ids must remain distinct.
#![cfg(target_os = "macos")]

use pdf_core::{Document, Object, SaveOptions};
use pdf_font::embed::embed_truetype_with_widths;
use pdf_font::{CidToGid, EmbedError, Font, GlyphMapping, ToUnicodeMap, embed_truetype};

fn source() -> Font {
    Font::parse(std::fs::read("/System/Library/Fonts/Helvetica.ttc").expect("macOS Helvetica"))
        .expect("TrueType font")
}

#[test]
fn sparse_codes_keep_glyph_aliases_unicode_and_outlines_after_save() {
    let source = source();
    let space = source.glyph_for_char(' ').expect("space glyph");
    let omega = source.glyph_for_char('Ω').expect("Greek glyph");
    let mappings = [
        GlyphMapping {
            code: 65535,
            glyph_id: omega,
            unicode: "Ω",
        },
        GlyphMapping {
            code: 0,
            glyph_id: 0,
            unicode: "",
        },
        GlyphMapping {
            code: 17,
            glyph_id: space,
            unicode: " ",
        },
        GlyphMapping {
            code: 31,
            glyph_id: space,
            unicode: "\u{a0}",
        },
    ];
    let mut doc = Document::new();
    let id = embed_truetype(&mut doc, &source, &mappings).expect("embed");
    let bytes = doc.save_to_bytes(&SaveOptions::default()).expect("save");
    let doc = Document::load(bytes).expect("reopen");
    let font = doc
        .resolve_dict(&Object::Reference(id))
        .expect("resolve font")
        .expect("font dictionary");
    let unicode = doc
        .resolve_stream(font.get(b"ToUnicode").expect("ToUnicode"))
        .expect("resolve cmap")
        .expect("cmap stream");
    let unicode = ToUnicodeMap::parse(&doc.decode_stream(&unicode).expect("decode cmap").data)
        .expect("parse cmap");
    assert_eq!(unicode.lookup(17).as_deref(), Some(" "));
    assert_eq!(unicode.lookup(31).as_deref(), Some("\u{a0}"));
    assert_eq!(unicode.lookup(65535).as_deref(), Some("Ω"));
    assert_eq!(unicode.lookup(0), None);
    let descendant = doc
        .resolve_dict(&font.get_array(b"DescendantFonts").expect("descendants")[0])
        .expect("resolve descendant")
        .expect("descendant dictionary");
    let map = doc
        .resolve_stream(descendant.get(b"CIDToGIDMap").expect("CID map"))
        .expect("resolve CID map")
        .expect("CID map stream");
    let map = doc.decode_stream(&map).expect("decode CID map").data;
    let descriptor = doc
        .resolve_dict(descendant.get(b"FontDescriptor").expect("descriptor"))
        .expect("resolve descriptor")
        .expect("descriptor dictionary");
    let program = doc
        .resolve_stream(descriptor.get(b"FontFile2").expect("font program"))
        .expect("resolve font program")
        .expect("font stream");
    let subset = Font::parse(doc.decode_stream(&program).expect("decode subset").data)
        .expect("parse subset");
    for mapping in mappings {
        let gid = subset
            .glyph_for_cid(u32::from(mapping.code), CidToGid::Map(&map))
            .expect("subset glyph");
        assert_eq!(
            subset.outline(gid).expect("subset outline"),
            source.outline(mapping.glyph_id).expect("source outline")
        );
        assert_eq!(subset.advance(gid), source.advance(mapping.glyph_id));
    }
}

#[test]
fn conflicting_codes_are_rejected_without_changing_the_document() {
    let font = source();
    let a = font.glyph_for_char('A').expect("A");
    let b = font.glyph_for_char('B').expect("B");
    let mut doc = Document::new();
    let before = doc.xref_size();
    let result = embed_truetype(
        &mut doc,
        &font,
        &[
            GlyphMapping {
                code: 4,
                glyph_id: a,
                unicode: "A",
            },
            GlyphMapping {
                code: 4,
                glyph_id: b,
                unicode: "B",
            },
        ],
    );
    assert!(matches!(result, Err(EmbedError::InvalidMapping(_))));
    assert_eq!(doc.xref_size(), before);
}

#[test]
fn custom_advances_share_outlines_without_sharing_different_metrics() {
    let source = source();
    let accent = source.glyph_for_char('é').expect("accented glyph");
    let space = source.glyph_for_char(' ').expect("space");
    let mappings = [
        GlyphMapping {
            code: 65,
            glyph_id: accent,
            unicode: "é",
        },
        GlyphMapping {
            code: 7,
            glyph_id: accent,
            unicode: "é",
        },
        GlyphMapping {
            code: 8,
            glyph_id: accent,
            unicode: "é",
        },
        GlyphMapping {
            code: 32,
            glyph_id: space,
            unicode: " ",
        },
    ];
    let widths = [601.5, 333.0, 601.5, 400.25];
    let mut doc = Document::new();
    let id = embed_truetype_with_widths(&mut doc, &source, &mappings, &widths)
        .expect("embed custom metrics");
    let doc =
        Document::load(doc.save_to_bytes(&SaveOptions::default()).expect("save")).expect("reopen");
    let font = doc
        .resolve_dict(&Object::Reference(id))
        .expect("resolve")
        .expect("font");
    let descendant = doc
        .resolve_dict(&font.get_array(b"DescendantFonts").expect("descendants")[0])
        .expect("resolve")
        .expect("descendant");
    let map = doc
        .resolve_stream(descendant.get(b"CIDToGIDMap").expect("glyph map"))
        .expect("resolve")
        .expect("stream");
    let map = doc.decode_stream(&map).expect("decode").data;
    let descriptor = doc
        .resolve_dict(descendant.get(b"FontDescriptor").expect("descriptor"))
        .expect("resolve")
        .expect("descriptor");
    let program = doc
        .resolve_stream(descriptor.get(b"FontFile2").expect("program"))
        .expect("resolve")
        .expect("stream");
    let program = Font::parse(doc.decode_stream(&program).expect("decode").data).expect("font");
    let mut dictionary_widths = std::collections::BTreeMap::new();
    let mut entries = descendant.get_array(b"W").expect("PDF advances").iter();
    while let Some(first) = entries.next() {
        let first = usize::try_from(first.as_i64().expect("first CID")).expect("nonnegative CID");
        match entries.next().expect("width entry") {
            Object::Array(values) => {
                dictionary_widths.extend(
                    values
                        .iter()
                        .enumerate()
                        .map(|(offset, width)| (first + offset, width.as_f64().expect("advance"))),
                );
            }
            last => {
                let last =
                    usize::try_from(last.as_i64().expect("last CID")).expect("nonnegative CID");
                let width = entries
                    .next()
                    .expect("range width")
                    .as_f64()
                    .expect("advance");
                dictionary_widths.extend((first..=last).map(|cid| (cid, width)));
            }
        }
    }
    for (mapping, expected) in mappings.iter().zip(widths) {
        let gid = program
            .glyph_for_cid(u32::from(mapping.code), CidToGid::Map(&map))
            .expect("mapped glyph");
        assert_eq!(
            program.outline(gid).expect("subset outline"),
            source.outline(mapping.glyph_id).expect("original outline")
        );
        let actual = f64::from(program.advance(gid).expect("advance")) * 1000.0
            / f64::from(program.units_per_em());
        assert!(
            (actual - expected).abs() <= 1.0,
            "advance {actual} must match PDF width {expected}"
        );
        assert_eq!(
            dictionary_widths[&usize::from(mapping.code)],
            expected,
            "font-unit rounding must not change PDF text positioning"
        );
    }
}

#[test]
fn invalid_custom_advances_leave_the_document_unchanged() {
    let font = source();
    let gid = font.glyph_for_char('A').expect("A");
    let mapping = [GlyphMapping {
        code: 4,
        glyph_id: gid,
        unicode: "A",
    }];
    let mut doc = Document::new();
    let before = doc.xref_size();
    for widths in [&[][..], &[f64::NAN], &[-1.0], &[1_000_000.0]] {
        assert!(matches!(
            embed_truetype_with_widths(&mut doc, &font, &mapping, widths),
            Err(EmbedError::InvalidMapping(_))
        ));
        assert_eq!(
            doc.xref_size(),
            before,
            "invalid metrics must not partially add a font"
        );
    }
}
