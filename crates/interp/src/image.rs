//! Image XObjects and inline images, with their PDF decoding and masks.

use crate::InterpError;
use crate::colorspace::{ColorSpace, ColorSpaceCache, MAX_COLORANTS};
use pdf_codec::{CcittParams, DctParams, pixels};
use pdf_core::{Dict, Document, ObjRef, Object, Stream};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorFamily {
    Gray,
    Rgb,
    Cmyk,
    Lab,
    Indexed,
    Separation,
    DeviceN,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePixels {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub alpha: Option<AlphaChannel>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlphaChannel {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StencilPixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageSamples {
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub family: ColorFamily,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct PdfImage {
    xobject: Option<ObjRef>,
    stream: Stream,
    width: u32,
    height: u32,
    bits_per_component: u8,
    sample_bits: u8,
    is_mask: bool,
    interpolate: bool,
    components: u8,
    color_space: Arc<ColorSpace>,
    filter: Option<Vec<u8>>,
    samples: Vec<u16>,
    decode: Vec<[f64; 2]>,
    alpha: Option<AlphaChannel>,
    color_key: Vec<[u16; 2]>,
    matte: Vec<f32>,
}

impl PdfImage {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn bits_per_component(&self) -> u8 {
        self.bits_per_component
    }
    pub fn is_mask(&self) -> bool {
        self.is_mask
    }
    pub fn interpolate(&self) -> bool {
        self.interpolate
    }
    /// Whether an intrinsic alpha channel, soft/stencil mask, or colour-key
    /// mask can make this image transparent. Does not convert or copy pixels.
    pub fn has_alpha(&self) -> bool {
        self.alpha.is_some() || !self.color_key.is_empty()
    }
    pub fn xobject(&self) -> Option<ObjRef> {
        self.xobject
    }
    pub fn stream(&self) -> &Stream {
        &self.stream
    }
    pub fn components(&self) -> u8 {
        self.components
    }
    pub fn color_space_name(&self) -> String {
        self.color_space.name.into()
    }
    pub fn filter(&self) -> Option<&[u8]> {
        self.filter.as_deref()
    }

    /// RGB8 display pixels after /Decode, colour conversion, and matte removal.
    pub fn decode_rgb(&self) -> Result<ImagePixels, InterpError> {
        let n = usize::from(self.components);
        let count = self.width as usize * self.height as usize;
        let max = f64::from((1u32 << self.sample_bits) - 1);
        let mut rgb = Vec::with_capacity(count * 3);
        let mut color_alpha = (!self.color_key.is_empty()).then(|| Vec::with_capacity(count));
        let mut v = [0.0f32; MAX_COLORANTS];
        for (i, samples) in self.samples.chunks_exact(n).enumerate() {
            let alpha = self.alpha.as_ref().map_or(1.0, |a| {
                f32::from(alpha_at(a, self.width, self.height, i)) / 255.0
            });
            for (c, (&s, value)) in samples.iter().zip(v.iter_mut()).enumerate() {
                let [lo, hi] = self.decode[c];
                *value = (lo + f64::from(s) / max * (hi - lo)) as f32;
                if let Some(matte) = self.matte.get(c) {
                    *value = if alpha > 0.0 {
                        ((*value - *matte) / alpha + *matte).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                }
            }
            let color = self.color_space.to_rgb(&v[..n]);
            rgb.extend(color.map(byte));
            if let Some(a) = &mut color_alpha {
                let transparent = samples
                    .iter()
                    .zip(&self.color_key)
                    .all(|(s, [lo, hi])| s >= lo && s <= hi);
                a.push(if transparent { 0 } else { byte(alpha) });
            }
        }
        let alpha = color_alpha
            .map(|data| AlphaChannel {
                width: self.width,
                height: self.height,
                data,
            })
            .or_else(|| self.alpha.clone());
        Ok(ImagePixels {
            width: self.width,
            height: self.height,
            rgb,
            alpha,
        })
    }

    /// Stencil coverage: 255 paints, 0 leaves the destination unchanged.
    pub fn decode_stencil(&self) -> Result<StencilPixels, InterpError> {
        let max = f64::from((1u32 << self.sample_bits) - 1);
        let [lo, hi] = self.decode.first().copied().unwrap_or([0.0, 1.0]);
        let data = self
            .samples
            .iter()
            .step_by(usize::from(self.components))
            .map(|s| byte((1.0 - lo - f64::from(*s) / max * (hi - lo)) as f32))
            .collect();
        Ok(StencilPixels {
            width: self.width,
            height: self.height,
            data,
        })
    }

    /// Samples in the original colour family, reduced to 8 bits without /Decode.
    pub fn decode_samples(&self) -> Result<ImageSamples, InterpError> {
        let data = if self.color_space.is_indexed() {
            self.samples.iter().map(|s| (*s).min(255) as u8).collect()
        } else {
            pixels::samples_to_u8(
                &self.samples,
                self.sample_bits,
                usize::from(self.components),
                None,
            )
        };
        Ok(ImageSamples {
            width: self.width,
            height: self.height,
            components: self.components,
            family: self.color_space.family(),
            data,
        })
    }

    pub(crate) fn load(
        doc: &Document,
        object: Option<ObjRef>,
        stream: Stream,
        resources: &Dict,
        cache: &ColorSpaceCache,
    ) -> Result<PdfImage, InterpError> {
        load(doc, object, stream, resources, cache, 0)
    }
}

fn load(
    doc: &Document,
    object: Option<ObjRef>,
    stream: Stream,
    resources: &Dict,
    cache: &ColorSpaceCache,
    depth: usize,
) -> Result<PdfImage, InterpError> {
    if depth >= 16 {
        return Err(InterpError::Limit("image mask recursion exceeds 16".into()));
    }
    let d = &stream.dict;
    let get = |long: &[u8], short: &[u8]| {
        d.get(long)
            .or_else(|| d.get(short))
            .map(|o| doc.resolve(o))
            .transpose()
    };
    let mut width = get(b"Width", b"W")?
        .and_then(|o| o.as_i64())
        .unwrap_or(0)
        .max(0) as u32;
    let mut height = get(b"Height", b"H")?
        .and_then(|o| o.as_i64())
        .unwrap_or(0)
        .max(0) as u32;
    let is_mask = get(b"ImageMask", b"IM")?
        .and_then(|o| o.as_bool())
        .unwrap_or(false);
    let interpolate = get(b"Interpolate", b"I")?
        .and_then(|o| o.as_bool())
        .unwrap_or(false);
    let color_object = get(b"ColorSpace", b"CS")?;
    let mut color_space = color_object
        .as_ref()
        .and_then(|o| ColorSpace::load(doc, o, Some(resources), cache))
        .unwrap_or_else(ColorSpace::gray);
    let mut bpc = get(b"BitsPerComponent", b"BPC")?
        .and_then(|o| o.as_i64())
        .unwrap_or(if is_mask { 1 } else { 8 }) as u8;
    if is_mask {
        bpc = 1;
        color_space = ColorSpace::gray();
    }
    let filters = get(b"Filter", b"F")?;
    let parms = get(b"DecodeParms", b"DP")?;
    let chain = pdf_core::filters::filter_chain(filters.as_ref(), parms.as_ref());
    let filter = chain
        .last()
        .map(|(n, _)| pdf_core::filters::canonical_filter_name(n.as_bytes()).to_vec());
    let decoded = pdf_core::filters::decode(stream.raw(), &chain, doc.decode_limit())?;
    let mut data = decoded.data;
    let mut sample_bits = bpc;
    let mut alpha = None;
    let mut components = color_space.components();
    if let Some(stop) = decoded.stopped {
        let parms = stop.parms.unwrap_or_default();
        match stop.filter.as_bytes() {
            b"DCTDecode" => {
                let image = pdf_codec::decode_dct(
                    &data,
                    &DctParams {
                        color_transform: parms.get_i64(b"ColorTransform").map(|v| v as u32),
                    },
                )
                .map_err(codec)?;
                width = image.width;
                height = image.height;
                sample_bits = 8;
                if color_object.is_none() {
                    color_space = match image.components {
                        4 => ColorSpace::cmyk(),
                        3 => ColorSpace::rgb(),
                        _ => ColorSpace::gray(),
                    };
                }
                components = usize::from(image.components);
                data = image.data;
            }
            b"JPXDecode" => {
                let image = pdf_codec::decode_jpx(&data).map_err(codec)?;
                width = image.info.width;
                height = image.info.height;
                bpc = image.info.bit_depth;
                sample_bits = 8;
                components = usize::from(image.info.channels - u8::from(image.info.has_alpha));
                if color_object.is_none() {
                    color_space = match components {
                        4 => ColorSpace::cmyk(),
                        3 => ColorSpace::rgb(),
                        _ => ColorSpace::gray(),
                    };
                }
                if image.info.has_alpha {
                    let mut colors =
                        Vec::with_capacity(width as usize * height as usize * components);
                    let mut opacity = Vec::with_capacity(width as usize * height as usize);
                    for p in image.data.chunks_exact(components + 1) {
                        colors.extend_from_slice(&p[..components]);
                        opacity.push(p[components]);
                    }
                    data = colors;
                    alpha = Some(AlphaChannel {
                        width,
                        height,
                        data: opacity,
                    });
                } else {
                    data = image.data;
                }
            }
            b"CCITTFaxDecode" => {
                let params = CcittParams {
                    k: parms.get_i64(b"K").unwrap_or(0) as i32,
                    columns: parms.get_i64(b"Columns").unwrap_or(1728).max(0) as u32,
                    rows: parms.get_i64(b"Rows").unwrap_or(i64::from(height)).max(0) as u32,
                    encoded_byte_align: parms.get_bool(b"EncodedByteAlign").unwrap_or(false),
                    end_of_line: parms.get_bool(b"EndOfLine").unwrap_or(false),
                    end_of_block: parms.get_bool(b"EndOfBlock").unwrap_or(true),
                    black_is_1: parms.get_bool(b"BlackIs1").unwrap_or(false),
                    damaged_rows_before_error: parms
                        .get_i64(b"DamagedRowsBeforeError")
                        .unwrap_or(0)
                        .max(0) as u32,
                };
                let image = pdf_codec::decode_ccitt(&data, &params)
                    .map_err(codec)?
                    .image;
                width = image.width;
                height = image.height;
                data = image.data;
                sample_bits = 1;
                components = 1;
            }
            b"JBIG2Decode" => {
                let globals = parms
                    .get(b"JBIG2Globals")
                    .map(|o| doc.resolve_stream(o))
                    .transpose()?
                    .flatten()
                    .map(|s| doc.decode_stream(&s))
                    .transpose()?
                    .map(|s| s.data);
                let mut image =
                    pdf_codec::decode_jbig2(&data, globals.as_deref()).map_err(codec)?;
                image.invert();
                width = image.width;
                height = image.height;
                data = image.data;
                sample_bits = 1;
                components = 1;
            }
            _ => return Err(InterpError::Limit("unsupported image filter".into())),
        }
    }
    if width == 0
        || height == 0
        || width > pdf_codec::MAX_DIMENSION
        || height > pdf_codec::MAX_DIMENSION
        || u64::from(width) * u64::from(height) * components as u64 > 128 * 1024 * 1024
        || !matches!(sample_bits, 1 | 2 | 4 | 8 | 16)
        || components == 0
        || components > MAX_COLORANTS
    {
        return Err(InterpError::Limit(
            "image dimensions or samples exceed bounds".into(),
        ));
    }
    let samples =
        pixels::unpack_samples(&data, width, height, sample_bits, components).map_err(codec)?;
    let decode_object = get(b"Decode", b"D")?;
    let mut decode = color_space.default_decode(sample_bits);
    decode.resize(components, [0.0, 1.0]);
    if let Some(a) = decode_object.as_ref().and_then(Object::as_array) {
        for (slot, pair) in decode.iter_mut().zip(a.as_chunks::<2>().0) {
            if let (Some(lo), Some(hi)) = (pair[0].as_f64(), pair[1].as_f64()) {
                *slot = [lo, hi];
            }
        }
    }
    let smask = doc.resolve_key(d, b"SMask")?;
    let mask = doc.resolve_key(d, b"Mask")?;
    let mut matte = Vec::new();
    if let Some(s) = smask.as_stream() {
        let image = load(
            doc,
            d.get_ref(b"SMask"),
            s.clone(),
            resources,
            cache,
            depth + 1,
        )?;
        let max = f64::from((1u32 << image.sample_bits) - 1);
        let [lo, hi] = image.decode[0];
        alpha = Some(AlphaChannel {
            width: image.width,
            height: image.height,
            data: image
                .samples
                .iter()
                .step_by(usize::from(image.components))
                .map(|s| byte((lo + f64::from(*s) / max * (hi - lo)) as f32))
                .collect(),
        });
        if let Some(a) = s.dict.get_array(b"Matte") {
            matte = a.iter().map(|o| o.as_f64().unwrap_or(0.0) as f32).collect();
        }
    } else if let Some(s) = mask.as_stream() {
        let image = load(
            doc,
            d.get_ref(b"Mask"),
            s.clone(),
            resources,
            cache,
            depth + 1,
        )?
        .decode_stencil()?;
        alpha = Some(AlphaChannel {
            width: image.width,
            height: image.height,
            data: image.data,
        });
    }
    let color_key = mask
        .as_array()
        .map(|a| {
            a.as_chunks::<2>()
                .0
                .iter()
                .take(components)
                .map(|p| {
                    [
                        p[0].as_i64().unwrap_or(0) as u16,
                        p[1].as_i64().unwrap_or(0) as u16,
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(PdfImage {
        xobject: object,
        stream,
        width,
        height,
        bits_per_component: bpc,
        sample_bits,
        is_mask,
        interpolate,
        components: components as u8,
        color_space,
        filter,
        samples,
        decode,
        alpha,
        color_key,
        matte,
    })
}
fn codec(e: pdf_codec::CodecError) -> InterpError {
    InterpError::Limit(e.to_string())
}
fn byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}
fn alpha_at(a: &AlphaChannel, w: u32, h: u32, i: usize) -> u8 {
    let x = i % w as usize;
    let y = i / w as usize;
    let mx = x * a.width as usize / w as usize;
    let my = y * a.height as usize / h as usize;
    a.data.get(my * a.width as usize + mx).copied().unwrap_or(0)
}
