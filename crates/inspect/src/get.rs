//! `get object|fonts|images|attachments`, `attach`, and `detach`.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command};
use goat_common::args::{flag, int_value, many, optional, required};
use goat_common::parse::parse_pages;
use goat_common::paths;
use goat_common::py::repr_str;
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_codec::{
    CcittParams, DctParams, PngColor, decode_ccitt, decode_dct, decode_jbig2, decode_jpx,
    encode_png, pixels,
};
use pdf_core::filters::filter_chain;
use pdf_core::{Dict, Document, Name, Object, Page, PdfDate, PdfString, Stream};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::doc::{self, ImageEntry};

pub(crate) fn register(registry: &mut Registry) {
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("images")
                .about("extract embedded images")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("outdir").short('o').long("outdir")),
            get_images,
        ),
    );
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("fonts")
                .about("list fonts")
                .arg(Arg::new("file").required(true)),
            get_fonts,
        ),
    );
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("object")
                .about("show one PDF object and the start of its decoded stream")
                .arg(Arg::new("file").required(true))
                .arg(
                    Arg::new("target")
                        .required(true)
                        .help("object number, catalog, trailer, or page:N"),
                )
                .arg(
                    Arg::new("max_bytes")
                        .long("max-bytes")
                        .value_parser(int_value)
                        .default_value("4096")
                        .help("stream bytes to return"),
                ),
            get_object,
        ),
    );
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("attachments")
                .about("extract embedded files")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("outdir").short('o').long("outdir")),
            get_attachments,
        ),
    );
    registry.command(Verb::new(
        Command::new("attach")
            .about("embed a file attachment")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("attachment").required(true))
            .arg(Arg::new("output").short('o').long("output")),
        attach,
    ));
    registry.command(Verb::new(
        Command::new("detach")
            .about("remove embedded file attachments")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("names").long("name").action(ArgAction::Append))
            .arg(
                Arg::new("remove_all")
                    .long("all")
                    .action(ArgAction::SetTrue),
            )
            .group(
                ArgGroup::new("target")
                    .args(["names", "remove_all"])
                    .required(true),
            )
            .arg(Arg::new("output").short('o').long("output")),
        detach,
    ));
}

// ----------------------------------------------------------------------------------- //
// get fonts / get object
// ----------------------------------------------------------------------------------- //

fn get_fonts(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_pymupdf(required::<String>(matches, "file")?)?;
    let document = &opened.doc;
    let mut fonts: Vec<(u32, Value)> = Vec::new();
    for page in doc::pages(document)? {
        for font in doc::page_fonts(document, &page) {
            let value = json!({"name": font.name, "type": font.subtype, "ext": font.ext, "encoding": font.encoding});
            match fonts.iter_mut().find(|(xref, _)| *xref == font.xref) {
                Some(slot) => slot.1 = value,
                None => fonts.push((font.xref, value)),
            }
        }
    }
    let mut result = doc::result("get-fonts", opened.inputs(), Vec::new());
    result.insert("count".to_owned(), json!(fonts.len()));
    result.insert(
        "fonts".to_owned(),
        Value::Array(fonts.into_iter().map(|(_, value)| value).collect()),
    );
    Ok(result)
}

/// `_object_xref`: `None` names the trailer.
fn object_xref(
    document: &Document,
    target: &str,
    page_count: usize,
) -> Result<Option<u32>, GoatError> {
    if target == "catalog" {
        return Ok(Some(document.catalog_ref().map_err(doc::pdf_error)?.num));
    }
    if target == "trailer" {
        return Ok(None);
    }
    if let Some(spec) = target.strip_prefix("page:") {
        let indices = parse_pages(spec, page_count)?;
        let [index] = indices[..] else {
            return Err(GoatError::message(format!(
                "{} must name one page",
                repr_str(target)
            )));
        };
        return Ok(Some(document.page(index).map_err(doc::pdf_error)?.id.num));
    }
    let size = document.xref_size();
    if !target.is_empty()
        && target.chars().all(|ch| ch.is_ascii_digit())
        && let Ok(number) = target.parse::<u32>()
        && number > 0
        && number < size
    {
        return Ok(Some(number));
    }
    Err(GoatError::message(format!(
        "{} is not catalog, trailer, page:N, or an object number from 1 to {}",
        repr_str(target),
        size.saturating_sub(1)
    )))
}

fn get_object(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let max_bytes = *required::<i64>(matches, "max_bytes")?;
    if !(1..=1_000_000).contains(&max_bytes) {
        return Err(GoatError::message(
            "--max-bytes must be between 1 and 1000000",
        ));
    }
    let max_bytes = usize::try_from(max_bytes).unwrap_or(4096);
    let opened = doc::open_unlocked(required::<String>(matches, "file")?, "get object")?;
    let document = &opened.doc;
    let page_count = document.page_count().map_err(doc::pdf_error)?;
    let xref = object_xref(document, required::<String>(matches, "target")?, page_count)?;
    let (source, data) = match xref {
        None => (
            Object::Dict(document.trailer().clone()).to_pretty_string(),
            None,
        ),
        Some(number) => match document.get(pdf_core::ObjRef::new(number, 0)) {
            Ok(object) => {
                let data = match &object {
                    Object::Stream(stream) => {
                        Some(document.decode_stream(stream).map_err(doc::pdf_error)?.data)
                    }
                    _ => None,
                };
                (object.to_pretty_string(), data)
            }
            Err(_) => ("null".to_owned(), None),
        },
    };
    let mut result = doc::result("get-object", opened.inputs(), Vec::new());
    result.insert(
        "xref".to_owned(),
        xref.map_or(Value::Null, |number| json!(number)),
    );
    result.insert("object".to_owned(), Value::String(source));
    result.insert(
        "stream".to_owned(),
        match data {
            None => Value::Null,
            Some(data) => {
                let head: String = data
                    .iter()
                    .take(max_bytes)
                    .map(|&byte| char::from(byte))
                    .collect();
                json!({"length": data.len(), "text": head, "truncated": data.len() > max_bytes})
            }
        },
    );
    Ok(result)
}

// ----------------------------------------------------------------------------------- //
// get images
// ----------------------------------------------------------------------------------- //

/// The colour model an image's `/ColorSpace` selects.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Color {
    Gray,
    Rgb,
    Cmyk,
    /// Palette entries in the base colour's channels.
    Indexed {
        base: Box<Color>,
        palette: Vec<u8>,
    },
    /// Separation, DeviceN, or an unknown space with this many channels.
    Other(usize),
}

impl Color {
    pub(crate) fn channels(&self) -> usize {
        match self {
            Color::Gray | Color::Indexed { .. } => 1,
            Color::Rgb => 3,
            Color::Cmyk => 4,
            Color::Other(n) => *n,
        }
    }

    fn from_channels(n: usize) -> Color {
        match n {
            1 => Color::Gray,
            3 => Color::Rgb,
            4 => Color::Cmyk,
            other => Color::Other(other),
        }
    }
}

fn color_space(document: &Document, space: Option<&Object>, depth: usize) -> Color {
    let Some(space) = space.and_then(|space| document.resolve(space).ok()) else {
        return Color::Gray;
    };
    if depth > 8 {
        return Color::Gray;
    }
    match &space {
        Object::Name(name) => match name.as_bytes() {
            b"DeviceGray" | b"G" | b"CalGray" => Color::Gray,
            b"DeviceRGB" | b"RGB" | b"CalRGB" | b"Lab" => Color::Rgb,
            b"DeviceCMYK" | b"CMYK" => Color::Cmyk,
            b"Pattern" => Color::Other(1),
            _ => Color::Gray,
        },
        Object::Array(items) => {
            let family = items.first().and_then(Object::as_name).unwrap_or(b"");
            match family {
                b"ICCBased" => {
                    let n = items
                        .get(1)
                        .and_then(|s| document.resolve_stream(s).ok().flatten())
                        .and_then(|s| s.dict.get_i64(b"N"))
                        .unwrap_or(3);
                    Color::from_channels(usize::try_from(n).unwrap_or(3))
                }
                b"CalRGB" | b"Lab" => Color::Rgb,
                b"CalGray" => Color::Gray,
                b"Indexed" | b"I" => {
                    let base = color_space(document, items.get(1), depth + 1);
                    let palette = match items.get(3).map(|lookup| document.resolve(lookup)) {
                        Some(Ok(Object::String(text))) => text.bytes.clone(),
                        Some(Ok(Object::Stream(stream))) => document
                            .decode_stream(&stream)
                            .map(|d| d.data)
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    Color::Indexed {
                        base: Box::new(base),
                        palette,
                    }
                }
                b"Separation" => Color::Other(1),
                b"DeviceN" => {
                    let n = items
                        .get(1)
                        .and_then(|names| document.resolve_array(names).ok().flatten())
                        .map_or(1, |n| n.len());
                    Color::Other(n.max(1))
                }
                b"DeviceGray" | b"G" => Color::Gray,
                b"DeviceRGB" | b"RGB" => Color::Rgb,
                b"DeviceCMYK" | b"CMYK" => Color::Cmyk,
                _ => Color::Gray,
            }
        }
        _ => Color::Gray,
    }
}

/// pikepdf's `mode` for the direct-extraction test.
fn direct_mode(color: &Color, bpc: u8) -> Option<&'static str> {
    match (color, bpc) {
        (Color::Gray, 8) => Some("L"),
        (Color::Rgb, 8) => Some("RGB"),
        (Color::Cmyk, 8) => Some("CMYK"),
        _ => None,
    }
}

/// A little-endian TIFF with one strip.
fn tiff(
    width: u32,
    height: u32,
    bits: &[u16],
    photometric: u16,
    compression: u16,
    extra: &[(u16, u32)],
    data: &[u8],
) -> Vec<u8> {
    let samples = u16::try_from(bits.len()).unwrap_or(1);
    let rows = height.max(1);
    // (tag, type, count, value-or-offset placeholder)
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, width),
        (257, 4, 1, height),
        (258, 3, u32::from(samples), 0),
        (259, 3, 1, u32::from(compression)),
        (262, 3, 1, u32::from(photometric)),
        (273, 4, 1, 0),
        (277, 3, 1, u32::from(samples)),
        (278, 4, 1, rows),
        (279, 4, 1, u32::try_from(data.len()).unwrap_or(u32::MAX)),
        (284, 3, 1, 1),
    ];
    for (tag, value) in extra {
        entries.push((*tag, 4, 1, *value));
    }
    entries.sort_by_key(|entry| entry.0);
    let count = u32::try_from(entries.len()).unwrap_or(0);
    let ifd_len = 2 + 12 * count + 4;
    let bits_offset = 8 + ifd_len;
    let bits_len = if samples > 2 {
        2 * u32::from(samples)
    } else {
        0
    };
    let data_offset = bits_offset + bits_len;
    let mut out = Vec::with_capacity(usize::try_from(data_offset).unwrap_or(0) + data.len());
    out.extend_from_slice(b"II*\0");
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap_or(0).to_le_bytes());
    for (tag, kind, n, value) in &entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        match tag {
            258 if samples > 2 => out.extend_from_slice(&bits_offset.to_le_bytes()),
            258 => {
                for bit in bits.iter().chain(std::iter::repeat(&0)).take(2) {
                    out.extend_from_slice(&bit.to_le_bytes());
                }
            }
            273 => out.extend_from_slice(&data_offset.to_le_bytes()),
            _ if *kind == 3 => {
                out.extend_from_slice(&u16::try_from(*value).unwrap_or(0).to_le_bytes());
                out.extend_from_slice(&[0, 0]);
            }
            _ => out.extend_from_slice(&value.to_le_bytes()),
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    if samples > 2 {
        for bit in bits {
            out.extend_from_slice(&bit.to_le_bytes());
        }
    }
    out.extend_from_slice(data);
    out
}

fn ccitt_params(parms: Option<&Dict>, width: u32, height: u32) -> CcittParams {
    let mut params = CcittParams {
        columns: width.max(1),
        rows: height,
        ..CcittParams::default()
    };
    if let Some(parms) = parms {
        params.k = parms
            .get_i64(b"K")
            .and_then(|k| i32::try_from(k).ok())
            .unwrap_or(0);
        params.columns = parms
            .get_i64(b"Columns")
            .and_then(|c| u32::try_from(c).ok())
            .filter(|c| *c > 0)
            .unwrap_or(1728);
        params.rows = parms
            .get_i64(b"Rows")
            .and_then(|r| u32::try_from(r).ok())
            .unwrap_or(height);
        params.encoded_byte_align = parms.get_bool(b"EncodedByteAlign").unwrap_or(false);
        params.end_of_line = parms.get_bool(b"EndOfLine").unwrap_or(false);
        params.end_of_block = parms.get_bool(b"EndOfBlock").unwrap_or(true);
        params.black_is_1 = parms.get_bool(b"BlackIs1").unwrap_or(false);
        params.damaged_rows_before_error = parms
            .get_i64(b"DamagedRowsBeforeError")
            .and_then(|d| u32::try_from(d).ok())
            .unwrap_or(0);
    }
    params
}

fn codec_error(error: pdf_codec::CodecError) -> GoatError {
    GoatError::message(error.to_string())
}

/// Packed 1-bit rows inverted in place (a `/Decode [1 0]` array).
fn invert(mut data: Vec<u8>) -> Vec<u8> {
    for byte in &mut data {
        *byte = !*byte;
    }
    data
}

/// 8-bit interleaved samples in the image's colour model, from the filter output.
pub(crate) struct Samples {
    pub(crate) color: Color,
    pub(crate) data: Vec<u8>,
}

/// The geometry and colour model an image dictionary declares.
pub(crate) struct ImageShape {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bpc: u8,
    pub(crate) image_mask: bool,
    pub(crate) color: Color,
}

impl ImageShape {
    pub(crate) fn of(document: &Document, dict: &Dict) -> Option<ImageShape> {
        let dimension = |key: &[u8]| {
            dict.get(key)
                .and_then(|v| document.resolve_i64(v).ok().flatten())
                .and_then(|v| u32::try_from(v).ok())
        };
        let width = dimension(b"Width")?;
        let height = dimension(b"Height")?;
        let image_mask = dict
            .get(b"ImageMask")
            .and_then(|m| document.resolve(m).ok())
            .and_then(|m| m.as_bool())
            .unwrap_or(false);
        let bpc = if image_mask {
            1
        } else {
            dict.get(b"BitsPerComponent")
                .and_then(|b| document.resolve_i64(b).ok().flatten())
                .and_then(|b| u8::try_from(b).ok())
                .unwrap_or(8)
        };
        let color = if image_mask {
            Color::Gray
        } else {
            color_space(document, dict.get(b"ColorSpace"), 0)
        };
        Some(ImageShape {
            width,
            height,
            bpc,
            image_mask,
            color,
        })
    }
}

pub(crate) fn decode_samples(
    document: &Document,
    stream: &Stream,
    shape: &ImageShape,
) -> Result<Samples, GoatError> {
    let (width, height, bpc, color) = (shape.width, shape.height, shape.bpc, shape.color.clone());
    let decoded = document.decode_stream(stream).map_err(doc::pdf_error)?;
    let decode_array: Option<Vec<f32>> = stream
        .dict
        .get(b"Decode")
        .and_then(|d| document.resolve_array(d).ok().flatten())
        .map(|items| {
            items
                .iter()
                .filter_map(Object::as_f64)
                .map(|v| v as f32)
                .collect()
        });
    let inverted = decode_array
        .as_ref()
        .is_some_and(|d| d.first().is_some_and(|first| *first >= 0.5));
    let Some(stopped) = decoded.stopped else {
        let channels = color.channels();
        // MuPDF pads a short (damaged or truncated) raster with zero bytes.
        let row_bytes = (width as usize * channels * usize::from(bpc)).div_ceil(8);
        let mut decoded = decoded;
        let expected = row_bytes.saturating_mul(height as usize);
        if decoded.data.len() < expected {
            decoded.data.resize(expected, 0);
        }
        let data = if bpc == 16 {
            pixels::sixteen_to_eight(&decoded.data)
        } else if bpc == 8 && decode_array.is_none() {
            decoded.data
        } else if let Color::Indexed { .. } = color {
            let samples = pixels::unpack_samples(&decoded.data, width, height, bpc, 1)
                .map_err(codec_error)?;
            return Ok(Samples {
                color,
                data: samples
                    .iter()
                    .map(|&s| u8::try_from(s).unwrap_or(u8::MAX))
                    .collect(),
            });
        } else {
            pixels::unpack_to_u8(
                &decoded.data,
                width,
                height,
                bpc,
                channels,
                decode_array.as_deref(),
            )
            .map_err(codec_error)?
        };
        return Ok(Samples { color, data });
    };
    let parms = stopped.parms.as_ref();
    match stopped.filter.as_bytes() {
        b"DCTDecode" => {
            let color_transform = parms
                .and_then(|p| p.get_i64(b"ColorTransform"))
                .and_then(|c| u32::try_from(c).ok());
            let image =
                decode_dct(&decoded.data, &DctParams { color_transform }).map_err(codec_error)?;
            let color = match (usize::from(image.components), &color) {
                (4, _) => Color::Cmyk,
                (n, Color::Indexed { .. }) => Color::from_channels(n),
                (n, existing) if existing.channels() == n => existing.clone(),
                (n, _) => Color::from_channels(n),
            };
            Ok(Samples {
                color,
                data: image.data,
            })
        }
        b"JPXDecode" => {
            let image = decode_jpx(&decoded.data).map_err(codec_error)?;
            let channels = usize::from(image.info.channels);
            let (color, data) = match (channels, image.info.has_alpha) {
                (4, true) => (Color::Rgb, pixels::rgba_to_rgb(&image.data)),
                (2, true) => (Color::Gray, image.data.chunks(2).map(|px| px[0]).collect()),
                (n, _) => (Color::from_channels(n), image.data),
            };
            Ok(Samples { color, data })
        }
        b"CCITTFaxDecode" => {
            let image = decode_ccitt(&decoded.data, &ccitt_params(parms, width, height))
                .map_err(codec_error)?;
            let data = if inverted {
                invert(image.image.data)
            } else {
                image.image.data
            };
            Ok(Samples {
                color: Color::Other(0),
                data,
            })
        }
        b"JBIG2Decode" => {
            let globals = parms
                .and_then(|p| p.get(b"JBIG2Globals"))
                .and_then(|g| document.resolve_stream(g).ok().flatten())
                .and_then(|g| document.decode_stream(&g).ok())
                .map(|g| g.data);
            let image = decode_jbig2(&decoded.data, globals.as_deref()).map_err(codec_error)?;
            let data = if inverted {
                invert(image.data)
            } else {
                image.data
            };
            Ok(Samples {
                color: Color::Other(0),
                data,
            })
        }
        other => Err(GoatError::message(format!(
            "unsupported image filter {}",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// `pikepdf.PdfImage.extract_to`: the extension and bytes for one image object.
fn extract_image(
    document: &Document,
    entry: &ImageEntry,
) -> Result<(&'static str, Vec<u8>), GoatError> {
    let stream = &entry.stream;
    let dict = &stream.dict;
    let shape = ImageShape::of(document, dict).unwrap_or(ImageShape {
        width: 0,
        height: 0,
        bpc: 8,
        image_mask: false,
        color: Color::Gray,
    });
    let (width, height, bpc) = (shape.width, shape.height, shape.bpc);
    let color = &shape.color;
    let filter = dict
        .get(b"Filter")
        .map(|f| document.resolve(f))
        .transpose()
        .map_err(doc::pdf_error)?;
    let parms = dict
        .get(b"DecodeParms")
        .map(|p| document.resolve(p))
        .transpose()
        .map_err(doc::pdf_error)?;
    let chain: Vec<(Name, Option<Dict>)> = filter_chain(filter.as_ref(), parms.as_ref())
        .into_iter()
        .map(|(name, parms)| {
            (
                Name::new(pdf_core::filters::canonical_filter_name(name.as_bytes()).to_vec()),
                parms,
            )
        })
        .collect();
    if let [(name, parms)] = &chain[..] {
        let color_transform = |default: i64| {
            parms
                .as_ref()
                .and_then(|p| p.get_i64(b"ColorTransform"))
                .unwrap_or(default)
        };
        match name.as_bytes() {
            b"CCITTFaxDecode" => {
                let params = ccitt_params(parms.as_ref(), width, height);
                let (compression, extra): (u16, Vec<(u16, u32)>) = if params.k < 0 {
                    (4, Vec::new())
                } else if params.k > 0 {
                    (3, vec![(292, 1)])
                } else {
                    (3, Vec::new())
                };
                let photometric = u16::from(params.black_is_1);
                return Ok((
                    "tif",
                    tiff(
                        width,
                        height,
                        &[1],
                        photometric,
                        compression,
                        &extra,
                        stream.raw(),
                    ),
                ));
            }
            b"DCTDecode" => {
                let direct = match direct_mode(color, bpc) {
                    Some("L") => true,
                    Some("RGB") => color_transform(1) == 1,
                    Some("CMYK") => color_transform(0) == 0,
                    _ => false,
                };
                if direct {
                    return Ok(("jpg", stream.raw().to_vec()));
                }
                // pikepdf refuses a CMYK JPEG with an Adobe colour transform; the
                // reference CLI then copies a lone DCT stream without a Decode array.
                if matches!(color, Color::Cmyk | Color::Other(_)) && !dict.contains_key(b"Decode") {
                    return Ok(("jpg", stream.raw().to_vec()));
                }
            }
            b"JPXDecode" => return Ok(("jp2", stream.raw().to_vec())),
            _ => {}
        }
    }
    if width == 0 || height == 0 {
        return Err(GoatError::message(format!(
            "image {} has no size",
            entry.xref
        )));
    }
    let lone_dct = matches!(&chain[..], [(name, _)] if name.as_bytes() == b"DCTDecode")
        && !dict.contains_key(b"Decode");
    let samples = match decode_samples(document, stream, &shape) {
        Ok(samples) => samples,
        // pikepdf declines; the reference CLI copies a lone DCT stream as stored.
        Err(_) if lone_dct => return Ok(("jpg", stream.raw().to_vec())),
        Err(error) => return Err(error),
    };
    let (color, data) = match samples.color {
        Color::Indexed { base, palette } => {
            let indices: Vec<u16> = samples.data.iter().map(|&i| u16::from(i)).collect();
            let expanded = pixels::expand_palette(&indices, &palette, base.channels());
            (*base, expanded)
        }
        other => (other, samples.data),
    };
    let png = |data: &[u8], kind: PngColor| {
        encode_png(data, width, height, kind, None).map_err(codec_error)
    };
    match color {
        Color::Other(0) => Ok(("png", png(&data, PngColor::Gray1)?)),
        Color::Gray | Color::Other(1) => Ok(("png", png(&data, PngColor::Gray)?)),
        Color::Rgb | Color::Other(3) => Ok(("png", png(&data, PngColor::Rgb)?)),
        Color::Cmyk | Color::Other(4) => Ok((
            "tiff",
            tiff(width, height, &[8, 8, 8, 8], 5, 1, &[(332, 1)], &data),
        )),
        Color::Indexed { .. } | Color::Other(_) => Err(GoatError::message(format!(
            "image {} uses an unsupported colour space",
            entry.xref
        ))),
    }
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), GoatError> {
    std::fs::write(path, data).map_err(|error| GoatError::os(&error, path))
}

fn get_images(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_pikepdf(required::<String>(matches, "file")?)?;
    let document = &opened.doc;
    let outdir = paths::out_dir(
        optional::<String>(matches, "outdir")?.map(String::as_str),
        &opened.display(),
        "images",
    )?;
    let mut seen = HashSet::new();
    let mut outputs = Vec::new();
    for page in doc::pages(document)? {
        for entry in doc::page_images(document, &page) {
            if !seen.insert(entry.xref) {
                continue;
            }
            let (ext, data) = extract_image(document, &entry)?;
            let path = outdir.join(format!("img_{}.{ext}", entry.xref));
            write_file(&path, &data)?;
            outputs.push(path.to_string_lossy().into_owned());
        }
    }
    let mut result = doc::result("get-images", opened.inputs(), outputs.clone());
    result.insert("count".to_owned(), json!(outputs.len()));
    Ok(result)
}

// ----------------------------------------------------------------------------------- //
// get attachments / attach / detach
// ----------------------------------------------------------------------------------- //

enum Payload {
    Catalog(Dict),
    Annotation(Dict),
}

fn get_attachments(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let opened = doc::open_pymupdf(required::<String>(matches, "file")?)?;
    let document = &opened.doc;
    let requested = optional::<String>(matches, "outdir")?
        .filter(|dir| !dir.is_empty())
        .cloned();
    let outdir_text =
        requested.unwrap_or_else(|| format!("{}_attachments", paths::stem(&opened.display())));
    let outdir: PathBuf = paths::resolve_lenient(&paths::expanduser(&outdir_text))?;
    let pages = doc::pages(document)?;
    let mut entries: Vec<(String, Payload)> = doc::embedded_files(document)?
        .into_iter()
        .map(|(name, filespec)| (name, Payload::Catalog(filespec)))
        .collect();
    entries.extend(
        doc::file_attachment_annotations(document, &pages)
            .into_iter()
            .map(|attachment| (attachment.name, Payload::Annotation(attachment.filespec))),
    );
    let mut planned = Vec::with_capacity(entries.len());
    let mut keys = HashSet::new();
    for (name, payload) in entries {
        let safe_name = paths::name(&name).to_owned();
        if matches!(safe_name.as_str(), "" | "." | "..") {
            return Err(GoatError::message("an attachment has no safe file name"));
        }
        let destination = outdir.join(&safe_name);
        let key: String = safe_name.nfc().collect::<String>().to_lowercase();
        if keys.contains(&key) || destination.symlink_metadata().is_ok() {
            return Err(GoatError::message(format!(
                "attachment output already exists: {}",
                destination.display()
            )));
        }
        keys.insert(key);
        planned.push((destination, payload));
    }
    paths::mkdir_parents(&outdir)?;
    let mut outputs = Vec::with_capacity(planned.len());
    for (destination, payload) in planned {
        let data = match &payload {
            Payload::Catalog(filespec) | Payload::Annotation(filespec) => {
                doc::filespec_data(document, filespec)?
            }
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    GoatError::message(format!(
                        "attachment output already exists: {}",
                        destination.display()
                    ))
                } else {
                    GoatError::os(&error, &destination)
                }
            })?;
        file.write_all(&data)
            .map_err(|error| GoatError::os(&error, &destination))?;
        outputs.push(destination.to_string_lossy().into_owned());
    }
    let mut result = doc::result("get-attachments", opened.inputs(), outputs.clone());
    result.insert("count".to_owned(), json!(outputs.len()));
    Ok(result)
}

/// `doc.embfile_add(name, data, filename=name)`: a Filespec with an embedded file stream,
/// appended to the catalog's EmbeddedFiles name tree.
fn embfile_add(document: &mut Document, name: &str, data: Vec<u8>) -> Result<(), GoatError> {
    let mut entries = document.names(b"EmbeddedFiles").map_err(doc::pdf_error)?;
    let exists = entries.iter().any(|(key, _)| {
        String::from_utf8(key.clone()).unwrap_or_else(|_| PdfString::literal(key.clone()).to_text())
            == name
    });
    if exists {
        return Err(GoatError::value_error(format!(
            "Name '{name}' already exists."
        )));
    }
    let now = PdfDate::now().format();
    let mut params = Dict::new();
    params.insert("Size", Object::from(data.len()));
    params.insert("CreationDate", Object::text(&now));
    params.insert("ModDate", Object::text(&now));
    let mut stream_dict = Dict::new();
    stream_dict.insert("Type", Object::name("EmbeddedFile"));
    stream_dict.insert("Params", Object::Dict(params));
    let stream_id = document.add(Stream::new(stream_dict, data));
    let mut embedded = Dict::new();
    embedded.insert("F", Object::Reference(stream_id));
    let mut filespec = Dict::new();
    filespec.insert("Type", Object::name("Filespec"));
    filespec.insert("F", Object::text(name));
    filespec.insert("UF", Object::text(name));
    filespec.insert("Desc", Object::text(name));
    filespec.insert("EF", Object::Dict(embedded));
    let filespec_id = document.add(filespec);
    entries.push((
        PdfString::from_text(name).bytes,
        Object::Reference(filespec_id),
    ));
    document
        .set_names(b"EmbeddedFiles", entries)
        .map_err(doc::pdf_error)
}

fn attach(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open(required::<String>(matches, "file")?, doc::Lib::PyMuPdf)?;
    let attachment = paths::resolve(required::<String>(matches, "attachment")?)?;
    let out = doc::output_path(matches, &opened.display(), "attached")?;
    if opened.doc.needs_password() {
        return Err(doc::closed_or_encrypted());
    }
    let name = attachment
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let data = std::fs::read(&attachment).map_err(|error| GoatError::os(&error, &attachment))?;
    embfile_add(&mut opened.doc, &name, data)?;
    doc::save(&opened.doc, &out, &doc::mupdf_save_options())?;
    let mut result = doc::result("attach", opened.inputs(), vec![out]);
    result.insert("attached".to_owned(), Value::String(name));
    Ok(result)
}

fn dedup(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

fn detach(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let mut opened = doc::open(required::<String>(matches, "file")?, doc::Lib::PyMuPdf)?;
    let names = dedup(
        many::<String>(matches, "names")?
            .into_iter()
            .cloned()
            .collect(),
    );
    if opened.doc.needs_password() {
        return Err(doc::closed_or_encrypted());
    }
    let pages: Vec<Page> = doc::pages(&opened.doc)?;
    let catalog_entries = doc::embedded_files(&opened.doc)?;
    let catalog_names: Vec<String> = catalog_entries
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let attachments = doc::file_attachment_annotations(&opened.doc, &pages);
    let available = dedup(
        catalog_names
            .iter()
            .cloned()
            .chain(attachments.iter().map(|a| a.name.clone()))
            .collect(),
    );
    let targets = if flag(matches, "remove_all")? {
        available.clone()
    } else {
        names
    };
    let missing: Vec<&str> = targets
        .iter()
        .filter(|name| !available.contains(name))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(GoatError::message(format!(
            "attachment not found: {}",
            missing.join(", ")
        )));
    }
    let target_set: HashSet<&str> = targets.iter().map(String::as_str).collect();
    let mut removed_count: usize = 0;
    // `embfile_del(name)` removes the first entry with that name; a repeated name goes
    // once per catalog occurrence, as PyMuPDF's loop over `embfile_names()` does.
    let mut entries = opened.doc.names(b"EmbeddedFiles").map_err(doc::pdf_error)?;
    let mut changed = false;
    for name in &catalog_names {
        if !target_set.contains(name.as_str()) {
            continue;
        }
        let position = entries.iter().position(|(key, _)| {
            String::from_utf8(key.clone())
                .unwrap_or_else(|_| PdfString::literal(key.clone()).to_text())
                == *name
        });
        if let Some(position) = position {
            entries.remove(position);
            changed = true;
        }
        removed_count += 1;
    }
    if changed {
        opened
            .doc
            .set_names(b"EmbeddedFiles", entries)
            .map_err(doc::pdf_error)?;
    }
    for attachment in &attachments {
        if target_set.contains(attachment.name.as_str()) {
            if let Some(page) = pages.get(attachment.page) {
                doc::delete_annot(&mut opened.doc, page, attachment.annot.id)?;
            }
            removed_count += 1;
        }
    }
    let out = doc::output_path(matches, &opened.display(), "detached")?;
    doc::save(&opened.doc, &out, &doc::mupdf_save_options())?;
    let mut result = doc::result("detach", opened.inputs(), vec![out]);
    result.insert(
        "removed".to_owned(),
        Value::Array(targets.into_iter().map(Value::String).collect()),
    );
    result.insert("count".to_owned(), json!(removed_count));
    Ok(result)
}
