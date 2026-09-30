use pdf_core::{
    Dict, Error, Name, Object,
    filters::{decode, flate_encode},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

macro_rules! known_answer {
    ($name:ident, $filter:literal, $input:expr, $output:expr) => {
        #[test]
        fn $name() -> TestResult {
            // The filter decodes its independently established known-answer bytes exactly.
            let result = decode($input, &[(Name::from($filter), None)], 4096)?;
            assert_eq!(result.data, $output);
            assert_eq!(result.stopped, None);
            Ok(())
        }
    };
}
known_answer!(
    flate,
    "FlateDecode",
    &[
        0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x06, 0x2c, 0x02, 0x15
    ],
    b"hello"
);
known_answer!(
    lzw,
    "LZWDecode",
    &[0x80, 0x0b, 0x60, 0x50, 0x22, 0x0c, 0x0c, 0x85, 0x01],
    b"-----A---B"
);
known_answer!(ascii_hex, "ASCIIHexDecode", b"48 65 6C 6c 6F>", b"Hello");
known_answer!(ascii_hex_odd_digit, "AHx", b"414>", b"A@");
known_answer!(
    ascii85,
    "ASCII85Decode",
    b"<~87cURD]j7BEbo7~>",
    b"Hello world"
);
known_answer!(ascii85_zero_and_partial, "A85", b"z@:B~>", b"\0\0\0\0ab");
known_answer!(
    run_length,
    "RunLengthDecode",
    b"\x02ABC\xfdD\x80A",
    b"ABCDDDD"
);

macro_rules! predictor {
    ($name:ident, $predictor:expr, $colors:expr, $columns:expr, $bpc:expr, $input:expr, $expected:expr) => {
        #[test]
        fn $name() -> TestResult {
            // Predictor reversal restores each sample using its row and component neighbors without arithmetic overflow.
            let mut parms = Dict::new();
            parms.insert("Predictor", $predictor);
            parms.insert("Colors", $colors);
            parms.insert("Columns", $columns);
            parms.insert("BitsPerComponent", $bpc);
            let data = flate_encode($input);
            let result = decode(&data, &[(Name::from("FlateDecode"), Some(parms))], 4096)?;
            assert_eq!(result.data, $expected);
            Ok(())
        }
    };
}
predictor!(
    png_all_row_types,
    12,
    1,
    3,
    8,
    &[
        0, 200, 200, 200, 2, 10, 20, 30, 3, 1, 2, 3, 1, 255, 2, 255, 4, 2, 1, 16
    ],
    [
        200, 200, 200, 210, 220, 230, 106, 165, 200, 255, 1, 0, 1, 2, 17
    ]
);
predictor!(
    png_multicomponent,
    11,
    2,
    2,
    8,
    &[1, 1, 2, 3, 4],
    [1, 2, 4, 6]
);
predictor!(
    tiff_multicomponent,
    2,
    2,
    2,
    8,
    &[10, 20, 1, 1, 5, 1, 1, 1],
    [10, 20, 11, 21, 5, 1, 6, 2]
);
predictor!(tiff_nibbles, 2, 1, 4, 4, &[0x3e, 0x12], [0x31, 0x24]);
predictor!(
    tiff_big_endian_words,
    2,
    1,
    3,
    16,
    &[0x01, 0x02, 0x00, 0xff, 0xff, 0xff],
    [0x01, 0x02, 0x02, 0x01, 0x02, 0x00]
);

macro_rules! bounded {
    ($name:ident, $filter:literal, $input:expr, $limit:expr) => {
        #[test]
        fn $name() {
            // Decompression exceeding the caller's byte limit fails instead of returning oversized data.
            assert!(matches!(
                decode($input, &[(Name::from($filter), None)], $limit),
                Err(Error::LimitExceeded(_))
            ));
        }
    };
}
bounded!(flate_limit, "FlateDecode", &flate_encode(&[0; 1024]), 100);
bounded!(run_length_limit, "RunLengthDecode", b"\x81A\x80", 100);
bounded!(
    lzw_limit,
    "LZWDecode",
    &[0x80, 0x0b, 0x60, 0x50, 0x22, 0x0c, 0x0c, 0x85, 0x01],
    9
);

#[test]
fn image_filter_stops_after_prior_filters() -> TestResult {
    // Decoding removes wrapper filters but returns image data and its parameters without trying to decode the image.
    let mut parms = Dict::new();
    parms.insert("ColorTransform", 0);
    let result = decode(
        b"FFD8FFD9>",
        &[
            (Name::from("ASCIIHexDecode"), None),
            (Name::from("DCT"), Some(parms.clone())),
        ],
        4096,
    )?;
    assert_eq!(result.data, [0xff, 0xd8, 0xff, 0xd9]);
    let stopped = result.stopped.ok_or("image filter")?;
    assert_eq!(
        (stopped.filter, stopped.parms),
        (Name::from("DCTDecode"), Some(parms))
    );
    Ok(())
}

#[test]
fn filter_array_pairs_parameters_by_position() -> TestResult {
    // Each filter in a chain receives only its corresponding DecodeParms entry.
    let filter = Object::Array(vec![Object::name("AHx"), Object::name("Fl")]);
    let mut parms = Dict::new();
    parms.insert("Predictor", 12);
    parms.insert("Columns", 2);
    let parms = Object::Array(vec![Object::Null, parms.into()]);
    let chain = pdf_core::filters::filter_chain(Some(&filter), Some(&parms));
    let zipped = flate_encode(&[2, 3, 4, 2, 1, 2]);
    let mut hex = String::new();
    for byte in zipped {
        use std::fmt::Write;
        write!(hex, "{byte:02x}")?;
    }
    let decoded = decode(hex.as_bytes(), &chain, 4096)?;
    assert_eq!(decoded.data, [3, 4, 4, 6]);
    Ok(())
}
