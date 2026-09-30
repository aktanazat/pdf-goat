//! Tokenizer parity with the Hugging Face `tokenizers` library on the pinned
//! `tokenizer.json`. Expected ids were captured once from
//! `Tokenizer.encode(text, add_special_tokens=False).ids` (tokenizers 0.23.2).
//! The tokenizer file is not committed (683 KB). Run `pdf-goat setup meaning`
//! before this suite; a missing or invalid model fails the tests.

use std::path::PathBuf;
use std::sync::LazyLock;

use crate::Model;

fn home() -> PathBuf {
    match std::env::var_os("PDF_GOAT_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".pdf-goat"),
    }
}

fn installed_model() -> &'static Model {
    static MODEL: LazyLock<Model> = LazyLock::new(|| {
        crate::load(&home()).unwrap_or_else(|error| {
            panic!("tokenizer parity requires `pdf-goat setup meaning`: {error}")
        })
    });
    &MODEL
}

fn assert_ids(text: &str, expected: &[u32]) {
    let model = installed_model();
    assert_eq!(model.tokenizer.encode(text), expected, "text: {text:?}");
}

macro_rules! parity {
    ($($name:ident: $text:expr => $expected:expr;)*) => {
        $(
            #[test]
            fn $name() {
                assert_ids($text, &$expected);
            }
        )*
    };
}

parity! {
    empty: "" => [];
    whitespace_only: " \t\n  " => [];
    plain_punctuation: "Hello, World!" => [6598, 16, 1094, 5];
    lowercase_accents: "Café crème brûlée naïve résumé" => [6674, 12681, 20388, 6993, 8313, 1069, 14749, 12752];
    uppercase_accents_dotted_i_sharp_s: "ÅNGSTRÖM İstanbul Straße ẞ" => [16082, 14693, 8966, 1364, 26813, 102];
    combining_acute: "e\u{301} cafe\u{301} vs café" => [47, 6674, 4449, 6674];
    chinese: "北京大学的研究人员发现了新方法" => [787, 761, 816, 823, 922, 1, 1, 762, 1, 1, 1, 1, 868, 869, 907];
    japanese_kana: "日本語のテキストとカタカナ" => [870, 882, 956, 677, 29245, 29233, 29239, 29246, 29198, 29232, 29241, 29232, 29247];
    korean_hangul: "한국어 텍스트 처리" => [475, 29012, 29027, 28997, 29020, 29026, 29005, 29014, 473, 29015, 29026, 29003, 29023, 29009, 29023, 471, 29014, 29000, 29025];
    unicode_punctuation: "don't stop—it's “quoted” text… ¿qué?" => [1129, 11, 62, 1650, 523, 1015, 11, 61, 529, 8345, 530, 2799, 535, 100, 9867, 35];
    long_words: "supercalifragilisticexpialidocious antidisestablishmentarianism unaffable" => [2571, 8295, 9134, 28187, 23417, 3594, 9294, 18318, 20279, 9091, 5319, 2430, 9527, 3361, 6881, 12608, 2678, 11205, 1970, 13483, 19967, 2474];
    // One word past max_input_chars_per_word (100) is a single [UNK]; one at
    // exactly 100 is split into pieces.
    word_over_100_chars: &format!("{} ok {}", "a".repeat(101), "b".repeat(100))
        => [&[1, 6935, 21867][..], &[9328; 49]].concat();
    url_and_email: "email@example.com https://example.org/path?q=1&x=2#frag" => [9379, 36, 1748, 18, 3018, 15776, 30, 19, 19, 1748, 18, 7923, 19, 3136, 35, 59, 33, 21, 10, 66, 33, 22, 7, 24318, 1296];
    ascii_symbols: "$100.00 + 5% = ~$105 <tag> {braces} [brackets] |pipe| `tick` ^caret" => [8, 1537, 18, 3008, 15, 25, 9, 33, 72, 8, 7752, 32, 5421, 34, 69, 16186, 1021, 71, 37, 18725, 39, 70, 7673, 70, 42, 15362, 42, 40, 1735, 1108];
    tabs_newlines_spaces: "tab\tnew\nline\r\nreturn  multiple   spaces" => [20634, 1053, 1246, 1715, 2680, 6264];
    format_chars_removed: "zero\u{200b}width soft\u{ad}hyphen bom\u{feff}mark" => [4723, 8154, 10933, 1238, 2736, 9542, 7464, 1374, 7951, 13766, 7030];
    control_chars_removed: "control\0null\u{7}bell\u{fffd}replacement\u{85}nel" => [1497, 10237, 2369, 16333, 1896, 23765, 9738, 2678, 10883];
    special_tokens_split: "[CLS] special [MASK] tokens[SEP][UNK][PAD]x" => [2, 1575, 4, 18210, 1021, 3, 1, 0, 66];
    special_token_lowercase_not_special: "[mask] [Cls]" => [37, 6314, 39, 37, 17862, 1021, 39];
    fullwidth: "Ｆｕｌｌｗｉｄｔｈ ＡＢＣ １２３" => [1, 1, 1];
    greek_cyrillic_hebrew_arabic: "Ελληνικά Кириллица עברית العربية" => [165, 28733, 28733, 23830, 15183, 17205, 28732, 13614, 195, 9331, 15862, 9331, 28442, 28442, 9331, 28757, 9266, 265, 28795, 28817, 28802, 28819, 276, 22679, 28836, 16155, 28822, 13504, 18439];
    emoji_zwj: "emoji \u{1f600}\u{1f44d}\u{1f3fd} \u{1f1fa}\u{1f1f8} family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}" => [6867, 28153, 1078, 1, 1, 1161, 1];
    numbers: "1234567890 3.14159 1e-10 -42 1,000,000" => [12144, 18967, 1581, 1587, 1626, 20063, 23, 18, 14477, 27160, 21, 1069, 17, 1190, 17, 3419, 21, 16, 1205, 16, 1205];
    ligatures: "ﬁnance ﬂoor ﬀ" => [990, 6235, 2407, 991, 15512, 1];
    unicode_spaces: "\u{a0}nbsp\u{2003}emspace\u{3000}ideographic\u{2028}sep" => [56, 4916, 1367, 28037, 14333, 7915, 7786, 13779, 18808];
    titlecase_digraph: "ǅungla ǈ Ǳ" => [1, 1, 1];
    symbols: "Ⅻ ② \u{338f} \u{2122} \u{a9} \u{b0} ± × ÷" => [1, 1, 1, 586, 81, 86, 87, 101, 105];
    private_use_removed: "private\u{e000}use" => [1803, 7563];
    vietnamese: "Tiếng Việt có dấu" => [4501, 2076, 18716, 1528, 3836, 1232];
    pdf_passage: "Section 4.2: The Board shall convene quarterly (see Annex B) to review risk-weighted assets." => [1936, 24, 18, 22, 30, 1002, 1610, 3624, 8536, 7165, 1069, 11180, 12, 1162, 16833, 44, 13, 1006, 2325, 2897, 17, 17221, 6051, 18];
}
