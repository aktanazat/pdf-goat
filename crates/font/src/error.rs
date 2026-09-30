use std::fmt;

/// Errors from parsing font programs, CMaps and system font files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontError {
    /// The data ended before a structure was complete.
    Truncated(&'static str),
    /// A structure holds values that cannot be valid.
    Malformed(&'static str),
    /// Valid data that uses a feature this crate does not implement.
    Unsupported(&'static str),
    /// A bound on recursion depth, operand count, work or allocation size was hit.
    LimitExceeded(&'static str),
    /// A required table, entry or glyph is absent.
    Missing(&'static str),
    /// Reading a font file from disk failed.
    Io(String),
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FontError::Truncated(what) => write!(f, "truncated font data: {what}"),
            FontError::Malformed(what) => write!(f, "malformed font data: {what}"),
            FontError::Unsupported(what) => write!(f, "unsupported font feature: {what}"),
            FontError::LimitExceeded(what) => write!(f, "font limit exceeded: {what}"),
            FontError::Missing(what) => write!(f, "missing font data: {what}"),
            FontError::Io(msg) => write!(f, "font file error: {msg}"),
        }
    }
}

impl std::error::Error for FontError {}

pub type Result<T, E = FontError> = std::result::Result<T, E>;
