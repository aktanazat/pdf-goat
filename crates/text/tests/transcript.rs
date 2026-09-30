use goat_common::{Ctx, GoatError, Registry};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

fn work() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pdf-transcript-test-")
        .tempdir_in("/private/var/tmp")
        .expect("isolated work directory")
}
fn fixture(dir: &Path, name: &str, issue: &str, term: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        goat_fixtures::transcript(
            issue,
            &[(
                term,
                &[
                    "CS 101 Intro to Computing A 4.00 16.00",
                    "MATH 201 Discrete Math B+ 3.00 9.99",
                ],
            )],
        ),
    )
    .expect("transcript fixture");
    path
}
fn run(dir: &Path, args: &[&str]) -> Result<Value, GoatError> {
    let mut registry = Registry::new();
    pdf_text::register(&mut registry);
    let cli = registry.into_cli(clap::Command::new("pdf-goat"), &[], &[]);
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let invocation = cli.parse(&args).unwrap_or_else(|e| panic!("{e:?}"));
    invocation
        .run(&Ctx::new(dir.join("home")))
        .map(Value::Object)
}

#[test]
fn column_order_keeps_identity_terms_and_transfer_courses_separate() {
    let work = work();
    let source = fixture(work.path(), "transcript.pdf", "2026-04-03", "Fall 2025");
    let parsed = pdf_text::transcript::parse_transcript(&source, None).expect("transcript parsing");
    assert_eq!(
        parsed["document_identity"],
        json!({"document_type":"academic_transcript","title":"OFFICIAL ACADEMIC TRANSCRIPT","institution":"UNIVERSITY OF TEST","student_name_present":true,"student_identifier_present":false})
    );
    assert_eq!(
        parsed["degree"],
        json!({"name":"Master of Science","status":"awarded","conferral_date":"2026-06-15"})
    );
    assert_eq!(parsed["terms"][0]["term"], "Fall 2025");
    assert_eq!(
        parsed["terms"][0]["courses"],
        json!([
            {"course":"CS 101","title":"Intro to Computing","grade":"A","units":4.0,"points":16.0,"page":1,"column":2},
            {"course":"MATH 201","title":"Discrete Math","grade":"B+","units":3.0,"points":9.99,"page":1,"column":2}
        ])
    );
    assert_eq!(
        parsed["transfer_credit"][0]["institution"],
        "EXAMPLE COLLEGE"
    );
    assert_eq!(
        parsed["transfer_credit"][0]["courses"][0]["course"],
        "HIST 100"
    );
    let lines = parsed["layout"]["pages"][0]["reading_order"]
        .as_array()
        .expect("reading order");
    let position = |text| {
        lines
            .iter()
            .position(|line| line["text"] == text)
            .expect("source line")
    };
    assert!(position("Degree Awarded: 2026-06-15") < position("Fall 2025"));
    assert!(position("Fall 2025") < position("CS 101 Intro to Computing A 4.00 16.00"));
    assert!(
        position("CS 101 Intro to Computing A 4.00 16.00")
            < position("MATH 201 Discrete Math B+ 3.00 9.99")
    );
}

#[test]
fn freshness_uses_printed_dates_and_term_coverage_not_degree_claims() {
    let work = work();
    let dir = work.path();
    let before = fixture(dir, "current-by-name.pdf", "2026-04-03", "Fall 2025");
    let current = fixture(dir, "current.pdf", "2026-07-01", "Spring 2026");
    let missing = fixture(dir, "missing.pdf", "2026-07-01", "Fall 2025");
    let unknown = dir.join("unknown.pdf");
    fs::write(&unknown, goat_fixtures::single_column()).expect("unknown issue fixture");
    for (source, conferred, verdict) in [
        (&before, None, "not_checked"),
        (&before, Some("2026-06-15"), "stale_before_conferral"),
        (&current, Some("2026-06-15"), "current"),
        (&missing, Some("2026-06-15"), "stale_missing_terms"),
        (&unknown, Some("2026-06-15"), "unknown_issue_date"),
    ] {
        assert_eq!(
            pdf_text::transcript::parse_transcript(source, conferred).expect("freshness evidence")
                ["freshness"]["verdict"],
            verdict
        );
    }
}

#[test]
fn provenance_names_the_exact_bytes_and_export_omits_geometry() {
    let work = work();
    let dir = work.path();
    let source = fixture(dir, "input.pdf", "2026-04-03", "Fall 2025");
    let output = dir.join("output.json");
    let value = run(
        dir,
        &[
            "transcript",
            "read",
            source.to_str().expect("source path"),
            "--conferred",
            "2026-06-15",
            "-o",
            output.to_str().expect("output path"),
        ],
    )
    .expect("transcript export");
    let bytes = fs::read(&source).expect("source bytes");
    assert_eq!(
        value["source_provenance"]["sha256"],
        format!("{:x}", Sha256::digest(&bytes))
    );
    assert_eq!(value["source_provenance"]["byte_size"], bytes.len());
    assert_eq!(value["source_provenance"]["page_count"], 1);
    assert_eq!(
        value["source_provenance"]["path"],
        source.to_str().expect("source path")
    );
    let recorded = chrono::DateTime::parse_from_rfc3339(
        value["source_provenance"]["modified_time"]
            .as_str()
            .expect("timestamp"),
    )
    .expect("ISO modified time");
    let modified = fs::metadata(&source)
        .expect("source metadata")
        .modified()
        .expect("modification time");
    let actual: chrono::DateTime<chrono::Utc> = modified.into();
    assert!(
        actual
            .signed_duration_since(recorded)
            .num_nanoseconds()
            .expect("small difference")
            .abs()
            <= 1000
    );
    let exported: Value =
        serde_json::from_slice(&fs::read(output).expect("export bytes")).expect("export JSON");
    let mut expected = value.as_object().expect("result object").clone();
    for key in ["verb", "inputs", "outputs"] {
        expected.remove(key);
    }
    assert_eq!(exported, Value::Object(expected));
    assert!(exported.get("layout").is_none());
}

#[test]
fn resolve_is_bounded_deduplicated_and_ranked_by_printed_issue_date() {
    let work = work();
    let dir = work.path();
    let candidates = dir.join("candidates");
    fs::create_dir(&candidates).expect("candidate directory");
    let older = fixture(&candidates, "z-official.pdf", "2026-04-03", "Fall 2025");
    let newer = fixture(&candidates, "a-copy.pdf", "2026-07-01", "Spring 2026");
    let nested = candidates.join("nested");
    fs::create_dir(&nested).expect("nested directory");
    fixture(&nested, "not-crawled.pdf", "2026-12-01", "Fall 2026");
    let value = run(
        dir,
        &[
            "transcript",
            "resolve",
            "--root",
            candidates.to_str().expect("root"),
            "--root",
            older.to_str().expect("older"),
        ],
    )
    .expect("bounded discovery");
    assert_eq!(value["candidate_count"], 2);
    assert_eq!(
        value["candidates"][0]["path"],
        newer.to_str().expect("newer")
    );
    assert_eq!(value["candidates"][0]["rank"], 1);
    assert_eq!(
        value["candidates"][1]["path"],
        older.to_str().expect("older")
    );
    assert_eq!(value["candidates"][1]["rank"], 2);
}

#[test]
fn transcript_export_rejects_hard_links_without_altering_source() {
    let work = work();
    let dir = work.path();
    let source = fixture(dir, "input.pdf", "2026-04-03", "Fall 2025");
    let original = fs::read(&source).expect("source bytes");
    let alias = dir.join("same-inode.json");
    fs::hard_link(&source, &alias).expect("source alias");
    for output in [&source, &alias] {
        let error = run(
            dir,
            &[
                "transcript",
                "read",
                source.to_str().expect("source path"),
                "--output",
                output.to_str().expect("output path"),
            ],
        )
        .expect_err("reject source alias");
        assert_eq!(error.to_string(), "output must differ from input");
        assert_eq!(fs::read(&source).expect("unchanged source"), original);
    }
}

#[test]
fn asserted_dates_follow_calendar_and_iso_week_boundaries() {
    let work = work();
    let source = fixture(work.path(), "dates.pdf", "2026-07-01", "Spring 2026");
    for value in [
        "0000-01-01",
        "2026-W1-1",
        "-001-01-01",
        "0000W011",
        "9999-W52-7",
        "2026-02-29",
    ] {
        let error = pdf_text::transcript::parse_transcript(&source, Some(value))
            .expect_err("invalid asserted date");
        assert_eq!(
            error.to_string(),
            "ValueError: --conferred must be YYYY-MM-DD"
        );
    }
    for (value, expected) in [
        ("2026-W01", "2025-12-29"),
        ("2026W011", "2025-12-29"),
        ("20260101", "2026-01-01"),
        ("2028-02-29", "2028-02-29"),
    ] {
        let parsed =
            pdf_text::transcript::parse_transcript(&source, Some(value)).expect("valid ISO date");
        assert_eq!(parsed["freshness"]["asserted_conferral_date"], expected);
    }
}
