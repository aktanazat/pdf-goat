//! Replaced images, including local, HTTP, data, and inline SVG sources.

use std::collections::HashMap;
use url::Url;

use pdf_codec::{
    ImageFormat, PdfColorSpace, PixelLayout, decode_image_file, detect_format, jpeg_passthrough,
};

use super::assets::Assets;
use super::fonts::Fonts;
use super::style::percent_decode_bytes;

pub type ImageId = usize;

pub enum Pixels {
    /// A baseline or progressive JPEG embedded as it is, with `DCTDecode`.
    Jpeg {
        data: Vec<u8>,
        color_space: PdfColorSpace,
        inverted: bool,
    },
    /// 8-bit samples, gray or RGB, with an optional 8-bit alpha plane.
    Raw {
        samples: Vec<u8>,
        gray: bool,
        alpha: Option<Vec<u8>>,
    },
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub intrinsic: (f32, f32),
    pub pixels: Pixels,
    pub text: Vec<super::svg::Glyph>,
}

#[derive(Default)]
pub struct Images {
    pub list: Vec<Image>,
    by_source: HashMap<String, Option<ImageId>>,
}

impl Images {
    /// A source is decoded once after resolving it against the document's base URL.
    pub fn load(
        &mut self,
        assets: &Assets,
        base: &Url,
        src: &str,
        fonts: &mut Fonts,
    ) -> Option<ImageId> {
        let key = base.join(src.trim()).ok()?.to_string();
        if let Some(&known) = self.by_source.get(&key) {
            return known;
        }
        let id = assets.fetch(base, src).and_then(|asset| {
            let image = if detect_format(&asset.bytes).is_some() {
                decode(&asset.bytes)
            } else {
                super::svg::rasterize(&asset.bytes, assets, &asset.url, fonts)
            }?;
            Some(self.insert(image))
        });
        self.by_source.insert(key, id);
        id
    }

    pub fn load_svg(
        &mut self,
        assets: &Assets,
        base: &Url,
        xml: &str,
        fonts: &mut Fonts,
    ) -> Option<ImageId> {
        let image = super::svg::rasterize(xml.as_bytes(), assets, base, fonts)?;
        Some(self.insert(image))
    }

    fn insert(&mut self, image: Image) -> ImageId {
        let id = self.list.len();
        self.list.push(image);
        id
    }
}

fn decode(bytes: &[u8]) -> Option<Image> {
    if detect_format(bytes)? == ImageFormat::Jpeg
        && let Ok(jpeg) = jpeg_passthrough(bytes, 72)
    {
        return Some(Image {
            width: jpeg.width,
            height: jpeg.height,
            intrinsic: (jpeg.width as f32, jpeg.height as f32),
            pixels: Pixels::Jpeg {
                data: bytes.to_vec(),
                color_space: jpeg.color_space,
                inverted: jpeg.inverted_cmyk,
            },
            text: Vec::new(),
        });
    }
    let decoded = decode_image_file(bytes, 0).ok()?;
    let (width, height) = (decoded.width, decoded.height);
    if decoded.layout == PixelLayout::Gray {
        return Some(Image {
            width,
            height,
            intrinsic: (width as f32, height as f32),
            pixels: Pixels::Raw {
                samples: decoded.samples8(),
                gray: true,
                alpha: None,
            },
            text: Vec::new(),
        });
    }
    let rgba = decoded.to_rgba8();
    let mut samples = Vec::with_capacity(rgba.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(rgba.len() / 4);
    for pixel in rgba.as_chunks::<4>().0 {
        samples.extend_from_slice(&pixel[..3]);
        alpha.push(pixel[3]);
    }
    let alpha = alpha.iter().any(|&a| a != 255).then_some(alpha);
    Some(Image {
        width,
        height,
        intrinsic: (width as f32, height as f32),
        pixels: Pixels::Raw {
            samples,
            gray: false,
            alpha,
        },
        text: Vec::new(),
    })
}

/// The payload of a `data:` URI, base64 or percent-encoded.
pub(super) fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let (header, payload) = uri.get(5..)?.split_once(',')?;
    let bytes = percent_decode_bytes(payload);
    if header
        .split(';')
        .any(|part| part.trim().eq_ignore_ascii_case("base64"))
    {
        base64_decode(std::str::from_utf8(&bytes).ok()?)
    } else {
        Some(bytes)
    }
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}
