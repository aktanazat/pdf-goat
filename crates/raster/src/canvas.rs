//! Drawing surface: fills, strokes, images, clip stack, and transparency groups.

use std::sync::Arc;

use crate::blend::{
    BlendMode, SrcRow, blend_pixel, blend_row, expand, knockout_pixel, lerp, over_alpha,
};
use crate::geom::{IntRect, Point, Rect, Transform};
use crate::image::{Filter, Image, ImageShader, MaskImage, Pixels, Shader, reduce, sample_mask};
use crate::mask::{Mask, scale_all};
use crate::path::Path;
use crate::pixmap::{Color, Pixmap, RasterError, color_byte, mul255, to_u8, unit, zeroed_bytes};
use crate::raster::{Edges, FillRule, Rasterizer};
use crate::stroke::{Stroke, stroke_edges};

/// Where paint colour comes from.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    Solid(Color),
    /// Per-pixel colour in device space (gradients, patterns).
    Shader(&'a dyn Shader),
}

/// How painted colour combines with the backdrop.
#[derive(Clone, Copy, Debug)]
pub struct Composite<'a> {
    /// Constant alpha (PDF `ca`/`CA`), `0.0..=1.0`.
    pub alpha: f32,
    pub blend_mode: BlendMode,
    /// Soft mask in device space (PDF `SMask` in the graphics state).
    pub soft_mask: Option<&'a Mask>,
    /// PDF `AIS`: alpha and soft masks reduce shape instead of opacity.
    /// This matters in knockout groups; the effective alpha is unchanged.
    pub alpha_is_shape: bool,
}

impl Default for Composite<'_> {
    fn default() -> Self {
        Composite {
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            soft_mask: None,
            alpha_is_shape: false,
        }
    }
}

/// Colour source plus compositing for fills, strokes, and stencil masks.
#[derive(Clone, Copy)]
pub struct Paint<'a> {
    pub source: Source<'a>,
    pub composite: Composite<'a>,
}

impl Paint<'_> {
    /// Opaque-compositing paint of one colour.
    pub fn solid(color: Color) -> Paint<'static> {
        Paint {
            source: Source::Solid(color),
            composite: Composite::default(),
        }
    }

    /// Paint from a shader.
    pub fn shader(shader: &dyn Shader) -> Paint<'_> {
        Paint {
            source: Source::Shader(shader),
            composite: Composite::default(),
        }
    }
}

/// Image drawing options.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageOptions<'a> {
    pub filter: Filter,
    /// Per-image alpha (PDF image `SMask`), placed in the same unit square
    /// as the image and sampled at its own resolution.
    pub soft_mask: Option<MaskImage<'a>>,
    pub composite: Composite<'a>,
}

#[derive(Clone)]
struct Clip {
    /// Rectangle part of the clip in device space.
    rect: Rect,
    /// Pixels fully inside `rect`.
    inner: IntRect,
    /// Pixels that can have nonzero clip coverage.
    bounds: IntRect,
    mask: Option<Arc<Mask>>,
}

impl Clip {
    fn from_rect(rect: Rect, mask: Option<Arc<Mask>>, limit: IntRect) -> Clip {
        let inner = IntRect::new(
            crate::geom::saturate_i32(rect.x0.ceil()),
            crate::geom::saturate_i32(rect.y0.ceil()),
            crate::geom::saturate_i32(rect.x1.floor()),
            crate::geom::saturate_i32(rect.y1.floor()),
        );
        let bounds = if rect.is_empty() {
            IntRect::EMPTY
        } else {
            rect.round_out().intersect(&limit)
        };
        Clip {
            rect,
            inner,
            bounds,
            mask,
        }
    }

    /// Multiplies `row` (pixels `x0..` of row `y`) by the clip coverage.
    fn apply(&self, x0: i32, y: i32, row: &mut [u8]) {
        let n = i32::try_from(row.len()).unwrap_or(i32::MAX);
        let fully_inner = y >= self.inner.y0
            && y < self.inner.y1
            && x0 >= self.inner.x0
            && x0.saturating_add(n) <= self.inner.x1;
        if !fully_inner {
            let fy = f64::from(y);
            let cy = overlap(fy, self.rect.y0, self.rect.y1);
            if cy <= 0.0 {
                row.fill(0);
                return;
            }
            for (i, v) in row.iter_mut().enumerate() {
                if *v == 0 {
                    continue;
                }
                let fx = f64::from(x0) + i as f64;
                let c = overlap(fx, self.rect.x0, self.rect.x1) * cy;
                if c < 1.0 {
                    *v = mul255(u32::from(*v), u32::from(to_u8(c as f32))) as u8;
                }
            }
        }
        if let Some(mask) = &self.mask {
            mask.mul_row(x0, y, row);
        }
    }
}

/// Length of `[p, p + 1] ∩ [lo, hi]`.
fn overlap(p: f64, lo: f64, hi: f64) -> f64 {
    ((p + 1.0).min(hi) - p.max(lo)).clamp(0.0, 1.0)
}

/// Maximum bytes held by open group pixel, alpha, and shape buffers.
pub const MAX_GROUP_BYTES: usize = 1 << 30;
/// Maximum number of simultaneously open transparency groups.
pub const MAX_GROUP_DEPTH: usize = 64;

struct Layer {
    pixmap: Pixmap,
    /// Initial backdrop: None = transparent, 0 = base, n = layers[n - 1].
    /// Ancestors remain unchanged until this layer is popped.
    initial: Option<usize>,
    knockout: bool,
    /// Contribution alpha, excluding the initial backdrop. Empty when isolated.
    alpha: Vec<u8>,
    /// Geometric shape, needed when an enclosing group uses knockout.
    shape: Vec<u8>,
    clip_depth: usize,
    entry_clip: Clip,
    dirty: IntRect,
    bytes: usize,
}

fn initial_pixmap<'a>(
    base: &'a Pixmap,
    layers: &'a [Layer],
    initial: Option<usize>,
) -> Option<&'a Pixmap> {
    match initial {
        None => None,
        Some(0) => Some(base),
        Some(n) => Some(&layers[n - 1].pixmap),
    }
}

struct DrawRow<'a> {
    x: u32,
    y: u32,
    origin: (i32, i32),
    source: SrcRow<'a>,
    coverage: &'a [u8],
    shape: &'a [u8],
    mode: BlendMode,
    /// MuPDF paints opaque solid objects directly even in a knockout group.
    knockout: bool,
}

fn draw_row(base: &mut Pixmap, layers: &mut [Layer], row: DrawRow<'_>) {
    let n = row.coverage.len();
    let x1 = row.x + n as u32;
    let Some((layer, earlier)) = layers.split_last_mut() else {
        blend_row(
            base.row_mut(row.y, row.x, x1),
            row.source,
            row.coverage,
            row.mode,
        );
        return;
    };
    let offset = row.y as usize * layer.pixmap.width() as usize + row.x as usize;
    let dst = layer.pixmap.row_mut(row.y, row.x, x1);
    if layer.knockout && row.knockout {
        let initial = initial_pixmap(base, earlier, layer.initial)
            .map(|p| p.row(row.y, row.x, x1).as_chunks::<4>().0);
        for (i, pixel) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let mut source = initial.map_or([0; 4], |p| p[i]);
            blend_row(
                &mut source,
                row.source.at(i),
                &row.coverage[i..i + 1],
                row.mode,
            );
            let shape = row.shape[i];
            knockout_pixel(pixel, source, shape);
            if let Some(alpha) = layer.alpha.get_mut(offset + i) {
                let sa = row.source.over_alpha(i, row.coverage[i], 0);
                *alpha = (mul255(u32::from(sa), u32::from(shape))
                    + mul255(u32::from(*alpha), 255 - u32::from(shape)))
                    as u8;
            }
        }
    } else {
        blend_row(dst, row.source, row.coverage, row.mode);
        if let Some(alpha) = layer.alpha.get_mut(offset..offset + n) {
            for (i, (alpha, &cov)) in alpha.iter_mut().zip(row.coverage).enumerate() {
                *alpha = row.source.over_alpha(i, cov, *alpha);
            }
        }
    }
    if let Some(accumulated) = layer.shape.get_mut(offset..offset + n) {
        for (accumulated, &shape) in accumulated.iter_mut().zip(row.shape) {
            *accumulated = over_alpha(*accumulated, shape, 255);
        }
    }
    layer.dirty = layer.dirty.union(&IntRect::new(
        row.x as i32 + row.origin.0,
        row.y as i32 + row.origin.1,
        x1 as i32 + row.origin.0,
        row.y as i32 + row.origin.1 + 1,
    ));
}

/// Samples a stencil mask to scale coverage rows.
struct Stencil<'a> {
    mask: MaskImage<'a>,
    inverse: Transform,
    filter: Filter,
}

impl Stencil<'_> {
    fn mul_row(&self, x0: i32, y: i32, row: &mut [u8]) {
        let start = self
            .inverse
            .apply(Point::new(f64::from(x0) + 0.5, f64::from(y) + 0.5));
        for (i, v) in row.iter_mut().enumerate() {
            if *v == 0 {
                continue;
            }
            let u = start.x + self.inverse.a * i as f64;
            let w = start.y + self.inverse.b * i as f64;
            let m = sample_mask(&self.mask, u, w, self.filter);
            *v = mul255(u32::from(*v), u32::from(m)) as u8;
        }
    }
}

/// A drawing surface over an RGBA8 premultiplied pixmap.
///
/// Device space is the pixel grid: `x` right, `y` down, pixel `(i, j)`
/// covering `[i, i + 1] × [j, j + 1]`. Callers fold the PDF page transform
/// into the transforms they pass.
pub struct Canvas {
    base: Pixmap,
    bounds: IntRect,
    layers: Vec<Layer>,
    group_bytes: usize,
    clips: Vec<Clip>,
    raster: Rasterizer,
    edges: Edges,
    cov: Mask,
    row_cov: Vec<u8>,
    row_shape: Vec<u8>,
    row_src: Vec<[u8; 4]>,
}

impl Canvas {
    /// A transparent canvas.
    pub fn new(width: u32, height: u32) -> Result<Canvas, RasterError> {
        Ok(Canvas::from_pixmap(Pixmap::new(width, height)?))
    }

    /// A transparent canvas retaining its absolute device-space origin.
    /// Keeping that origin through edge quantization avoids changing
    /// fractional coverage when a pattern cell is rendered offscreen.
    pub fn new_at(bounds: IntRect) -> Result<Canvas, RasterError> {
        let pixmap = Pixmap::new(bounds.width(), bounds.height())?;
        Ok(Self::from_pixmap_in(pixmap, bounds))
    }

    /// Draws onto an existing pixmap.
    pub fn from_pixmap(pixmap: Pixmap) -> Canvas {
        let bounds = pixmap.bounds();
        Self::from_pixmap_in(pixmap, bounds)
    }

    fn from_pixmap_in(pixmap: Pixmap, bounds: IntRect) -> Canvas {
        Canvas {
            base: pixmap,
            bounds,
            layers: Vec::new(),
            group_bytes: 0,
            clips: vec![Clip::from_rect(bounds.to_rect(), None, bounds)],
            raster: Rasterizer::default(),
            edges: Edges::default(),
            cov: Mask::default(),
            row_cov: Vec::new(),
            row_src: Vec::new(),
            row_shape: Vec::new(),
        }
    }

    pub fn width(&self) -> u32 {
        self.base.width()
    }

    pub fn height(&self) -> u32 {
        self.base.height()
    }

    /// The base pixmap. Open groups are not included until popped.
    pub fn pixmap(&self) -> &Pixmap {
        &self.base
    }

    /// Pops every open group with default compositing and returns the result.
    pub fn finish(mut self) -> Pixmap {
        while self.pop_group(&Composite::default()) {}
        self.base
    }

    /// Absolute device-space bounds of the backing pixmap.
    pub fn bounds(&self) -> IntRect {
        self.bounds
    }

    fn top_clip(&self) -> &Clip {
        // clips[0] is the canvas rectangle and is never popped.
        &self.clips[self.clips.len() - 1]
    }

    /// Fills the current target (innermost open group, else the base) with
    /// `color`, ignoring the clip.
    pub fn clear(&mut self, color: Color) {
        let full = self.bounds();
        let Some((layer, earlier)) = self.layers.split_last_mut() else {
            self.base.fill(color);
            return;
        };
        let source = color.to_premultiplied();
        if let Some(initial) = initial_pixmap(&self.base, earlier, layer.initial) {
            layer.pixmap.data_mut().copy_from_slice(initial.data());
            for pixel in layer.pixmap.data_mut().as_chunks_mut::<4>().0 {
                blend_pixel(pixel, source, BlendMode::Normal);
            }
        } else {
            layer.pixmap.fill(color);
        }
        layer.alpha.fill(source[3]);
        layer.shape.fill(if source[3] == 0 { 0 } else { 255 });
        layer.dirty = full;
    }

    /// Fills `path` mapped through `transform`.
    pub fn fill_path(
        &mut self,
        path: &Path,
        transform: &Transform,
        rule: FillRule,
        paint: &Paint<'_>,
    ) {
        self.edges.reset(self.top_clip().bounds);
        self.edges.add_path(path, transform);
        let mut cov = std::mem::take(&mut self.cov);
        if self.raster.rasterize(&mut self.edges, rule, &mut cov) {
            let extents = std::mem::take(&mut self.raster.extents);
            self.paint(&cov, &extents, (0, 0), paint.source, None, &paint.composite);
            self.raster.extents = extents;
        }
        self.cov = cov;
    }

    /// Fills a rectangle given in user space.
    pub fn fill_rect(&mut self, rect: Rect, transform: &Transform, paint: &Paint<'_>) {
        self.fill_path(&Path::from_rect(rect), transform, FillRule::NonZero, paint);
    }

    /// Strokes `path`; the stroke geometry is built in user space and then
    /// mapped through `transform`.
    pub fn stroke_path(
        &mut self,
        path: &Path,
        transform: &Transform,
        stroke: &Stroke,
        paint: &Paint<'_>,
    ) {
        self.edges.reset(self.top_clip().bounds);
        stroke_edges(path, transform, stroke, &mut self.edges);
        let mut cov = std::mem::take(&mut self.cov);
        if self
            .raster
            .rasterize(&mut self.edges, FillRule::NonZero, &mut cov)
        {
            let extents = std::mem::take(&mut self.raster.extents);
            self.paint(&cov, &extents, (0, 0), paint.source, None, &paint.composite);
            self.raster.extents = extents;
        }
        self.cov = cov;
    }

    /// Pixels that filling `path` can reach: the bounds of its flattened,
    /// non-horizontal edges rounded out and limited to the clip. A shading
    /// painted through the path is scissored to this, as MuPDF does.
    pub fn fill_bounds(&mut self, path: &Path, transform: &Transform) -> IntRect {
        self.edges.reset(self.top_clip().bounds);
        self.edges.add_path(path, transform);
        self.edge_bounds()
    }

    /// The whole-pixel scissor MuPDF clips to instead of a mask when `path`
    /// scan converts to an axis-aligned rectangle (`fz_is_rect_gel`): its
    /// edges land on the 17 × 15 subsample grid and the pixel bounds round
    /// out from there. `None` for any other shape.
    pub fn fill_scissor(&mut self, path: &Path, transform: &Transform) -> Option<IntRect> {
        self.edges.reset(self.top_clip().bounds);
        self.edges.add_path(path, transform);
        self.edges.is_rect().then(|| self.edge_bounds())
    }

    /// Pixels that stroking `path` can reach; see [`Canvas::fill_bounds`].
    pub fn stroke_bounds(
        &mut self,
        path: &Path,
        transform: &Transform,
        stroke: &Stroke,
    ) -> IntRect {
        self.edges.reset(self.top_clip().bounds);
        stroke_edges(path, transform, stroke, &mut self.edges);
        self.edge_bounds()
    }

    fn edge_bounds(&self) -> IntRect {
        self.edges.bounds().intersect(&self.top_clip().bounds)
    }

    /// Paints a precomputed coverage mask (for example a cached glyph).
    pub fn fill_mask(&mut self, mask: &Mask, paint: &Paint<'_>) {
        self.paint(mask, &[], (0, 0), paint.source, None, &paint.composite);
    }

    /// Paints `mask` moved by whole pixels `(dx, dy)`: a glyph bitmap
    /// rendered relative to its origin, placed at the origin's pixel.
    pub fn fill_mask_at(&mut self, mask: &Mask, dx: i32, dy: i32, paint: &Paint<'_>) {
        self.paint(mask, &[], (dx, dy), paint.source, None, &paint.composite);
    }

    /// Draws `image` into the parallelogram that `transform` makes of the
    /// unit square (PDF image space: sample row 0 at the top, `v = 1`).
    /// Axis-aligned placements snap to whole pixels so abutting images leave
    /// no seams.
    pub fn draw_image(
        &mut self,
        image: &Image<'_>,
        transform: &Transform,
        options: &ImageOptions<'_>,
    ) {
        let t = gridfit(transform);
        let Some(inverse) = t.invert() else {
            return;
        };
        if !self.unit_square_coverage(&t) {
            return;
        }
        let (kx, ky) = reduction(&t, image.width(), image.height());
        let (pixels, soft_mask) = if options.filter == Filter::Bilinear && (kx > 1 || ky > 1) {
            (reduce(image, options.soft_mask.as_ref(), kx, ky), None)
        } else {
            (Pixels::Borrowed(*image), options.soft_mask)
        };
        let shader = ImageShader {
            pixels,
            soft_mask,
            inverse,
            filter: options.filter,
        };
        let cov = std::mem::take(&mut self.cov);
        let extents = std::mem::take(&mut self.raster.extents);
        self.paint(
            &cov,
            &extents,
            (0, 0),
            Source::Shader(&shader),
            None,
            &options.composite,
        );
        self.cov = cov;
        self.raster.extents = extents;
    }

    /// Paints `paint` through a stencil mask placed like an image
    /// (PDF `ImageMask`): each pixel's coverage is scaled by the sampled mask.
    pub fn draw_stencil(
        &mut self,
        mask: &MaskImage<'_>,
        transform: &Transform,
        filter: Filter,
        paint: &Paint<'_>,
    ) {
        let t = gridfit(transform);
        let Some(inverse) = t.invert() else {
            return;
        };
        if !self.unit_square_coverage(&t) {
            return;
        }
        let (kx, ky) = reduction(&t, mask.width(), mask.height());
        let reduced;
        let mask = if filter == Filter::Bilinear && (kx > 1 || ky > 1) {
            reduced = reduce_mask(mask, kx, ky);
            match MaskImage::new(
                mask.width().div_ceil(kx),
                mask.height().div_ceil(ky),
                &reduced,
            ) {
                Ok(m) => m,
                Err(_) => *mask,
            }
        } else {
            *mask
        };
        let stencil = Stencil {
            mask,
            inverse,
            filter,
        };
        let cov = std::mem::take(&mut self.cov);
        let extents = std::mem::take(&mut self.raster.extents);
        self.paint(
            &cov,
            &extents,
            (0, 0),
            paint.source,
            Some(&stencil),
            &paint.composite,
        );
        self.cov = cov;
        self.raster.extents = extents;
    }

    fn unit_square_coverage(&mut self, t: &Transform) -> bool {
        self.edges.reset(self.top_clip().bounds);
        let unit = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
        ];
        self.edges.add_polygon(&unit, t);
        self.raster
            .rasterize(&mut self.edges, FillRule::NonZero, &mut self.cov)
    }

    /// Intersects the clip with `path`. As in MuPDF, a path that scan
    /// converts to an axis-aligned rectangle becomes a whole-pixel scissor
    /// (see [`Canvas::fill_scissor`]); any other shape becomes a coverage
    /// mask.
    pub fn push_clip_path(&mut self, path: &Path, transform: &Transform, rule: FillRule) {
        let top = self.top_clip().clone();
        self.edges.reset(top.bounds);
        self.edges.add_path(path, transform);
        if self.edges.is_rect() {
            let bounds = self.edges.bounds().intersect(&top.bounds);
            self.push_clip_rect(bounds.to_rect(), &Transform::IDENTITY);
            return;
        }
        let mut mask = Mask::default();
        if !self.raster.rasterize(&mut self.edges, rule, &mut mask) {
            self.clips.push(Clip {
                bounds: IntRect::EMPTY,
                ..top
            });
            return;
        }
        if let Some(prev) = &top.mask {
            let rect = mask.rect();
            let w = rect.width() as usize;
            for (ry, row) in mask.data_mut().chunks_exact_mut(w).enumerate() {
                prev.mul_row(rect.x0, rect.y0 + ry as i32, row);
            }
        }
        let bounds = mask.nonzero_bounds().intersect(&top.bounds);
        self.clips.push(Clip {
            rect: top.rect,
            inner: top.inner,
            bounds,
            mask: Some(Arc::new(mask)),
        });
    }

    /// Intersects the clip with a rectangle in user space. Axis-aligned
    /// transforms keep an exact analytic rectangle (fractional edges give
    /// partial coverage); others fall back to a path clip.
    pub fn push_clip_rect(&mut self, rect: Rect, transform: &Transform) {
        if !transform.is_axis_aligned() {
            self.push_clip_path(&Path::from_rect(rect), transform, FillRule::NonZero);
            return;
        }
        let top = self.top_clip().clone();
        let device = transform.map_rect(rect);
        let rect = top.rect.intersect(&device);
        let clip = Clip::from_rect(rect, top.mask.clone(), top.bounds);
        self.clips.push(clip);
    }

    /// Intersects the clip with a device-space coverage mask.
    pub fn push_clip_mask(&mut self, mask: &Mask) {
        let top = self.top_clip().clone();
        let region = top.bounds;
        let mut combined = Mask::new(region, 255, 0).unwrap_or_default();
        let w = region.width() as usize;
        if w > 0 {
            for (ry, row) in combined.data_mut().chunks_exact_mut(w).enumerate() {
                let y = region.y0 + ry as i32;
                mask.mul_row(region.x0, y, row);
                if let Some(prev) = &top.mask {
                    prev.mul_row(region.x0, y, row);
                }
            }
        }
        let bounds = combined.nonzero_bounds().intersect(&top.bounds);
        self.clips.push(Clip {
            rect: top.rect,
            inner: top.inner,
            bounds,
            mask: Some(Arc::new(combined)),
        });
    }

    /// Removes the innermost clip. Cannot remove canvas bounds or clips
    /// inherited by the current transparency group.
    pub fn pop_clip(&mut self) -> bool {
        let floor = self.layers.last().map_or(1, |layer| layer.clip_depth);
        if self.clips.len() > floor {
            self.clips.pop();
            true
        } else {
            false
        }
    }

    /// Number of pushed clips (0 when only the canvas bounds clip).
    pub fn clip_depth(&self) -> usize {
        self.clips.len() - 1
    }

    /// Restores `depth`, without removing clips inherited by an open group.
    pub fn restore_clip_depth(&mut self, depth: usize) {
        let floor = self.layers.last().map_or(1, |layer| layer.clip_depth);
        self.clips.truncate(depth.saturating_add(1).max(floor));
    }

    /// Pixels that the current clip can let through.
    pub fn clip_bounds(&self) -> IntRect {
        self.top_clip().bounds
    }

    /// Starts an isolated, non-knockout group. Allocation or limit failure
    /// leaves pixels, clips, and group depth unchanged.
    pub fn push_group(&mut self) -> Result<(), RasterError> {
        self.begin_group(true, false)
    }

    /// Starts a PDF transparency group. A non-isolated group inherits its
    /// enclosing backdrop; a knockout group's objects replace siblings by
    /// shape and blend against the group's initial backdrop.
    ///
    /// Inherited clips apply once, when the group is popped. Clips pushed
    /// inside the group apply to its contents and cannot pop inherited clips.
    /// Buffer use is bounded by [`MAX_GROUP_BYTES`] and [`MAX_GROUP_DEPTH`].
    /// On any error the drawing state is unchanged; no group is flattened.
    pub fn begin_group(&mut self, isolated: bool, knockout: bool) -> Result<(), RasterError> {
        if self.layers.len() >= MAX_GROUP_DEPTH {
            return Err(RasterError::GroupDepthLimit {
                limit: MAX_GROUP_DEPTH,
            });
        }
        let parent = self.layers.last();
        let initial = if isolated {
            None
        } else if let Some(parent) = parent.filter(|p| p.knockout) {
            parent.initial
        } else {
            Some(self.layers.len())
        };
        let track_shape = parent.is_some_and(|p| p.knockout || !p.shape.is_empty());
        let pixels = self.base.data().len() / 4;
        let bytes = pixels * (4 + usize::from(initial.is_some()) + usize::from(track_shape));
        let requested = self.group_bytes.saturating_add(bytes);
        if requested > MAX_GROUP_BYTES {
            return Err(RasterError::GroupMemoryLimit {
                requested,
                limit: MAX_GROUP_BYTES,
            });
        }
        let mut pixmap = Pixmap::new(self.width(), self.height())?;
        if let Some(backdrop) = initial_pixmap(&self.base, &self.layers, initial) {
            pixmap.data_mut().copy_from_slice(backdrop.data());
        }
        let alpha = zeroed_bytes(if initial.is_some() { pixels } else { 0 })?;
        let shape = zeroed_bytes(if track_shape { pixels } else { 0 })?;
        self.layers
            .try_reserve(1)
            .map_err(|_| RasterError::AllocationFailed {
                bytes: (self.layers.len() + 1) * std::mem::size_of::<Layer>(),
            })?;
        let entry_clip = self.top_clip().clone();
        let local_clip = Clip::from_rect(entry_clip.bounds.to_rect(), None, self.bounds());
        let clip_depth = self.clips.len();
        self.layers.push(Layer {
            pixmap,
            initial,
            knockout,
            alpha,
            shape,
            clip_depth,
            entry_clip,
            dirty: IntRect::EMPTY,
            bytes,
        });
        self.clips[clip_depth - 1] = local_clip;
        self.group_bytes = requested;
        Ok(())
    }

    /// Composites the innermost group as one object, removing any inherited
    /// backdrop contribution before applying `composite`. Group shape, not
    /// its bounding box or opacity, is used in an enclosing knockout group.
    /// Restores the entry clip and drops clips pushed inside the group.
    /// Returns false when no group is open.
    pub fn pop_group(&mut self, composite: &Composite<'_>) -> bool {
        let Some(layer) = self.layers.pop() else {
            return false;
        };
        self.group_bytes -= layer.bytes;
        self.clips.truncate(layer.clip_depth);
        self.clips[layer.clip_depth - 1] = layer.entry_clip;
        let Canvas {
            base,
            bounds,
            layers,
            clips,
            row_cov,
            row_shape,
            ..
        } = self;
        let clip = &clips[clips.len() - 1];
        let region = layer.dirty.intersect(&clip.bounds);
        if region.is_empty() {
            return true;
        }
        let alpha = color_byte(composite.alpha);
        let n = region.width() as usize;
        for y in region.y0..region.y1 {
            let x0 = (region.x0 - bounds.x0) as u32;
            let x1 = (region.x1 - bounds.x0) as u32;
            let local_y = (y - bounds.y0) as u32;
            let offset = local_y as usize * base.width() as usize + x0 as usize;
            let pixels = layer.pixmap.row(local_y, x0, x1);
            let source = if layer.initial.is_some() {
                SrcRow::Backdrop {
                    pixels,
                    alpha: &layer.alpha[offset..offset + n],
                }
            } else {
                SrcRow::Pixels(pixels)
            };
            row_shape.clear();
            if layer.shape.is_empty() {
                row_shape.resize(n, 255);
            } else {
                row_shape.extend_from_slice(&layer.shape[offset..offset + n]);
            }
            clip.apply(region.x0, y, row_shape);
            if composite.alpha_is_shape {
                scale_all(row_shape, u32::from(alpha));
                if let Some(mask) = composite.soft_mask {
                    mask.mul_row(region.x0, y, row_shape);
                }
            }
            row_cov.clear();
            row_cov.resize(n, alpha);
            clip.apply(region.x0, y, row_cov);
            if let Some(mask) = composite.soft_mask {
                mask.mul_row(region.x0, y, row_cov);
            }
            draw_row(
                base,
                layers,
                DrawRow {
                    x: x0,
                    y: local_y,
                    origin: (bounds.x0, bounds.y0),
                    source,
                    coverage: row_cov,
                    shape: row_shape,
                    mode: composite.blend_mode,
                    knockout: true,
                },
            );
        }
        true
    }

    /// Number of open groups.
    pub fn group_depth(&self) -> usize {
        self.layers.len()
    }

    /// Composites `src` through coverage `cov` (moved by whole pixels
    /// `offset`), the clip, an optional stencil, and the composite's alpha
    /// and soft mask. `extents` holds, per row of `cov`, the pixel range the
    /// rasterizer wrote (empty when unknown).
    fn paint(
        &mut self,
        cov: &Mask,
        extents: &[[u32; 2]],
        offset: (i32, i32),
        src: Source<'_>,
        stencil: Option<&Stencil<'_>>,
        composite: &Composite<'_>,
    ) {
        let alpha = color_byte(composite.alpha);
        let grouped = !self.layers.is_empty();
        if alpha == 0 && !grouped {
            return;
        }
        let Canvas {
            base,
            bounds,
            layers,
            clips,
            row_cov,
            row_shape,
            row_src,
            ..
        } = self;
        let clip = &clips[clips.len() - 1];
        let cov_rect = {
            let r = cov.rect();
            IntRect::new(
                r.x0.saturating_add(offset.0),
                r.y0.saturating_add(offset.1),
                r.x1.saturating_add(offset.0),
                r.y1.saturating_add(offset.1),
            )
        };
        let region = cov_rect.intersect(&clip.bounds).intersect(bounds);
        if region.is_empty() {
            return;
        }
        let solid = match src {
            Source::Solid(c) => Some((c.to_rgb8(), color_byte(unit(c.a) * composite.alpha))),
            Source::Shader(_) => None,
        };
        let cov_alpha = if solid.is_some() { 255 } else { alpha };
        if !grouped && solid.is_some_and(|(_, a)| a == 0) {
            return;
        }
        let n = region.width() as usize;
        let dx = (region.x0 - cov_rect.x0) as usize;
        // A solid colour through an unmodified coverage row inside the clip
        // rectangle paints straight from the mask.
        let direct = !grouped
            && stencil.is_none()
            && composite.soft_mask.is_none()
            && clip.mask.is_none()
            && region.x0 >= clip.inner.x0
            && region.x1 <= clip.inner.x1;
        for y in region.y0..region.y1 {
            let Some(row) = cov.row(y - offset.1) else {
                continue;
            };
            // The pixels of this row worth visiting: the rasterizer's extent
            // when it is known, else the whole region.
            let (a0, b0) = match extents.get((y - cov_rect.y0) as usize) {
                Some(&[lo, hi]) => (
                    (lo as usize).clamp(dx, dx + n) - dx,
                    (hi as usize).clamp(dx, dx + n) - dx,
                ),
                None => (0, n),
            };
            if a0 >= b0 {
                continue;
            }
            let x_start = region.x0 + a0 as i32;
            let row = &row[dx + a0..dx + b0];
            if let (true, Some((color, alpha))) =
                (direct && y >= clip.inner.y0 && y < clip.inner.y1, solid)
            {
                let Some((a, b)) = nonzero_span(row) else {
                    continue;
                };
                blend_row(
                    base.row_mut(
                        (y - bounds.y0) as u32,
                        (x_start + a as i32 - bounds.x0) as u32,
                        (x_start + b as i32 - bounds.x0) as u32,
                    ),
                    SrcRow::Solid { color, alpha },
                    &row[a..b],
                    composite.blend_mode,
                );
                continue;
            }
            row_cov.clear();
            row_cov.extend_from_slice(row);
            clip.apply(x_start, y, row_cov);
            if let Some(st) = stencil {
                st.mul_row(x_start, y, row_cov);
            }
            if grouped {
                row_shape.clear();
                row_shape.extend_from_slice(row_cov);
                if composite.alpha_is_shape {
                    scale_all(row_shape, u32::from(solid.map_or(alpha, |(_, a)| a)));
                    if let Some(mask) = composite.soft_mask {
                        mask.mul_row(x_start, y, row_shape);
                    }
                }
            }
            if cov_alpha != 255 {
                scale_all(row_cov, u32::from(cov_alpha));
            }
            if let Some(mask) = composite.soft_mask {
                mask.mul_row(x_start, y, row_cov);
            }
            let Some((a, b)) = nonzero_span(if grouped { row_shape } else { row_cov }) else {
                continue;
            };
            let x0 = x_start + a as i32;
            let source = match (solid, src) {
                (Some((color, alpha)), _) => SrcRow::Solid { color, alpha },
                (None, Source::Shader(s)) => {
                    row_src.clear();
                    row_src.resize(b - a, [0; 4]);
                    s.shade_row(x0, y, row_src);
                    if grouped && composite.alpha_is_shape {
                        for (shape, pixel) in row_shape[a..b].iter_mut().zip(row_src.iter()) {
                            *shape = mul255(u32::from(*shape), u32::from(pixel[3])) as u8;
                        }
                    }
                    SrcRow::Pixels(row_src.as_flattened())
                }
                (None, Source::Solid(_)) => continue,
            };
            if grouped {
                for shape in row_shape.iter_mut() {
                    *shape = lerp(255, 0, expand(*shape));
                }
            }
            draw_row(
                base,
                layers,
                DrawRow {
                    x: (x0 - bounds.x0) as u32,
                    y: (y - bounds.y0) as u32,
                    origin: (bounds.x0, bounds.y0),
                    source,
                    coverage: &row_cov[a..b],
                    shape: if grouped { &row_shape[a..b] } else { &[] },
                    mode: composite.blend_mode,
                    knockout: solid.is_none_or(|(_, alpha)| alpha != 255)
                        || composite.soft_mask.is_some(),
                },
            );
        }
    }
}

/// First nonzero index and one past the last.
fn nonzero_span(row: &[u8]) -> Option<(usize, usize)> {
    let a = row.iter().position(|&v| v != 0)?;
    let b = row.iter().rposition(|&v| v != 0).map_or(a + 1, |i| i + 1);
    Some((a, b))
}

/// Snaps an axis-aligned unit-square placement to whole device pixels,
/// keeping at least one pixel per nonzero side.
fn gridfit(t: &Transform) -> Transform {
    if !t.is_axis_aligned() || !t.is_finite() {
        return *t;
    }
    let e = t.e.round();
    let f = t.f.round();
    let fix = |len: f64, orig: f64| {
        if orig == 0.0 {
            0.0
        } else if len == 0.0 {
            orig.signum()
        } else {
            len
        }
    };
    Transform::new(
        fix((t.e + t.a).round() - e, t.a),
        fix((t.f + t.b).round() - f, t.b),
        fix((t.e + t.c).round() - e, t.c),
        fix((t.f + t.d).round() - f, t.d),
        e,
        f,
    )
}

/// Integer box-filter factors that bring an image drawn smaller than half
/// size back to at least half a device pixel per sample.
fn reduction(t: &Transform, width: u32, height: u32) -> (u32, u32) {
    let factor = |len: f64, n: u32| {
        let per_sample = len / f64::from(n);
        if per_sample.is_finite() && per_sample > 0.0 && per_sample < 0.5 {
            ((1.0 / per_sample).floor() as u32).clamp(1, n)
        } else {
            1
        }
    };
    (
        factor(t.a.hypot(t.b), width),
        factor(t.c.hypot(t.d), height),
    )
}

fn reduce_mask(mask: &MaskImage<'_>, kx: u32, ky: u32) -> Vec<u8> {
    let (w, h) = (mask.width(), mask.height());
    let ow = w.div_ceil(kx);
    let oh = h.div_ceil(ky);
    let data = mask.data();
    let mut out = Vec::with_capacity(ow as usize * oh as usize);
    for by in 0..oh {
        let y0 = by * ky;
        let y1 = (y0 + ky).min(h);
        for bx in 0..ow {
            let x0 = bx * kx;
            let x1 = (x0 + kx).min(w);
            let mut sum = 0u64;
            for y in y0..y1 {
                let row = &data[y as usize * w as usize..][..w as usize];
                sum += row[x0 as usize..x1 as usize]
                    .iter()
                    .map(|&v| u64::from(v))
                    .sum::<u64>();
            }
            let n = u64::from(x1 - x0) * u64::from(y1 - y0);
            out.push(((sum + n / 2) / n) as u8);
        }
    }
    out
}
