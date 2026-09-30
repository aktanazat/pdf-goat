//! Anti-aliased 2D rasterizer: paths, strokes, clips, images, and shaders.
//!
//! Device space is the pixel grid of a [`Pixmap`]: `x` right, `y` down,
//! pixel `(i, j)` covering `[i, i + 1] × [j, j + 1]`. User-space geometry
//! reaches device space through a PDF-order [`Transform`]. Pixels are RGBA8
//! with premultiplied alpha; coverage and masks are 8-bit.

mod blend;
mod canvas;
mod geom;
mod image;
mod mask;
mod path;
mod pixmap;
mod raster;
mod shade;
mod stroke;

pub use blend::BlendMode;
pub use canvas::{
    Canvas, Composite, FillStyle, ImageOptions, MAX_GROUP_BYTES, MAX_GROUP_DEPTH, Paint, Source,
};
pub use geom::{IntRect, Point, Rect, Transform};
pub use image::{Filter, FnShader, Image, ImageFormat, MaskImage, PixmapShader, Shader};
pub use mask::Mask;
pub use path::{Path, PathBuilder, PathEl};
pub use pixmap::{Color, MAX_DIMENSION, MAX_PIXELS, Pixmap, RasterError};
pub use raster::FillRule;
pub use shade::{ShadePainter, ShadeSource, ShadeVertex};
pub use stroke::{Dash, LineCap, LineJoin, Stroke};
