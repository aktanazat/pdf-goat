//! Drawing surface: fills, strokes, images, clip stack, and transparency groups.

use std::sync::Arc;

use crate::blend::{BlendMode, SrcRow, blend_pixel, blend_row, knockout_pixel};
use crate::geom::{IntRect, Point, Rect, Transform};
use crate::image::{Filter, Image, ImageShader, MaskImage, Pixels, Shader, reduce, sample_mask};
use crate::mask::{Mask, scale_all};
use crate::path::{FlattenSink, Path, flatten};
use crate::pixmap::{Color, Pixmap, RasterError, mul255, to_u8, zeroed_bytes};
use crate::raster::{DEVICE_TOLERANCE, Edges, FillRule, Rasterizer};
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
    /// Anti-aliased edges; when off a pixel is painted if at least half of it
    /// is covered.
    pub anti_alias: bool,
}

impl Paint<'_> {
    /// Opaque-compositing, anti-aliased paint of one colour.
    pub fn solid(color: Color) -> Paint<'static> {
        Paint {
            source: Source::Solid(color),
            composite: Composite::default(),
            anti_alias: true,
        }
    }

    /// Anti-aliased paint from a shader.
    pub fn shader(shader: &dyn Shader) -> Paint<'_> {
        Paint {
            source: Source::Shader(shader),
            composite: Composite::default(),
            anti_alias: true,
        }
    }
}

/// Fill options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillStyle {
    pub rule: FillRule,
    /// PDF thin-line rule, meant for vector art (leave it off for glyphs).
    /// Each subpath whose points lie within a strip thinner than one pixel
    /// (measured across `x`, across `y`, or across the line from its first
    /// point to its farthest one) is also painted as a band exactly one
    /// pixel thick, centred on that strip and spanning the points along it.
    /// Zero-height rules and lines drawn as fills thus become one-pixel
    /// lines; a subpath that collapses to a single point paints nothing.
    pub thin_line: bool,
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
    source: SrcRow<'a>,
    coverage: &'a [u8],
    shape: &'a [u8],
    mode: BlendMode,
}

fn draw_row(base: &mut Pixmap, layers: &mut [Layer], row: DrawRow<'_>) {
    let x1 = row.x + row.coverage.len() as u32;
    let Some((layer, earlier)) = layers.split_last_mut() else {
        blend_row(
            base.row_mut(row.y, row.x, x1),
            row.source,
            row.coverage,
            row.mode,
        );
        return;
    };
    let initial = initial_pixmap(base, earlier, layer.initial)
        .map(|p| p.row(row.y, row.x, x1).as_chunks::<4>().0);
    let offset = row.y as usize * layer.pixmap.width() as usize + row.x as usize;
    let dst = layer.pixmap.row_mut(row.y, row.x, x1);
    for (i, pixel) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let source = row
            .source
            .get(i)
            .map(|v| mul255(u32::from(v), u32::from(row.coverage[i])) as u8);
        let shape = row.shape[i];
        if layer.knockout {
            knockout_pixel(
                pixel,
                initial.map_or([0; 4], |p| p[i]),
                source,
                shape,
                row.mode,
            );
        } else {
            blend_pixel(pixel, source, row.mode);
        }
        if let Some(alpha) = layer.alpha.get_mut(offset + i) {
            let keep = 255 - u32::from(if layer.knockout { shape } else { source[3] });
            *alpha = (u32::from(source[3]) + mul255(u32::from(*alpha), keep)).min(255) as u8;
        }
        if let Some(accumulated) = layer.shape.get_mut(offset + i) {
            *accumulated =
                (u32::from(shape) + mul255(u32::from(*accumulated), 255 - u32::from(shape))) as u8;
        }
    }
    layer.dirty = layer.dirty.union(&IntRect::new(
        row.x as i32,
        row.y as i32,
        x1 as i32,
        row.y as i32 + 1,
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

/// Collects, one subpath at a time in device space, the one-pixel bands the
/// thin-line rule adds to a fill.
struct ThinLines<'a> {
    edges: &'a mut Edges,
    points: &'a mut Vec<Point>,
}

impl ThinLines<'_> {
    fn flush(&mut self) {
        if let Some(band) = thin_band(self.points) {
            // Reversed: the same (negative) orientation as stroke pieces.
            self.edges.add_polygon(&band, &Transform::IDENTITY, true);
        }
        self.points.clear();
    }
}

impl FlattenSink for ThinLines<'_> {
    fn move_to(&mut self, p: Point) {
        self.flush();
        self.points.push(p);
    }

    fn line_to(&mut self, p: Point, _smooth: bool) {
        self.points.push(p);
    }

    fn close(&mut self) {}
}

/// The band a thin subpath gets: of the strips across `x`, across `y`, and
/// across the line from the first point to the farthest one, the thinnest
/// that is under one pixel, widened to exactly one pixel about its centre
/// and spanning the points along it. `None` when no strip is that thin or
/// all points coincide.
fn thin_band(pts: &[Point]) -> Option<[Point; 4]> {
    let &p0 = pts.first()?;
    let far = pts
        .iter()
        .copied()
        .max_by(|a, b| (*a - p0).length().total_cmp(&(*b - p0).length()))?;
    let reach = (far - p0).length();
    if !(reach > 0.0 && reach.is_finite()) {
        return None;
    }
    let mut best: Option<(f64, Point)> = None;
    for u in [
        Point::new(1.0, 0.0),
        Point::new(0.0, 1.0),
        (far - p0) * (1.0 / reach),
    ] {
        let (lo, hi) = extent(pts, p0, Point::new(-u.y, u.x));
        let width = hi - lo;
        if width < 1.0 && best.is_none_or(|(w, _)| width < w) {
            best = Some((width, u));
        }
    }
    let (_, u) = best?;
    let v = Point::new(-u.y, u.x);
    let (s0, s1) = extent(pts, p0, u);
    let (lo, hi) = extent(pts, p0, v);
    let mid = 0.5 * (lo + hi);
    let at = |s: f64, o: f64| p0 + u * s + v * o;
    Some([
        at(s0, mid - 0.5),
        at(s1, mid - 0.5),
        at(s1, mid + 0.5),
        at(s0, mid + 0.5),
    ])
}

/// Range of `(p - origin) · dir` over `pts`.
fn extent(pts: &[Point], origin: Point, dir: Point) -> (f64, f64) {
    pts.iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &p| {
            let t = (p - origin).dot(dir);
            (lo.min(t), hi.max(t))
        })
}

/// A drawing surface over an RGBA8 premultiplied pixmap.
///
/// Device space is the pixel grid: `x` right, `y` down, pixel `(i, j)`
/// covering `[i, i + 1] × [j, j + 1]`. Callers fold the PDF page transform
/// into the transforms they pass.
pub struct Canvas {
    base: Pixmap,
    layers: Vec<Layer>,
    group_bytes: usize,
    clips: Vec<Clip>,
    raster: Rasterizer,
    edges: Edges,
    cov: Mask,
    row_cov: Vec<u8>,
    row_shape: Vec<u8>,
    row_src: Vec<[u8; 4]>,
    thin_points: Vec<Point>,
    thin_cov: Mask,
    max_cov: Mask,
}

impl Canvas {
    /// A transparent canvas.
    pub fn new(width: u32, height: u32) -> Result<Canvas, RasterError> {
        Ok(Canvas::from_pixmap(Pixmap::new(width, height)?))
    }

    /// Draws onto an existing pixmap.
    pub fn from_pixmap(pixmap: Pixmap) -> Canvas {
        let full = pixmap.bounds();
        Canvas {
            base: pixmap,
            layers: Vec::new(),
            group_bytes: 0,
            clips: vec![Clip::from_rect(full.to_rect(), None, full)],
            raster: Rasterizer::default(),
            edges: Edges::default(),
            cov: Mask::default(),
            row_cov: Vec::new(),
            row_src: Vec::new(),
            row_shape: Vec::new(),
            thin_points: Vec::new(),
            thin_cov: Mask::default(),
            max_cov: Mask::default(),
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

    fn bounds(&self) -> IntRect {
        self.base.bounds()
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
        style: FillStyle,
        paint: &Paint<'_>,
    ) {
        self.edges.clear();
        self.edges.add_path(path, transform);
        let limit = self.top_clip().bounds;
        let mut cov = std::mem::take(&mut self.cov);
        let mut filled =
            self.raster
                .rasterize(&self.edges, limit, style.rule, paint.anti_alias, &mut cov);
        if style.thin_line {
            self.edges.clear();
            let mut sink = ThinLines {
                edges: &mut self.edges,
                points: &mut self.thin_points,
            };
            flatten(path, transform, DEVICE_TOLERANCE, &mut sink);
            sink.flush();
            if self.raster.rasterize(
                &self.edges,
                limit,
                FillRule::NonZero,
                paint.anti_alias,
                &mut self.thin_cov,
            ) {
                if filled {
                    max_into(&cov, &self.thin_cov, &mut self.max_cov);
                    std::mem::swap(&mut cov, &mut self.max_cov);
                } else {
                    std::mem::swap(&mut cov, &mut self.thin_cov);
                }
                filled = true;
            }
        }
        if filled {
            self.paint(&cov, paint.source, None, &paint.composite);
        }
        self.cov = cov;
    }

    /// Fills a rectangle given in user space.
    pub fn fill_rect(&mut self, rect: Rect, transform: &Transform, paint: &Paint<'_>) {
        self.fill_path(
            &Path::from_rect(rect),
            transform,
            FillStyle::default(),
            paint,
        );
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
        self.edges.clear();
        stroke_edges(path, transform, stroke, &mut self.edges);
        let limit = self.top_clip().bounds;
        let mut cov = std::mem::take(&mut self.cov);
        if self.raster.rasterize(
            &self.edges,
            limit,
            FillRule::NonZero,
            paint.anti_alias,
            &mut cov,
        ) {
            self.paint(&cov, paint.source, None, &paint.composite);
        }
        self.cov = cov;
    }

    /// Paints a precomputed coverage mask (for example a cached glyph).
    pub fn fill_mask(&mut self, mask: &Mask, paint: &Paint<'_>) {
        self.paint(mask, paint.source, None, &paint.composite);
    }

    /// Paints an opaque Gouraud triangle in device coordinates. Samples are
    /// taken at integer device points and RGB components are floored to bytes.
    /// Scanlines include their left/top edges and exclude right/bottom edges;
    /// adjoining triangles therefore own each sample exactly once, regardless
    /// of winding or draw order. Edges are not independently anti-aliased.
    pub fn fill_mesh_triangle(&mut self, points: [Point; 3], colors: [[f32; 3]; 3]) {
        if points.iter().any(|p| !p.is_finite()) {
            return;
        }
        let points = points.map(|p| Point::new(p.x.clamp(-1e300, 1e300), p.y.clamp(-1e300, 1e300)));
        let [a, b, c] = points;
        let ab = b - a;
        let ac = c - a;
        let scale = ab.x.abs().max(ab.y.abs()).max(ac.x.abs()).max(ac.y.abs());
        if scale == 0.0 {
            return;
        }
        let u = Point::new(ab.x / scale, ab.y / scale);
        let v = Point::new(ac.x / scale, ac.y / scale);
        let area = u.cross(v);
        if area == 0.0 {
            return;
        }
        let colors = colors.map(|rgb| rgb.map(|value| f64::from(crate::pixmap::unit(value))));
        let delta_b: [f64; 3] = std::array::from_fn(|k| colors[1][k] - colors[0][k]);
        let delta_c: [f64; 3] = std::array::from_fn(|k| colors[2][k] - colors[0][k]);
        let step: [f64; 3] =
            std::array::from_fn(|k| 255.0 * (v.y * delta_b[k] - u.y * delta_c[k]) / area / scale);
        let limit = self.top_clip().bounds.intersect(&self.bounds());
        let y0 = crate::geom::saturate_i32(a.y.min(b.y).min(c.y).ceil()).max(limit.y0);
        let y1 = crate::geom::saturate_i32(a.y.max(b.y).max(c.y).ceil()).min(limit.y1);
        let Canvas {
            base,
            layers,
            clips,
            row_cov,
            row_src,
            ..
        } = self;
        let clip = &clips[clips.len() - 1];
        for y in y0..y1 {
            let fy = f64::from(y);
            let mut left = f64::INFINITY;
            let mut right = f64::NEG_INFINITY;
            for i in 0..3 {
                let mut start = points[i];
                let mut end = points[(i + 1) % 3];
                if start.y > end.y {
                    std::mem::swap(&mut start, &mut end);
                }
                if fy >= start.y && fy < end.y {
                    // A shared edge always uses the same low-to-high endpoint
                    // order, so its rounded intersection is identical on both sides.
                    let t = (fy - start.y) / (end.y - start.y);
                    let x = (1.0 - t) * start.x + t * end.x;
                    left = left.min(x);
                    right = right.max(x);
                }
            }
            let x0 = crate::geom::saturate_i32(left.ceil()).max(limit.x0);
            let x1 = crate::geom::saturate_i32(right.ceil()).min(limit.x1);
            if x0 >= x1 {
                continue;
            }
            let point = Point::new((f64::from(x0) - a.x) / scale, (fy - a.y) / scale);
            let wb = point.cross(v) / area;
            let wc = u.cross(point) / area;
            let first: [f64; 3] =
                std::array::from_fn(|k| 255.0 * (colors[0][k] + wb * delta_b[k] + wc * delta_c[k]));
            let n = (x1 - x0) as usize;
            row_src.clear();
            for i in 0..n {
                let rgb: [u8; 3] = std::array::from_fn(|k| {
                    (first[k] + i as f64 * step[k]).clamp(0.0, 255.0) as u8
                });
                row_src.push([rgb[0], rgb[1], rgb[2], 255]);
            }
            row_cov.clear();
            row_cov.resize(n, 255);
            clip.apply(x0, y, row_cov);
            draw_row(
                base,
                layers,
                DrawRow {
                    x: x0 as u32,
                    y: y as u32,
                    source: SrcRow::Pixels(row_src.as_flattened()),
                    coverage: row_cov,
                    shape: row_cov,
                    mode: BlendMode::Normal,
                },
            );
        }
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
        self.paint(&cov, Source::Shader(&shader), None, &options.composite);
        self.cov = cov;
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
        self.paint(&cov, paint.source, Some(&stencil), &paint.composite);
        self.cov = cov;
    }

    fn unit_square_coverage(&mut self, t: &Transform) -> bool {
        self.edges.clear();
        let unit = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
        ];
        self.edges.add_polygon(&unit, t, false);
        let limit = self.top_clip().bounds;
        self.raster
            .rasterize(&self.edges, limit, FillRule::NonZero, true, &mut self.cov)
    }

    /// Intersects the clip with `path`.
    pub fn push_clip_path(&mut self, path: &Path, transform: &Transform, rule: FillRule) {
        if transform.is_axis_aligned()
            && let Some(r) = path.as_rect()
        {
            self.push_clip_rect(r, transform);
            return;
        }
        let top = self.top_clip().clone();
        self.edges.clear();
        self.edges.add_path(path, transform);
        let mut mask = Mask::default();
        if !self
            .raster
            .rasterize(&self.edges, top.bounds, rule, true, &mut mask)
        {
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
            layers,
            clips,
            row_cov,
            row_shape,
            row_src,
            ..
        } = self;
        let clip = &clips[clips.len() - 1];
        let region = layer.dirty.intersect(&clip.bounds);
        if region.is_empty() {
            return true;
        }
        let alpha = to_u8(composite.alpha);
        let n = region.width() as usize;
        for y in region.y0..region.y1 {
            let x0 = region.x0 as u32;
            let x1 = region.x1 as u32;
            let offset = y as usize * base.width() as usize + x0 as usize;
            let src = layer.pixmap.row(y as u32, x0, x1).as_chunks::<4>().0;
            let initial = initial_pixmap(base, layers, layer.initial)
                .map(|p| p.row(y as u32, x0, x1).as_chunks::<4>().0);
            row_src.clear();
            for (i, pixel) in src.iter().enumerate() {
                let mut source = *pixel;
                if let Some(initial) = initial {
                    let a = layer.alpha[offset + i];
                    for k in 0..3 {
                        let backdrop = mul255(u32::from(initial[i][k]), 255 - u32::from(a)) as u8;
                        source[k] = source[k].saturating_sub(backdrop).min(a);
                    }
                    source[3] = a;
                }
                row_src.push(source);
            }
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
                    y: y as u32,
                    source: SrcRow::Pixels(row_src.as_flattened()),
                    coverage: row_cov,
                    shape: row_shape,
                    mode: composite.blend_mode,
                },
            );
        }
        true
    }

    /// Number of open groups.
    pub fn group_depth(&self) -> usize {
        self.layers.len()
    }

    /// Composites `src` through coverage `cov`, the clip, an optional
    /// stencil, and the composite's alpha and soft mask.
    fn paint(
        &mut self,
        cov: &Mask,
        src: Source<'_>,
        stencil: Option<&Stencil<'_>>,
        composite: &Composite<'_>,
    ) {
        let alpha = to_u8(composite.alpha);
        let grouped = !self.layers.is_empty();
        if alpha == 0 && !grouped {
            return;
        }
        let Canvas {
            base,
            layers,
            clips,
            row_cov,
            row_shape,
            row_src,
            ..
        } = self;
        let clip = &clips[clips.len() - 1];
        let region = cov.rect().intersect(&clip.bounds).intersect(&base.bounds());
        if region.is_empty() {
            return;
        }
        let (solid, cov_alpha) = match src {
            Source::Solid(c) => {
                let p = c.to_premultiplied();
                (
                    Some(p.map(|v| mul255(u32::from(v), u32::from(alpha)) as u8)),
                    255,
                )
            }
            Source::Shader(_) => (None, alpha),
        };
        if !grouped && solid.is_some_and(|c| c[3] == 0) {
            return;
        }
        let n = region.width() as usize;
        let dx = (region.x0 - cov.rect().x0) as usize;
        for y in region.y0..region.y1 {
            let Some(row) = cov.row(y) else {
                continue;
            };
            row_cov.clear();
            row_cov.extend_from_slice(&row[dx..dx + n]);
            clip.apply(region.x0, y, row_cov);
            if let Some(st) = stencil {
                st.mul_row(region.x0, y, row_cov);
            }
            if grouped {
                row_shape.clear();
                row_shape.extend_from_slice(row_cov);
                if composite.alpha_is_shape {
                    scale_all(row_shape, u32::from(alpha));
                    if let Source::Solid(c) = src {
                        scale_all(row_shape, u32::from(c.to_premultiplied()[3]));
                    }
                    if let Some(mask) = composite.soft_mask {
                        mask.mul_row(region.x0, y, row_shape);
                    }
                }
            }
            if cov_alpha != 255 {
                scale_all(row_cov, u32::from(cov_alpha));
            }
            if let Some(mask) = composite.soft_mask {
                mask.mul_row(region.x0, y, row_cov);
            }
            let Some((a, b)) = nonzero_span(if grouped { row_shape } else { row_cov }) else {
                continue;
            };
            let x0 = (region.x0 + a as i32) as u32;
            let source = match (solid, src) {
                (Some(c), _) => SrcRow::Solid(c),
                (None, Source::Shader(s)) => {
                    row_src.clear();
                    row_src.resize(b - a, [0; 4]);
                    s.shade_row(x0 as i32, y, row_src);
                    if grouped && composite.alpha_is_shape {
                        for (shape, pixel) in row_shape[a..b].iter_mut().zip(row_src.iter()) {
                            *shape = mul255(u32::from(*shape), u32::from(pixel[3])) as u8;
                        }
                    }
                    SrcRow::Pixels(row_src.as_flattened())
                }
                (None, Source::Solid(_)) => continue,
            };
            draw_row(
                base,
                layers,
                DrawRow {
                    x: x0,
                    y: y as u32,
                    source,
                    coverage: &row_cov[a..b],
                    shape: if grouped { &row_shape[a..b] } else { &[] },
                    mode: composite.blend_mode,
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

/// Per-pixel maximum of two coverage masks over the union of their rects.
fn max_into(a: &Mask, b: &Mask, out: &mut Mask) {
    let rect = a.rect().union(&b.rect());
    out.reset(rect);
    let w = rect.width() as usize;
    if w == 0 {
        return;
    }
    for (ry, row) in out.data_mut().chunks_exact_mut(w).enumerate() {
        let y = rect.y0 + ry as i32;
        for (i, v) in row.iter_mut().enumerate() {
            let x = rect.x0 + i as i32;
            *v = a.value(x, y).max(b.value(x, y));
        }
    }
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
