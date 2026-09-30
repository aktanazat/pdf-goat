//! The `pdf-goat` binary: parses the command line, runs the verb, writes the job ledger,
//! and prints JSON (with `--agent` or when stdout is not a terminal) or human text.

mod builtin;
mod human;
mod office;
mod registry;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Instant;

use goat_common::ledger::{Job, ledger_detail, record_job};
use goat_common::py::{json_pretty, str_value};
use goat_common::{Ctx, ParseFailure};
use serde_json::{Map, Value};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let json_mode = args.iter().any(|arg| arg == "--agent") || !std::io::stdout().is_terminal();
    let start = Instant::now();
    let (mut result, status, message, ledger) = match registry::cli().parse(&args) {
        Err(ParseFailure::Help(text)) => {
            return write(std::io::stdout(), &text);
        }
        Err(ParseFailure::Usage(message)) => {
            (envelope("pdf-goat", &message), false, Some(message), None)
        }
        Ok(invocation) => {
            let ctx = Ctx::from_env();
            let ledger = invocation
                .ledgered()
                .then(|| (ctx.home().to_path_buf(), invocation.name.clone()));
            match invocation.run(&ctx) {
                Ok(result) => (result, true, None, ledger),
                Err(error) => (
                    envelope(&invocation.name, &error.envelope_text()),
                    false,
                    Some(error.ledger_text().to_owned()),
                    ledger,
                ),
            }
        }
    };
    let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

    if let Some((home, name)) = ledger {
        let verb = match result.get("verb") {
            Some(Value::String(verb)) => verb.clone(),
            _ => name,
        };
        let empty = Value::Array(Vec::new());
        let detail = Value::Object(ledger_detail(&result));
        let job = Job {
            verb: &verb,
            status: if status { "success" } else { "error" },
            inputs: result.get("inputs").unwrap_or(&empty),
            outputs: result.get("outputs").unwrap_or(&empty),
            detail: &detail,
            message: message.as_deref(),
            duration_ms,
        };
        if let Err(error) = record_job(&home, &job) {
            // Python lets the ledger failure escape main: a traceback and exit 1.
            let _ = writeln!(std::io::stderr(), "pdf-goat: {}", error.envelope_text());
            return ExitCode::FAILURE;
        }
    }

    result.insert("ok".into(), status.into());
    let written = if json_mode {
        write(
            std::io::stdout(),
            &format!("{}\n", json_pretty(&Value::Object(result))),
        )
    } else if status {
        match human::render(&result) {
            Ok(text) => write(std::io::stdout(), &text),
            Err(error) => {
                let _ = writeln!(std::io::stderr(), "pdf-goat: {}", error.envelope_text());
                return ExitCode::FAILURE;
            }
        }
    } else {
        let error = result.get("error").map(str_value).unwrap_or_default();
        write(std::io::stderr(), &format!("error: {error}\n"))
    };
    if status { written } else { ExitCode::FAILURE }
}

/// `{"verb", "inputs": [], "outputs": [], "error"}`.
fn envelope(verb: &str, error: &str) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("verb".into(), verb.into());
    result.insert("inputs".into(), Value::Array(Vec::new()));
    result.insert("outputs".into(), Value::Array(Vec::new()));
    result.insert("error".into(), error.into());
    result
}

fn write(mut stream: impl Write, text: &str) -> ExitCode {
    match stream
        .write_all(text.as_bytes())
        .and_then(|()| stream.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
