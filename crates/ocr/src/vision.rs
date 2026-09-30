//! The Vision calls behind [`crate::recognize`] and
//! [`crate::supported_languages`].

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{AnyThread, sel};
use objc2_core_foundation::{CFData, CFIndex, CFMutableData, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSRange, NSString};
use objc2_vision::{
    VNDetectTextRectanglesRequest, VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest,
    VNRecognizedText, VNRecognizedTextObservation, VNRectangleObservation, VNRequest,
    VNRequestTextRecognitionLevel,
};

use crate::{Bitmap, Line, OcrError, OcrOptions, PixelFormat, Point, Quad, Rect, Word};

/// A detected text rectangle counts as read once recognized lines cover
/// this share of its area; a line from a second pass counts as new below it.
const READ_SHARE: f64 = 0.5;
/// Unread text rectangles considered on one image.
const MAX_UNREAD: usize = 256;
/// Regions read a second time on one image.
const MAX_REGIONS: usize = 16;

/// The whole image as a Vision region of interest.
const WHOLE: CGRect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0));

/// An accurate-level request with language correction on, the settings
/// every call here shares.
fn base_request() -> Retained<VNRecognizeTextRequest> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request
}

/// [`base_request`] reading `options`' languages, which
/// [`check_languages`] has accepted.
fn text_request(options: &OcrOptions) -> Retained<VNRecognizeTextRequest> {
    let request = base_request();
    if !options.languages.is_empty() {
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
    autoreleasepool(|_| supported(&base_request()))
}

/// Vision quietly ignores a code it does not know, which would read the
/// page in its default language instead.
fn check_languages(options: &OcrOptions) -> Result<(), OcrError> {
    if options.languages.is_empty() {
        return Ok(());
    }
    let supported = supported(&base_request())?;
    match options
        .languages
        .iter()
        .find(|code| !supported.contains(code))
    {
        Some(unknown) => Err(OcrError::UnsupportedLanguage {
            language: unknown.clone(),
            supported,
        }),
        None => Ok(()),
    }
}

/// Callers have checked that the bitmap is non-empty and its buffer covers
/// `stride * height` bytes.
pub(crate) fn recognize(bitmap: &Bitmap<'_>, options: &OcrOptions) -> Result<Vec<Line>, OcrError> {
    autoreleasepool(|_| {
        check_languages(options)?;
        let image = cg_image(bitmap)?;
        let no_options = NSDictionary::<VNImageOption, AnyObject>::new();
        // SAFETY: `image` is a valid CGImage that outlives the handler's use
        // of it (every perform below runs synchronously), and `no_options` is
        // an empty dictionary of the declared key and value types.
        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                &image,
                &no_options,
            )
        };
        let size = Size {
            width: f64::from(bitmap.width),
            height: f64::from(bitmap.height),
        };

        let request = text_request(options);
        // SAFETY: `new` is the plain NSObject constructor of a request class.
        let detect = unsafe { VNDetectTextRectanglesRequest::new() };
        perform(&handler, &[&request, &detect])?;
        let mut lines = read_lines(&request, WHOLE, size);
        // SAFETY: `results` is a plain getter, valid once perform returned.
        let detected: Vec<Rect> = unsafe { detect.results() }.map_or_else(Vec::new, |found| {
            found
                .iter()
                // SAFETY: `boundingBox` is a plain CGRect property getter on a
                // live observation Vision returned.
                .map(|observation| size.rect(unsafe { observation.boundingBox() }, WHOLE))
                .collect()
        });

        for region in unread_regions(&detected, &lines, size) {
            let retry = text_request(options);
            let roi = size.normalized(region);
            // SAFETY: `roi` lies inside the unit square, as Vision requires:
            // `unread_regions` clamps every region to the image.
            unsafe { retry.setRegionOfInterest(roi) };
            perform(&handler, &[&retry])?;
            for line in read_lines(&retry, roi, size) {
                if covered_share(line.quad.bounds(), &lines) < READ_SHARE {
                    insert_in_reading_order(&mut lines, line);
                }
            }
        }
        Ok(lines)
    })
}

fn perform(handler: &VNImageRequestHandler, requests: &[&VNRequest]) -> Result<(), OcrError> {
    handler
        .performRequests_error(&NSArray::from_slice(requests))
        .map_err(|error| OcrError::Vision(describe(&error)))
}

fn read_lines(request: &VNRecognizeTextRequest, roi: CGRect, size: Size) -> Vec<Line> {
    request.results().map_or_else(Vec::new, |observations| {
        observations
            .iter()
            .filter_map(|observation| line(&observation, roi, size))
            .collect()
    })
}

/// Detected text rectangles recognized lines leave mostly unread, grown by
/// half a line on every side and merged where they meet, so a run of
/// skipped lines is read again as one block. Top to bottom, clamped to the
/// image.
fn unread_regions(detected: &[Rect], lines: &[Line], size: Size) -> Vec<Rect> {
    let mut regions: Vec<Rect> = detected
        .iter()
        .filter(|rect| rect.width > 0.0 && rect.height > 0.0)
        .filter(|rect| covered_share(**rect, lines) < READ_SHARE)
        .take(MAX_UNREAD)
        .map(|rect| {
            let margin = rect.height / 2.0;
            let x0 = (rect.x - margin).max(0.0);
            let y0 = (rect.y - margin).max(0.0);
            let x1 = (rect.x + rect.width + margin).min(size.width);
            let y1 = (rect.y + rect.height + margin).min(size.height);
            Rect {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            }
        })
        .filter(|rect| rect.width > 0.0 && rect.height > 0.0)
        .collect();
    let mut merged = true;
    while merged {
        merged = false;
        'pairs: for i in 0..regions.len() {
            for j in i + 1..regions.len() {
                if overlap(regions[i], regions[j]) > 0.0 {
                    let other = regions.swap_remove(j);
                    regions[i] = union(regions[i], other);
                    merged = true;
                    break 'pairs;
                }
            }
        }
    }
    regions.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    regions.truncate(MAX_REGIONS);
    regions
}

/// The share of `rect` that `lines` cover, from 0 to 1.
fn covered_share(rect: Rect, lines: &[Line]) -> f64 {
    let area = rect.width * rect.height;
    if area <= 0.0 {
        return 1.0;
    }
    let covered: f64 = lines
        .iter()
        .map(|line| overlap(rect, line.quad.bounds()))
        .sum();
    (covered / area).min(1.0)
}

fn overlap(a: Rect, b: Rect) -> f64 {
    let width = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let height = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    width.max(0.0) * height.max(0.0)
}

fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    Rect {
        x: x0,
        y: y0,
        width: (a.x + a.width).max(b.x + b.width) - x0,
        height: (a.y + a.height).max(b.y + b.height) - y0,
    }
}

/// Puts a second-pass line where a reader meets it: after the nearest line
/// above it that shares its columns, else before the nearest such line
/// below, else last.
fn insert_in_reading_order(lines: &mut Vec<Line>, line: Line) {
    let rect = line.quad.bounds();
    let middle = |r: Rect| r.y + r.height / 2.0;
    let centre = middle(rect);
    let same_column = |r: Rect| r.x < rect.x + rect.width && rect.x < r.x + r.width;
    let mut above: Option<(usize, f64)> = None;
    let mut below: Option<(usize, f64)> = None;
    for (index, other) in lines.iter().enumerate() {
        let bounds = other.quad.bounds();
        if !same_column(bounds) {
            continue;
        }
        let y = middle(bounds);
        if y < centre {
            if above.is_none_or(|(_, best)| y > best) {
                above = Some((index + 1, y));
            }
        } else if below.is_none_or(|(_, best)| y < best) {
            below = Some((index, y));
        }
    }
    let at = above.or(below).map_or(lines.len(), |(index, _)| index);
    lines.insert(at, line);
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

/// The bitmap's size in pixels, mapping Vision's coordinates (normalized
/// to a region of interest, origin bottom-left) to top-left pixels.
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

impl Size {
    fn point(self, point: CGPoint, roi: CGRect) -> Point {
        Point {
            x: (roi.origin.x + point.x * roi.size.width) * self.width,
            y: (1.0 - roi.origin.y - point.y * roi.size.height) * self.height,
        }
    }

    fn rect(self, rect: CGRect, roi: CGRect) -> Rect {
        let top_left = self.point(
            CGPoint::new(rect.origin.x, rect.origin.y + rect.size.height),
            roi,
        );
        Rect {
            x: top_left.x,
            y: top_left.y,
            width: rect.size.width * roi.size.width * self.width,
            height: rect.size.height * roi.size.height * self.height,
        }
    }

    fn quad(self, observation: &VNRectangleObservation, roi: CGRect) -> Quad {
        // SAFETY: plain CGPoint property getters on a live observation
        // Vision returned.
        let (top_left, top_right, bottom_right, bottom_left) = unsafe {
            (
                observation.topLeft(),
                observation.topRight(),
                observation.bottomRight(),
                observation.bottomLeft(),
            )
        };
        Quad {
            top_left: self.point(top_left, roi),
            top_right: self.point(top_right, roi),
            bottom_right: self.point(bottom_right, roi),
            bottom_left: self.point(bottom_left, roi),
        }
    }

    /// A pixel box inside the image as a Vision region of interest.
    fn normalized(self, rect: Rect) -> CGRect {
        CGRect::new(
            CGPoint::new(
                rect.x / self.width,
                1.0 - (rect.y + rect.height) / self.height,
            ),
            CGSize::new(rect.width / self.width, rect.height / self.height),
        )
    }
}

fn line(observation: &VNRecognizedTextObservation, roi: CGRect, size: Size) -> Option<Line> {
    let candidate = observation.topCandidates(1).firstObject()?;
    let text = candidate.string().to_string();
    let words = words(&candidate, &text, roi, size);
    Some(Line {
        confidence: candidate.confidence(),
        quad: size.quad(observation, roi),
        text,
        words,
    })
}

/// The whitespace-separated words of `text`, each placed through
/// `boundingBoxForRange:error:` on its UTF-16 range.
fn words(candidate: &VNRecognizedText, text: &str, roi: CGRect, size: Size) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let mut offset = 0usize;
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_whitespace() {
            if !current.is_empty() {
                let range = NSRange::new(start, offset - start);
                // SAFETY: `range` lies inside the candidate's own string: it
                // was measured in UTF-16 units over that string's characters.
                let placed = unsafe { candidate.boundingBoxForRange_error(range) };
                words.push(Word {
                    text: std::mem::take(&mut current),
                    quad: placed.ok().map(|observation| size.quad(&observation, roi)),
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
