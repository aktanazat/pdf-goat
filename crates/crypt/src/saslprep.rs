//! SASLprep (RFC 4013) for revision 5 and 6 passwords: map, NFKC-normalize,
//! reject prohibited output, apply the bidi rule, then cut to 127 bytes of
//! UTF-8 (ISO 32000-2 algorithm 2.A steps a and b).
//!
//! Unassigned code points are allowed (the RFC 4013 "query" reading; the
//! alternative needs the full assigned-code-point table). Bidi classes come
//! from the RFC 3454 table D.1 ranges plus `char::is_alphabetic` as the LCat
//! approximation, which is exact for the scripts a password realistically
//! mixes with Hebrew or Arabic. A password that is not UTF-8 cannot be
//! prepared and is used as given.

use unicode_normalization::UnicodeNormalization;

pub(crate) const MAX_BYTES: usize = 127;

pub(crate) fn prepare(password: &[u8]) -> Result<Vec<u8>, String> {
    let Ok(text) = std::str::from_utf8(password) else {
        return Ok(truncate(password.to_vec()));
    };
    let mapped: String = text
        .chars()
        .filter_map(|c| {
            if is_non_ascii_space(c) {
                Some(' ')
            } else if is_mapped_to_nothing(c) {
                None
            } else {
                Some(c)
            }
        })
        .collect();
    let normalized: String = mapped.nfkc().collect();
    if let Some(c) = normalized.chars().find(|&c| is_prohibited(c)) {
        return Err(format!(
            "password contains U+{:04X}, which SASLprep prohibits",
            u32::from(c)
        ));
    }
    check_bidi(&normalized)?;
    Ok(truncate(normalized.into_bytes()))
}

fn truncate(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.truncate(MAX_BYTES);
    bytes
}

/// RFC 3454 table C.1.2.
fn is_non_ascii_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200B}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// RFC 3454 table B.1.
fn is_mapped_to_nothing(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{1806}'
            | '\u{180B}'..='\u{180D}'
            | '\u{200B}'..='\u{200D}'
            | '\u{2060}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
    )
}

/// RFC 4013 section 2.3: tables C.1.2, C.2.1, C.2.2, C.3, C.4, C.5, C.6,
/// C.7, C.8, and C.9 (C.5 surrogates cannot occur in a `char`).
fn is_prohibited(c: char) -> bool {
    let cp = u32::from(c);
    is_non_ascii_space(c)
        || matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}')
        || matches!(
            c,
            '\u{0080}'..='\u{009F}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{180E}'
                | '\u{200C}'
                | '\u{200D}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{2060}'..='\u{2063}'
                | '\u{206A}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFC}'
                | '\u{1D173}'..='\u{1D17A}'
        )
        || matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
        || matches!(c, '\u{FDD0}'..='\u{FDEF}')
        || cp & 0xFFFE == 0xFFFE
        || matches!(c, '\u{FFF9}'..='\u{FFFD}')
        || matches!(c, '\u{2FF0}'..='\u{2FFB}')
        || matches!(
            c,
            '\u{0340}' | '\u{0341}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
        )
        || matches!(c, '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// RFC 3454 table D.1: characters with bidirectional property R or AL.
fn is_rand_al(c: char) -> bool {
    matches!(
        c,
        '\u{05BE}'
            | '\u{05C0}'
            | '\u{05C3}'
            | '\u{05D0}'..='\u{05EA}'
            | '\u{05F0}'..='\u{05F4}'
            | '\u{061B}'
            | '\u{061F}'
            | '\u{0621}'..='\u{063A}'
            | '\u{0640}'..='\u{064A}'
            | '\u{066D}'..='\u{066F}'
            | '\u{0671}'..='\u{06D5}'
            | '\u{06DD}'
            | '\u{06E5}'
            | '\u{06E6}'
            | '\u{06FA}'..='\u{06FE}'
            | '\u{0700}'..='\u{070D}'
            | '\u{0710}'
            | '\u{0712}'..='\u{072C}'
            | '\u{0780}'..='\u{07A5}'
            | '\u{07B1}'
            | '\u{200F}'
            | '\u{FB1D}'
            | '\u{FB1F}'..='\u{FB28}'
            | '\u{FB2A}'..='\u{FB36}'
            | '\u{FB38}'..='\u{FB3C}'
            | '\u{FB3E}'
            | '\u{FB40}'
            | '\u{FB41}'
            | '\u{FB43}'
            | '\u{FB44}'
            | '\u{FB46}'..='\u{FBB1}'
            | '\u{FBD3}'..='\u{FD3D}'
            | '\u{FD50}'..='\u{FD8F}'
            | '\u{FD92}'..='\u{FDC7}'
            | '\u{FDF0}'..='\u{FDFC}'
            | '\u{FE70}'..='\u{FE74}'
            | '\u{FE76}'..='\u{FEFC}'
    )
}

/// Combining marks of the right-to-left scripts (bidi class NSM, not L).
fn is_rtl_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0591}'..='\u{05C7}'
            | '\u{0610}'..='\u{061A}'
            | '\u{064B}'..='\u{065F}'
            | '\u{0670}'
            | '\u{06D6}'..='\u{06ED}'
            | '\u{0711}'
            | '\u{0730}'..='\u{074A}'
            | '\u{07A6}'..='\u{07B0}'
            | '\u{0816}'..='\u{082D}'
            | '\u{0859}'..='\u{085B}'
            | '\u{08D3}'..='\u{08FF}'
            | '\u{FB1E}'
    )
}

fn is_l_cat(c: char) -> bool {
    c.is_alphabetic() && !is_rand_al(c) && !is_rtl_mark(c)
}

/// RFC 3454 section 6 requirements 2 and 3.
fn check_bidi(text: &str) -> Result<(), String> {
    if !text.chars().any(is_rand_al) {
        return Ok(());
    }
    if text.chars().any(is_l_cat) {
        return Err(
            "password mixes right-to-left and left-to-right letters, which SASLprep prohibits"
                .into(),
        );
    }
    let first_and_last_rtl =
        text.chars().next().is_some_and(is_rand_al) && text.chars().last().is_some_and(is_rand_al);
    if !first_and_last_rtl {
        return Err(
            "password with right-to-left letters must start and end with one (SASLprep bidi rule)"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::prepare;

    #[test]
    fn ascii_passes_through_and_is_cut_at_127_bytes() {
        assert_eq!(prepare(b"user pw").unwrap(), b"user pw");
        let long: Vec<u8> = (0..200).map(|i| b'a' + (i % 26) as u8).collect();
        assert_eq!(prepare(&long).unwrap(), &long[..127]);
    }

    #[test]
    fn maps_spaces_and_soft_hyphen_then_normalizes_nfkc() {
        // RFC 4013 examples: "I\u00ADX" -> "IX", "\u2168" (Roman numeral nine) -> "IX",
        // no-break space -> space.
        assert_eq!(prepare("I\u{00AD}X".as_bytes()).unwrap(), b"IX");
        assert_eq!(prepare("\u{2168}".as_bytes()).unwrap(), b"IX");
        assert_eq!(prepare("a\u{00A0}b".as_bytes()).unwrap(), b"a b");
        // Decomposed e + combining acute composes to U+00E9.
        assert_eq!(
            prepare("e\u{0301}".as_bytes()).unwrap(),
            "\u{00E9}".as_bytes()
        );
    }

    #[test]
    fn rejects_control_characters_and_mixed_bidi() {
        assert_eq!(
            prepare(b"tab\there").unwrap_err(),
            "password contains U+0009, which SASLprep prohibits"
        );
        // RFC 4013 example: U+0627 U+0031 is invalid (does not end with RandAL).
        assert!(
            prepare("\u{0627}1".as_bytes())
                .unwrap_err()
                .contains("must start and end")
        );
        assert!(
            prepare("\u{05D0}a\u{05D1}".as_bytes())
                .unwrap_err()
                .contains("mixes")
        );
        // Pure Hebrew with vowel points is fine.
        assert_eq!(
            prepare("\u{05E9}\u{05B8}\u{05DC}".as_bytes()).unwrap(),
            "\u{05E9}\u{05B8}\u{05DC}".as_bytes()
        );
    }

    #[test]
    fn non_utf8_bytes_are_used_as_given() {
        assert_eq!(
            prepare(&[0xff, 0xfe, b'x']).unwrap(),
            vec![0xff, 0xfe, b'x']
        );
    }
}
