//! The Vision calls behind [`crate::recognize`] and
//! [`crate::supported_languages`].

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{AnyThread, sel};
use objc2_core_foundation::{CFData, CFIndex, CFMutableData, CFRetained, CGRect};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSRange, NSString};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedText,
    VNRecognizedTextObservation, VNRequest, VNRequestTextRecognitionLevel,
};

use crate::{Bitmap, Line, OcrError, OcrOptions, PixelFormat, Rect, Word};

/// An accurate-level request with language correction on, the settings
/// every call here shares.
fn text_request() -> Retained<VNRecognizeTextRequest> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request
}

fn supported(request: &VNRecognizeTextRequest) -> Result<Vec<String>, OcrError> {
    // SAFETY: a plain query on a live, fully initialized request; the
    // returned array holds NSStrings as declared.
    let codes = unsafe { request.supportedRecognitionLanguagesAndReturnError() }
        .map_err(|error| OcrError::Vision(describe(&error)))?;
    Ok(codes.iter().map(|code| code.to_string()).collect())
}

pub(crate) fn supported_languages() -> Result<Vec<String>, OcrError> {
    autoreleasepool(|_| supported(&text_request()))
}

/// Callers have checked that the bitmap is non-empty and its buffer covers
/// `stride * height` bytes.
pub(crate) fn recognize(bitmap: &Bitmap<'_>, options: &OcrOptions) -> Result<Vec<Line>, OcrError> {
    autoreleasepool(|_| {
        let request = text_request();
        if !options.languages.is_empty() {
            // Vision quietly ignores a code it does not know, which would
            // read the page in its default language instead.
            let supported = supported(&request)?;
            if let Some(unknown) = options
                .languages
                .iter()
                .find(|code| !supported.contains(code))
            {
                return Err(OcrError::UnsupportedLanguage {
                    language: unknown.clone(),
                    supported,
                });
            }
            let codes: Vec<Retained<NSString>> = options
                .languages
                .iter()
                .map(|code| NSString::from_str(code))
                .collect();
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&codes));
        }
        if request.respondsToSelector(sel!(setAutomaticallyDetectsLanguage:)) {
            request.setAutomaticallyDetectsLanguage(options.detect_language);
        }
        let image = cg_image(bitmap)?;

        let no_options = NSDictionary::<VNImageOption, AnyObject>::new();
        // SAFETY: `image` is a valid CGImage that outlives the handler's use
        // of it (perform runs synchronously below), and `no_options` is an
        // empty dictionary of the declared key and value types.
        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                &image,
                &no_options,
            )
        };
        let as_request: &VNRequest = &request;
        handler
            .performRequests_error(&NSArray::from_slice(&[as_request]))
            .map_err(|error| OcrError::Vision(describe(&error)))?;

        let Some(observations) = request.results() else {
            return Ok(Vec::new());
        };
        let width = f64::from(bitmap.width);
        let height = f64::from(bitmap.height);
        Ok(observations
            .iter()
            .filter_map(|observation| line(&observation, width, height))
            .collect())
    })
}

fn describe(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

fn cg_image(bitmap: &Bitmap<'_>) -> Result<CFRetained<CGImage>, OcrError> {
    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    let (space, alpha, stride, provider) = match bitmap.format {
        PixelFormat::Gray8 => {
            let data = CFData::from_bytes(&bitmap.data[..bitmap.stride * height]);
            (
                CGColorSpace::new_device_gray(),
                CGImageAlphaInfo::None,
                bitmap.stride,
                CGDataProvider::with_cf_data(Some(&data)),
            )
        }
        PixelFormat::Rgba8 | PixelFormat::Rgba8Premultiplied => {
            let data = flatten_on_white(bitmap)?;
            (
                CGColorSpace::new_device_rgb(),
                CGImageAlphaInfo::NoneSkipLast,
                width * 4,
                CGDataProvider::with_cf_data(Some(&data)),
            )
        }
    };
    let space = space.ok_or_else(|| OcrError::Vision("no device color space".into()))?;
    let provider = provider
        .ok_or_else(|| OcrError::Vision("could not wrap the bitmap for Core Graphics".into()))?;
    let bits_per_pixel = 8 * bitmap.format.bytes_per_pixel();
    // SAFETY: `decode` is null, which CGImageCreate accepts as "no decode
    // array". The provider owns a copy of exactly `stride * height` bytes,
    // matching the width, stride and sample layout passed alongside it.
    let image = unsafe {
        CGImage::new(
            width,
            height,
            8,
            bits_per_pixel,
            stride,
            Some(&space),
            CGBitmapInfo(alpha.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    };
    image.ok_or_else(|| OcrError::Vision("Core Graphics rejected the bitmap".into()))
}

/// Tightly packed RGBX of the bitmap composited over white paper. Vision
/// reads color and ignores alpha, so a transparent background would
/// otherwise read as black behind black text.
fn flatten_on_white(bitmap: &Bitmap<'_>) -> Result<CFRetained<CFMutableData>, OcrError> {
    let too_large = || OcrError::InvalidBitmap("the image is too large".into());
    let row_bytes = bitmap.width as usize * 4;
    let row_length = CFIndex::try_from(row_bytes).map_err(|_| too_large())?;
    let capacity = CFIndex::try_from(bitmap.height)
        .ok()
        .and_then(|height| row_length.checked_mul(height))
        .ok_or_else(too_large)?;
    let out = CFMutableData::new(None, capacity)
        .ok_or_else(|| OcrError::Vision("could not allocate the flattened image".into()))?;
    let premultiplied = bitmap.format == PixelFormat::Rgba8Premultiplied;
    let mut flat_row = Vec::with_capacity(row_bytes);
    for row in bitmap
        .data
        .chunks(bitmap.stride)
        .take(bitmap.height as usize)
    {
        flat_row.clear();
        for pixel in row[..row_bytes].as_chunks::<4>().0 {
            let a = u32::from(pixel[3]);
            for &c in &pixel[..3] {
                let c = u32::from(c);
                let covered = if premultiplied {
                    c.min(a)
                } else {
                    (c * a + 127) / 255
                };
                flat_row.push((covered + 255 - a) as u8);
            }
            flat_row.push(255);
        }
        // SAFETY: `flat_row` is a live buffer of exactly `row_length` bytes,
        // and `out` is a valid mutable CFData we own.
        unsafe { CFMutableData::append_bytes(Some(&out), flat_row.as_ptr(), row_length) };
    }
    Ok(out)
}

/// Vision's normalized, bottom-left-origin box in top-left pixels.
fn to_pixels(normalized: CGRect, width: f64, height: f64) -> Rect {
    Rect {
        x: normalized.origin.x * width,
        y: (1.0 - normalized.origin.y - normalized.size.height) * height,
        width: normalized.size.width * width,
        height: normalized.size.height * height,
    }
}

fn line(observation: &VNRecognizedTextObservation, width: f64, height: f64) -> Option<Line> {
    let candidate = observation.topCandidates(1).firstObject()?;
    let text = candidate.string().to_string();
    // SAFETY: `boundingBox` is a plain CGRect property getter on a live
    // observation Vision returned.
    let bbox = to_pixels(unsafe { observation.boundingBox() }, width, height);
    let words = words(&candidate, &text, width, height);
    Some(Line {
        confidence: candidate.confidence(),
        text,
        bbox,
        words,
    })
}

/// The whitespace-separated words of `text`, each boxed through
/// `boundingBoxForRange:error:` on its UTF-16 range.
fn words(candidate: &VNRecognizedText, text: &str, width: f64, height: f64) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let mut offset = 0usize;
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_whitespace() {
            if !current.is_empty() {
                let range = NSRange::new(start, offset - start);
                words.push(Word {
                    text: std::mem::take(&mut current),
                    bbox: word_box(candidate, range, width, height),
                });
            }
        } else {
            if current.is_empty() {
                start = offset;
            }
            current.push(c);
        }
        offset += c.len_utf16();
    }
    words
}

fn word_box(candidate: &VNRecognizedText, range: NSRange, width: f64, height: f64) -> Option<Rect> {
    // SAFETY: `range` lies inside the candidate's own string: it was measured
    // in UTF-16 units over that string's characters.
    let observation = unsafe { candidate.boundingBoxForRange_error(range) }.ok()?;
    // SAFETY: `boundingBox` is a plain CGRect property getter on a live
    // observation Vision returned.
    Some(to_pixels(
        unsafe { observation.boundingBox() },
        width,
        height,
    ))
}
