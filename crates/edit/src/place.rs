//! `edit add-text` and `edit add-image` draw into the page content at a point
//! or inside a rectangle of the `search` frame: crop box top-left origin, y
//! down, /Rotate ignored. Text uses a Standard 14 font or an embedded TrueType
//! subset with ToUnicode, so `text` and `search` read it back, and its box is
//! the one extraction measures for the new glyphs. The existing content is
//! balanced and wrapped in q/Q first, so a transformation, clip or colour it
//! leaves set cannot move or hide the addition.

use std::collections::HashMap;
use std::fs;

use goat_common::GoatError;
use goat_common::paths::resolve;
use pdf_codec::{ImageFormat, PdfColorSpace, PixelLayout};
use pdf_core::{
    Dict, Document, Matrix, ObjRef, Object, Operation, Page, PdfString, Point, Rect, Stream,
    parse_content, write_content,
};
use pdf_font::{
    BaseEncoding, Font, FontKind, FontLocator, FontRequest, GlyphMapping, MatchQuality, Script,
    Standard14, embed_truetype,
};
use pdf_interp::ContentSource;
use pdf_text::{Block, TextFlags};

use crate::EditError;
use crate::redact::{num, numbers, op, unrotated_transform};

/// One line of text to draw.
pub(crate) struct Text<'a> {
    pub text: &'a str,
    /// Start of the baseline in the `search` frame.
    pub at: Point,
    /// A Standard 14 name, an installed font's name, or a font file path.
    pub font: &'a str,
    pub size: f64,
    /// 1 (gray) or 3 (RGB) components from 0 to 1.
    pub color: &'a [f64],
}

/// Where the text landed and the font that drew it.
pub(crate) struct Drawn {
    /// Union of the new glyphs' extraction boxes in the `search` frame.
    pub bbox: Rect,
    /// PostScript name of the font used.
    pub font: String,
}

pub(crate) fn add_text(
    doc: &mut Document,
    index: usize,
    text: &Text<'_>,
) -> Result<Drawn, GoatError> {
    check_text(text)?;
    let fill = fill_color(text.color)?;
    let face = face(text.font)?;
    let encoded = match &face {
        Face::Standard(font) => standard(doc, *font, text.text)?,
        Face::Embedded(font) => embedded(doc, font, text.font, text.text)?,
    };
    let mut page = doc.page(index).map_err(EditError::from)?;
    let origin = text.at.transform(&from_frame(&page)?);
    let size = text.size / page.user_unit();
    let mut resources = std::mem::take(&mut page.resources);
    let name = add_resource(doc, &mut resources, "Font", "GoatF", encoded.font)?;
    let ops = [
        op("q", vec![]),
        op("BT", vec![]),
        fill,
        op("Tf", vec![Object::name(name.as_str()), num(size)]),
        op("Tm", numbers(&[1.0, 0.0, 0.0, 1.0, origin.x, origin.y])),
        op("Tj", vec![Object::String(PdfString::hex(encoded.bytes))]),
        op("ET", vec![]),
        op("Q", vec![]),
    ];
    let part = append_isolated(doc, page, resources, write_content(&ops))?;
    let bbox = drawn_text(doc, index, part)?.ok_or_else(|| {
        GoatError::message(format!(
            "--at {},{} puts the text outside page {}",
            text.at.x,
            text.at.y,
            index + 1
        ))
    })?;
    Ok(Drawn {
        bbox,
        font: encoded.name,
    })
}

/// Draws a PNG or JPEG inside `rect` (the `search` frame), keeping its aspect
/// ratio and centred, upright by its EXIF orientation; returns the box it fills.
pub(crate) fn add_image(
    doc: &mut Document,
    index: usize,
    data: &[u8],
    rect: Rect,
) -> Result<Rect, GoatError> {
    let rect = rect.normalized();
    if ![rect.x0, rect.y0, rect.x1, rect.y1]
        .iter()
        .all(|v| v.is_finite())
        || rect.is_empty()
    {
        return Err(GoatError::message(
            "--rect must have positive width and height",
        ));
    }
    let image = image_xobject(doc, data)?;
    let (width, height) = if (5..=8).contains(&image.orientation) {
        (image.height, image.width)
    } else {
        (image.width, image.height)
    };
    let (width, height) = (f64::from(width), f64::from(height));
    let scale = (rect.width() / width).min(rect.height() / height);
    let (w, h) = (width * scale, height * scale);
    let x0 = rect.x0 + (rect.width() - w) / 2.0;
    let y0 = rect.y0 + (rect.height() - h) / 2.0;
    let placed = Rect::new(x0, y0, x0 + w, y0 + h);
    let mut page = doc.page(index).map_err(EditError::from)?;
    let user = placed.transform(&from_frame(&page)?).normalized();
    let mut resources = std::mem::take(&mut page.resources);
    let name = add_resource(doc, &mut resources, "XObject", "GoatIm", image.stream)?;
    let ops = [
        op("q", vec![]),
        op("cm", numbers(&image_matrix(image.orientation, user))),
        op("Do", vec![Object::name(name.as_str())]),
        op("Q", vec![]),
    ];
    append_isolated(doc, page, resources, write_content(&ops))?;
    Ok(placed)
}

fn check_text(text: &Text<'_>) -> Result<(), GoatError> {
    if text.text.is_empty() {
        return Err(GoatError::message("--text is empty"));
    }
    if let Some(ch) = text.text.chars().find(|ch| ch.is_control()) {
        return Err(GoatError::message(format!(
            "--text must be one line without control characters; found U+{:04X}",
            u32::from(ch)
        )));
    }
    if !(text.at.x.is_finite() && text.at.y.is_finite()) {
        return Err(GoatError::message("--at must be two finite numbers"));
    }
    if !(text.size.is_finite() && text.size > 0.0) {
        return Err(GoatError::message("--size must be a positive number"));
    }
    Ok(())
}

/// The fill operator for 1 (gray) or 3 (RGB) colour components.
fn fill_color(color: &[f64]) -> Result<Operation, GoatError> {
    let operator = match color.len() {
        1 => "g",
        3 => "rg",
        _ => "",
    };
    if operator.is_empty() || !color.iter().all(|c| (0.0..=1.0).contains(c)) {
        return Err(GoatError::value_error(
            "need 1 or 3 color components in range 0 to 1",
        ));
    }
    Ok(op(operator, numbers(color)))
}

enum Face {
    Standard(Standard14),
    Embedded(Font),
}

/// `--font`: a font file path, one of the Standard 14 names (any case), or an
/// installed face matched by PostScript or family name. A stand-in from
/// another family is refused rather than drawn in the wrong face.
fn face(spec: &str) -> Result<Face, GoatError> {
    let lower = spec.to_ascii_lowercase();
    if spec.contains('/')
        || [".ttf", ".otf", ".ttc"]
            .iter()
            .any(|ext| lower.ends_with(ext))
    {
        let path = resolve(spec)?;
        let data = fs::read(&path).map_err(|e| GoatError::os(&e, &path))?;
        let font = Font::parse(data)
            .map_err(|e| GoatError::message(format!("{}: {e}", path.display())))?;
        return Ok(Face::Embedded(font));
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
            "font not found: {spec}; use a Standard 14 name, an installed font's name, or a .ttf/.ttc path"
        ))),
    }
}

/// A font resource and the text's string operand.
struct Encoded {
    font: ObjRef,
    name: String,
    bytes: Vec<u8>,
}

/// Single-byte codes: WinAnsi for the Latin faces, the built-in encoding for
/// Symbol and ZapfDingbats. ZapfDingbats also takes printable ASCII as its
/// codes, so `4` draws ✔ as it does in PyMuPDF.
fn standard(doc: &mut Document, font: Standard14, text: &str) -> Result<Encoded, GoatError> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut missing = Vec::new();
    for ch in text.chars() {
        let code = match font {
            Standard14::Symbol => BaseEncoding::Symbol.from_unicode(ch),
            Standard14::ZapfDingbats => BaseEncoding::ZapfDingbats.from_unicode(ch).or_else(|| {
                u8::try_from(ch)
                    .ok()
                    .filter(|code| (0x21..=0x7E).contains(code))
            }),
            _ => BaseEncoding::WinAnsi.from_unicode(ch).filter(|&code| {
                BaseEncoding::WinAnsi
                    .glyph_name(code)
                    .and_then(|name| font.glyph_width(name))
                    .is_some()
            }),
        };
        match code {
            Some(code) => bytes.push(code),
            None => missing.push(ch),
        }
    }
    unencodable(font.name(), &missing)?;
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Font"));
    dict.insert("Subtype", Object::name("Type1"));
    dict.insert("BaseFont", Object::name(font.name()));
    match font {
        Standard14::Symbol | Standard14::ZapfDingbats => {
            let encoding = if font == Standard14::Symbol {
                BaseEncoding::Symbol
            } else {
                BaseEncoding::ZapfDingbats
            };
            let cmap = builtin_to_unicode(encoding, &bytes);
            dict.insert("ToUnicode", doc.add(Stream::new(Dict::new(), cmap)));
        }
        _ => {
            dict.insert("Encoding", Object::name("WinAnsiEncoding"));
        }
    }
    Ok(Encoded {
        font: doc.add(dict),
        name: font.name().to_owned(),
        bytes,
    })
}

/// A TrueType subset with one two-byte Identity-H code per distinct character
/// and a ToUnicode map, so extraction reads the text back.
fn embedded(doc: &mut Document, font: &Font, spec: &str, text: &str) -> Result<Encoded, GoatError> {
    let name = font.postscript_name().unwrap_or_else(|| spec.to_owned());
    if font.kind() != FontKind::TrueType {
        return Err(GoatError::message(format!(
            "{name} has PostScript (CFF or Type 1) outlines; add-text embeds TrueType outlines only"
        )));
    }
    let mut codes: HashMap<char, u16> = HashMap::new();
    let mut mappings: Vec<GlyphMapping<'_>> = Vec::new();
    let mut missing = Vec::new();
    let mut bytes = Vec::with_capacity(text.len() * 2);
    for (offset, ch) in text.char_indices() {
        let code = match codes.get(&ch) {
            Some(&code) => code,
            None => {
                let Some(glyph_id) = font.glyph_for_char(ch).filter(|&glyph| glyph != 0) else {
                    missing.push(ch);
                    continue;
                };
                let code = u16::try_from(mappings.len() + 1).map_err(|_| {
                    GoatError::message("--text has more distinct characters than a font can encode")
                })?;
                codes.insert(ch, code);
                mappings.push(GlyphMapping {
                    code,
                    glyph_id,
                    unicode: &text[offset..offset + ch.len_utf8()],
                });
                code
            }
        };
        bytes.extend_from_slice(&code.to_be_bytes());
    }
    unencodable(&name, &missing)?;
    let font = embed_truetype(doc, font, &mappings)
        .map_err(|e| GoatError::message(format!("cannot embed {name}: {e}")))?;
    Ok(Encoded { font, name, bytes })
}

/// A one-byte ToUnicode CMap for the `codes` drawn with Symbol or ZapfDingbats,
/// so extraction reads ✔ rather than the built-in code.
fn builtin_to_unicode(encoding: BaseEncoding, codes: &[u8]) -> Vec<u8> {
    let mut entries: Vec<(u8, char)> = codes
        .iter()
        .filter_map(|&code| Some((code, encoding.to_unicode(code)?)))
        .collect();
    entries.sort_unstable();
    entries.dedup();
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<00> <FF>\nendcodespacerange\n",
    );
    for block in entries.chunks(100) {
        out.push_str(&format!("{} beginbfchar\n", block.len()));
        for (code, ch) in block {
            out.push_str(&format!("<{code:02X}> <"));
            for unit in ch.encode_utf16(&mut [0; 2]).iter() {
                out.push_str(&format!("{unit:04X}"));
            }
            out.push_str(">\n");
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out.into_bytes()
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

struct Image {
    stream: ObjRef,
    width: u32,
    height: u32,
    orientation: u16,
}

/// A JPEG embedded unchanged as DCT, or a PNG as 8-bit samples with its alpha
/// as a soft mask.
fn image_xobject(doc: &mut Document, data: &[u8]) -> Result<Image, GoatError> {
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
fn image_matrix(orientation: u16, r: Rect) -> [f64; 6] {
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
