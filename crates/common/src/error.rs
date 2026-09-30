//! Shared command errors and their JSON and ledger representations.

use std::fmt;
use std::io;
use std::path::Path;

use crate::py::repr_str;

/// A failed verb.
///
/// [`GoatError::Exception`] includes a category prefix in the JSON `error` field;
/// the ledger's `message` column stores only its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoatError {
    /// A command error: the envelope and ledger both carry the text as written.
    Message(String),
    /// A categorized error: the envelope shows `"{kind}: {message}"`, the ledger
    /// keeps `message` alone.
    Exception { kind: String, message: String },
}

impl GoatError {
    /// A `PdfGoatError` with this text.
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    /// A Python exception of class `kind`, such as `ValueError`.
    pub fn exception(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Exception {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// A `ValueError`.
    pub fn value_error(message: impl Into<String>) -> Self {
        Self::exception("ValueError", message)
    }

    /// `_open_unlocked`: `{command} cannot read an encrypted PDF without a password`.
    pub fn needs_password(command: &str) -> Self {
        Self::message(format!(
            "{command} cannot read an encrypted PDF without a password"
        ))
    }

    /// An `OSError` on one path, as Python words it:
    /// `FileNotFoundError: [Errno 2] No such file or directory: '/x'`.
    pub fn os(error: &io::Error, path: &Path) -> Self {
        os_error(error, Some(&repr_str(&path.to_string_lossy())))
    }

    /// An `OSError` that names no path, such as a failed `os.getcwd()`.
    pub fn os_unnamed(error: &io::Error) -> Self {
        os_error(error, None)
    }

    /// An `OSError` on a rename, as `os.replace` words it:
    /// `[Errno 18] Cross-device link: '/a' -> '/b'`.
    pub fn os_pair(error: &io::Error, from: &Path, to: &Path) -> Self {
        os_error(
            error,
            Some(&format!(
                "{} -> {}",
                repr_str(&from.to_string_lossy()),
                repr_str(&to.to_string_lossy())
            )),
        )
    }

    /// The envelope's `error` field.
    pub fn envelope_text(&self) -> String {
        match self {
            Self::Message(text) => text.clone(),
            Self::Exception { kind, message } => format!("{kind}: {message}"),
        }
    }

    /// The ledger's `message` column: Python's `str(error)`.
    pub fn ledger_text(&self) -> &str {
        match self {
            Self::Message(text) => text,
            Self::Exception { message, .. } => message,
        }
    }
}

fn os_error(error: &io::Error, subject: Option<&str>) -> GoatError {
    let Some(code) = error.raw_os_error() else {
        return GoatError::exception("OSError", error.to_string());
    };
    let kind = match error.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::AlreadyExists => "FileExistsError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        io::ErrorKind::NotADirectory => "NotADirectoryError",
        _ => "OSError",
    };
    // `io::Error` prints strerror followed by " (os error N)"; Python prints strerror alone.
    let described = io::Error::from_raw_os_error(code).to_string();
    let strerror = described
        .strip_suffix(&format!(" (os error {code})"))
        .unwrap_or(&described);
    let message = match subject {
        Some(subject) => format!("[Errno {code}] {strerror}: {subject}"),
        None => format!("[Errno {code}] {strerror}"),
    };
    GoatError::exception(kind, message)
}

impl fmt::Display for GoatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(text) => formatter.write_str(text),
            Self::Exception { kind, message } => write!(formatter, "{kind}: {message}"),
        }
    }
}

impl std::error::Error for GoatError {}
