use pdf_text::pyre::{ASCII, DOTALL, IGNORECASE, MULTILINE, compile, escape};

#[test]
fn empty_matches_retry_nonempty_alternatives_without_skipping_text() {
    for (pattern, expected) in [
        ("a*", vec![(0, 1, "a"), (1, 1, "")]),
        ("|a", vec![(0, 0, ""), (0, 1, "a"), (1, 1, "")]),
        (".*?", vec![(0, 0, ""), (0, 1, "a"), (1, 1, "")]),
    ] {
        let regex = compile(pattern, 0).expect("Python regex");
        let found = regex.finditer("a").expect("bounded matching");
        assert_eq!(
            found
                .iter()
                .map(|m| (m.start, m.end, m.text))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            regex.sub_literal("a", "X").expect("literal mask"),
            "X".repeat(expected.len())
        );
    }
}

#[test]
fn word_boundaries_and_case_folding_use_python_unicode_rules() {
    let regex = compile(r"\b\w+\b", 0).expect("word matcher");
    let found = regex
        .finditer("café ² ٣ x\u{301}")
        .expect("Unicode matches");
    assert_eq!(
        found
            .iter()
            .map(|m| (m.start, m.end, m.text))
            .collect::<Vec<_>>(),
        vec![(0, 4, "café"), (5, 6, "²"), (7, 8, "٣"), (9, 10, "x")]
    );
    assert_eq!(
        compile("i", IGNORECASE)
            .expect("case folding")
            .sub_literal("i I İ ı", "X")
            .expect("mask"),
        "X X X X"
    );
    assert_eq!(
        compile("[a-z]+", IGNORECASE | ASCII)
            .expect("ASCII-only folding")
            .sub_literal("İıſK abc", "X")
            .expect("mask"),
        "İıſK X"
    );
    assert_eq!(
        compile(r"\B", 0)
            .expect("nonboundary")
            .sub_literal("\n", "X")
            .expect("newline boundaries"),
        "X\nX"
    );
    assert_eq!(
        compile(r"\B", 0)
            .expect("nonboundary")
            .finditer("")
            .expect("empty text"),
        vec![]
    );
}

#[test]
fn captures_support_fixed_lookbehind_named_references_and_conditionals() {
    let regex =
        compile(r"(?<=ID: )(?P<part>[A-Z]+)\s+(?P=part)", IGNORECASE).expect("capture syntax");
    assert_eq!(
        regex
            .sub(r"[\g<part>]", "ID: AA aa")
            .expect("named replacement"),
        "ID: [AA]"
    );
    assert!(
        compile(r"(?P<part>a)\1", 0)
            .expect("numbered named reference")
            .is_match("aa")
            .expect("matching")
    );
    assert_eq!(
        compile(r"(a)?(?(1)b|c)", 0)
            .expect("conditional")
            .sub_literal("ab c ac", "X")
            .expect("conditional mask"),
        "X X aX"
    );
    for pattern in [r"(?<=a+)b", r"(?<=a|bb)c"] {
        assert_eq!(
            compile(pattern, 0)
                .expect_err("variable-width lookbehind")
                .to_string(),
            "error: look-behind requires fixed-width pattern"
        );
    }
}

#[test]
fn character_classes_names_and_scoped_flags_do_not_gain_rust_syntax() {
    assert!(
        compile("[[]", 0)
            .expect("literal opening bracket")
            .is_match("[")
            .expect("bracket match")
    );
    assert_eq!(
        compile("[a&&b]", 0)
            .expect("ampersand is literal")
            .sub_literal("a&b", "X")
            .expect("mask"),
        "XXX"
    );
    assert_eq!(
        compile(r"\N{EM DASH}", 0)
            .expect("Unicode name")
            .sub_literal("a—b", "-")
            .expect("name match"),
        "a-b"
    );
    assert_eq!(
        compile(r"(?a:\w+)\s+(?u:\w+)", 0)
            .expect("scoped character classes")
            .sub_literal("ab café", "X")
            .expect("scoped match"),
        "X"
    );
    assert!(
        compile("(?ims:a.+b)", 0)
            .expect("all flags")
            .is_match("A\nB")
            .expect("multiline dotall case fold")
    );
    assert!(
        compile("a.+b", IGNORECASE | MULTILINE | DOTALL)
            .expect("combined flags")
            .is_match("A\nB")
            .expect("flags")
    );
    assert!(
        compile(&escape("[a].*? # (x)"), 0)
            .expect("escaped query")
            .is_match("[a].*? # (x)")
            .expect("literal search")
    );
}

#[test]
fn replacement_templates_are_validated_even_when_nothing_matches() {
    let regex = compile("(a)", 0).expect("capture");
    assert_eq!(
        regex
            .sub(r"\2", "none")
            .expect_err("bad capture before matching")
            .to_string(),
        "error: invalid group reference 2 at position 1"
    );
    assert_eq!(
        regex
            .sub(r"\g<missing>", "none")
            .expect_err("bad name before matching")
            .to_string(),
        "IndexError: unknown group name 'missing'"
    );
    assert_eq!(
        regex
            .sub(r"\100\0\g<0>", "a")
            .expect("octal and full-match references"),
        "@\0a"
    );
    assert_eq!(
        regex
            .sub_literal("a", r"\g<missing>")
            .expect("literal replacement"),
        r"\g<missing>"
    );
}

#[test]
fn invalid_patterns_report_python_positions_and_do_not_silently_match() {
    for (pattern, message) in [
        ("[z-a]", "bad character range z-a at position 1"),
        (r"[\w-a]", r"bad character range \w-a at position 1"),
        ("a**", "multiple repeat at position 2"),
        ("a{3,2}", "min repeat greater than max repeat at position 2"),
        (
            "a(?i)b",
            "global flags not at the start of the expression at position 1",
        ),
        ("(?P<1>a)", "bad character in group name '1' at position 4"),
        ("(?P=nope)", "unknown group name 'nope' at position 4"),
    ] {
        assert_eq!(
            compile(pattern, 0)
                .expect_err("invalid pattern")
                .to_string(),
            format!("error: {message}")
        );
    }
}
