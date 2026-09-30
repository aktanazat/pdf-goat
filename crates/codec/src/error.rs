use std::fmt;

/// Error returned by every decoder and encoder in this crate.
///
/// `codec` names the codec or file format ("ccitt", "dct", "jpx", "jbig2",
/// "png", "gif", "bmp", "tiff", "pnm", "webp", "pixels"); `detail` is a short
/// human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    /// The data does not follow the codec's syntax, or is truncated.
    Malformed { codec: &'static str, detail: String },
    /// Well-formed data using a feature this crate does not implement.
    Unsupported { codec: &'static str, detail: String },
    /// A size derived from the data or the parameters exceeds a safety limit.
    Limit { codec: &'static str, detail: String },
    /// The caller's parameters are inconsistent with each other or the data.
    InvalidParams { codec: &'static str, detail: String },
}

impl CodecError {
    pub(crate) fn malformed(codec: &'static str, detail: impl Into<String>) -> Self {
        Self::Malformed {
            codec,
            detail: detail.into(),
        }
    }

    pub(crate) fn unsupported(codec: &'static str, detail: impl Into<String>) -> Self {
        Self::Unsupported {
            codec,
            detail: detail.into(),
        }
    }

    pub(crate) fn limit(codec: &'static str, detail: impl Into<String>) -> Self {
        Self::Limit {
            codec,
            detail: detail.into(),
        }
    }

    pub(crate) fn invalid(codec: &'static str, detail: impl Into<String>) -> Self {
        Self::InvalidParams {
            codec,
            detail: detail.into(),
        }
    }

    /// The codec or file format that produced the error.
    pub fn codec(&self) -> &'static str {
        match self {
            Self::Malformed { codec, .. }
            | Self::Unsupported { codec, .. }
            | Self::Limit { codec, .. }
            | Self::InvalidParams { codec, .. } => codec,
        }
    }

    /// The human-readable reason.
    pub fn detail(&self) -> &str {
        match self {
            Self::Malformed { detail, .. }
            | Self::Unsupported { detail, .. }
            | Self::Limit { detail, .. }
            | Self::InvalidParams { detail, .. } => detail,
        }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { codec, detail } => write!(f, "{codec}: malformed data: {detail}"),
            Self::Unsupported { codec, detail } => write!(f, "{codec}: unsupported: {detail}"),
            Self::Limit { codec, detail } => write!(f, "{codec}: limit exceeded: {detail}"),
            Self::InvalidParams { codec, detail } => {
                write!(f, "{codec}: invalid parameters: {detail}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

pub type Result<T, E = CodecError> = std::result::Result<T, E>;

/// Largest image dimension (width or height) any decoder accepts.
pub const MAX_DIMENSION: u32 = 1 << 20;

/// Largest pixel count (width × height) any decoder accepts.
pub const MAX_PIXELS: u64 = 1 << 28;

/// Rejects dimensions that would make output allocations unreasonable.
pub(crate) fn check_dimensions(codec: &'static str, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(CodecError::malformed(
            codec,
            format!("empty image {width}x{height}"),
        ));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(CodecError::limit(
            codec,
            format!("dimension {width}x{height} exceeds {MAX_DIMENSION}"),
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(CodecError::limit(
            codec,
            format!("{width}x{height} exceeds {MAX_PIXELS} pixels"),
        ));
    }
    Ok(())
}
