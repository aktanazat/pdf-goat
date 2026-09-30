//! The binary's public protocol, argument constraints, and job ledger.
//! Each run gets its own `PDF_GOAT_HOME`.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

fn home() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("temp home")
}

/// Runs `pdf-goat --agent args...` and returns the exit code and the printed JSON.
fn run(home: &Path, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_pdf-goat"))
        .arg("--agent")
        .args(args)
        .env("PDF_GOAT_HOME", home)
        .output()
        .expect("run pdf-goat");
    let body = serde_json::from_slice(&output.stdout).expect("JSON on stdout");
    (output.status.code().expect("exit code"), body)
}

#[test]
fn usage_errors_are_rejected_before_the_ledger_is_opened() {
    let cases: &[&[&str]] = &[
        &[],
        &["nope"],
        &["office", "export", "file.docx"],
        &[
            "office",
            "run",
            "job.py",
            "--input",
            "file.docx",
            "--new",
            "writer",
        ],
        &["jobs", "--limit", "abc"],
    ];
    for args in cases {
        let home = home();
        let (code, body) = run(home.path(), args);
        assert_eq!(code, 1, "{args:?}");
        assert_eq!(body["verb"], "pdf-goat", "{args:?}");
        assert!(body["error"].is_string(), "{args:?}");
        assert_eq!(body["ok"], false, "{args:?}");
        assert!(
            !home.path().join("ledger.db").exists(),
            "{args:?} was ledgered"
        );
    }
}

#[test]
fn a_mistyped_secret_flag_is_neither_echoed_nor_ledgered() {
    let home = home();
    let output = Command::new(env!("CARGO_BIN_EXE_pdf-goat"))
        .args([
            "--agent",
            "office",
            "export",
            "SRC",
            "-o",
            "OUT",
            "--passwrod",
            "review-secret-7Qx",
        ])
        .env("PDF_GOAT_HOME", home.path())
        .output()
        .expect("run pdf-goat");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1));
    let body: Value = serde_json::from_slice(&output.stdout).expect("JSON error");
    assert_eq!(body["ok"], false);
    assert!(!stdout.contains("review-secret-7Qx"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("review-secret-7Qx"));
    assert!(!home.path().join("ledger.db").exists());
}

#[test]
fn capabilities_exposes_required_and_exclusive_office_inputs() {
    let home = home();
    let (code, body) = run(home.path(), &["capabilities", "office"]);
    assert_eq!(code, 0);
    let commands = &body["schemas"]["office"]["commands"];
    let group = &commands["run"]["mutually_exclusive_groups"][0];
    assert_eq!(group["required"], true);
    let mut choices: Vec<_> = group["arguments"]
        .as_array()
        .expect("exclusive inputs")
        .iter()
        .map(|value| value.as_str().expect("argument name"))
        .collect();
    choices.sort_unstable();
    assert_eq!(choices, ["file", "new"]);
    let mut required: Vec<_> = commands["export"]["arguments"]
        .as_array()
        .expect("export arguments")
        .iter()
        .filter(|argument| argument["required"] == true)
        .map(|argument| argument["name"].as_str().expect("argument name"))
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["file", "output"]);

    let (code, body) = run(home.path(), &["capabilities", "nope"]);
    assert_eq!(
        (code, &body["verb"], &body["ok"]),
        (1, &json!("capabilities"), &json!(false))
    );
}

#[test]
fn office_rejects_a_non_positive_timeout_and_ledgers_the_failure() {
    let home = home();
    let script = home.path().join("job.py");
    std::fs::write(&script, "pass\n").expect("script");
    let script = script.to_string_lossy().into_owned();
    for timeout in ["0", "-5"] {
        let (code, body) = run(
            home.path(),
            &[
                "office",
                "run",
                &script,
                "--new",
                "writer",
                "--timeout",
                timeout,
            ],
        );
        assert_eq!(code, 1);
        assert_eq!(body["verb"], "office");
        assert_eq!(body["ok"], false);
    }
    let (code, body) = run(
        home.path(),
        &["office", "run", "missing.py", "--new", "writer"],
    );
    assert_eq!((code, &body["ok"]), (1, &json!(false)));

    let (code, body) = run(home.path(), &["jobs", "--limit", "2"]);
    assert_eq!(code, 0);
    assert_eq!(body["count"], 2);
    let jobs = body["jobs"].as_array().expect("jobs");
    assert!(
        jobs[0]["id"].as_i64() > jobs[1]["id"].as_i64(),
        "newest first"
    );
    assert_eq!(
        (&jobs[1]["verb"], &jobs[1]["status"]),
        (&json!("office"), &json!("error"))
    );

    let (_, body) = run(home.path(), &["jobs", "--limit", "-1"]);
    assert_eq!(body["count"], 3, "a negative limit lists every row");
}
