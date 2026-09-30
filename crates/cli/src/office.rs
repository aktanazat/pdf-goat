//! `office run` and `office export`: standalone LibreOffice jobs (`office.py`). Each job runs
//! in a disposable user profile and its own process group, never the interactive office.

use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command as Process, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Arg, ArgGroup, ArgMatches, Command};
use goat_common::args::{int_value, optional, required};
use goat_common::paths::{exists, expanduser, mkdir_parents, resolve, resolve_lenient, suffix};
use goat_common::py::{json_compact, str_value, strip};
use goat_common::{Ctx, GoatError, Registry, Verb};
use serde_json::{Map, Value};
use signal_hook::SigId;
use signal_hook::consts::SIGTERM;
use tempfile::TempDir;

const SOFFICE: &str = "/Applications/LibreOffice.app/Contents/MacOS/soffice";
const WORKER: &str = include_str!("../resources/office_worker.py");
const MACRO: &str = "vnd.sun.star.script:pdf_goat_job.py$main?language=Python&location=user";
const POLL: Duration = Duration::from_millis(50);
const STOP_GRACE: Duration = Duration::from_secs(3);
/// The shell's exit status for a process ended by SIGTERM.
const TERMINATED: i32 = 128 + SIGTERM;

pub fn register(registry: &mut Registry) {
    let timeout = || {
        Arg::new("timeout")
            .long("timeout")
            .value_parser(int_value)
            .default_value("120")
            .help("job limit in seconds")
    };
    registry.family_verb(
        "office",
        Verb::new(
            Command::new("run")
                .about("run a trusted Python script on a document")
                .arg(
                    Arg::new("script")
                        .required(true)
                        .help("Python file; receives document, desktop, uno, prop"),
                )
                .arg(
                    Arg::new("file")
                        .long("input")
                        .help("edit a private copy of this document"),
                )
                .arg(
                    Arg::new("new")
                        .long("new")
                        .value_parser(["writer", "calc", "impress"]),
                )
                .group(ArgGroup::new("source").args(["file", "new"]).required(true))
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .help("save a new Office file or PDF; omit to inspect"),
                )
                .arg(timeout()),
            office_run,
        ),
    );
    registry.family_verb(
        "office",
        Verb::new(
            Command::new("export")
                .about("export an Office document without a script")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("output").short('o').long("output").required(true))
                .arg(timeout()),
            office_export,
        ),
    );
}

fn office_run(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let request = Request {
        verb: "office-run",
        file: optional::<String>(matches, "file")?,
        kind: optional::<String>(matches, "new")?,
        script: optional::<String>(matches, "script")?,
        output: optional::<String>(matches, "output")?,
        timeout: *required::<i64>(matches, "timeout")?,
    };
    cmd_office(&request)
}

fn office_export(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let request = Request {
        verb: "office-export",
        file: optional::<String>(matches, "file")?,
        kind: None,
        script: None,
        output: optional::<String>(matches, "output")?,
        timeout: *required::<i64>(matches, "timeout")?,
    };
    cmd_office(&request)
}

struct Request<'a> {
    verb: &'static str,
    file: Option<&'a String>,
    kind: Option<&'a String>,
    script: Option<&'a String>,
    output: Option<&'a String>,
    timeout: i64,
}

/// `cmd_office`: resolves the paths, runs the job, and merges its receipt into the result.
fn cmd_office(request: &Request<'_>) -> Result<Map<String, Value>, GoatError> {
    let source = request.file.map(|file| resolve(file)).transpose()?;
    let script = request.script.map(|script| resolve(script)).transpose()?;
    let output = request
        .output
        .map(|output| resolve_lenient(&expanduser(output)))
        .transpose()?;
    let job = Job {
        source: source.as_deref(),
        kind: request.kind.map(String::as_str),
        script: script.as_deref(),
        output: output.as_deref(),
        timeout: request.timeout,
    };
    let receipt = match run_office(&job) {
        Ok(receipt) => receipt,
        Err(Stopped::Failed(error)) => return Err(error),
        // Python leaves through `SystemExit(143)`: no result, no ledger row.
        Err(Stopped::Terminated) => std::process::exit(TERMINATED),
    };
    let text = |path: &Path| Value::from(path.to_string_lossy().into_owned());
    let mut result = Map::new();
    result.insert("verb".into(), request.verb.into());
    result.insert(
        "inputs".into(),
        [source.as_deref(), script.as_deref()]
            .into_iter()
            .flatten()
            .map(text)
            .collect(),
    );
    result.insert(
        "outputs".into(),
        output.as_deref().map(text).into_iter().collect(),
    );
    result.extend(receipt);
    Ok(result)
}

struct Job<'a> {
    source: Option<&'a Path>,
    kind: Option<&'a str>,
    script: Option<&'a Path>,
    output: Option<&'a Path>,
    timeout: i64,
}

/// Why a job ended without a receipt.
enum Stopped {
    Failed(GoatError),
    /// SIGTERM arrived; the office is stopped and its files removed.
    Terminated,
}

impl From<GoatError> for Stopped {
    fn from(error: GoatError) -> Self {
        Self::Failed(error)
    }
}

fn failed(message: impl Into<String>) -> Stopped {
    Stopped::Failed(GoatError::message(message))
}

/// `run_office(...)`: the receipt the worker macro wrote.
fn run_office(job: &Job<'_>) -> Result<Map<String, Value>, Stopped> {
    if job.timeout <= 0 {
        return Err(failed("timeout must be a positive number of seconds"));
    }
    let soffice = Path::new(SOFFICE);
    if !soffice.is_file() {
        return Err(failed(
            "office commands require LibreOffice for macOS in /Applications/LibreOffice.app",
        ));
    }
    if let Some(output) = job.output {
        for path in [job.source, job.script].into_iter().flatten() {
            if output == path || exists(output)? && same_file(output, path)? {
                return Err(failed(
                    "output must differ from the input document and script",
                ));
            }
        }
        if let Some(parent) = output.parent() {
            mkdir_parents(parent)?;
        }
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let _signal = SignalGuard::register(&cancelled)?;
    let work = temp_dir("pdf-goat-office-", Path::new("/private/var/tmp"))?;
    let url = match (job.source, job.kind) {
        (Some(source), _) => {
            let snapshot = work
                .path()
                .join(format!("input{}", suffix(&source.to_string_lossy())));
            fs::copy(source, &snapshot).map_err(|error| GoatError::os(&error, source))?;
            file_uri(&snapshot)
        }
        (None, Some("writer")) => "private:factory/swriter".to_owned(),
        (None, Some("calc")) => "private:factory/scalc".to_owned(),
        (None, Some("impress")) => "private:factory/simpress".to_owned(),
        (None, other) => {
            return Err(Stopped::Failed(GoatError::exception(
                "KeyError",
                other.map_or_else(|| "None".to_owned(), |kind| format!("'{kind}'")),
            )));
        }
    };
    let staging = match job.output {
        Some(output) => {
            let parent = output.parent().unwrap_or(Path::new("/"));
            let staging = temp_dir(".pdf-goat-office-", parent)?;
            let staged = staging.path().join(output.file_name().unwrap_or_default());
            Some((staging, staged))
        }
        None => None,
    };
    let profile = work.path().join("profile");
    let macros = profile.join("user/Scripts/python");
    fs::create_dir_all(&macros).map_err(|error| GoatError::os(&error, &macros))?;
    let installed = macros.join("pdf_goat_job.py");
    fs::write(&installed, WORKER).map_err(|error| GoatError::os(&error, &installed))?;
    let request = work.path().join("job.json");
    let mut body = Map::new();
    body.insert("url".into(), url.into());
    body.insert(
        "script".into(),
        job.script
            .map_or(String::new(), |script| {
                script.to_string_lossy().into_owned()
            })
            .into(),
    );
    body.insert(
        "output".into(),
        staging
            .as_ref()
            .map_or(String::new(), |(_, staged)| {
                staged.to_string_lossy().into_owned()
            })
            .into(),
    );
    fs::write(&request, json_compact(&Value::Object(body)))
        .map_err(|error| GoatError::os(&error, &request))?;
    let log_path = work.path().join("soffice.log");
    let log = File::create(&log_path).map_err(|error| GoatError::os(&error, &log_path))?;
    let log_copy = log
        .try_clone()
        .map_err(|error| GoatError::os_unnamed(&error))?;

    let child = Process::new(soffice)
        .arg(format!("-env:UserInstallation={}", file_uri(&profile)))
        .args([
            "--headless",
            "--norestore",
            "--nologo",
            "--nodefault",
            MACRO,
        ])
        .env("PDF_GOAT_OFFICE_JOB", &request)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_copy)
        .process_group(0)
        .spawn()
        .map_err(|error| GoatError::os(&error, soffice))?;
    let mut office = Office { child };
    let status = match office.wait(job.timeout, &cancelled)? {
        Some(status) => status,
        None => {
            return Err(failed(format!(
                "LibreOffice job exceeded {} seconds",
                job.timeout
            )));
        }
    };
    let receipt_path = work.path().join("result.json");
    if !status.success() || !receipt_path.is_file() {
        let log = fs::read(&log_path).map_err(|error| GoatError::os(&error, &log_path))?;
        return Err(failed(format!(
            "LibreOffice job failed (exit {}): {}",
            return_code(status),
            strip(&String::from_utf8_lossy(&log))
        )));
    }
    let receipt =
        fs::read_to_string(&receipt_path).map_err(|error| GoatError::os(&error, &receipt_path))?;
    let receipt: Value = serde_json::from_str(&receipt)
        .map_err(|error| GoatError::exception("JSONDecodeError", error.to_string()))?;
    let Value::Object(receipt) = receipt else {
        return Err(Stopped::Failed(GoatError::exception(
            "TypeError",
            "the LibreOffice receipt is not a JSON object",
        )));
    };
    if let Some(error) = receipt.get("error") {
        return Err(failed(format!(
            "LibreOffice job failed: {}",
            str_value(error)
        )));
    }
    // Python stops the process group before it looks for the output.
    drop(office);
    if cancelled.load(Ordering::SeqCst) {
        return Err(Stopped::Terminated);
    }
    if let (Some((_staging, staged)), Some(output)) = (&staging, job.output) {
        if !staged.is_file() {
            return Err(failed("LibreOffice did not produce the requested output"));
        }
        fs::rename(staged, output).map_err(|error| GoatError::os_pair(&error, staged, output))?;
    }
    Ok(receipt)
}

/// `tempfile.TemporaryDirectory(prefix=..., dir=...)`.
fn temp_dir(prefix: &str, parent: &Path) -> Result<TempDir, GoatError> {
    tempfile::Builder::new()
        .prefix(prefix)
        .rand_bytes(8)
        .tempdir_in(parent)
        .map_err(|error| GoatError::os(&error, parent))
}

/// `os.path.samefile`: the same device and inode.
fn same_file(left: &Path, right: &Path) -> Result<bool, GoatError> {
    let left = fs::metadata(left).map_err(|error| GoatError::os(&error, left))?;
    let right = fs::metadata(right).map_err(|error| GoatError::os(&error, right))?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

/// `PurePosixPath.as_uri()`: `file://` and the path's bytes, percent-encoding all but
/// unreserved characters and `/`.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// `Popen.returncode`: the exit code, or the negated signal number.
fn return_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(-1)
}

/// The running office. Dropping it stops the whole process group: SIGTERM, up to three
/// seconds of grace, then SIGKILL, and reaps the child.
struct Office {
    child: Child,
}

impl Office {
    /// Waits up to `timeout` seconds; `None` when the job outlives it. SIGTERM ends the
    /// wait as [`Stopped::Terminated`].
    fn wait(
        &mut self,
        timeout: i64,
        cancelled: &AtomicBool,
    ) -> Result<Option<ExitStatus>, Stopped> {
        let limit = Duration::from_secs(u64::try_from(timeout).unwrap_or(0));
        let deadline = Instant::now().checked_add(limit);
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return Err(Stopped::Terminated);
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| GoatError::os_unnamed(&error))?
            {
                return Ok(Some(status));
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            thread::sleep(POLL);
        }
    }

    fn signal_group(&self, signal: &str) {
        // The group may already be gone; Python ignores ProcessLookupError the same way.
        let _ = Process::new("/bin/kill")
            .args(["-s", signal, "--", &format!("-{}", self.child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Office {
    fn drop(&mut self) {
        self.signal_group("TERM");
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(None) => thread::sleep(POLL),
                _ => break,
            }
        }
        self.signal_group("KILL");
        let _ = self.child.wait();
    }
}

/// SIGTERM sets `cancelled` while a job runs, so the job can stop the office and remove its
/// files before the process exits.
struct SignalGuard(SigId);

impl SignalGuard {
    fn register(cancelled: &Arc<AtomicBool>) -> Result<Self, GoatError> {
        signal_hook::flag::register(SIGTERM, Arc::clone(cancelled))
            .map(Self)
            .map_err(|error| GoatError::os_unnamed(&error))
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}
