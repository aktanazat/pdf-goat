//! Text strings (ISO 32000-2 7.9.2.2): PDFDocEncoding, UTF-16BE with a byte
//! order mark, and UTF-8 with a byte order mark.

/// Unicode for PDFDocEncoding bytes 0x18..=0x1F.
const LOW: [u16; 8] = [
    0x02D8, 0x02C7, 0x02C6, 0x02D9, 0x02DD, 0x02DB, 0x02DA, 0x02DC,
];

/// Unicode for PDFDocEncoding bytes 0x80..=0x9F; 0x9F is undefined.
const HIGH: [u16; 32] = [
    0x2022, 0x2020, 0x2021, 0x2026, 0x2014, 0x2013, 0x0192, 0x2044, 0x2039, 0x203A, 0x2212, 0x2030,
    0x201E, 0x201C, 0x201D, 0x2018, 0x2019, 0x201A, 0x2122, 0xFB01, 0xFB02, 0x0141, 0x0152, 0x0160,
    0x0178, 0x017D, 0x0131, 0x0142, 0x0153, 0x0161, 0x017E, 0xFFFD,
];

const UTF16_BOM: [u8; 2] = [0xFE, 0xFF];
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The character a PDFDocEncoding byte stands for. Undefined bytes (0x7F,
/// 0x9F, 0xAD) decode to U+FFFD.
pub fn pdfdoc_char(byte: u8) -> char {
    let code = match byte {
        0x18..=0x1F => LOW[usize::from(byte - 0x18)],
        0x7F | 0xAD => 0xFFFD,
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        0xA0 => 0x20AC,
        _ => u16::from(byte),
    };
    char::from_u32(u32::from(code)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// The PDFDocEncoding byte for `c`, if it has one. Control characters other
/// than tab, line feed, and carriage return have none.
pub fn pdfdoc_byte(c: char) -> Option<u8> {
    let code = u32::from(c);
    match code {
        0x09 | 0x0A | 0x0D | 0x20..=0x7E | 0xA1..=0xAC | 0xAE..=0xFF => u8::try_from(code).ok(),
        0x20AC => Some(0xA0),
        _ => {
            let code = u16::try_from(code).ok()?;
            if let Some(i) = LOW.iter().position(|&u| u == code) {
                return u8::try_from(0x18 + i).ok();
            }
            HIGH[..31]
                .iter()
                .position(|&u| u == code)
                .and_then(|i| u8::try_from(0x80 + i).ok())
        }
    }
}

/// Decode PDFDocEncoding bytes.
pub fn decode_pdfdoc(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| pdfdoc_char(b)).collect()
}

/// Encode as PDFDocEncoding, or `None` when a character has no byte.
pub fn encode_pdfdoc(text: &str) -> Option<Vec<u8>> {
    text.chars().map(pdfdoc_byte).collect()
}

/// UTF-16BE with the FE FF byte order mark.
pub fn encode_utf16be(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.extend_from_slice(&UTF16_BOM);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

/// UTF-8 with the EF BB BF byte order mark (PDF 2.0).
pub fn encode_utf8(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + text.len());
    out.extend_from_slice(&UTF8_BOM);
    out.extend_from_slice(text.as_bytes());
    out
}

/// A text string's bytes: PDFDocEncoding when every character has a byte,
/// otherwise UTF-16BE with a byte order mark.
pub fn encode_text_string(text: &str) -> Vec<u8> {
    encode_pdfdoc(text).unwrap_or_else(|| encode_utf16be(text))
}

/// Decode a text string: UTF-16BE (or, leniently, UTF-16LE) or UTF-8 when
/// the matching byte order mark leads, otherwise PDFDocEncoding. Language
/// escapes (ESC ... ESC) inside UTF-16 text are dropped.
pub fn decode_text_string(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&UTF16_BOM) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&UTF8_BOM) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    decode_pdfdoc(bytes)
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let (pairs, _) = bytes.as_chunks::<2>();
    let units = pairs.iter().map(|&pair| {
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    });
    let mut out = String::with_capacity(bytes.len() / 2);
    let mut in_escape = false;
    for c in char::decode_utf16(units) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        if c == '\u{1B}' {
            in_escape = !in_escape;
        } else if !in_escape {
            out.push(c);
        }
    }
    out
}
