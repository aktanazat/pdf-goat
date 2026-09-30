//! `security sign`, `security verify`, `convert ocr`, and `convert from-office` for
//! pdf-goat (`cmd_sec_sign`, `cmd_sec_verify`, `cmd_convert_ocr`, `cmd_convert_from_office`).

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{flag, optional, required};
use goat_common::paths::{AtomicOutput, default_out, ensure_parent, resolve};
use goat_common::py::strip;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::Document;
use serde_json::{Map, Value};

mod ocr;
mod pkcs7;
mod sign;
mod verify;
mod xref;

pub use pkcs7::human_friendly;
pub use verify::verify_file;

const OFFICE_TIMEOUT: Duration = Duration::from_secs(180);
const OFFICE_POLL: Duration = Duration::from_millis(50);

pub fn register(registry: &mut Registry) {
    let output = || Arg::new("output").short('o').long("output");
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("sign")
                .about("sign with a self-signed certificate")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("name").long("name"))
                .arg(Arg::new("reason").long("reason"))
                .arg(Arg::new("field").long("field"))
                .arg(output()),
            sec_sign,
        ),
    );
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("verify")
                .about("verify signatures")
                .arg(Arg::new("file").required(true)),
            sec_verify,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("ocr")
                .about("add a searchable text layer with macOS Vision")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("re-OCR even if text exists"),
                )
                .arg(output()),
            convert_ocr,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("from-office")
                .about("convert a .docx, .xlsx, or .pptx file to PDF with office2pdf")
                .arg(Arg::new("file").required(true))
                .arg(output()),
            convert_from_office,
        ),
    );
}

fn path_text(path: &Path) -> Value {
    Value::from(path.to_string_lossy().into_owned())
}

fn read_source(path: &Path) -> Result<Vec<u8>, GoatError> {
    fs::read(path).map_err(|error| GoatError::os(&error, path))
}

/// Writes `data` through a `.part` sibling, as `AtomicOutput` does.
fn write_output(out: &Path, data: &[u8]) -> Result<(), GoatError> {
    let atomic = AtomicOutput::new(out);
    fs::write(atomic.partial(), data).map_err(|error| GoatError::os(&error, atomic.partial()))?;
    atomic.commit()
}

fn sec_sign(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = resolve(required::<String>(matches, "file")?)?;
    let src_text = src.to_string_lossy().into_owned();
    let out = match optional::<String>(matches, "output")? {
        Some(output) => ensure_parent(output)?,
        None => ensure_parent(&default_out(&src_text, "signed", "pdf")?)?,
    };
    let common_name = optional::<String>(matches, "name")?.map_or("pdf-goat demo", String::as_str);
    let reason = optional::<String>(matches, "reason")?.map_or("Approval", String::as_str);
    let field = optional::<String>(matches, "field")?.map_or("Signature1", String::as_str);
    let signed = sign::sign_document(read_source(&src)?, field, reason, common_name)
        .map_err(GoatError::message)?;
    write_output(&out, &signed)?;
    let mut result = Map::new();
    result.insert("verb".into(), "sec-sign".into());
    result.insert("inputs".into(), Value::Array(vec![path_text(&src)]));
    result.insert("outputs".into(), Value::Array(vec![path_text(&out)]));
    result.insert("signer".into(), common_name.into());
    result.insert("self_signed".into(), true.into());
    Ok(result)
}

fn sec_verify(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = resolve(required::<String>(matches, "file")?)?;
    let signatures = verify::verify_file(&read_source(&src)?).map_err(GoatError::message)?;
    let mut result = Map::new();
    result.insert("verb".into(), "sec-verify".into());
    result.insert("inputs".into(), Value::Array(vec![path_text(&src)]));
    result.insert("outputs".into(), Value::Array(Vec::new()));
    result.insert("signature_count".into(), signatures.len().into());
    result.insert(
        "signatures".into(),
        signatures.into_iter().map(Value::Object).collect(),
    );
    Ok(result)
}

fn convert_ocr(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = resolve(required::<String>(matches, "file")?)?;
    let src_text = src.to_string_lossy().into_owned();
    let out = match optional::<String>(matches, "output")? {
        Some(output) => ensure_parent(output)?,
        None => ensure_parent(&default_out(&src_text, "ocr", "pdf")?)?,
    };
    let force = flag(matches, "force")?;
    let data = ocr::ocr_document(read_source(&src)?, force).map_err(GoatError::message)?;
    write_output(&out, &data)?;
    let mut result = Map::new();
    result.insert("verb".into(), "convert-ocr".into());
    result.insert("inputs".into(), Value::Array(vec![path_text(&src)]));
    result.insert("outputs".into(), Value::Array(vec![path_text(&out)]));
    Ok(result)
}

fn convert_from_office(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = resolve(required::<String>(matches, "file")?)?;
    let Some(office2pdf) = find_on_path("office2pdf") else {
        return Err(GoatError::message(
            "convert from-office requires office2pdf on PATH (cargo install office2pdf-cli)",
        ));
    };
    let out = match optional::<String>(matches, "output")? {
        Some(output) => ensure_parent(output)?,
        None => ensure_parent(&src.with_extension("pdf").to_string_lossy())?,
    };
    let atomic = AtomicOutput::new(&out);
    let mut child = Process::new(&office2pdf)
        .arg(&src)
        .arg("-o")
        .arg(atomic.partial())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GoatError::os(&error, Path::new(&office2pdf)))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut stdout);
        }
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut stderr);
        }
        (stdout, stderr)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= OFFICE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GoatError::message("office2pdf timed out after 180s"));
            }
            Ok(None) => thread::sleep(OFFICE_POLL),
            Err(error) => return Err(GoatError::os_unnamed(&error)),
        }
    };
    let (stdout, stderr) = reader.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    if !status.success() || !atomic.partial().exists() {
        let text = if stderr.is_empty() { &stdout } else { &stderr };
        let shown: String = strip(text).chars().take(200).collect();
        return Err(GoatError::message(format!("office2pdf failed: {shown}")));
    }
    atomic.commit()?;
    let warnings: Vec<Value> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("Warning: "))
        .map(Value::from)
        .collect();
    let pages = Document::open(&out)
        .and_then(|doc| doc.page_count())
        .map_err(|error| GoatError::message(format!("cannot open {}: {error}", out.display())))?;
    let mut result = Map::new();
    result.insert("verb".into(), "convert-from-office".into());
    result.insert("inputs".into(), Value::Array(vec![path_text(&src)]));
    result.insert("outputs".into(), Value::Array(vec![path_text(&out)]));
    result.insert("engine".into(), "office2pdf".into());
    result.insert("pages".into(), pages.into());
    result.insert("warnings".into(), Value::Array(warnings));
    Ok(result)
}

/// `shutil.which(name)`: the first executable named `name` on `PATH`.
fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|meta| {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            })
        })
}
