//! Image codecs for pdf-goat: the PDF image filters (CCITTFaxDecode,
//! DCTDecode, JPXDecode, JBIG2Decode), JPEG and PNG encoders, raster file
//! decoding for `from-images`, and sample-unpacking helpers for rendering.
//!
//! Every decoder returns [`CodecError`] on malformed input and never panics;
//! dimensions are bounded by [`MAX_DIMENSION`] and [`MAX_PIXELS`].

mod bits;
mod bmp;
mod ccitt;
mod dct;
mod error;
mod gif;
mod ifd;
mod image;
mod imagefile;
mod jbig2;
mod jpeg_enc;
mod jpx;
pub mod pixels;
mod png;
mod pnm;
mod tiff;
mod webp;

pub use ccitt::{CcittImage, CcittParams, decode_ccitt, encode_ccitt_g4};
pub use dct::{DctImage, DctParams, JpegInfo, decode_dct, is_jpeg, jpeg_info};
pub use error::{CodecError, MAX_DIMENSION, MAX_PIXELS, Result};
pub use image::{BilevelImage, DecodedImage, PixelLayout, row_stride};
pub use imagefile::{
    ImageFormat, ImageProbe, JpegPassthrough, JpxPassthrough, PdfColorSpace, PngPassthrough,
    decode_image_file, detect_format, icc_color_space, jpeg_passthrough, jpx_passthrough,
    png_passthrough, probe_image_file, round_dpi,
};
pub use jbig2::{decode_jbig2, decode_jbig2_file, is_jbig2_file};
pub use jpeg_enc::{JpegColor, encode_jpeg};
pub use jpx::{JpxColorSpace, JpxImage, JpxInfo, decode_jpx, is_jpx, jpx_info};
pub use png::{PngColor, PngInfo, decode_png, encode_png, is_png, png_info};
pub use tiff::{
    Group4Strip, TiffPageInfo, decode_tiff, tiff_group4_strip, tiff_page_count, tiff_page_info,
};
