//! Public font contracts. The system-font cases require the macOS Helvetica
//! collection and an installed face covering Japanese; missing assets fail.

use pdf_font::{
    BaseEncoding, CMap, CharCode, CidCollection, Encoding, Font, FontKind, FontLocator,
    FontRequest, MatchQuality, PathOp, Rect, Script, Standard14, ToUnicodeMap,
    glyph_name_to_unicode, subset_truetype, write_to_unicode_cmap,
};

const HELVETICA_TTC: &str = "/System/Library/Fonts/Helvetica.ttc";

// ---- Type 1 fixture -------------------------------------------------------------------------

enum Cs {
    N(i32),
    Op(u8),
    Esc(u8),
}

fn encrypt(plain: &[u8], mut r: u16) -> Vec<u8> {
    plain
        .iter()
        .map(|&p| {
            let c = p ^ (r >> 8) as u8;
            r = u16::from(c)
                .wrapping_add(r)
                .wrapping_mul(52845)
                .wrapping_add(22719);
            c
        })
        .collect()
}

fn charstring(items: &[Cs]) -> Vec<u8> {
    let mut plain = vec![0u8; 4];
    for item in items {
        match *item {
            Cs::N(v @ -107..=107) => plain.push((v + 139) as u8),
            Cs::N(v @ 108..=1131) => {
                plain.extend_from_slice(&[((v - 108) / 256 + 247) as u8, ((v - 108) % 256) as u8])
            }
            Cs::N(v @ -1131..=-108) => {
                plain.extend_from_slice(&[((-v - 108) / 256 + 251) as u8, ((-v - 108) % 256) as u8])
            }
            Cs::N(v) => {
                plain.push(255);
                plain.extend_from_slice(&v.to_be_bytes());
            }
            Cs::Op(op) => plain.push(op),
            Cs::Esc(op) => plain.extend_from_slice(&[12, op]),
        }
    }
    encrypt(&plain, 4330)
}

const T1_CLEAR: &str = "%!PS-AdobeFont-1.0: TestT1 001.000\n12 dict begin\n/FontName /TestT1 def\n/FontType 1 def\n\
/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n/FontBBox {0 0 600 900} readonly def\n\
/Encoding StandardEncoding def\ncurrentdict end\ncurrentfile eexec\n";

/// The eexec-encrypted private part: `.notdef`, a square `A`, a square `acute` at (100,800),
/// and `Aacute` built with seac (asb 100, adx 150).
fn t1_encrypted() -> Vec<u8> {
    use Cs::{Esc, N, Op};
    let glyphs = [
        (".notdef", charstring(&[N(0), N(250), Op(13), Op(14)])),
        (
            "A",
            charstring(&[
                N(50),
                N(600),
                Op(13),
                N(0),
                N(0),
                Op(21),
                N(500),
                N(0),
                Op(5),
                N(0),
                N(700),
                Op(5),
                N(-500),
                N(0),
                Op(5),
                Op(9),
                Op(14),
            ]),
        ),
        (
            "acute",
            charstring(&[
                N(100),
                N(300),
                Op(13),
                N(0),
                N(800),
                Op(21),
                N(100),
                N(0),
                Op(5),
                N(0),
                N(100),
                Op(5),
                N(-100),
                N(0),
                Op(5),
                Op(9),
                Op(14),
            ]),
        ),
        (
            "Aacute",
            charstring(&[
                N(50),
                N(600),
                Op(13),
                N(100),
                N(150),
                N(0),
                N(65),
                N(194),
                Esc(6),
            ]),
        ),
    ];
    let mut private = vec![0u8; 4];
    private.extend_from_slice(
        b"dup /Private 8 dict dup begin\n/RD {string currentfile exch readstring pop} executeonly def\n\
/ND {noaccess def} executeonly def\n/NP {noaccess put} executeonly def\n/lenIV 4 def\n/Subrs 0 array\nND\n\
2 index /CharStrings 4 dict dup begin\n",
    );
    for (name, cs) in &glyphs {
        private.extend_from_slice(format!("/{name} {} RD ", cs.len()).as_bytes());
        private.extend_from_slice(cs);
        private.extend_from_slice(b" ND\n");
    }
    private.extend_from_slice(b"end\nend\nreadonly put\nnoaccess put\ndup /FontName get exch definefont pop\nmark currentfile closefile\n");
    encrypt(&private, 55665)
}

fn t1_trailer() -> Vec<u8> {
    let mut t = Vec::new();
    for _ in 0..8 {
        t.extend_from_slice(&[b'0'; 64]);
        t.push(b'\n');
    }
    t.extend_from_slice(b"cleartomark\n");
    t
}

fn t1_binary() -> Vec<u8> {
    [T1_CLEAR.as_bytes().to_vec(), t1_encrypted(), t1_trailer()].concat()
}

fn t1_hex() -> Vec<u8> {
    let hex: Vec<u8> = t1_encrypted()
        .chunks(32)
        .flat_map(|line| {
            line.iter()
                .map(|b| format!("{b:02x}"))
                .chain(["\n".to_owned()])
                .collect::<String>()
                .into_bytes()
        })
        .collect();
    [T1_CLEAR.as_bytes().to_vec(), hex, t1_trailer()].concat()
}

fn t1_pfb() -> Vec<u8> {
    let segment = |kind: u8, body: &[u8]| {
        let mut s = vec![0x80, kind];
        s.extend_from_slice(&(body.len() as u32).to_le_bytes());
        s.extend_from_slice(body);
        s
    };
    [
        segment(1, T1_CLEAR.as_bytes()),
        segment(2, &t1_encrypted()),
        segment(1, &t1_trailer()),
        vec![0x80, 3],
    ]
    .concat()
}

fn rect(x_min: f32, y_min: f32, x_max: f32, y_max: f32) -> Rect {
    Rect {
        x_min,
        y_min,
        x_max,
        y_max,
    }
}

#[test]
fn type1_binary_hex_and_pfb_forms_decode_the_same_glyphs() {
    for (form, data) in [
        ("binary", t1_binary()),
        ("hex", t1_hex()),
        ("pfb", t1_pfb()),
    ] {
        let font = Font::parse(data).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert_eq!(font.kind(), FontKind::Type1, "{form}");
        assert_eq!(font.postscript_name().as_deref(), Some("TestT1"), "{form}");
        assert_eq!(font.num_glyphs(), 4, "{form}");
        assert_eq!(font.glyph_name(0).as_deref(), Some(".notdef"), "{form}");
        let a = font
            .glyph_by_name("A")
            .unwrap_or_else(|| panic!("{form}: no A"));
        assert_eq!(font.advance(a), Some(600.0), "{form}");
        assert_eq!(
            font.outline(a).ok().and_then(|o| o.bounds()),
            Some(rect(50.0, 0.0, 550.0, 700.0)),
            "{form}"
        );
        assert_eq!(font.units_per_em(), 1000, "{form}");
    }
}

#[test]
fn type1_seac_places_accent_at_adx_plus_sidebearing_difference() {
    let font = Font::parse(t1_binary()).expect("fixture parses");
    let gid = font.glyph_by_name("Aacute").expect("Aacute");
    let outline = font.outline(gid).expect("seac outline");
    assert_eq!(outline.contour_count(), 2);
    // Accent origin = adx + sbx - asb = 150 + 50 - 100; its own hsbw puts the contour at x 100.
    assert!(
        outline.ops().contains(&PathOp::MoveTo(200.0, 800.0)),
        "{:?}",
        outline.ops()
    );
    assert_eq!(outline.bounds(), Some(rect(50.0, 0.0, 550.0, 900.0)));
    assert_eq!(font.advance(gid), Some(600.0));
}

#[test]
fn type1_glyph_bounds_preserve_seac_accent_placement() {
    let font = Font::parse(t1_binary()).expect("fixture parses");
    let gid = font.glyph_by_name("Aacute").expect("Aacute");
    assert_eq!(
        font.glyph_bounds(gid),
        Ok(Some(rect(50.0, 0.0, 550.0, 900.0)))
    );
}

#[test]
fn type1_empty_charstring_has_no_bounds() {
    let font = Font::parse(t1_binary()).expect("fixture parses");
    assert_eq!(font.glyph_bounds(0), Ok(None));
}

#[test]
fn type1_builtin_standard_encoding_selects_glyphs_by_code() {
    let font = Font::parse(t1_binary()).expect("fixture parses");
    let encoding = font.builtin_encoding().expect("built-in encoding");
    assert_eq!(encoding.glyph_name(65), Some("A"));
    assert_eq!(font.simple_glyph(65, None, false), font.glyph_by_name("A"));
    assert_eq!(
        font.simple_glyph(200, Some("Aacute"), false),
        font.glyph_by_name("Aacute")
    );
}

#[test]
fn truncated_type1_never_yields_a_different_glyph() {
    let full = t1_binary();
    let reference = Font::parse(full.clone()).expect("fixture parses");
    let a_outline = reference
        .outline(reference.glyph_by_name("A").expect("A"))
        .expect("A outline");
    for len in 0..full.len() {
        let Ok(font) = Font::parse(full[..len].to_vec()) else {
            continue;
        };
        for gid in 0..font.num_glyphs() {
            let outline = font.outline(gid);
            if font.glyph_name(gid).as_deref() == Some("A")
                && let Ok(outline) = outline
            {
                assert_eq!(outline, a_outline, "prefix of {len} bytes");
            }
        }
    }
}

#[test]
fn malformed_font_programs_are_rejected() {
    let cases: [(&str, Vec<u8>); 7] = [
        ("empty", Vec::new()),
        ("text", b"hello world".to_vec()),
        (
            "sfnt directory cut short",
            vec![0, 1, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0],
        ),
        (
            "OTTO without tables",
            vec![b'O', b'T', b'T', b'O', 0, 0, 0, 0, 0, 0, 0, 0],
        ),
        (
            "collection without faces",
            b"ttcf\x00\x01\x00\x00\x00\x00\x00\x00".to_vec(),
        ),
        (
            "collection face past end",
            b"ttcf\x00\x01\x00\x00\x00\x00\x00\x01\xFF\xFF\xFF\x00".to_vec(),
        ),
        (
            "pfb segment longer than data",
            vec![0x80, 1, 0xFF, 0xFF, 0xFF, 0x7F, b'%', b'!'],
        ),
    ];
    for (label, data) in cases {
        assert!(Font::parse(data).is_err(), "{label} parsed");
    }
}

// ---- CMaps and ToUnicode ----------------------------------------------------------------

#[test]
fn to_unicode_handles_array_ranges_surrogates_and_ligatures() {
    let map = ToUnicodeMap::parse(
        b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
2 beginbfchar\n<0003> <0020>\n<0010> <D83DDE00>\nendbfchar\n\
2 beginbfrange\n<0041> <0043> [<0061> <0066006C> <0063>]\n<0100> <0102> <0041>\nendbfrange\n\
endcmap CMapName currentdict /CMap defineresource pop end end",
    )
    .expect("parses");
    let cases: [(u32, Option<&str>); 8] = [
        (0x03, Some(" ")),
        (0x10, Some("\u{1F600}")),
        (0x41, Some("a")),
        (0x42, Some("fl")),
        (0x43, Some("c")),
        (0x100, Some("A")),
        (0x102, Some("C")),
        (0x103, None),
    ];
    for (code, want) in cases {
        assert_eq!(map.lookup(code).as_deref(), want, "code {code:#x}");
    }
}

#[test]
fn written_to_unicode_cmap_reads_back() {
    let mut entries: Vec<(u16, String)> = (1..=250u16)
        .map(|c| {
            (
                c,
                char::from_u32(0x4E00 + u32::from(c))
                    .map(String::from)
                    .unwrap_or_default(),
            )
        })
        .collect();
    entries.push((300, "ffi".to_owned()));
    entries.push((301, "\u{1D400}".to_owned()));
    let borrowed: Vec<(u16, &str)> = entries.iter().map(|(c, t)| (*c, t.as_str())).collect();
    let map = ToUnicodeMap::parse(&write_to_unicode_cmap(&borrowed)).expect("parses");
    for (code, text) in &entries {
        assert_eq!(
            map.lookup(u32::from(*code)).as_deref(),
            Some(text.as_str()),
            "code {code}"
        );
    }
    assert_eq!(map.lookup(0), None);
}

#[test]
fn identity_h_splits_bytes_into_two_byte_cids() {
    let cmap = CMap::predefined("Identity-H").expect("Identity-H");
    assert_eq!(
        cmap.decode(&[0x00, 0x41, 0x30, 0x42]),
        vec![
            (CharCode { code: 0x41, len: 2 }, 0x41),
            (
                CharCode {
                    code: 0x3042,
                    len: 2
                },
                0x3042
            )
        ]
    );
}

#[test]
fn unijis_ucs2_h_maps_known_codes() {
    let cmap = CMap::predefined("UniJIS-UCS2-H").expect("UniJIS-UCS2-H");
    for (code, cid) in [
        (0x0020, 1),
        (0x005C, 97),
        (0x00A5, 61),
        (0x00A7, 720),
        (0x0100, 9366),
        (0x0131, 146),
    ] {
        assert_eq!(cmap.cid(CharCode { code, len: 2 }), cid, "code {code:#06x}");
    }
    assert_eq!(cmap.collection(), Some(CidCollection::Japan1));
}

#[test]
fn japan1_cids_map_back_to_their_unicode() {
    let cmap = CMap::predefined("UniJIS-UCS2-H").expect("UniJIS-UCS2-H");
    for ch in ['A', 'あ', 'ア', '漢', '。'] {
        let cid = cmap.cid(CharCode {
            code: u32::from(ch),
            len: 2,
        });
        assert_ne!(cid, 0, "{ch}");
        assert_eq!(
            CidCollection::Japan1.cid_to_unicode(cid, false),
            Some(ch),
            "cid {cid}"
        );
    }
}

#[test]
fn embedded_cmap_splits_mixed_length_codes() {
    let cmap = CMap::parse(
        b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
/CMapName /Test-Mixed def\n\
2 begincodespacerange\n<00> <80>\n<8140> <9FFC>\nendcodespacerange\n\
1 begincidrange\n<20> <7E> 1\nendcidrange\n\
1 begincidchar\n<8140> 633\nendcidchar\nendcmap",
    )
    .expect("parses");
    assert_eq!(
        cmap.decode(&[0x41, 0x81, 0x40, 0x42]),
        vec![
            (CharCode { code: 0x41, len: 1 }, 34),
            (
                CharCode {
                    code: 0x8140,
                    len: 2
                },
                633
            ),
            (CharCode { code: 0x42, len: 1 }, 35),
        ]
    );
}

#[test]
fn usecmap_layers_embedded_mappings_over_a_predefined_cmap() {
    let cmap = CMap::parse(
        b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n/CMapName /Test-Over def\n\
/UniJIS-UCS2-H usecmap\n1 begincidchar\n<0041> 5000\nendcidchar\nendcmap",
    )
    .expect("parses");
    assert_eq!(cmap.cid(CharCode { code: 0x41, len: 2 }), 5000);
    assert_eq!(cmap.cid(CharCode { code: 0x20, len: 2 }), 1);
    assert_eq!(
        cmap.decode(&[0x00, 0x20]),
        vec![(CharCode { code: 0x20, len: 2 }, 1)]
    );
}

// ---- Encodings and standard-14 metrics -----------------------------------------------------

#[test]
fn differences_resolve_through_agl_and_uni_names() {
    let mut encoding = Encoding::new(Some(BaseEncoding::WinAnsi));
    encoding.apply_differences([(0x41u8, &["uni0391", "u1F600", "f_i"][..])]);
    assert_eq!(encoding.to_unicode(0x41).as_deref(), Some("\u{391}"));
    assert_eq!(encoding.to_unicode(0x42).as_deref(), Some("\u{1F600}"));
    assert_eq!(encoding.to_unicode(0x43).as_deref(), Some("fi"));
    assert_eq!(encoding.to_unicode(0x80).as_deref(), Some("€"));
    assert_eq!(glyph_name_to_unicode("a.sc").as_deref(), Some("a"));
}

#[test]
fn base_encodings_round_trip_unicode() {
    let cases: [(BaseEncoding, u8, char); 4] = [
        (BaseEncoding::WinAnsi, 0x80, '€'),
        (BaseEncoding::MacRoman, 0xA5, '•'),
        (BaseEncoding::Symbol, 0x61, 'α'),
        (BaseEncoding::ZapfDingbats, 0x21, '✁'),
    ];
    for (encoding, code, ch) in cases {
        assert_eq!(
            encoding.to_unicode(code),
            Some(ch),
            "{encoding:?} {code:#x}"
        );
        assert_eq!(encoding.from_unicode(ch), Some(code), "{encoding:?} {ch}");
    }
}

#[test]
fn standard14_text_width_matches_pymupdf_helv() {
    for (text, units) in [("Hello World", 5167.0f32), ("Page 1 of 3", 5115.0)] {
        let width = Standard14::Helvetica.text_width(text, 11.0);
        assert!((width - units * 0.011).abs() < 1e-3, "{text}: {width}");
    }
    assert_eq!(Standard14::Courier.code_width(b'A'), 600);
    assert_eq!(Standard14::Symbol.code_width(0x61), 631);
    assert_eq!(
        Standard14::from_base_font("ABCDEF+Helvetica-Bold"),
        Some(Standard14::HelveticaBold)
    );
}

// ---- TrueType subsetting and system fonts ----------------------------------------------------

#[test]
fn truetype_subset_keeps_composite_components_and_reparses() {
    let data = std::fs::read(HELVETICA_TTC)
        .expect("font contracts require the macOS Helvetica collection");
    let font = Font::parse_face(data, 0).expect("Helvetica face 0");
    let aring = font.glyph_for_char('Å').expect("Å");
    let b = font.glyph_for_char('b').expect("b");
    let subset = subset_truetype(&font, &[aring, b]).expect("subset");
    // .notdef, Å, b, and Å's components.
    assert!(subset.glyph_map.len() > 3, "{:?}", subset.glyph_map);

    let words: u32 = subset
        .data
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_be_bytes(w)
        })
        .fold(0u32, u32::wrapping_add);
    assert_eq!(words, 0xB1B0_AFBA, "whole-font checksum");

    let reparsed = Font::parse(subset.data.clone()).expect("subset parses");
    assert_eq!(usize::from(reparsed.num_glyphs()), subset.glyph_map.len());
    for (old, ch) in [(aring, 'Å'), (b, 'b')] {
        let new = subset.new_gid(old).expect("mapped");
        assert_eq!(reparsed.glyph_for_char(ch), Some(new), "{ch}");
        assert_eq!(
            reparsed.outline(new).expect("outline"),
            font.outline(old).expect("outline"),
            "{ch}"
        );
        assert_eq!(reparsed.advance(new), font.advance(old), "{ch}");
        let expected =
            font.advance(old).expect("advance") * 1000.0 / f32::from(font.units_per_em());
        assert!(
            (subset.pdf_widths()[usize::from(new)] - expected).abs() < 1e-3,
            "{ch}"
        );
    }
}

#[test]
fn system_locator_finds_exact_standard_faces() {
    let request = FontRequest {
        base_font: "ABCDEF+Helvetica-Bold",
        flags: 0,
        weight: None,
        script: Script::Latin,
    };
    let found = FontLocator::system().find(&request).expect("substitute");
    assert_eq!(found.quality, MatchQuality::Exact);
    let font = found.load().expect("loads");
    assert_eq!(font.postscript_name().as_deref(), Some("Helvetica-Bold"));
}

#[test]
fn system_locator_covers_japanese_requests_with_kana() {
    let request = FontRequest {
        base_font: "MS-Mincho",
        flags: 0,
        weight: None,
        script: Script::Japanese,
    };
    let found = FontLocator::system()
        .find(&request)
        .expect("font contracts require a system face covering Japanese");
    let font = found.load().expect("loads");
    assert!(
        font.glyph_for_char('あ').is_some(),
        "{}",
        found.path.display()
    );
}
