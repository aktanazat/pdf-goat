//! Text recognition through the macOS Vision framework.
//!
//! [`recognize`] runs Vision's accurate text recognizer, with language
//! correction on, over an 8-bit gray or RGBA bitmap and returns the lines it
//! read with per-word boxes. Boxes are in pixels of the bitmap with a top-left
//! origin. On every other target the crate builds and [`recognize`] returns
//! [`OcrError::Unsupported`].

use std::fmt;

#[cfg(target_os = "macos")]
mod vision;

/// Sample layout of a [`Bitmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// One byte per pixel, 0 black to 255 white.
    Gray8,
    /// Four bytes per pixel, R G B A, color not multiplied by alpha.
    Rgba8,
    /// Four bytes per pixel, R G B A, color already multiplied by alpha
    /// (the layout `pdf-raster` pixmaps use).
    Rgba8Premultiplied,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::Rgba8 | Self::Rgba8Premultiplied => 4,
        }
    }
}

/// A borrowed image. Row `y` starts at byte `y * stride` of `data`, and
/// `data` holds at least `stride * height` bytes.
#[derive(Debug, Clone, Copy)]
pub struct Bitmap<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes from the start of one row to the next, at least
    /// `width * format.bytes_per_pixel()`.
    pub stride: usize,
    pub format: PixelFormat,
    pub data: &'a [u8],
}

/// Recognition settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrOptions {
    /// Vision language codes in priority order, such as `en-US`, `fr-FR`,
    /// `zh-Hans` (see [`supported_languages`]). Empty leaves Vision's default
    /// (English). A code Vision does not list is
    /// [`OcrError::UnsupportedLanguage`], never silently ignored.
    pub languages: Vec<String>,
    /// Let Vision pick the language itself (macOS 13 and later; ignored on
    /// older systems, which lack the setting).
    pub detect_language: bool,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            languages: Vec::new(),
            detect_language: true,
        }
    }
}

/// An axis-aligned box in bitmap pixels, origin at the top-left corner, y
/// growing downward.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A run of non-whitespace characters inside a [`Line`].
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    /// Where Vision places the word; `None` when Vision gave no box for its
    /// character range.
    pub bbox: Option<Rect>,
}

/// One recognized line of text, in the order Vision reports lines.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    /// Vision's confidence for the line, 0.0 to 1.0.
    pub confidence: f32,
    pub bbox: Rect,
    /// The line's whitespace-separated words, in text order.
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrError {
    /// This target has no Vision framework.
    Unsupported,
    /// The bitmap's dimensions, stride and buffer do not agree.
    InvalidBitmap(String),
    /// A requested language is not one Vision's accurate recognizer lists.
    UnsupportedLanguage {
        language: String,
        supported: Vec<String>,
    },
    /// Vision or Core Graphics refused the image or the request.
    Vision(String),
}

impl fmt::Display for OcrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("OCR needs the macOS Vision framework"),
            Self::InvalidBitmap(detail) => write!(f, "invalid bitmap for OCR: {detail}"),
            Self::UnsupportedLanguage {
                language,
                supported,
            } => write!(
                f,
                "OCR language {language:?} is not supported; supported: {}",
                supported.join(", ")
            ),
            Self::Vision(detail) => write!(f, "text recognition failed: {detail}"),
        }
    }
}

impl std::error::Error for OcrError {}

/// The language codes Vision's accurate recognizer accepts on this system,
/// in Vision's order.
pub fn supported_languages() -> Result<Vec<String>, OcrError> {
    #[cfg(target_os = "macos")]
    {
        vision::supported_languages()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(OcrError::Unsupported)
    }
}

/// Recognize the text in `bitmap`.
///
/// RGBA bitmaps are composited over white first, as page pixels over paper;
/// Vision itself ignores alpha. An image with no width or height, or one
/// Vision finds no text in, gives an empty list. Runs synchronously on the
/// calling thread and may be called from several threads at once. The first
/// call in a process after boot can take tens of seconds while the system
/// loads its recognition models; later calls take tens to hundreds of
/// milliseconds per page.
pub fn recognize(bitmap: &Bitmap<'_>, options: &OcrOptions) -> Result<Vec<Line>, OcrError> {
    #[cfg(target_os = "macos")]
    {
        if bitmap.width == 0 || bitmap.height == 0 {
            return Ok(Vec::new());
        }
        check_layout(bitmap)?;
        vision::recognize(bitmap, options)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (bitmap, options);
        Err(OcrError::Unsupported)
    }
}

#[cfg(target_os = "macos")]
fn check_layout(bitmap: &Bitmap<'_>) -> Result<(), OcrError> {
    let row = (bitmap.width as usize)
        .checked_mul(bitmap.format.bytes_per_pixel())
        .ok_or_else(|| OcrError::InvalidBitmap("the row size overflows".into()))?;
    if bitmap.stride < row {
        return Err(OcrError::InvalidBitmap(format!(
            "stride {} is shorter than a {row}-byte row",
            bitmap.stride
        )));
    }
    let needed = bitmap
        .stride
        .checked_mul(bitmap.height as usize)
        .ok_or_else(|| OcrError::InvalidBitmap("the image size overflows".into()))?;
    if bitmap.data.len() < needed {
        return Err(OcrError::InvalidBitmap(format!(
            "{} bytes given, {needed} needed for {}x{} at stride {}",
            bitmap.data.len(),
            bitmap.width,
            bitmap.height,
            bitmap.stride
        )));
    }
    Ok(())
}
