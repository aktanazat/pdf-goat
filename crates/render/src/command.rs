use std::path::{Path, PathBuf};

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{int_value, optional, required};
use goat_common::parse::{page_indices, parse_rect};
use goat_common::paths::{self, AtomicOutput};
use goat_common::pool::{PageTask, map_pages};
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::{Document, Rect};
use pdf_interp::Interpreter;
use pdf_raster::Pixmap;
use serde_json::{Map, Value, json};

use crate::{RenderOptions, render_area};

pub(crate) fn register(registry: &mut Registry) {
    registry.command(Verb::new(
        Command::new("render")
            .about("render pages to images")
            .arg(Arg::new("file").required(true))
            .arg(Arg::new("pages").long("pages").help("default: all pages"))
            .arg(
                Arg::new("dpi")
                    .long("dpi")
                    .value_parser(int_value)
                    .default_value("150"),
            )
            .arg(
                Arg::new("format")
                    .long("format")
                    .value_parser(["png", "jpg", "ppm"])
                    .default_value("png"),
            )
            .arg(
                Arg::new("clip")
                    .long("clip")
                    .help("render only x0,y0,x1,y1 in PDF points"),
            )
            .arg(Arg::new("outdir").short('o').long("outdir")),
        render,
    ));
    registry.family_verb(
        "compare",
        Verb::new(
            Command::new("visual")
                .about("pixel diff")
                .arg(Arg::new("file").required(true))
                .arg(Arg::new("other").required(true))
                .arg(
                    Arg::new("dpi")
                        .long("dpi")
                        .value_parser(int_value)
                        .default_value("100"),
                )
                .arg(Arg::new("outdir").short('o').long("outdir")),
            visual,
        ),
    );
}

fn error(value: impl std::fmt::Display) -> GoatError {
    GoatError::message(value.to_string())
}

struct RenderDoc {
    doc: Document,
    interpreter: Interpreter,
}

impl RenderDoc {
    fn open(path: &Path) -> Result<Self, GoatError> {
        let doc = Document::open(path).map_err(error)?;
        if doc.needs_password() {
            return Err(GoatError::value_error("document closed or encrypted"));
        }
        Ok(Self {
            doc,
            interpreter: Interpreter::new(),
        })
    }

    fn page(&self, index: usize, dpi: f64, clip: Option<Rect>) -> Result<Pixmap, GoatError> {
        let page = self.doc.page(index).map_err(error)?;
        render_area(
            &self.doc,
            &self.interpreter,
            &page,
            &RenderOptions {
                dpi,
                ..RenderOptions::default()
            },
            clip,
            true,
        )
        .map_err(error)
    }
}

struct RenderTask {
    source: PathBuf,
    directory: PathBuf,
    stem: String,
    format: String,
    dpi: f64,
    clip: Option<Rect>,
}

impl PageTask for RenderTask {
    type Doc = RenderDoc;
    type Value = String;

    fn open(&self) -> Result<Self::Doc, GoatError> {
        RenderDoc::open(&self.source)
    }

    fn page(&self, doc: &mut Self::Doc, index: usize) -> Result<Self::Value, GoatError> {
        let pixmap = doc.page(index, self.dpi, self.clip)?;
        let output =
            self.directory
                .join(format!("{}_p{:03}.{}", self.stem, index + 1, self.format));
        save_image(&output, &pixmap, &self.format, self.dpi)?;
        Ok(output.to_string_lossy().into_owned())
    }
}

fn save_image(path: &Path, pixmap: &Pixmap, format: &str, dpi: f64) -> Result<(), GoatError> {
    let rgb = pixmap.to_rgb8([255; 3]);
    let data = match format {
        "png" => pdf_codec::encode_png(
            &rgb,
            pixmap.width(),
            pixmap.height(),
            pdf_codec::PngColor::Rgb,
            Some((dpi, dpi)),
        )
        .map_err(error)?,
        "jpg" => pdf_codec::encode_jpeg(
            &rgb,
            pixmap.width(),
            pixmap.height(),
            pdf_codec::JpegColor::Rgb,
            95,
            Some((dpi, dpi)),
        )
        .map_err(error)?,
        "ppm" => {
            let mut bytes =
                format!("P6\n{} {}\n255\n", pixmap.width(), pixmap.height()).into_bytes();
            bytes.extend_from_slice(&rgb);
            bytes
        }
        _ => return Err(GoatError::value_error("unsupported image format")),
    };
    save_bytes(path, &data)
}

fn save_bytes(path: &Path, data: &[u8]) -> Result<(), GoatError> {
    let output = AtomicOutput::new(path);
    std::fs::write(output.partial(), data).map_err(|e| GoatError::os(&e, output.partial()))?;
    output.commit()
}

fn render(args: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = paths::resolve(required::<String>(args, "file")?)?;
    let mut doc = RenderDoc::open(&source)?;
    let dpi = *required::<i64>(args, "dpi")?;
    let format = required::<String>(args, "format")?;
    let clip = optional::<String>(args, "clip")?
        .map(|value| {
            let numbers = parse_rect(value)?;
            let [x0, y0, x1, y1] = numbers.as_slice() else {
                return Err(GoatError::value_error("Rect: bad seq len"));
            };
            let clip = Rect::new(*x0, *y0, *x1, *y1).normalized();
            if clip.is_empty() {
                return Err(GoatError::message("--clip must have positive area"));
            }
            Ok(clip)
        })
        .transpose()?;
    let indices = page_indices(
        optional::<String>(args, "pages")?.map(String::as_str),
        doc.doc.page_count().map_err(error)?,
    )?;
    if let Some(clip) = clip {
        for &index in &indices {
            let page = doc.doc.page(index).map_err(error)?;
            if pdf_interp::page_bounds(&page).intersect(&clip).is_empty() {
                return Err(GoatError::message(format!(
                    "--clip does not overlap page {}",
                    index + 1
                )));
            }
        }
    }
    let stem = paths::stem(&source.to_string_lossy()).to_owned();
    let directory = paths::out_dir(
        optional::<String>(args, "outdir")?.map(String::as_str),
        &source.to_string_lossy(),
        "render",
    )?;
    let task = RenderTask {
        source: source.clone(),
        directory,
        stem,
        format: format.clone(),
        dpi: dpi as f64,
        clip,
    };
    let outputs = map_pages(&task, &mut doc, &indices, &ctx.pool()?)?;
    Ok(Map::from_iter([
        ("verb".into(), json!("render")),
        ("inputs".into(), json!([source])),
        ("outputs".into(), json!(outputs)),
        ("dpi".into(), json!(dpi)),
        ("format".into(), json!(format)),
        ("clip".into(), json!(clip.map(|r| [r.x0, r.y0, r.x1, r.y1]))),
    ]))
}

struct VisualTask {
    left: PathBuf,
    right: PathBuf,
    directory: PathBuf,
    dpi: f64,
}

impl PageTask for VisualTask {
    type Doc = (RenderDoc, RenderDoc);
    type Value = (String, Value);

    fn open(&self) -> Result<Self::Doc, GoatError> {
        Ok((RenderDoc::open(&self.left)?, RenderDoc::open(&self.right)?))
    }

    fn page(&self, doc: &mut Self::Doc, index: usize) -> Result<Self::Value, GoatError> {
        let left = doc.0.page(index, self.dpi, None)?;
        let right = doc.1.page(index, self.dpi, None)?;
        let (width, height) = (left.width(), left.height());
        let mut difference = left.to_rgb8([255; 3]);
        drop(left);
        let right_rgb = resize_rgb(
            right.to_rgb8([255; 3]),
            right.width() as usize,
            right.height() as usize,
            width as usize,
            height as usize,
        );
        drop(right);
        let mut changed = 0u64;
        let mut bbox = [width, height, 0, 0];
        for (i, (a, b)) in difference
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(right_rgb.as_chunks::<3>().0)
            .enumerate()
        {
            for (a, b) in a.iter_mut().zip(b) {
                *a = a.abs_diff(*b);
            }
            let gray = (19595 * u32::from(a[0])
                + 38470 * u32::from(a[1])
                + 7471 * u32::from(a[2])
                + 32768)
                >> 16;
            changed += u64::from(gray != 0);
            if a.iter().any(|value| *value != 0) {
                let (x, y) = (i as u32 % width, i as u32 / width);
                bbox = [
                    bbox[0].min(x),
                    bbox[1].min(y),
                    bbox[2].max(x + 1),
                    bbox[3].max(y + 1),
                ];
            }
        }
        let output = self.directory.join(format!("diff_p{}.png", index + 1));
        let png = pdf_codec::encode_png(&difference, width, height, pdf_codec::PngColor::Rgb, None)
            .map_err(error)?;
        save_bytes(&output, &png)?;
        let ratio = (changed as f64 / (u64::from(width) * u64::from(height)) as f64 * 10000.0)
            .round_ties_even()
            / 10000.0;
        Ok((
            output.to_string_lossy().into_owned(),
            json!({
                "page": index + 1, "changed_ratio": ratio,
                "bbox": (bbox[2] != 0 && bbox[3] != 0).then_some(bbox),
            }),
        ))
    }
}

fn visual(args: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let left = paths::resolve(required::<String>(args, "file")?)?;
    let right = paths::resolve(required::<String>(args, "other")?)?;
    let directory = paths::out_dir(
        optional::<String>(args, "outdir")?.map(String::as_str),
        &left.to_string_lossy(),
        "diff",
    )?;
    let task = VisualTask {
        left,
        right,
        directory,
        dpi: *required::<i64>(args, "dpi")? as f64,
    };
    let mut docs = task.open()?;
    let count = docs
        .0
        .doc
        .page_count()
        .map_err(error)?
        .min(docs.1.doc.page_count().map_err(error)?);
    let indices: Vec<_> = (0..count).collect();
    let (outputs, pages): (Vec<_>, Vec<_>) = map_pages(&task, &mut docs, &indices, &ctx.pool()?)?
        .into_iter()
        .unzip();
    Ok(Map::from_iter([
        ("verb".into(), json!("compare-visual")),
        ("inputs".into(), json!([task.left, task.right])),
        ("outputs".into(), json!(outputs)),
        ("pages".into(), json!(pages)),
    ]))
}

// The visual comparison contract uses Pillow's default bicubic RGB resize:
// widened support when reducing, normalized 22-bit coefficients and rounding
// after each axis, rather than a bilinear point sample.
fn coefficients(input: usize, output: usize) -> Vec<(usize, Vec<i32>)> {
    let scale = input as f64 / output as f64;
    let filter_scale = scale.max(1.0);
    let support = filter_scale * 2.0;
    (0..output)
        .map(|x| {
            let center = (x as f64 + 0.5) * scale;
            let start = ((center - support + 0.5) as isize).clamp(0, input as isize) as usize;
            let end = ((center + support + 0.5) as isize).clamp(0, input as isize) as usize;
            let values: Vec<_> = (start..end)
                .map(|i| {
                    let x = ((i as f64 - center + 0.5) / filter_scale).abs();
                    if x < 1.0 {
                        ((1.5 * x - 2.5) * x) * x + 1.0
                    } else if x < 2.0 {
                        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
                    } else {
                        0.0
                    }
                })
                .collect();
            let total: f64 = values.iter().sum();
            (
                start,
                values
                    .into_iter()
                    .map(|v| (v / total * f64::from(1 << 22)).round() as i32)
                    .collect(),
            )
        })
        .collect()
}

fn resize_rgb(
    mut data: Vec<u8>,
    width: usize,
    height: usize,
    new_width: usize,
    new_height: usize,
) -> Vec<u8> {
    if width != new_width {
        let weights = coefficients(width, new_width);
        let mut out = vec![0; new_width * height * 3];
        for y in 0..height {
            for (x, (start, weights)) in weights.iter().enumerate() {
                for c in 0..3 {
                    let sum = weights
                        .iter()
                        .enumerate()
                        .fold(1i64 << 21, |sum, (i, &weight)| {
                            sum + i64::from(data[(y * width + start + i) * 3 + c])
                                * i64::from(weight)
                        });
                    out[(y * new_width + x) * 3 + c] = (sum >> 22).clamp(0, 255) as u8;
                }
            }
        }
        data = out;
    }
    if height != new_height {
        let weights = coefficients(height, new_height);
        let mut out = vec![0; new_width * new_height * 3];
        for (y, (start, weights)) in weights.iter().enumerate() {
            for x in 0..new_width {
                for c in 0..3 {
                    let sum = weights
                        .iter()
                        .enumerate()
                        .fold(1i64 << 21, |sum, (i, &weight)| {
                            sum + i64::from(data[((start + i) * new_width + x) * 3 + c])
                                * i64::from(weight)
                        });
                    out[(y * new_width + x) * 3 + c] = (sum >> 22).clamp(0, 255) as u8;
                }
            }
        }
        data = out;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::resize_rgb;

    #[test]
    fn different_page_sizes_use_bicubic_filtering_in_both_directions() {
        let pixels = vec![
            255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 128, 64, 32,
        ];
        // Independent Pillow RGB.resize results, including negative lobes and
        // the widened reduction filter. Bilinear and nearest both differ.
        assert_eq!(resize_rgb(pixels.clone(), 3, 2, 1, 1), [109, 110, 94]);
        assert_eq!(
            resize_rgb(pixels, 3, 2, 5, 3),
            [
                255, 0, 0, 171, 101, 0, 0, 255, 0, 0, 98, 169, 0, 0, 255, 128, 0, 0, 131, 100, 50,
                128, 255, 128, 92, 122, 144, 59, 25, 135, 0, 0, 0, 90, 98, 106, 255, 255, 255, 196,
                145, 119, 126, 53, 0,
            ]
        );
    }
}
