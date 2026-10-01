//! The visible face of a signature: a form XObject that fits an image and lines of Helvetica
//! text into the signature box, the image on the left when it shows both.

use pdf_core::{Dict, Document, ObjRef, Object, Operation, PdfString, Rect, Stream, write_content};
use pdf_font::{BaseEncoding, Standard14};

const FONT: Standard14 = Standard14::Helvetica;
/// Space kept clear inside each edge of the box, in user units.
const PADDING: f64 = 2.0;
/// Baseline-to-baseline distance as a multiple of the font size.
const LEADING: f64 = 1.2;
/// The smallest font size the text shrinks to before the box counts as too small.
const MIN_FONT_SIZE: f64 = 4.0;
/// The largest share of the box's width an image takes when text shares the box.
const MAX_IMAGE_SHARE: f64 = 0.5;
/// Halvings of the font size range when fitting text: finer than 0.01 pt.
const FIT_STEPS: usize = 24;

/// What a visible signature shows: text, an image, or both.
pub struct Face<'a> {
    pub text: Option<&'a str>,
    pub image: Option<&'a [u8]>,
}

/// Characters of `text` Helvetica cannot show, as `✔ (U+2714)`; line breaks are allowed.
fn unshowable(text: &str) -> Vec<String> {
    let mut missing: Vec<char> = Vec::new();
    for ch in text.chars() {
        if ch != '\n'
            && ch != '\r'
            && BaseEncoding::WinAnsi.from_unicode(ch).is_none()
            && !missing.contains(&ch)
        {
            missing.push(ch);
        }
    }
    missing
        .into_iter()
        .map(|ch| format!("{ch} (U+{:04X})", u32::from(ch)))
        .collect()
}

/// The normal appearance of a `width` × `height` signature box (user units): a form
/// XObject drawing `face`.
pub fn form(
    doc: &mut Document,
    width: f64,
    height: f64,
    face: &Face<'_>,
) -> Result<ObjRef, String> {
    let inner = Rect::new(PADDING, PADDING, width - PADDING, height - PADDING);
    if inner.is_empty() {
        return Err("the signature box is too small to show anything".to_owned());
    }
    if let Some(text) = face.text {
        let missing = unshowable(text);
        if !missing.is_empty() {
            return Err(format!(
                "the signature text uses characters Helvetica cannot show: {}; give --appearance-text without them",
                missing.join(", ")
            ));
        }
    }
    let mut resources = Dict::new();
    let mut ops = vec![Operation::new("q", vec![])];
    let mut text_area = inner;
    if let Some(data) = face.image {
        let image = pdf_edit::image_xobject(doc, data).map_err(|error| {
            let message = error.to_string();
            let detail = message.strip_prefix("--image").unwrap_or(&message);
            format!("--appearance-image{detail}")
        })?;
        let (w, h) = if (5..=8).contains(&image.orientation) {
            (f64::from(image.height), f64::from(image.width))
        } else {
            (f64::from(image.width), f64::from(image.height))
        };
        let area = if face.text.is_some() {
            let split = (inner.height() * w / h).min(inner.width() * MAX_IMAGE_SHARE);
            text_area.x0 = inner.x0 + split + PADDING;
            Rect::new(inner.x0, inner.y0, inner.x0 + split, inner.y1)
        } else {
            inner
        };
        let scale = (area.width() / w).min(area.height() / h);
        let (across, up) = (w * scale, h * scale);
        let (cx, cy) = ((area.x0 + area.x1) / 2.0, (area.y0 + area.y1) / 2.0);
        let placed = Rect::new(
            cx - across / 2.0,
            cy - up / 2.0,
            cx + across / 2.0,
            cy + up / 2.0,
        );
        let matrix = pdf_edit::image_matrix(image.orientation, placed);
        ops.push(Operation::new("q", vec![]));
        ops.push(Operation::new("cm", Vec::from(matrix.map(Object::Real))));
        ops.push(Operation::new("Do", vec![Object::name("Img")]));
        ops.push(Operation::new("Q", vec![]));
        let mut xobjects = Dict::new();
        xobjects.insert("Img", image.stream);
        resources.insert("XObject", xobjects);
    }
    if let Some(text) = face.text {
        let (size, lines) = fit(text, text_area)?;
        let (ascent, _) = extent();
        let top = (text_area.y0 + text_area.y1 + block_height(lines.len(), size)) / 2.0;
        let mut font = Dict::new();
        font.insert("Type", Object::name("Font"));
        font.insert("Subtype", Object::name("Type1"));
        font.insert("BaseFont", Object::name(FONT.name()));
        font.insert("Encoding", Object::name("WinAnsiEncoding"));
        let mut fonts = Dict::new();
        fonts.insert("Helv", doc.add(font));
        resources.insert("Font", fonts);
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new(
            "Tf",
            vec![Object::name("Helv"), Object::Real(size)],
        ));
        ops.push(Operation::new("TL", vec![Object::Real(LEADING * size)]));
        ops.push(Operation::new("g", vec![Object::Integer(0)]));
        ops.push(Operation::new(
            "Td",
            vec![
                Object::Real(text_area.x0),
                Object::Real(top - ascent * size),
            ],
        ));
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                ops.push(Operation::new("T*", vec![]));
            }
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(PdfString::hex(FONT.encode_text(line)))],
            ));
        }
        ops.push(Operation::new("ET", vec![]));
    }
    ops.push(Operation::new("Q", vec![]));

    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Form"));
    dict.insert(
        "BBox",
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(width),
            Object::Real(height),
        ],
    );
    dict.insert("Resources", resources);
    Ok(doc.add(Stream::new(dict, write_content(&ops))))
}

/// Helvetica's ascent and descent as fractions of the font size.
fn extent() -> (f64, f64) {
    let metrics = FONT.metrics();
    let [_, low, _, high] = metrics.bbox;
    (
        f64::from(metrics.ascent.unwrap_or(high)) / 1000.0,
        f64::from(metrics.descent.unwrap_or(low)) / 1000.0,
    )
}

/// Height of `lines` lines of text at `size`, from the first ascent to the last descent.
fn block_height(lines: usize, size: f64) -> f64 {
    let (ascent, descent) = extent();
    lines.saturating_sub(1) as f64 * LEADING * size + (ascent - descent) * size
}

/// The largest font size at which `text`, wrapped at spaces, fits `area`, and its lines.
fn fit(text: &str, area: Rect) -> Result<(f64, Vec<String>), String> {
    let fits = |size: f64| {
        wrap(text, area.width() / size)
            .filter(|lines| block_height(lines.len(), size) <= area.height())
    };
    let too_small = || {
        format!(
            "the signature text does not fit --rect at {MIN_FONT_SIZE} pt; use a larger box or shorter text"
        )
    };
    let mut best = (MIN_FONT_SIZE, fits(MIN_FONT_SIZE).ok_or_else(too_small)?);
    let (mut low, mut high) = (MIN_FONT_SIZE, area.height() / block_height(1, 1.0));
    for _ in 0..FIT_STEPS {
        let mid = (low + high) / 2.0;
        match fits(mid) {
            Some(lines) => {
                best = (mid, lines);
                low = mid;
            }
            None => high = mid,
        }
    }
    Ok(best)
}

/// `text` broken into lines no wider than `limit` font sizes: at each line break of the text,
/// then greedily at spaces. `None` when a single word is wider than `limit`.
fn wrap(text: &str, limit: f64) -> Option<Vec<String>> {
    let unit = |s: &str| f64::from(FONT.text_width(s, 1.0));
    let space = unit(" ");
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut used = 0.0;
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let width = unit(word);
            if width > limit {
                return None;
            }
            if line.is_empty() {
                used = width;
            } else if used + space + width <= limit {
                line.push(' ');
                used += space + width;
            } else {
                lines.push(std::mem::take(&mut line));
                used = width;
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    Some(lines)
}
