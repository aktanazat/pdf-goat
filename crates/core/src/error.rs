//! The one error type every fallible `pdf-core` operation returns.

use std::fmt;

use crate::object::ObjRef;

#[derive(Debug)]
pub enum Error {
    /// Reading or writing a file failed.
    Io(std::io::Error),
    /// The bytes are not valid PDF syntax at `offset` (an offset into the
    /// buffer being parsed: the file, a decoded object stream, or a content
    /// stream).
    Syntax { offset: usize, message: String },
    /// An object has the wrong type or an invalid value where no single byte
    /// offset applies (for example `/Root` is not a dictionary).
    Invalid(String),
    /// A referenced object is absent or free.
    MissingObject(ObjRef),
    /// The document is encrypted and the empty password does not open it.
    NeedsPassword,
    /// The password opens neither the user nor the owner side.
    WrongPassword,
    /// A feature this crate does not implement.
    Unsupported(String),
    /// A safety bound was hit: nesting depth, decoded size, object count, or
    /// the length of a reference chain.
    LimitExceeded(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn syntax(offset: usize, message: impl Into<String>) -> Error {
        Error::Syntax {
            offset,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Error {
        Error::Invalid(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "i/o error: {err}"),
            Error::Syntax { offset, message } => {
                write!(f, "syntax error at byte {offset}: {message}")
            }
            Error::Invalid(message) => write!(f, "invalid pdf: {message}"),
            Error::MissingObject(id) => write!(f, "object {} {} is missing", id.num, id.generation),
            Error::NeedsPassword => f.write_str("the document needs a password"),
            Error::WrongPassword => f.write_str("incorrect password"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::LimitExceeded(what) => write!(f, "limit exceeded: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<pdf_crypt::CryptError> for Error {
    fn from(err: pdf_crypt::CryptError) -> Error {
        match err {
            pdf_crypt::CryptError::WrongPassword => Error::WrongPassword,
            pdf_crypt::CryptError::Unsupported(what) => {
                Error::Unsupported(format!("encryption: {what}"))
            }
            pdf_crypt::CryptError::Corrupt(what) => {
                Error::Invalid(format!("encrypted data: {what}"))
            }
        }
    }
}
