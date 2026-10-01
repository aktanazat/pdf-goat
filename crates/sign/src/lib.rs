//! `security sign`, `security verify`, `convert ocr`, and `convert from-office` for
//! pdf-goat (`cmd_sec_sign`, `cmd_sec_verify`, `cmd_convert_ocr`, `cmd_convert_from_office`).

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command};
use goat_common::args::{flag, int_value, many, optional, required};
use goat_common::parse::parse_rect;
use goat_common::paths::{AtomicOutput, default_out, ensure_parent, resolve};
use goat_common::py::strip;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Document, PdfDate, Rect};
use serde_json::{Map, Value};

use appearance::Face;
use cades::CadesSigner;
use mdp::Permission;
use net::Http;
use pkcs7::SelfSigned;
use sign::{Signer, TimeStamping};

mod appearance;
mod cades;
mod ltv;
mod mdp;
mod net;
mod ocr;
mod p12;
mod pkcs7;
mod sign;
mod tsp;
mod verify;
mod xref;

pub use pkcs7::human_friendly;
pub use verify::{Checks, verify_file};

const OFFICE_TIMEOUT: Duration = Duration::from_secs(180);
const OFFICE_POLL: Duration = Duration::from_millis(50);

pub fn register(registry: &mut Registry) {
    let output = || Arg::new("output").short('o').long("output");
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("sign")
                .about("sign with a PKCS#12 identity (PAdES) or a self-signed certificate")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("name").long("name"))
                .arg(Arg::new("reason").long("reason"))
                .arg(Arg::new("field").long("field").help(
                    "signature field: an existing empty one is signed in place, else a new one is made (default Signature1)",
                ))
                .arg(
                    Arg::new("p12")
                        .long("p12")
                        .help("sign with the key and certificates of this .p12/.pfx file"),
                )
                .arg(
                    Arg::new("password_env")
                        .long("password-env")
                        .requires("p12")
                        .help("environment variable holding the .p12 password (default: none)"),
                )
                .arg(
                    Arg::new("pss")
                        .long("pss")
                        .action(ArgAction::SetTrue)
                        .requires("p12")
                        .help("sign an RSA key with RSASSA-PSS instead of PKCS#1 v1.5"),
                )
                .arg(
                    Arg::new("page")
                        .long("page")
                        .value_parser(int_value)
                        .help("1-based page that holds a new signature field (default 1)"),
                )
                .arg(Arg::new("rect").long("rect").help(
                    "x0,y0,x1,y1 in search's frame: show the signature in this box (default: invisible)",
                ))
                .arg(
                    Arg::new("appearance_text")
                        .long("appearance-text")
                        .requires("box")
                        .help("text in the box; a newline starts a line (default: the signer and time, unless --appearance-image)"),
                )
                .arg(
                    Arg::new("appearance_image")
                        .long("appearance-image")
                        .requires("box")
                        .help("PNG or JPEG in the box, left of any text"),
                )
                .arg(
                    Arg::new("tsa")
                        .long("tsa")
                        .requires("p12")
                        .help("time-stamp authority URL (RFC 3161): time-stamp the signature, PAdES B-T"),
                )
                .arg(
                    Arg::new("ltv")
                        .long("ltv")
                        .action(ArgAction::SetTrue)
                        .requires("p12")
                        .help("keep what long-term validation needs: the certificate chains with OCSP responses and CRLs fetched from the addresses they name, PAdES B-LT; with --tsa, also time-stamp the document, B-LTA"),
                )
                .arg(
                    Arg::new("timeout")
                        .long("timeout")
                        .value_parser(int_value)
                        .default_value("30")
                        .help("seconds each network request (--tsa, --ltv) may take"),
                )
                .arg(
                    Arg::new("certify")
                        .long("certify")
                        .value_parser(PossibleValuesParser::new(["1", "2", "3"]).try_map(
                            |level| {
                                level
                                    .parse()
                                    .ok()
                                    .and_then(Permission::from_p)
                                    .ok_or("not a DocMDP level")
                            },
                        ))
                        .help("certify the document, allowing after signing 1: no changes, 2: form filling and signing, 3: also annotations"),
                )
                .group(ArgGroup::new("box").args(["rect", "field"]).multiple(true))
                .arg(output()),
            sec_sign,
        ),
    );
    registry.family_verb(
        "security",
        Verb::new(
            Command::new("verify")
                .about("verify signatures")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("trust")
                        .long("trust")
                        .action(ArgAction::Append)
                        .help("also trust the root certificates in this PEM or DER file; repeatable (default: the system's roots)"),
                )
                .arg(
                    Arg::new("online")
                        .long("online")
                        .action(ArgAction::SetTrue)
                        .help("ask the certificates' OCSP responders and CRLs for the revocation answers the document lacks"),
                )
                .arg(
                    Arg::new("timeout")
                        .long("timeout")
                        .value_parser(int_value)
                        .default_value("30")
                        .help("seconds each network request (--online) may take"),
                ),
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
    let name = optional::<String>(matches, "name")?.map(String::as_str);
    let reason = optional::<String>(matches, "reason")?.map_or("Approval", String::as_str);
    let field = optional::<String>(matches, "field")?.map_or("Signature1", String::as_str);
    let page = optional::<i64>(matches, "page")?.copied();
    let rect = optional::<String>(matches, "rect")?
        .map(|spec| rect_arg(spec))
        .transpose()?;
    let image = optional::<String>(matches, "appearance_image")?
        .map(|path| resolve(path).and_then(|path| read_source(&path)))
        .transpose()?;
    let (signer, signer_name) = match optional::<String>(matches, "p12")? {
        Some(p12) => {
            let password = match optional::<String>(matches, "password_env")? {
                Some(var) => std::env::var(var).map_err(|_| {
                    GoatError::message(format!(
                        "the environment variable {var} named by --password-env is not set"
                    ))
                })?,
                None => String::new(),
            };
            let identity =
                p12::load(&read_source(&resolve(p12)?)?, &password).map_err(GoatError::message)?;
            let signer =
                CadesSigner::new(identity, flag(matches, "pss")?).map_err(GoatError::message)?;
            let subject = &signer.certificate().tbs_certificate.subject;
            let signer_name =
                pkcs7::common_name(subject).unwrap_or_else(|| human_friendly(subject));
            (Signer::Pades(signer), signer_name)
        }
        None => {
            let common_name = name.unwrap_or("pdf-goat demo");
            let signer = SelfSigned::generate(common_name).map_err(GoatError::message)?;
            (Signer::Demo(signer), common_name.to_owned())
        }
    };
    let date = PdfDate::now();
    let tsa_url = optional::<String>(matches, "tsa")?;
    let ltv = flag(matches, "ltv")?;
    let http = (tsa_url.is_some() || ltv)
        .then(|| network_timeout(matches).map(Http::new))
        .transpose()?;
    // No-break spaces keep the time on one line when the text wraps to fit the box.
    let default_text = format!(
        "Digitally signed by {signer_name}\nDate:\u{a0}{:04}-{:02}-{:02}\u{a0}{:02}:{:02}:{:02}\u{a0}UTC",
        date.year, date.month, date.day, date.hour, date.minute, date.second
    );
    let appearance_text = optional::<String>(matches, "appearance_text")?;
    let text = match appearance_text {
        Some(text) if text.trim().is_empty() => {
            return Err(GoatError::message("--appearance-text is empty"));
        }
        Some(text) => Some(text.as_str()),
        None if image.is_none() => Some(default_text.as_str()),
        None => None,
    };
    let certify = optional::<Permission>(matches, "certify")?.copied();
    let request = sign::Request {
        signer: &signer,
        field,
        reason,
        name: match signer {
            Signer::Demo(_) => Some(signer_name.as_str()),
            Signer::Pades(_) => name,
        },
        date,
        page,
        rect,
        face: Face {
            text,
            image: image.as_deref(),
        },
        face_chosen: appearance_text.is_some() || image.is_some(),
        tsa: tsa_url
            .zip(http.as_ref())
            .map(|(url, http)| TimeStamping { http, url }),
        certify,
        ltv: http.as_ref().filter(|_| ltv),
    };
    let signed = sign::sign_document(read_source(&src)?, &request).map_err(GoatError::message)?;
    write_output(&out, &signed.data)?;
    let pades_level = match (&signer, request.tsa.is_some(), ltv) {
        (Signer::Demo(_), _, _) => Value::Null,
        (Signer::Pades(_), false, _) => "B-B".into(),
        (Signer::Pades(_), true, false) => "B-T".into(),
        (Signer::Pades(_), true, true) => "B-LTA".into(),
    };
    let mut result = Map::new();
    result.insert("verb".into(), "sec-sign".into());
    result.insert("inputs".into(), Value::Array(vec![path_text(&src)]));
    result.insert("outputs".into(), Value::Array(vec![path_text(&out)]));
    result.insert("signer".into(), signer_name.into());
    result.insert(
        "self_signed".into(),
        matches!(signer, Signer::Demo(_)).into(),
    );
    result.insert("pades_level".into(), pades_level);
    result.insert("visible".into(), signed.visible.into());
    result.insert(
        "timestamp".into(),
        signed
            .time_stamp
            .map_or(Value::Null, |time| tsp::iso_utc(&time).into()),
    );
    result.insert("field".into(), field.into());
    result.insert("field_created".into(), signed.field_created.into());
    result.insert("certified".into(), certify.is_some().into());
    result.insert(
        "docmdp_level".into(),
        signed
            .permission
            .map_or(Value::Null, |permission| permission.p().into()),
    );
    result.insert(
        "dss".into(),
        signed.ltv.as_ref().map_or(Value::Null, dss_summary),
    );
    result.insert(
        "document_timestamp".into(),
        signed
            .document_time_stamp
            .map_or(Value::Null, |time| tsp::iso_utc(&time).into()),
    );
    Ok(result)
}

/// What `--ltv` put in the document security store.
fn dss_summary(material: &ltv::Material) -> Value {
    let mut summary = Map::new();
    summary.insert("certificates".into(), material.certs.len().into());
    summary.insert("ocsp_responses".into(), material.ocsps.len().into());
    summary.insert("crls".into(), material.crls.len().into());
    summary.insert(
        "unchecked".into(),
        material
            .unchecked
            .iter()
            .map(|name| Value::from(name.as_str()))
            .collect(),
    );
    Value::Object(summary)
}

/// `--timeout` as the limit on each network request.
fn network_timeout(matches: &ArgMatches) -> Result<Duration, GoatError> {
    u64::try_from(*required::<i64>(matches, "timeout")?)
        .ok()
        .filter(|&seconds| seconds > 0)
        .map(Duration::from_secs)
        .ok_or_else(|| GoatError::message("--timeout must be a positive number of seconds"))
}

/// `--rect` as a normalized rectangle with area, as `edit add-image` reads it.
fn rect_arg(spec: &str) -> Result<Rect, GoatError> {
    let values = parse_rect(spec)?;
    let &[x0, y0, x1, y1] = values.as_slice() else {
        return Err(GoatError::value_error("Rect: bad seq len"));
    };
    let rect = Rect::new(x0, y0, x1, y1).normalized();
    if !values.iter().all(|v| v.is_finite()) || rect.is_empty() {
        return Err(GoatError::message(
            "--rect must have positive width and height",
        ));
    }
    Ok(rect)
}

fn sec_verify(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let src = resolve(required::<String>(matches, "file")?)?;
    let mut anchors = ltv::system_roots().map_err(GoatError::message)?;
    for path in many::<String>(matches, "trust")? {
        let certs = ltv::certificates_in(&read_source(&resolve(path)?)?)
            .map_err(|error| GoatError::message(format!("--trust {path}: {error}")))?;
        anchors.extend(certs);
    }
    let online = flag(matches, "online")?
        .then(|| network_timeout(matches).map(Http::new))
        .transpose()?;
    let checks = Checks { anchors, online };
    let signatures =
        verify::verify_file(&read_source(&src)?, &checks).map_err(GoatError::message)?;
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
    let ocr = ocr::ocr_document(read_source(&src)?, force).map_err(GoatError::message)?;
    write_output(&out, &ocr.data)?;
    let mut result = Map::new();
    result.insert("verb".into(), "convert-ocr".into());
    result.insert("inputs".into(), Value::Array(vec![path_text(&src)]));
    result.insert("outputs".into(), Value::Array(vec![path_text(&out)]));
    result.insert(
        "standard".into(),
        ocr.standard.map_or(Value::Null, Value::from),
    );
    result.insert(
        "warnings".into(),
        Value::Array(ocr.warnings.into_iter().map(Value::from).collect()),
    );
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
