//! `edit add-text` and `edit add-image` draw into the page content at a point
//! or inside a rectangle of the `search` frame: crop box top-left origin, y
//! down, /Rotate ignored. Text uses a Standard 14 font or an embedded subset of
//! a TrueType, OpenType, CFF or Type 1 font with ToUnicode, so `text` and
//! `search` read it back, and its box is the one extraction measures for the
//! new glyphs. Embedded fonts are kerned by their own pair tables. The existing
//! content is balanced and wrapped in q/Q first, so a transformation, clip or
//! colour it leaves set cannot move or hide the addition.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use goat_common::GoatError;
use goat_common::paths::resolve;
use pdf_codec::{ImageFormat, PdfColorSpace, PixelLayout};
use pdf_core::{
    Dict, Document, Matrix, ObjRef, Object, Operation, Page, PdfString, Point, Rect, Stream,
    parse_content, write_content,
};
use pdf_font::{
    BaseEncoding, EmbedGlyph, Font, FontLocator, FontRequest, MatchQuality, Script, Standard14,
    embed_font, write_simple_to_unicode_cmap,
};
use pdf_interp::ContentSource;
use pdf_text::{Block, TextFlags};

use crate::EditError;
use crate::kern::Kerning;
use crate::redact::{num, numbers, op, unrotated_transform};

/// Font files `--font` takes as paths.
const FONT_FILES: [&str; 6] = [".ttf", ".otf", ".ttc", ".otc", ".pfb", ".pfa"];

/// Horizontal alignment of each line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Center,
    Right,
}

/// Where text goes, in the `search` frame.
#[derive(Clone, Copy)]
pub(crate) enum Anchor {
    /// The start of the first baseline. Lines wrap at `width` points when it is
    /// given and align inside it, else on the point.
    At { point: Point, width: Option<f64> },
    /// A box the whole block fits at the largest size it can, centred vertically and
    /// placed across by the alignment; no larger than the text's size when `capped`.
    Fit { rect: Rect, capped: bool },
}

pub(crate) struct Text<'a> {
    /// Line breaks are typed or written `\n`; `\\` is a backslash.
    pub text: &'a str,
    pub anchor: Anchor,
    /// A Standard 14 name, an installed font's name, or a font file path.
    pub font: &'a str,
    /// A face of a font collection file: an index from 0 or the face's name.
    pub face: Option<&'a str>,
    pub size: f64,
    /// 1 (gray), 3 (RGB) or 4 (CMYK) components from 0 to 1.
    pub color: &'a [f64],
    pub align: Align,
    /// Degrees counter-clockwise, about the anchor point or the box centre.
    pub rotate: f64,
    /// 0 (invisible) to 1 (opaque).
    pub opacity: f64,
}

/// Where the text landed and how it was drawn.
pub(crate) struct Drawn {
    /// Each page's index and the union of the new glyphs' extraction boxes on it,
    /// in the `search` frame.
    pub placements: Vec<(usize, Rect)>,
    /// PostScript name of the font used.
    pub font: String,
    /// Font size in points.
    pub size: f64,
}

/// An image's placement in the `search` frame.
pub(crate) struct Picture {
    pub rect: Rect,
    /// Degrees counter-clockwise about the box centre.
    pub rotate: f64,
    /// 0 (invisible) to 1 (opaque).
    pub opacity: f64,
    /// Fill the box, ignoring the image's aspect ratio.
    pub stretch: bool,
}

pub(crate) fn add_text(
    doc: &mut Document,
    pages: &[usize],
    text: &Text<'_>,
) -> Result<Drawn, GoatError> {
    let content = unescape(text.text);
    check_text(&content, text)?;
    fill_color(text.color)?;
    let face = face(text.font, text.face)?;
    let encoded = match &face {
        Face::Standard(font) => standard(doc, *font, &content)?,
        Face::Embedded(font) => embedded(doc, font, text.font, &content)?,
    };
    // Vertical metrics in 1/1000 em, like every width below.
    let (ascent, descent) = face.vertical();
    let leading = (ascent - descent).max(1200.0);
    let wrap_at = match text.anchor {
        Anchor::At {
            width: Some(width), ..
        } => Some(width / text.size * 1000.0),
        _ => None,
    };
    let lines: Vec<Vec<char>> = content
        .split('\n')
        .flat_map(|line| {
            let chars: Vec<char> = line.chars().collect();
            match wrap_at {
                Some(limit) => wrap(&encoded, chars, limit),
                None => vec![chars],
            }
        })
        .collect();
    let widths: Vec<f64> = lines.iter().map(|line| encoded.measure(line)).collect();
    let widest = widths.iter().copied().fold(0.0, f64::max);
    let below = leading * (lines.len() - 1) as f64;
    let depth = ascent - descent + below;
    let (cos, sin) = rotation(text.rotate);
    let size = match text.anchor {
        Anchor::At { .. } => text.size,
        Anchor::Fit { rect, capped } => {
            let (w, h) = (widest / 1000.0, depth / 1000.0);
            let fit = (rect.width() / (w * cos.abs() + h * sin.abs()))
                .min(rect.height() / (w * sin.abs() + h * cos.abs()));
            if !(fit.is_finite() && fit > 0.0) {
                return Err(GoatError::message("cannot fit the text in --rect"));
            }
            if capped { fit.min(text.size) } else { fit }
        }
    };
    // The width the lines align in.
    let span = match text.anchor {
        Anchor::At { width, .. } => width.map_or(0.0, |width| width / size * 1000.0),
        Anchor::Fit { .. } => widest,
    };
    let turn = |x: f64, y: f64| (x * cos - y * sin, x * sin + y * cos);
    let state = opacity_state(doc, text.opacity);
    let mut placements = Vec::with_capacity(pages.len());
    for &index in pages {
        let mut page = doc.page(index).map_err(EditError::from)?;
        let to_user = from_frame(&page)?;
        let unit = page.user_unit();
        // User space units per 1/1000 em.
        let scale = size / 1000.0 / unit;
        let origin = match text.anchor {
            Anchor::At { point, .. } => point.transform(&to_user),
            Anchor::Fit { rect, .. } => {
                let across = (widest * cos.abs() + depth * sin.abs()) * size / 1000.0;
                let x = match text.align {
                    Align::Left => rect.x0 + across / 2.0,
                    Align::Center => (rect.x0 + rect.x1) / 2.0,
                    Align::Right => rect.x1 - across / 2.0,
                };
                let centre = Point::new(x, (rect.y0 + rect.y1) / 2.0).transform(&to_user);
                // The block's centre, seen from the start of its first baseline.
                let (dx, dy) = turn(
                    widest / 2.0 * scale,
                    (ascent + descent - below) / 2.0 * scale,
                );
                Point::new(centre.x - dx, centre.y - dy)
            }
        };
        let mut resources = std::mem::take(&mut page.resources);
        let font = add_resource(doc, &mut resources, "Font", "GoatF", encoded.font)?;
        let mut ops = vec![op("q", vec![])];
        if let Some(state) = state {
            let name = add_resource(doc, &mut resources, "ExtGState", "GoatGS", state)?;
            ops.push(op("gs", vec![Object::name(name.as_str())]));
        }
        ops.push(op("BT", vec![]));
        ops.push(fill_color(text.color)?);
        ops.push(op(
            "Tf",
            vec![Object::name(font.as_str()), num(size / unit)],
        ));
        for (row, (line, width)) in lines.iter().zip(&widths).enumerate() {
            if line.is_empty() {
                continue;
            }
            let offset = match text.align {
                Align::Left => 0.0,
                Align::Center => (span - width) / 2.0,
                Align::Right => span - width,
            };
            let (dx, dy) = turn(offset * scale, -leading * row as f64 * scale);
            ops.push(op(
                "Tm",
                numbers(&[cos, sin, -sin + 0.0, cos, origin.x + dx, origin.y + dy]),
            ));
            ops.push(encoded.show(line));
        }
        ops.push(op("ET", vec![]));
        ops.push(op("Q", vec![]));
        let part = append_isolated(doc, page, resources, write_content(&ops))?;
        let bbox = drawn_text(doc, index, part)?.ok_or_else(|| outside(text.anchor, index))?;
        placements.push((index, bbox));
    }
    Ok(Drawn {
        placements,
        font: encoded.name,
        size,
    })
}

/// Draws a PNG or JPEG in `picture.rect` on each page of `pages`: centred,
/// upright by its EXIF orientation, turned by `rotate`, and as large as fits
/// with its aspect ratio kept unless `stretch`. Returns each page and the box
/// the image covers there.
pub(crate) fn add_image(
    doc: &mut Document,
    pages: &[usize],
    data: &[u8],
    picture: &Picture,
) -> Result<Vec<(usize, Rect)>, GoatError> {
    let rect = picture.rect.normalized();
    if ![rect.x0, rect.y0, rect.x1, rect.y1]
        .iter()
        .all(|v| v.is_finite())
        || rect.is_empty()
    {
        return Err(GoatError::message(
            "--rect must have positive width and height",
        ));
    }
    check_look(picture.rotate, picture.opacity)?;
    if picture.stretch && picture.rotate.rem_euclid(90.0) != 0.0 {
        return Err(GoatError::message(
            "--stretch needs --rotate to be a multiple of 90",
        ));
    }
    let image = image_xobject(doc, data)?;
    let (width, height) = if (5..=8).contains(&image.orientation) {
        (image.height, image.width)
    } else {
        (image.width, image.height)
    };
    let (width, height) = (f64::from(width), f64::from(height));
    let (cos, sin) = rotation(picture.rotate);
    let (abs_cos, abs_sin) = (cos.abs(), sin.abs());
    // The upright image's size before it turns.
    let (w, h) = match (picture.stretch, abs_sin > abs_cos) {
        (true, false) => (rect.width(), rect.height()),
        (true, true) => (rect.height(), rect.width()),
        (false, _) => {
            let scale = (rect.width() / (width * abs_cos + height * abs_sin))
                .min(rect.height() / (width * abs_sin + height * abs_cos));
            (width * scale, height * scale)
        }
    };
    let (across, down) = (w * abs_cos + h * abs_sin, w * abs_sin + h * abs_cos);
    let centre = Point::new((rect.x0 + rect.x1) / 2.0, (rect.y0 + rect.y1) / 2.0);
    let placed = Rect::new(
        centre.x - across / 2.0,
        centre.y - down / 2.0,
        centre.x + across / 2.0,
        centre.y + down / 2.0,
    );
    let state = opacity_state(doc, picture.opacity);
    let mut placements = Vec::with_capacity(pages.len());
    for &index in pages {
        let mut page = doc.page(index).map_err(EditError::from)?;
        let at = centre.transform(&from_frame(&page)?);
        let unit = page.user_unit();
        let (w, h) = (w / unit, h / unit);
        let [a, b, c, d, e, f] = image_matrix(
            image.orientation,
            Rect::new(-w / 2.0, -h / 2.0, w / 2.0, h / 2.0),
        );
        let matrix = Matrix { a, b, c, d, e, f }.concat(&Matrix {
            a: cos,
            b: sin,
            c: -sin + 0.0,
            d: cos,
            e: at.x,
            f: at.y,
        });
        let mut resources = std::mem::take(&mut page.resources);
        let name = add_resource(doc, &mut resources, "XObject", "GoatIm", image.stream)?;
        let mut ops = vec![op("q", vec![])];
        if let Some(state) = state {
            let state = add_resource(doc, &mut resources, "ExtGState", "GoatGS", state)?;
            ops.push(op("gs", vec![Object::name(state.as_str())]));
        }
        ops.push(op(
            "cm",
            numbers(&[matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]),
        ));
        ops.push(op("Do", vec![Object::name(name.as_str())]));
        ops.push(op("Q", vec![]));
        append_isolated(doc, page, resources, write_content(&ops))?;
        placements.push((index, placed));
    }
    Ok(placements)
}

/// `--text` with its two-character escapes `\n` (a line break) and `\\` (a
/// backslash) replaced and CRLF line ends made LF; other backslashes stay.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match (ch, chars.peek()) {
            ('\\', Some('n')) => {
                chars.next();
                out.push('\n');
            }
            ('\\', Some('\\')) => {
                chars.next();
                out.push('\\');
            }
            ('\r', Some('\n')) => {}
            _ => out.push(ch),
        }
    }
    out
}

fn check_text(content: &str, text: &Text<'_>) -> Result<(), GoatError> {
    if content.is_empty() {
        return Err(GoatError::message("--text is empty"));
    }
    if content.chars().all(char::is_whitespace) {
        return Err(GoatError::message("--text has nothing to draw"));
    }
    if let Some(ch) = content.chars().find(|&ch| ch.is_control() && ch != '\n') {
        return Err(GoatError::message(format!(
            "--text must hold no control characters but line breaks; found U+{:04X}",
            u32::from(ch)
        )));
    }
    match text.anchor {
        Anchor::At { point, width } => {
            if !(point.x.is_finite() && point.y.is_finite()) {
                return Err(GoatError::message("--at must be two finite numbers"));
            }
            if width.is_some_and(|width| !(width.is_finite() && width > 0.0)) {
                return Err(GoatError::message("--width must be a positive number"));
            }
        }
        Anchor::Fit { rect, .. } => {
            if ![rect.x0, rect.y0, rect.x1, rect.y1]
                .iter()
                .all(|v| v.is_finite())
                || rect.is_empty()
            {
                return Err(GoatError::message(
                    "--rect must have positive width and height",
                ));
            }
        }
    }
    if !(text.size.is_finite() && text.size > 0.0) {
        return Err(GoatError::message("--size must be a positive number"));
    }
    check_look(text.rotate, text.opacity)
}

fn check_look(rotate: f64, opacity: f64) -> Result<(), GoatError> {
    if !rotate.is_finite() {
        return Err(GoatError::message("--rotate must be a number of degrees"));
    }
    if !(0.0..=1.0).contains(&opacity) {
        return Err(GoatError::message("--opacity must be from 0 to 1"));
    }
    Ok(())
}

/// Cosine and sine of `degrees`, exact at multiples of 90.
fn rotation(degrees: f64) -> (f64, f64) {
    let turn = degrees.rem_euclid(360.0);
    if turn == 0.0 {
        (1.0, 0.0)
    } else if turn == 90.0 {
        (0.0, 1.0)
    } else if turn == 180.0 {
        (-1.0, 0.0)
    } else if turn == 270.0 {
        (0.0, -1.0)
    } else {
        let radians = turn.to_radians();
        (radians.cos(), radians.sin())
    }
}

fn outside(anchor: Anchor, index: usize) -> GoatError {
    let place = match anchor {
        Anchor::At { point, .. } => format!("--at {},{}", point.x, point.y),
        Anchor::Fit { rect, .. } => {
            format!("--rect {},{},{},{}", rect.x0, rect.y0, rect.x1, rect.y1)
        }
    };
    GoatError::message(format!("{place} puts the text outside page {}", index + 1))
}

/// An ExtGState with fill and stroke alpha `opacity`; `None` when opaque.
fn opacity_state(doc: &mut Document, opacity: f64) -> Option<ObjRef> {
    (opacity < 1.0).then(|| {
        let mut dict = Dict::new();
        dict.insert("Type", Object::name("ExtGState"));
        dict.insert("ca", Object::Real(opacity));
        dict.insert("CA", Object::Real(opacity));
        doc.add(dict)
    })
}

/// The fill operator for 1 (gray), 3 (RGB) or 4 (CMYK) colour components.
fn fill_color(color: &[f64]) -> Result<Operation, GoatError> {
    let operator = match color.len() {
        1 => "g",
        3 => "rg",
        4 => "k",
        _ => "",
    };
    if operator.is_empty() || !color.iter().all(|c| (0.0..=1.0).contains(c)) {
        return Err(GoatError::value_error(
            "need 1, 3 or 4 color components in range 0 to 1",
        ));
    }
    Ok(op(operator, numbers(color)))
}

enum Face {
    Standard(Standard14),
    Embedded(Font),
}

impl Face {
    /// Ascent and descent in 1/1000 em.
    fn vertical(&self) -> (f64, f64) {
        match self {
            Face::Standard(font) => {
                let metrics = font.metrics();
                (
                    f64::from(metrics.ascent.unwrap_or(metrics.bbox[3])),
                    f64::from(metrics.descent.unwrap_or(metrics.bbox[1])),
                )
            }
            Face::Embedded(font) => {
                let metrics = font.metrics();
                let em = f64::from(font.units_per_em()).max(1.0);
                (
                    f64::from(metrics.ascent) / em * 1000.0,
                    f64::from(metrics.descent) / em * 1000.0,
                )
            }
        }
    }
}

/// `--font`: a font file path, one of the Standard 14 names (any case), or an
/// installed face matched by PostScript or family name. A stand-in from
/// another family is refused rather than drawn in the wrong face. `face` picks
/// a face of a collection file.
fn face(spec: &str, face: Option<&str>) -> Result<Face, GoatError> {
    let lower = spec.to_ascii_lowercase();
    if spec.contains('/') || FONT_FILES.iter().any(|ext| lower.ends_with(ext)) {
        let path = resolve(spec)?;
        let data = fs::read(&path).map_err(|e| GoatError::os(&e, &path))?;
        let index = match face {
            Some(face) => collection_face(&data, &path, face)?,
            None => 0,
        };
        let font = Font::parse_face(data, index)
            .map_err(|e| GoatError::message(format!("{}: {e}", path.display())))?;
        return Ok(Face::Embedded(font));
    }
    if face.is_some() {
        return Err(GoatError::message(
            "--face picks a face of a font file given to --font; name an installed face in --font instead",
        ));
    }
    if let Some(font) = Standard14::ALL
        .into_iter()
        .find(|font| font.name().eq_ignore_ascii_case(spec))
    {
        return Ok(Face::Standard(font));
    }
    let request = FontRequest {
        base_font: spec,
        flags: 0,
        weight: None,
        script: Script::Latin,
    };
    match FontLocator::system().find(&request) {
        Some(found) if found.quality != MatchQuality::Fallback => {
            let font = found
                .load()
                .map_err(|e| GoatError::message(format!("{}: {e}", found.path.display())))?;
            Ok(Face::Embedded(font))
        }
        _ => Err(GoatError::message(format!(
            "font not found: {spec}; use a Standard 14 name, an installed font's name, or a font file path ({})",
            FONT_FILES.join(" ")
        ))),
    }
}

/// The index of `face` in the font file `data`: a number from 0, or a face's
/// PostScript name or "Family Style" name in any case.
fn collection_face(data: &[u8], path: &Path, face: &str) -> Result<u32, GoatError> {
    let count = Font::face_count(data)
        .map_err(|e| GoatError::message(format!("{}: {e}", path.display())))?;
    if let Ok(index) = face.parse::<u32>() {
        if index < count {
            return Ok(index);
        }
        return Err(GoatError::message(format!(
            "--face {face}: {} has {count} face(s), numbered from 0",
            path.display()
        )));
    }
    let mut names = Vec::new();
    for index in 0..count {
        let Ok(font) = Font::parse_face(data.to_vec(), index) else {
            continue;
        };
        let postscript = font.postscript_name().unwrap_or_default();
        let full = format!(
            "{} {}",
            font.family_name().unwrap_or_default(),
            font.style_name().unwrap_or_default()
        );
        if face.eq_ignore_ascii_case(&postscript) || face.eq_ignore_ascii_case(&full) {
            return Ok(index);
        }
        names.push(format!("{index} {postscript}"));
    }
    Err(GoatError::message(format!(
        "--face {face} is not in {}; its faces are {}",
        path.display(),
        names.join(", ")
    )))
}

/// A character's code in the font resource and its advance.
struct Coded {
    code: Vec<u8>,
    /// Advance in 1/1000 em.
    width: f64,
    /// The embedded font's glyph, for kerning.
    glyph: Option<u16>,
}

/// A font resource and how it draws each character of the text.
struct Encoded<'f> {
    font: ObjRef,
    /// PostScript name, reported as `font`.
    name: String,
    chars: HashMap<char, Coded>,
    kerning: Kerning<'f>,
    /// Glyph units per em, the kerning's unit.
    em: f64,
}

impl Encoded<'_> {
    /// Kerning between two characters in 1/1000 em.
    fn kern(&self, left: char, right: char) -> f64 {
        match (self.chars[&left].glyph, self.chars[&right].glyph) {
            (Some(left), Some(right)) => {
                f64::from(self.kerning.pair(left, right)) / self.em * 1000.0
            }
            _ => 0.0,
        }
    }

    /// The advance of `line` in 1/1000 em, kerning included.
    fn measure(&self, line: &[char]) -> f64 {
        line.iter().map(|ch| self.chars[ch].width).sum::<f64>()
            + line
                .windows(2)
                .map(|pair| self.kern(pair[0], pair[1]))
                .sum::<f64>()
    }

    /// Tj for `line`, or TJ with the kerning between its characters.
    fn show(&self, line: &[char]) -> Operation {
        let mut parts = Vec::new();
        let mut run = Vec::new();
        for (index, ch) in line.iter().enumerate() {
            let kern = if index == 0 {
                0.0
            } else {
                self.kern(line[index - 1], *ch)
            };
            if kern != 0.0 {
                parts.push(Object::String(PdfString::hex(std::mem::take(&mut run))));
                parts.push(num(-kern));
            }
            run.extend_from_slice(&self.chars[ch].code);
        }
        if parts.is_empty() {
            return op("Tj", vec![Object::String(PdfString::hex(run))]);
        }
        parts.push(Object::String(PdfString::hex(run)));
        op("TJ", vec![Object::Array(parts)])
    }
}

/// `line` broken at spaces into lines no wider than `limit` (1/1000 em); a
/// word wider than `limit` keeps a line of its own.
fn wrap(encoded: &Encoded<'_>, line: Vec<char>, limit: f64) -> Vec<Vec<char>> {
    let mut lines = Vec::new();
    let mut current: Vec<char> = Vec::new();
    for (index, word) in line.split(|&ch| ch == ' ').enumerate() {
        if index == 0 {
            current.extend_from_slice(word);
            continue;
        }
        let mut candidate = current.clone();
        candidate.push(' ');
        candidate.extend_from_slice(word);
        if current.is_empty() || encoded.measure(&candidate) <= limit {
            current = candidate;
        } else {
            lines.push(std::mem::replace(&mut current, word.to_vec()));
        }
    }
    lines.push(current);
    lines
}

/// Single-byte codes: WinAnsi for the Latin faces, the built-in encoding for
/// Symbol and ZapfDingbats. ZapfDingbats also takes printable ASCII as its
/// codes, so `4` draws ✔ as it does in PyMuPDF.
fn standard(
    doc: &mut Document,
    font: Standard14,
    text: &str,
) -> Result<Encoded<'static>, GoatError> {
    let encoding = match font {
        Standard14::Symbol => BaseEncoding::Symbol,
        Standard14::ZapfDingbats => BaseEncoding::ZapfDingbats,
        _ => BaseEncoding::WinAnsi,
    };
    let mut chars = HashMap::new();
    let mut missing = Vec::new();
    for ch in text.chars().filter(|&ch| ch != '\n') {
        if chars.contains_key(&ch) {
            continue;
        }
        let code = match font {
            Standard14::ZapfDingbats => encoding.from_unicode(ch).or_else(|| {
                u8::try_from(ch)
                    .ok()
                    .filter(|code| (0x21..=0x7E).contains(code))
            }),
            _ => encoding.from_unicode(ch),
        };
        let width = code
            .and_then(|code| encoding.glyph_name(code))
            .and_then(|name| font.glyph_width(name));
        match (code, width) {
            (Some(code), Some(width)) => {
                chars.insert(
                    ch,
                    Coded {
                        code: vec![code],
                        width: f64::from(width),
                        glyph: None,
                    },
                );
            }
            _ => missing.push(ch),
        }
    }
    unencodable(font.name(), &missing)?;
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Font"));
    dict.insert("Subtype", Object::name("Type1"));
    dict.insert("BaseFont", Object::name(font.name()));
    if matches!(encoding, BaseEncoding::WinAnsi) {
        dict.insert("Encoding", Object::name("WinAnsiEncoding"));
    } else {
        // Extraction reads ✔ rather than the built-in code.
        let mut entries: Vec<(u8, String)> = chars
            .values()
            .filter_map(|coded| {
                let code = coded.code[0];
                Some((code, encoding.to_unicode(code)?.to_string()))
            })
            .collect();
        entries.sort_unstable();
        entries.dedup();
        let entries: Vec<(u8, &str)> = entries
            .iter()
            .map(|(code, text)| (*code, text.as_str()))
            .collect();
        let cmap = write_simple_to_unicode_cmap(&entries);
        dict.insert("ToUnicode", doc.add(Stream::new(Dict::new(), cmap)));
    }
    Ok(Encoded {
        font: doc.add(dict),
        name: font.name().to_owned(),
        chars,
        kerning: Kerning::None,
        em: 1000.0,
    })
}

/// A subset of `font` with a ToUnicode map, drawing each distinct character of
/// `text`.
fn embedded<'f>(
    doc: &mut Document,
    font: &'f Font,
    spec: &str,
    text: &str,
) -> Result<Encoded<'f>, GoatError> {
    let name = font.postscript_name().unwrap_or_else(|| spec.to_owned());
    let em = f64::from(font.units_per_em());
    let mut seen = HashSet::new();
    let mut requests = Vec::new();
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for (offset, ch) in text.char_indices() {
        if ch == '\n' || seen.contains(&ch) {
            continue;
        }
        let glyph = font
            .glyph_for_char(ch)
            .filter(|&glyph| glyph != 0)
            .and_then(|glyph| Some((glyph, font.advance(glyph)?)));
        let Some((glyph, advance)) = glyph else {
            missing.push(ch);
            continue;
        };
        seen.insert(ch);
        requests.push(EmbedGlyph {
            glyph_id: glyph,
            unicode: &text[offset..offset + ch.len_utf8()],
        });
        found.push((ch, glyph, f64::from(advance) / em * 1000.0));
    }
    unencodable(&name, &missing)?;
    let embedded = embed_font(doc, font, &requests)
        .map_err(|e| GoatError::message(format!("cannot embed {name}: {e}")))?;
    let chars = found
        .into_iter()
        .zip(embedded.codes)
        .map(|((ch, glyph, width), code)| {
            (
                ch,
                Coded {
                    code,
                    width,
                    glyph: Some(glyph),
                },
            )
        })
        .collect();
    Ok(Encoded {
        font: embedded.font,
        name,
        chars,
        kerning: Kerning::of(font),
        em,
    })
}

/// Fails naming each character `font` has no glyph for, in order of first use.
fn unencodable(font: &str, missing: &[char]) -> Result<(), GoatError> {
    if missing.is_empty() {
        return Ok(());
    }
    let mut distinct = Vec::new();
    for &ch in missing {
        if !distinct.contains(&ch) {
            distinct.push(ch);
        }
    }
    let list = distinct
        .iter()
        .map(|ch| format!("{ch} (U+{:04X})", u32::from(*ch)))
        .collect::<Vec<_>>()
        .join(", ");
    let hint = if distinct
        .iter()
        .any(|ch| matches!(ch, '✓' | '✔' | '✗' | '✘'))
    {
        "; ZapfDingbats draws ✓ ✔ ✗ ✘"
    } else {
        ""
    };
    Err(GoatError::message(format!(
        "{font} cannot encode {list}; choose a --font that has these characters{hint}"
    )))
}

/// Maps the `search` frame to the page's user space.
fn from_frame(page: &Page) -> Result<Matrix, GoatError> {
    unrotated_transform(page)
        .invert()
        .ok_or_else(|| GoatError::message("page has a singular transform"))
}

/// Adds `object` to the `category` resources under the first unused
/// `{prefix}{n}` name, so an existing resource is never replaced.
fn add_resource(
    doc: &Document,
    resources: &mut Dict,
    category: &str,
    prefix: &str,
    object: ObjRef,
) -> Result<String, GoatError> {
    let mut entries = doc
        .resolve_key(resources, category.as_bytes())
        .map_err(EditError::from)?
        .as_dict()
        .cloned()
        .unwrap_or_default();
    let mut n = 1;
    let name = loop {
        let name = format!("{prefix}{n}");
        if !entries.contains_key(name.as_bytes()) {
            break name;
        }
        n += 1;
    };
    entries.insert(name.as_str(), object);
    resources.insert(category, entries);
    Ok(name)
}

/// Makes `bytes` the page's last content stream and stores `resources` on the
/// page. The existing streams are first balanced and wrapped in q/Q, so no
/// state they leave set reaches the new stream. Returns the new stream's index
/// in /Contents, the `part` extraction reports for what it draws.
fn append_isolated(
    doc: &mut Document,
    page: Page,
    resources: Dict,
    bytes: Vec<u8>,
) -> Result<usize, GoatError> {
    let (unopened, unclosed) = q_balance(&doc.page_content(&page).map_err(EditError::from)?)?;
    let mut dict = page.dict;
    let mut parts = match dict.get(b"Contents") {
        Some(value) => match doc.resolve(value).map_err(EditError::from)? {
            Object::Array(items) => items,
            Object::Stream(_) => vec![value.clone()],
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    let mut data = Vec::new();
    if !parts.is_empty() {
        let head = doc.add(Stream::new(Dict::new(), b"q\n".repeat(unopened + 1)));
        parts.insert(0, Object::Reference(head));
        data = b"Q\n".repeat(unclosed + 1);
    }
    data.extend(bytes);
    parts.push(Object::Reference(doc.add(Stream::new(Dict::new(), data))));
    let part = parts.len() - 1;
    dict.insert("Contents", parts);
    dict.insert("Resources", resources);
    doc.set(page.id, dict);
    Ok(part)
}

/// `Q` operators with no open `q` (they would pop a wrapper's `q`) and `q`
/// operators still open at the end of the content.
fn q_balance(content: &[u8]) -> Result<(usize, usize), GoatError> {
    let mut depth = 0usize;
    let mut unopened = 0usize;
    for operation in parse_content(content).map_err(EditError::from)? {
        match operation.operator.as_slice() {
            b"q" => depth += 1,
            b"Q" if depth == 0 => unopened += 1,
            b"Q" => depth -= 1,
            _ => {}
        }
    }
    Ok((unopened, depth))
}

/// Union of the extraction boxes of the glyphs content stream `part` draws,
/// read with `search`'s flags; `None` when none land on the page.
fn drawn_text(doc: &Document, index: usize, part: usize) -> Result<Option<Rect>, GoatError> {
    let page = pdf_text::extract_page(
        doc,
        index,
        TextFlags::WORDS.without(TextFlags::PRESERVE_LIGATURES),
    )?;
    let source = ContentSource::Page { part };
    Ok(page
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text(block) => Some(block),
            Block::Image(_) => None,
        })
        .flat_map(|block| &block.lines)
        .flat_map(|line| &line.chars)
        .filter(|ch| ch.source.source == source)
        .map(|ch| ch.quad.rect())
        .reduce(|a, b| a.union(&b)))
}

/// An embedded image XObject and what is needed to draw it upright.
pub struct Image {
    pub stream: ObjRef,
    pub width: u32,
    pub height: u32,
    pub orientation: u16,
}

/// A JPEG embedded unchanged as DCT, or a PNG as 8-bit samples with its alpha
/// as a soft mask.
pub fn image_xobject(doc: &mut Document, data: &[u8]) -> Result<Image, GoatError> {
    let codec = |e: pdf_codec::CodecError| GoatError::message(format!("--image: {e}"));
    match pdf_codec::detect_format(data) {
        Some(ImageFormat::Jpeg) => {
            let jpeg = pdf_codec::jpeg_passthrough(data, 72).map_err(codec)?;
            let mut dict = image_dict(jpeg.width, jpeg.height);
            dict.insert("Filter", Object::name("DCTDecode"));
            dict.insert(
                "ColorSpace",
                color_space(doc, jpeg.color_space, jpeg.icc_profile),
            );
            // Adobe (APP14) CMYK and YCCK JPEGs store inverted ink values.
            if jpeg.inverted_cmyk {
                dict.insert("Decode", numbers(&[1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0]));
            }
            Ok(Image {
                stream: doc.add(Stream::new(dict, data.to_vec())),
                width: jpeg.width,
                height: jpeg.height,
                orientation: jpeg.exif_orientation.unwrap_or(1),
            })
        }
        Some(ImageFormat::Png) => {
            let png = pdf_codec::decode_png(data).map_err(codec)?;
            let rgba = png.to_rgba8();
            let pixels = rgba.as_chunks::<4>().0;
            let (space, samples): (PdfColorSpace, Vec<u8>) =
                if matches!(png.layout, PixelLayout::Gray | PixelLayout::GrayAlpha) {
                    (
                        PdfColorSpace::DeviceGray,
                        pixels.iter().map(|px| px[0]).collect(),
                    )
                } else {
                    (
                        PdfColorSpace::DeviceRGB,
                        pixels.iter().flat_map(|px| [px[0], px[1], px[2]]).collect(),
                    )
                };
            let signature = if space == PdfColorSpace::DeviceGray {
                *b"GRAY"
            } else {
                *b"RGB "
            };
            let profile = png
                .icc_profile
                .filter(|profile| pdf_codec::icc_color_space(profile) == Some(signature));
            let mut dict = image_dict(png.width, png.height);
            dict.insert("ColorSpace", color_space(doc, space, profile));
            if pixels.iter().any(|px| px[3] < 255) {
                let mut mask = image_dict(png.width, png.height);
                mask.insert("ColorSpace", Object::name("DeviceGray"));
                let alpha = pixels.iter().map(|px| px[3]).collect();
                dict.insert("SMask", doc.add(Stream::new(mask, alpha)));
            }
            Ok(Image {
                stream: doc.add(Stream::new(dict, samples)),
                width: png.width,
                height: png.height,
                orientation: 1,
            })
        }
        _ => Err(GoatError::message("--image must be a PNG or JPEG file")),
    }
}

fn image_dict(width: u32, height: u32) -> Dict {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Image"));
    dict.insert("Width", width);
    dict.insert("Height", height);
    dict.insert("BitsPerComponent", 8_i64);
    dict
}

fn color_space(doc: &mut Document, space: PdfColorSpace, profile: Option<Vec<u8>>) -> Object {
    let device = match space {
        PdfColorSpace::DeviceGray => "DeviceGray",
        PdfColorSpace::DeviceRGB => "DeviceRGB",
        PdfColorSpace::DeviceCMYK => "DeviceCMYK",
    };
    let Some(profile) = profile else {
        return Object::name(device);
    };
    let mut dict = Dict::new();
    dict.insert("N", i64::from(space.components()));
    dict.insert("Alternate", Object::name(device));
    Object::Array(vec![
        Object::name("ICCBased"),
        Object::Reference(doc.add(Stream::new(dict, profile))),
    ])
}

/// The image matrix that shows the stored samples upright in `r` (user space)
/// for an EXIF orientation; 1 and unknown values draw the samples as stored.
pub fn image_matrix(orientation: u16, r: Rect) -> [f64; 6] {
    let (x, y, w, h) = (r.x0, r.y0, r.width(), r.height());
    match orientation {
        2 => [-w, 0.0, 0.0, h, x + w, y],
        3 => [-w, 0.0, 0.0, -h, x + w, y + h],
        4 => [w, 0.0, 0.0, -h, x, y + h],
        5 => [0.0, -h, -w, 0.0, x + w, y + h],
        6 => [0.0, -h, w, 0.0, x, y + h],
        7 => [0.0, h, w, 0.0, x, y],
        8 => [0.0, h, -w, 0.0, x + w, y],
        _ => [w, 0.0, 0.0, h, x, y],
    }
}
