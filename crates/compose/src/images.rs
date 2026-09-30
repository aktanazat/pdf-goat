//! `from-images`: `cmd_from_images` first flattens palette and alpha images onto white
//! (`_normalize_image`), then img2pdf 0.6.3 lays every frame out on a page of its own.

use std::path::Path;

use goat_common::GoatError;
use goat_common::py::repr_str;
use pdf_codec::{
    CodecError, DecodedImage, ImageFormat, PdfColorSpace, PixelLayout, PngColor, decode_image_file,
    decode_png, decode_tiff, detect_format, encode_ccitt_g4, encode_png, icc_color_space,
    jpeg_passthrough, jpx_info, jpx_passthrough, png_info, png_passthrough, round_dpi,
    tiff_group4_strip, tiff_page_count, tiff_page_info,
};
use pdf_core::filters::flate_encode;
use pdf_core::{Dict, Document, Object, PdfDate, PdfString, SaveOptions, Stream};

use crate::pdf_error;

/// img2pdf's resolution for an image that states none.
const DEFAULT_DPI: u32 = 96;
/// The largest page side PDF allows in default user space units.
const MAX_PAGE_SIDE: f64 = 14_400.0;

/// What `_normalize_image` hands img2pdf for one source.
pub(crate) enum Normalized {
    /// The RGB PNG it saved after compositing onto white. It has no `pHYs` chunk, so
    /// img2pdf lays it out at 96 dpi.
    Flat(Vec<u8>),
    /// The file itself, with its first frame when deciding needed a decode.
    File {
        data: Vec<u8>,
        first: Option<DecodedImage>,
    },
}

/// The `/ColorSpace` img2pdf writes for a frame.
enum Color {
    Gray,
    Rgb,
    Cmyk {
        inverted: bool,
    },
    /// JPEG 2000 whose reader takes the colour space from the codestream.
    Unspecified,
}

#[derive(Clone, Copy)]
enum Filter {
    Dct,
    Jpx,
    Ccitt {
        black_is_1: bool,
    },
    /// A PNG `IDAT` payload under `/Predictor 15`.
    Png {
        colors: u8,
    },
    /// Raw samples, zlib-compressed.
    Flate,
}

/// One page's image as img2pdf embeds it.
struct Frame {
    width: u32,
    height: u32,
    dpi: (u32, u32),
    color: Color,
    icc: Option<Vec<u8>>,
    depth: u8,
    filter: Filter,
    data: Vec<u8>,
    /// The alpha plane as an 8-bit gray `IDAT` payload.
    smask: Option<Vec<u8>>,
    rotation: i64,
}

/// Every source through `_normalize_image`, in order; the first unreadable one stops it.
pub(crate) fn normalize_all(sources: &[&Path]) -> Result<Vec<Normalized>, GoatError> {
    sources.iter().map(|path| normalize(path)).collect()
}

/// `img2pdf.convert` over the normalized sources: the PDF bytes.
pub(crate) fn convert(sources: Vec<Normalized>) -> Result<Vec<u8>, GoatError> {
    let mut frames = Vec::new();
    for source in sources {
        read_images(source, &mut frames)?;
    }
    write_pdf(&frames)
}

fn image_error(error: CodecError) -> GoatError {
    GoatError::exception("OSError", error.to_string())
}

/// `_normalize_image`: Pillow opens the file, and modes RGBA, LA, and P are pasted onto
/// white and saved as a PNG.
fn normalize(path: &Path) -> Result<Normalized, GoatError> {
    let data = std::fs::read(path).map_err(|error| GoatError::os(&error, path))?;
    let unidentified = || {
        GoatError::exception(
            "UnidentifiedImageError",
            format!(
                "cannot identify image file {}",
                repr_str(&path.to_string_lossy())
            ),
        )
    };
    // Pillow reads neither JBIG2 files nor PAM.
    let format = match detect_format(&data) {
        None | Some(ImageFormat::Jbig2) => return Err(unidentified()),
        Some(ImageFormat::Pnm) if data.starts_with(b"P7") => return Err(unidentified()),
        Some(format) => format,
    };
    let (flatten, first) = match format {
        ImageFormat::Png => (
            matches!(png_info(&data).map_err(image_error)?.color_type, 3 | 4 | 6),
            None,
        ),
        ImageFormat::Jpeg | ImageFormat::Pnm | ImageFormat::Jbig2 => (false, None),
        ImageFormat::Jpeg2000 => (jpx_info(&data).map_err(image_error)?.has_alpha, None),
        ImageFormat::Gif => (true, None),
        ImageFormat::Tiff => {
            let info = tiff_page_info(&data, 0).map_err(image_error)?;
            let color = match info.photometric {
                2 | 6 => 3,
                5 => 4,
                _ => 1,
            };
            (
                info.photometric == 3 || info.samples_per_pixel > color,
                None,
            )
        }
        ImageFormat::Bmp | ImageFormat::WebP => {
            let first = decode_image_file(&data, 0).map_err(image_error)?;
            let flatten = match first.layout {
                PixelLayout::Rgba | PixelLayout::GrayAlpha => true,
                PixelLayout::Indexed => bmp_gray_mode(&first).is_none(),
                _ => false,
            };
            (flatten, Some(first))
        }
    };
    if !flatten {
        return Ok(Normalized::File { data, first });
    }
    let image = match first {
        Some(image) => image,
        None => decode_image_file(&data, 0).map_err(image_error)?,
    };
    let rgb = image.to_rgb8_on_white();
    let png =
        encode_png(&rgb, image.width, image.height, PngColor::Rgb, None).map_err(image_error)?;
    Ok(Normalized::Flat(png))
}

/// Pillow's BMP reader drops a palette whose entries are gray and equal to their index
/// (black then white for two colours), reporting mode `1` or `L` instead of `P`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrayMode {
    Bilevel,
    Gray,
}

fn bmp_gray_mode(image: &DecodedImage) -> Option<GrayMode> {
    let palette = image.palette.as_deref()?;
    let entries = palette.as_chunks::<3>().0;
    let two = entries.len() == 2;
    let gray = entries.iter().enumerate().all(|(index, rgb)| {
        let expected = if two {
            [0u8, 255][index]
        } else {
            u8::try_from(index).unwrap_or(u8::MAX)
        };
        rgb.iter().all(|&channel| channel == expected)
    });
    match (gray, two) {
        (false, _) => None,
        (true, true) => Some(GrayMode::Bilevel),
        (true, false) => Some(GrayMode::Gray),
    }
}

/// img2pdf's `read_images` for one normalized source.
fn read_images(source: Normalized, frames: &mut Vec<Frame>) -> Result<(), GoatError> {
    let (data, first) = match source {
        Normalized::Flat(png) => return png_frames(&png, frames),
        Normalized::File { data, first } => (data, first),
    };
    match detect_format(&data) {
        Some(ImageFormat::Jpeg) => {
            let jpeg = jpeg_passthrough(&data, DEFAULT_DPI).map_err(image_error)?;
            let rotation = exif_rotation(jpeg.exif_orientation)?;
            let color = match jpeg.color_space {
                PdfColorSpace::DeviceGray => Color::Gray,
                PdfColorSpace::DeviceRGB => Color::Rgb,
                PdfColorSpace::DeviceCMYK => Color::Cmyk {
                    inverted: jpeg.inverted_cmyk,
                },
            };
            frames.push(Frame {
                width: jpeg.width,
                height: jpeg.height,
                dpi: jpeg.dpi,
                color,
                icc: jpeg.icc_profile,
                depth: 8,
                filter: Filter::Dct,
                data,
                smask: None,
                rotation,
            });
        }
        Some(ImageFormat::Jpeg2000) => {
            let jpx = jpx_passthrough(&data, DEFAULT_DPI).map_err(image_error)?;
            let color = match jpx.color_space {
                Some(PdfColorSpace::DeviceGray) => Color::Gray,
                Some(PdfColorSpace::DeviceRGB) => Color::Rgb,
                Some(PdfColorSpace::DeviceCMYK) => Color::Cmyk { inverted: false },
                None => Color::Unspecified,
            };
            frames.push(Frame {
                width: jpx.width,
                height: jpx.height,
                dpi: jpx.dpi,
                color,
                icc: jpx.icc_profile,
                depth: jpx.bit_depth,
                filter: Filter::Jpx,
                data,
                smask: None,
                rotation: 0,
            });
        }
        Some(ImageFormat::Png) => png_frames(&data, frames)?,
        Some(ImageFormat::Tiff) => tiff_frames(&data, first, frames)?,
        Some(ImageFormat::Pnm) => {
            let image = match first {
                Some(image) => image,
                None => decode_image_file(&data, 0).map_err(image_error)?,
            };
            let dpi = round_dpi(image.dpi, DEFAULT_DPI);
            // PBM files open in mode `1`.
            let mode = if data.starts_with(b"P1") || data.starts_with(b"P4") {
                Some(GrayMode::Bilevel)
            } else {
                None
            };
            frames.push(pillow_frame(image, dpi, mode, false)?);
        }
        Some(ImageFormat::Bmp | ImageFormat::WebP | ImageFormat::Gif | ImageFormat::Jbig2)
        | None => {
            let image = match first {
                Some(image) => image,
                None => decode_image_file(&data, 0).map_err(image_error)?,
            };
            let dpi = round_dpi(image.dpi, DEFAULT_DPI);
            let mode = if image.layout == PixelLayout::Indexed {
                bmp_gray_mode(&image)
            } else {
                None
            };
            frames.push(pillow_frame(image, dpi, mode, false)?);
        }
    }
    Ok(())
}

/// A PNG: its `IDAT` embedded unchanged when img2pdf allows that, else decoded.
fn png_frames(data: &[u8], frames: &mut Vec<Frame>) -> Result<(), GoatError> {
    if let Some(png) = png_passthrough(data, DEFAULT_DPI).map_err(image_error)? {
        // Palette PNGs never get here: `_normalize_image` flattened them.
        let gray = png.channels == 1;
        let color = if gray { Color::Gray } else { Color::Rgb };
        frames.push(Frame {
            width: png.width,
            height: png.height,
            dpi: png.dpi,
            icc: usable_icc(png.icc_profile, gray),
            color,
            depth: png.bit_depth,
            filter: Filter::Png {
                colors: png.channels,
            },
            data: png.idat,
            smask: None,
            rotation: 0,
        });
        return Ok(());
    }
    let info = png_info(data).map_err(image_error)?;
    let dpi = match (info.dpi, info.aspect) {
        (Some(dpi), _) => round_dpi(Some(dpi), DEFAULT_DPI),
        (None, Some((x, y))) if x > 0 && y > 0 => {
            let base = f64::from(DEFAULT_DPI);
            let ratio = if x > y {
                (base * f64::from(x) / f64::from(y), base)
            } else {
                (base, base * f64::from(y) / f64::from(x))
            };
            round_dpi(Some(ratio), DEFAULT_DPI)
        }
        _ => (DEFAULT_DPI, DEFAULT_DPI),
    };
    let image = decode_png(data).map_err(image_error)?;
    frames.push(pillow_frame(image, dpi, None, info.has_transparency)?);
    Ok(())
}

/// Every page of a TIFF: single-strip Group 4 pages pass through, the rest are decoded.
fn tiff_frames(
    data: &[u8],
    mut first: Option<DecodedImage>,
    frames: &mut Vec<Frame>,
) -> Result<(), GoatError> {
    if tiff_page_info(data, 0)
        .map_err(image_error)?
        .bits_per_sample
        > 8
    {
        return Err(GoatError::value_error(
            "PIL is unable to preserve more than 8 bits per sample",
        ));
    }
    let pages = tiff_page_count(data).map_err(image_error)?;
    for page in 0..pages {
        let info = tiff_page_info(data, page).map_err(image_error)?;
        let dpi = round_dpi(info.dpi, DEFAULT_DPI);
        if let Some(strip) = tiff_group4_strip(data, page).map_err(image_error)? {
            frames.push(Frame {
                width: strip.width,
                height: strip.height,
                dpi,
                color: Color::Gray,
                icc: None,
                depth: 1,
                filter: Filter::Ccitt {
                    black_is_1: !strip.white_is_zero,
                },
                data: strip.data,
                smask: None,
                rotation: 0,
            });
            continue;
        }
        let image = match first.take() {
            Some(image) if page == 0 => image,
            _ => decode_tiff(data, page).map_err(image_error)?,
        };
        frames.push(pillow_frame(image, dpi, None, false)?);
    }
    Ok(())
}

/// img2pdf's frame loop for a Pillow image. `mode` overrides the layout for palettes
/// Pillow reports as gray; `color_key` marks a gray or RGB PNG with a `tRNS` colour key,
/// which img2pdf turns into RGB plus a soft mask.
fn pillow_frame(
    image: DecodedImage,
    dpi: (u32, u32),
    mode: Option<GrayMode>,
    color_key: bool,
) -> Result<Frame, GoatError> {
    let (width, height) = (image.width, image.height);
    let bilevel = mode == Some(GrayMode::Bilevel)
        || (mode.is_none() && image.layout == PixelLayout::Gray && image.bit_depth == 1);
    let frame = |color, icc, depth, filter, data, smask| Frame {
        width,
        height,
        dpi,
        color,
        icc,
        depth,
        filter,
        data,
        smask,
        rotation: 0,
    };
    if bilevel {
        // `transcode_monochrome`: Pillow writes the bits with 0 = black, which the Group 4
        // coder takes as white runs, so the stream decodes under `/BlackIs1 true`.
        let bits = bilevel_bits(&image);
        let ccitt = encode_ccitt_g4(&bits, width, height, true).map_err(image_error)?;
        return Ok(frame(
            Color::Gray,
            None,
            1,
            Filter::Ccitt { black_is_1: true },
            ccitt,
            None,
        ));
    }
    if mode == Some(GrayMode::Gray) {
        let gray = image.samples8();
        let idat = png_idat(&gray, width, height, PngColor::Gray)?;
        let icc = usable_icc(image.icc_profile, true);
        return Ok(frame(
            Color::Gray,
            icc,
            8,
            Filter::Png { colors: 1 },
            idat,
            None,
        ));
    }
    match image.layout {
        PixelLayout::Gray => {
            let gray = image.samples8();
            let idat = png_idat(&gray, width, height, PngColor::Gray)?;
            let icc = usable_icc(image.icc_profile, true);
            Ok(frame(
                Color::Gray,
                icc,
                8,
                Filter::Png { colors: 1 },
                idat,
                None,
            ))
        }
        PixelLayout::GrayAlpha if !color_key => {
            let samples = image.samples8();
            let (gray, alpha): (Vec<u8>, Vec<u8>) = samples
                .as_chunks::<2>()
                .0
                .iter()
                .map(|px| (px[0], px[1]))
                .unzip();
            let idat = png_idat(&gray, width, height, PngColor::Gray)?;
            let smask = png_idat(&alpha, width, height, PngColor::Gray)?;
            let icc = usable_icc(image.icc_profile, true);
            Ok(frame(
                Color::Gray,
                icc,
                8,
                Filter::Png { colors: 1 },
                idat,
                Some(smask),
            ))
        }
        PixelLayout::GrayAlpha | PixelLayout::Rgba => {
            let rgba = image.to_rgba8();
            let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
            let mut alpha = Vec::with_capacity(rgba.len() / 4);
            for px in rgba.as_chunks::<4>().0 {
                rgb.extend_from_slice(&px[..3]);
                alpha.push(px[3]);
            }
            let idat = png_idat(&rgb, width, height, PngColor::Rgb)?;
            let smask = png_idat(&alpha, width, height, PngColor::Gray)?;
            let icc = usable_icc(image.icc_profile, image.layout == PixelLayout::GrayAlpha);
            Ok(frame(
                Color::Rgb,
                icc,
                8,
                Filter::Png { colors: 3 },
                idat,
                Some(smask),
            ))
        }
        PixelLayout::Rgb | PixelLayout::Indexed => {
            // Only gray palettes (Pillow modes `1` and `L`) reach this loop unflattened,
            // and `mode` routed those above.
            let rgb = if image.layout == PixelLayout::Rgb {
                image.samples8()
            } else {
                image.to_rgb8_on_white()
            };
            let idat = png_idat(&rgb, width, height, PngColor::Rgb)?;
            Ok(frame(
                Color::Rgb,
                image.icc_profile,
                8,
                Filter::Png { colors: 3 },
                idat,
                None,
            ))
        }
        PixelLayout::Cmyk => {
            let cmyk = image.samples8();
            Ok(frame(
                Color::Cmyk { inverted: false },
                image.icc_profile,
                8,
                Filter::Flate,
                flate_encode(&cmyk),
                None,
            ))
        }
    }
}

/// Packed rows with 1 = white, the codec's bilevel convention.
fn bilevel_bits(image: &DecodedImage) -> Vec<u8> {
    if image.bit_depth == 1 {
        return image.data.clone();
    }
    let samples = image.samples8();
    let width = image.width as usize;
    let stride = width.div_ceil(8);
    let mut out = vec![0u8; stride * image.height as usize];
    if width == 0 {
        return out;
    }
    for (row, line) in samples
        .chunks_exact(width)
        .zip(out.chunks_exact_mut(stride))
    {
        for (x, &sample) in row.iter().enumerate() {
            // Two-entry palettes hold indices; gray samples are 0 or 255.
            if sample >= 1 && (image.layout == PixelLayout::Indexed || sample >= 128) {
                line[x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

/// img2pdf drops a non-gray ICC profile from a gray image.
fn usable_icc(profile: Option<Vec<u8>>, gray: bool) -> Option<Vec<u8>> {
    let profile = profile?;
    if gray && icc_color_space(&profile) != Some(*b"GRAY") {
        return None;
    }
    Some(profile)
}

/// The `IDAT` payload of an 8-bit gray or RGB PNG, as Pillow's `save` and img2pdf's
/// `parse_png` produce it.
fn png_idat(
    samples: &[u8],
    width: u32,
    height: u32,
    color: PngColor,
) -> Result<Vec<u8>, GoatError> {
    let png = encode_png(samples, width, height, color, None).map_err(image_error)?;
    let mut idat = Vec::new();
    let mut pos = 8usize;
    while let Some(head) = png.get(pos..pos + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let body = png.get(pos + 8..pos + 8 + len).unwrap_or_default();
        if &head[4..8] == b"IDAT" {
            idat.extend_from_slice(body);
        }
        pos += 12 + len;
    }
    Ok(idat)
}

/// The page rotation for an EXIF orientation, or img2pdf's refusal.
fn exif_rotation(orientation: Option<u16>) -> Result<i64, GoatError> {
    match orientation {
        None | Some(1) => Ok(0),
        Some(6) => Ok(90),
        Some(3) => Ok(180),
        Some(8) => Ok(270),
        Some(value @ (2 | 4 | 5 | 7)) => Err(GoatError::exception(
            "ExifOrientationError",
            format!(
                "Unsupported flipped rotation mode ({value}): use --rotation=ifvalid or \
                 rotation=img2pdf.Rotation.ifvalid to ignore"
            ),
        )),
        Some(value) => Err(GoatError::exception(
            "ExifOrientationError",
            format!(
                "Invalid rotation ({value}): use --rotation=ifvalid or rotation=img2pdf.Rotation.ifvalid to ignore"
            ),
        )),
    }
}

/// `find_scale`: the power of ten that brings the larger side under 14400.
fn find_scale(width: f64, height: f64) -> f64 {
    let oversized = width.max(height) / MAX_PAGE_SIDE;
    10f64.powf(oversized.log10().ceil())
}

fn color_space(frame: &Frame) -> Option<Object> {
    let (device, channels): (&str, i64) = match &frame.color {
        Color::Gray => ("DeviceGray", 1),
        Color::Rgb => ("DeviceRGB", 3),
        Color::Cmyk { .. } => ("DeviceCMYK", 4),
        Color::Unspecified => return None,
    };
    let Some(profile) = &frame.icc else {
        return Some(Object::name(device));
    };
    let mut dict = Dict::new();
    dict.insert("Alternate", Object::name(device));
    dict.insert("N", channels);
    Some(Object::Array(vec![
        Object::name("ICCBased"),
        Object::Stream(Stream::new(dict, profile.clone())),
    ]))
}

fn predictor_parms(colors: u8, width: u32, depth: u8) -> Dict {
    let mut parms = Dict::new();
    parms.insert("Predictor", 15);
    parms.insert("Colors", i64::from(colors));
    parms.insert("Columns", width);
    parms.insert("BitsPerComponent", i64::from(depth));
    parms
}

fn image_stream(doc: &mut Document, frame: &Frame) -> Stream {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Image"));
    let filter = match frame.filter {
        Filter::Dct => Object::name("DCTDecode"),
        Filter::Jpx => Object::name("JPXDecode"),
        Filter::Ccitt { .. } => Object::Array(vec![Object::name("CCITTFaxDecode")]),
        Filter::Png { .. } | Filter::Flate => Object::name("FlateDecode"),
    };
    dict.insert("Filter", filter);
    dict.insert("Width", frame.width);
    dict.insert("Height", frame.height);
    if let Some(space) = color_space(frame) {
        dict.insert("ColorSpace", space);
    }
    dict.insert("BitsPerComponent", i64::from(frame.depth));
    if let Some(alpha) = &frame.smask {
        let mut smask = Dict::new();
        smask.insert("Type", Object::name("XObject"));
        smask.insert("Subtype", Object::name("Image"));
        smask.insert("Filter", Object::name("FlateDecode"));
        smask.insert("Width", frame.width);
        smask.insert("Height", frame.height);
        smask.insert("ColorSpace", Object::name("DeviceGray"));
        smask.insert("BitsPerComponent", i64::from(frame.depth));
        smask.insert("DecodeParms", predictor_parms(1, frame.width, frame.depth));
        let smask = doc.add(Stream::new(smask, alpha.clone()));
        dict.insert("SMask", smask);
    }
    if let Color::Cmyk { inverted: true } = frame.color {
        dict.insert(
            "Decode",
            Object::Array(
                [1, 0, 1, 0, 1, 0, 1, 0]
                    .into_iter()
                    .map(Object::Integer)
                    .collect(),
            ),
        );
    }
    match frame.filter {
        Filter::Ccitt { black_is_1 } => {
            let mut parms = Dict::new();
            parms.insert("K", -1);
            parms.insert("BlackIs1", black_is_1);
            parms.insert("Columns", frame.width);
            parms.insert("Rows", frame.height);
            dict.insert("DecodeParms", Object::Array(vec![Object::Dict(parms)]));
        }
        Filter::Png { colors } => {
            dict.insert(
                "DecodeParms",
                predictor_parms(colors, frame.width, frame.depth),
            );
        }
        Filter::Dct | Filter::Jpx | Filter::Flate => {}
    }
    Stream::new(dict, frame.data.clone())
}

/// img2pdf's `convert` with the default layout: each page is exactly the image's size at
/// its resolution, the image drawn at the origin.
fn write_pdf(frames: &[Frame]) -> Result<Vec<u8>, GoatError> {
    let mut doc = Document::new();
    let now = PdfDate::now().format();
    let mut info = Dict::new();
    info.insert("CreationDate", PdfString::literal(now.as_bytes().to_vec()));
    info.insert("ModDate", PdfString::literal(now.into_bytes()));
    doc.set_info(info);
    let mut version = (1u8, 3u8);
    for (index, frame) in frames.iter().enumerate() {
        let mut width = 72.0 * f64::from(frame.width) / f64::from(frame.dpi.0.max(1));
        let mut height = 72.0 * f64::from(frame.height) / f64::from(frame.dpi.1.max(1));
        let mut user_unit = None;
        if width > MAX_PAGE_SIDE || height > MAX_PAGE_SIDE {
            let unit = find_scale(width, height);
            width /= unit;
            height /= unit;
            user_unit = Some(unit);
            version = version.max((1, 6));
        }
        if matches!(frame.filter, Filter::Jpx) {
            version = version.max((1, 5));
        }
        if frame.smask.is_some() {
            version = version.max((1, 4));
        }
        let image = image_stream(&mut doc, frame);
        let image = doc.add(image);
        let content = format!(
            "q\n{width:.4} 0 0 {height:.4} {:.4} {:.4} cm\n/Im0 Do\nQ",
            0.0, 0.0
        );
        let content = doc.add(Stream::new(Dict::new(), content.into_bytes()));
        let mut xobjects = Dict::new();
        xobjects.insert("Im0", image);
        let mut resources = Dict::new();
        resources.insert("XObject", xobjects);
        let mut page = Dict::new();
        page.insert("Type", Object::name("Page"));
        page.insert(
            "MediaBox",
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Real(width),
                Object::Real(height),
            ]),
        );
        page.insert("Resources", resources);
        page.insert("Contents", content);
        if frame.rotation != 0 {
            page.insert("Rotate", frame.rotation);
        }
        if let Some(unit) = user_unit {
            page.insert("UserUnit", Object::Real(unit));
        }
        let page = doc.add(page);
        doc.insert_page(index, page).map_err(pdf_error)?;
    }
    let options = SaveOptions {
        compress_streams: true,
        new_id: true,
        version: Some(version),
        ..SaveOptions::default()
    };
    doc.save_to_bytes(&options).map_err(pdf_error)
}
