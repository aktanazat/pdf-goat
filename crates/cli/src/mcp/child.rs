//! One `pdf-goat --agent` child per call: no stdin, stdout and stderr collected, and
//! stopped when the call is cancelled.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio_util::task::TaskTracker;

/// How long a cancelled child has to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// A child that ran to its end.
pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub enum Failure {
    /// The binary could not be started.
    Start(io::Error),
    /// Reading the child's output or waiting for it failed.
    Lost(io::Error),
    /// The call was cancelled; the child has been stopped and reaped.
    Cancelled,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(error) => write!(f, "could not start pdf-goat: {error}"),
            Self::Lost(error) => write!(f, "lost track of pdf-goat: {error}"),
            Self::Cancelled => f.write_str("cancelled; pdf-goat was stopped"),
        }
    }
}

/// Runs `pdf-goat --agent ARGS...` to its end, or stops it once `cancelled` resolves.
/// `children` tracks the child from spawn until it is reaped.
pub async fn run(
    pdf_goat: &Path,
    args: &[OsString],
    children: &TaskTracker,
    cancelled: impl Future<Output = ()>,
) -> Result<Finished, Failure> {
    let _alive = children.token();
    let mut child = Command::new(pdf_goat)
        .arg("--agent")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(Failure::Start)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let finished = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
        let status = child.wait().await?;
        Ok::<_, io::Error>(Finished {
            status,
            stdout: out,
            stderr: err,
        })
    };
    let outcome = tokio::select! {
        biased;
        () = cancelled => None,
        result = finished => Some(result),
    };
    match outcome {
        Some(result) => result.map_err(Failure::Lost),
        None => {
            stop(&mut child).await;
            Err(Failure::Cancelled)
        }
    }
}

/// SIGTERM, up to [`STOP_GRACE`] to exit, then SIGKILL; the child is reaped either way.
async fn stop(child: &mut Child) {
    if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: `kill` takes no pointers, and `id` is `Some` only until the child is
        // reaped, so `pid` still names this child.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        // `kill` sends SIGKILL and reaps; it fails only once the child is already gone.
        let _ = child.kill().await;
    }
}
