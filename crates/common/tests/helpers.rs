//! Argument helpers, the ledger's detail summary, and the page map, through the public API.

use std::time::Duration;

use goat_common::GoatError;
use goat_common::ledger::ledger_detail;
use goat_common::output::human_size;
use goat_common::parse::{parse_color, parse_pages, parse_point};
use goat_common::pool::{PageTask, PoolTuning, map_pages};
use serde_json::{Value, json};

#[test]
fn page_specs_parse_as_python_int_does() {
    let cases: &[(&str, Result<Vec<usize>, &str>)] = &[
        (" 3 ", Ok(vec![2])),
        ("3- 1", Ok(vec![2, 1, 0])),
        ("+2", Ok(vec![1])),
        ("1_0", Ok(vec![9])),
        ("\u{663}", Ok(vec![2])),
        ("2 -3", Ok(vec![1, 2])),
        ("2-", Err("'2-' is not a page number or range")),
        ("x", Err("'x' is not a page number or range")),
        ("-2", Err("'-2' is not a page number or range")),
        ("1-2-3", Err("'1-2-3' is not a page number or range")),
        ("0", Err("page 0 is outside the range 1 to 10")),
    ];
    for (spec, expected) in cases {
        let actual = parse_pages(spec, 10).map_err(|error| error.envelope_text());
        assert_eq!(actual, expected.clone().map_err(str::to_owned), "{spec:?}");
    }
}

#[test]
fn points_and_colors_fail_with_python_messages() {
    let points: &[(&str, &str)] = &[
        (
            "1",
            "ValueError: not enough values to unpack (expected 2, got 1)",
        ),
        (
            "1,2,3",
            "ValueError: too many values to unpack (expected 2)",
        ),
        ("a,b", "ValueError: could not convert string to float: 'a'"),
    ];
    for (spec, message) in points {
        assert_eq!(
            parse_point(spec).map_err(|error| error.envelope_text()),
            Err((*message).to_owned()),
            "{spec:?}"
        );
    }
    type Parsed = Result<Option<Vec<f64>>, &'static str>;
    let colors: &[(&str, Parsed)] = &[
        (
            "#zz0000",
            Err("ValueError: invalid literal for int() with base 16: 'zz'"),
        ),
        (
            "#fff",
            Err("ValueError: invalid literal for int() with base 16: ''"),
        ),
        ("#ff8000", Ok(Some(vec![1.0, 0.5019607843137255, 0.0]))),
    ];
    for (spec, expected) in colors {
        let actual = parse_color(Some(spec)).map_err(|error| error.envelope_text());
        assert_eq!(actual, expected.clone().map_err(str::to_owned), "{spec:?}");
    }
}

#[test]
fn human_sizes_step_by_1024() {
    let cases = [
        (0, "0B"),
        (1023, "1023B"),
        (1024, "1.0KB"),
        (1536, "1.5KB"),
        (3 << 40, "3.0TB"),
    ];
    for (bytes, text) in cases {
        assert_eq!(human_size(bytes), text, "{bytes}");
    }
}

#[test]
fn the_ledger_keeps_counts_and_verdicts_never_strings() {
    let result = json!({
        "verb": "x", "inputs": ["/a.pdf"], "outputs": [],
        "freshness": {"verdict": "stale", "age": 3},
        "parse_quality": {"confidence": 0.5, "course_count": 2, "notes": "free text"},
        "pages": [1, 2, 3], "candidates": [1], "courses": {"a": 1}, "terms": [], "fields": [1, 2],
        "password": "hunter2-secret", "risk": "high", "encrypted": true, "ratio": 1.5, "next": null,
    });
    let Value::Object(result) = result else {
        unreachable!()
    };
    // `courses` summarizes to `course_count`, replacing the value parse_quality copied.
    let expected = json!({
        "freshness_verdict": "stale", "confidence": 0.5, "course_count": 1,
        "page_count": 3, "candidate_count": 1, "term_count": 0, "fields_count": 2,
        "password_char_count": 14, "risk": "high", "encrypted": true, "ratio": 1.5, "next": null,
    });
    let detail = ledger_detail(&result);
    assert_eq!(Value::Object(detail.clone()), expected);
    assert!(!Value::Object(detail).to_string().contains("hunter2"));

    let Value::Object(paged) = json!({"pages": [1, 2], "total_pages": 9, "risk": "unknown"}) else {
        unreachable!()
    };
    assert_eq!(
        Value::Object(ledger_detail(&paged)),
        json!({"pages_count": 2, "total_pages": 9, "risk_char_count": 7})
    );
}

/// Doubles each index; fails on the listed ones.
struct Doubler {
    failing: Vec<usize>,
}

impl PageTask for Doubler {
    type Doc = ();
    type Value = usize;

    fn open(&self) -> Result<(), GoatError> {
        Ok(())
    }

    fn page(&self, (): &mut (), index: usize) -> Result<usize, GoatError> {
        if self.failing.contains(&index) {
            return Err(GoatError::message(format!("page {index} failed")));
        }
        Ok(index * 2)
    }
}

#[test]
fn the_pool_returns_what_the_sequential_map_returns() {
    let sequential = PoolTuning {
        max_workers: 1,
        min_pages: 1,
        switch_after: Duration::MAX,
    };
    let pooled = PoolTuning {
        max_workers: 4,
        min_pages: 1,
        switch_after: Duration::ZERO,
    };
    let indices: Vec<usize> = (0..200).rev().collect();
    let expected: Vec<usize> = indices.iter().map(|index| index * 2).collect();
    for tuning in [sequential, pooled] {
        let task = Doubler {
            failing: Vec::new(),
        };
        assert_eq!(
            map_pages(&task, &mut (), &indices, &tuning),
            Ok(expected.clone()),
            "{tuning:?}"
        );
        let task = Doubler {
            failing: vec![190, 5],
        };
        let failure =
            map_pages(&task, &mut (), &indices, &tuning).map_err(|error| error.envelope_text());
        assert_eq!(
            failure,
            Err("page 190 failed".to_owned()),
            "the earliest page in the given order wins: {tuning:?}"
        );
    }
}
