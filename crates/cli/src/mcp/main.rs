//! `pdf-goat-mcp`: an MCP server on stdio that hands every PDF operation to the
//! `pdf-goat` binary beside it. It holds no PDF logic: each call runs one
//! `pdf-goat --agent` child and returns that child's JSON.

mod child;
mod reply;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;

use rmcp::ServiceExt;
use rmcp::service::{QuitReason, ServerInitializeError};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::server::Server;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("pdf-goat-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let pdf_goat = sibling_pdf_goat()?;
    let session = tempfile::Builder::new()
        .prefix("pdf-goat-mcp-")
        .tempdir()
        .map_err(|error| format!("cannot create a session directory: {error}"))?;
    let session_dir = session.path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the async runtime: {error}"))?;
    let served = runtime.block_on(serve(Server::new(pdf_goat, session_dir.clone())));
    // Stdin is read on a blocking thread that cannot be interrupted. After a signal the
    // client may still hold stdin open, and waiting for that thread would hang.
    runtime.shutdown_background();
    if let Err(error) = session.close() {
        eprintln!(
            "pdf-goat-mcp: cannot remove {}: {error}",
            session_dir.display()
        );
    }
    served
}

/// The `pdf-goat` binary in the directory of this executable, symlinks resolved.
fn sibling_pdf_goat() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("cannot locate this executable: {error}"))?;
    let pdf_goat = exe.with_file_name("pdf-goat");
    if pdf_goat.is_file() {
        Ok(pdf_goat)
    } else {
        Err(format!(
            "{} is missing; pdf-goat-mcp runs the pdf-goat binary built beside it \
             (cargo build --release -p pdf-goat)",
            pdf_goat.display()
        ))
    }
}

/// Serves stdio until the client closes stdin or a signal arrives, then waits until
/// every pdf-goat child has been stopped and reaped, so none outlives the server.
async fn serve(server: Server) -> Result<(), String> {
    let shutdown = CancellationToken::new();
    watch_signals(shutdown.clone())?;
    let children = server.children();
    let served = match server
        .serve_with_ct(rmcp::transport::stdio(), shutdown.clone())
        .await
    {
        Ok(running) => match running.waiting().await {
            Ok(QuitReason::JoinError(error)) => Err(format!("the server loop failed: {error}")),
            // Stdin closed, or `shutdown` was cancelled.
            Ok(_) => Ok(()),
            Err(error) => Err(format!("the server loop failed: {error}")),
        },
        // A signal, or stdin closed before the client initialized.
        Err(ServerInitializeError::Cancelled | ServerInitializeError::ConnectionClosed(_)) => {
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    };
    // Every call's token descends from `shutdown`: cancelling it makes each running
    // call stop its child (SIGTERM, then SIGKILL after three seconds).
    shutdown.cancel();
    children.close();
    children.wait().await;
    served
}

/// SIGTERM or SIGINT stops the server like a closed stdin, without the grace rmcp
/// gives in-flight calls to finish.
fn watch_signals(shutdown: CancellationToken) -> Result<(), String> {
    let mut terminate = signal(SignalKind::terminate())
        .map_err(|error| format!("cannot watch SIGTERM: {error}"))?;
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|error| format!("cannot watch SIGINT: {error}"))?;
    tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        shutdown.cancel();
    });
    Ok(())
}
