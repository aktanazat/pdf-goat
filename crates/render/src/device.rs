use std::collections::HashMap;
use std::sync::Arc;

use pdf_core::{Matrix, Point, Rect};
use pdf_interp as interp;
use pdf_raster as raster;

use crate::RenderError;

const MAX_LAYER_BYTES: u64 = 1 << 30;
const MAX_NESTING: usize = 32;

fn limit(message: &str) -> RenderError {
    interp::InterpError::Limit(message.to_owned()).into()
}

fn transform(m: Matrix) -> raster::Transform {
    raster::Transform::new(m.a, m.b, m.c, m.d, m.e, m.f)
}

/// MuPDF 1.27.2 geometry.c `fz_transform_rect` (AGPL-3.0).
/// Bounds must round in the same float precision as the painted geometry.
fn rect(r: Rect, m: Matrix) -> raster::Rect {
    if r.is_empty() {
        return raster::Rect::new(0.0, 0.0, 0.0, 0.0);
    }
    let [a, b, c, d, e, f] = [m.a, m.b, m.c, m.d, m.e, m.f].map(|v| v as f32);
    let point = |x: f64, y: f64| {
        let (x, y) = (x as f32, y as f32);
        [x.mul_add(a, y * c) + e, x.mul_add(b, y * d) + f]
    };
    let (p, q) = (point(r.x0, r.y0), point(r.x1, r.y1));
    let mut bounds = [
        p[0].min(q[0]),
        p[1].min(q[1]),
        p[0].max(q[0]),
        p[1].max(q[1]),
    ];
    let axis = (b.abs() < f32::EPSILON && c.abs() < f32::EPSILON)
        || (a.abs() < f32::EPSILON && d.abs() < f32::EPSILON);
    if !axis {
        for p in [point(r.x0, r.y1), point(r.x1, r.y0)] {
            bounds[0] = bounds[0].min(p[0]);
            bounds[1] = bounds[1].min(p[1]);
            bounds[2] = bounds[2].max(p[0]);
            bounds[3] = bounds[3].max(p[1]);
        }
    }
    raster::Rect::new(
        bounds[0].into(),
        bounds[1].into(),
        bounds[2].into(),
        bounds[3].into(),
    )
}

fn path(source: &interp::Path) -> raster::Path {
    let mut out = raster::PathBuilder::new();
    for element in source.elements() {
        match *element {
            interp::PathEl::MoveTo(p) => {
                out.move_to(p.x, p.y);
            }
            interp::PathEl::LineTo(p) => {
                out.line_to(p.x, p.y);
            }
            interp::PathEl::CurveTo(a, b, p) => {
                out.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y);
            }
            interp::PathEl::Close => {
                out.close();
            }
        }
    }
    out.finish()
}

/// A glyph program's outline in font units, quadratics kept, with the
/// matrix to one-em glyph space.
pub(crate) struct Outline {
    pub(crate) units: raster::Path,
    pub(crate) to_em: raster::Transform,
    pub(crate) font: Arc<pdf_font::Font>,
    pub(crate) gid: u16,
}

fn glyph_outline(outline: &interp::GlyphOutline) -> Outline {
    Outline {
        units: glyph_path(&outline.ops),
        to_em: transform(outline.to_em),
        font: Arc::clone(&outline.font),
        gid: outline.gid,
    }
}

fn glyph_path(ops: &[interp::GlyphOp]) -> raster::Path {
    let mut out = raster::PathBuilder::new();
    for op in ops {
        match *op {
            interp::GlyphOp::MoveTo(x, y) => {
                out.move_to(f64::from(x), f64::from(y));
            }
            interp::GlyphOp::LineTo(x, y) => {
                out.line_to(f64::from(x), f64::from(y));
            }
            interp::GlyphOp::QuadTo(cx, cy, x, y) => {
                out.quad_to(f64::from(cx), f64::from(cy), f64::from(x), f64::from(y));
            }
            interp::GlyphOp::CurveTo(x1, y1, x2, y2, x, y) => {
                out.cubic_to(
                    f64::from(x1),
                    f64::from(y1),
                    f64::from(x2),
                    f64::from(y2),
                    f64::from(x),
                    f64::from(y),
                );
            }
            interp::GlyphOp::Close => {
                out.close();
            }
        }
    }
    out.finish()
}

fn rule(value: interp::FillRule) -> raster::FillRule {
    match value {
        interp::FillRule::NonZero => raster::FillRule::NonZero,
        interp::FillRule::EvenOdd => raster::FillRule::EvenOdd,
    }
}

fn blend(value: interp::BlendMode) -> raster::BlendMode {
    use interp::BlendMode as I;
    use raster::BlendMode as R;
    match value {
        I::Normal => R::Normal,
        I::Multiply => R::Multiply,
        I::Screen => R::Screen,
        I::Overlay => R::Overlay,
        I::Darken => R::Darken,
        I::Lighten => R::Lighten,
        I::ColorDodge => R::ColorDodge,
        I::ColorBurn => R::ColorBurn,
        I::HardLight => R::HardLight,
        I::SoftLight => R::SoftLight,
        I::Difference => R::Difference,
        I::Exclusion => R::Exclusion,
        I::Hue => R::Hue,
        I::Saturation => R::Saturation,
        I::Color => R::Color,
        I::Luminosity => R::Luminosity,
    }
}

fn stroke(value: &interp::StrokeStyle) -> raster::Stroke {
    raster::Stroke {
        width: value.width,
        cap: match value.cap {
            interp::LineCap::Butt => raster::LineCap::Butt,
            interp::LineCap::Round => raster::LineCap::Round,
            interp::LineCap::Square => raster::LineCap::Square,
        },
        join: match value.join {
            interp::LineJoin::Miter => raster::LineJoin::Miter,
            interp::LineJoin::Round => raster::LineJoin::Round,
            interp::LineJoin::Bevel => raster::LineJoin::Bevel,
        },
        miter_limit: value.miter_limit,
        dash: (!value.dash.is_empty()).then(|| raster::Dash {
            array: value.dash.clone(),
            phase: value.dash_phase,
        }),
    }
}

struct Group {
    alpha: f32,
    alpha_is_shape: bool,
    blend_mode: raster::BlendMode,
    mask: Option<Arc<raster::Mask>>,
}

pub(crate) struct RasterDevice {
    canvas: raster::Canvas,
    draw_text: bool,
    depth: usize,
    error: Option<RenderError>,
    glyphs: HashMap<(u64, u32), Arc<Outline>>,
    glyph_cache: raster::GlyphCache,
    native_glyphs: Option<crate::glyph::Glyphs>,
    masks: HashMap<u64, Arc<raster::Mask>>,
    mask_bytes: usize,
    groups: Vec<Group>,
    /// The next clip is a tiling cell's /BBox. MuPDF gives the cell pixmap
    /// that box rounded out to whole pixels and never antialiases it.
    cell_scissor: bool,
}

impl RasterDevice {
    pub(crate) fn new(
        width: u32,
        height: u32,
        alpha: bool,
        draw_text: bool,
    ) -> Result<Self, RenderError> {
        Self::new_at(raster::IntRect::from_size(width, height), alpha, draw_text)
    }

    fn new_at(bounds: raster::IntRect, alpha: bool, draw_text: bool) -> Result<Self, RenderError> {
        let mut canvas = raster::Canvas::new_at(bounds)?;
        if !alpha {
            canvas.clear(raster::Color::WHITE);
        }
        Ok(Self {
            canvas,
            draw_text,
            depth: 0,
            error: None,
            glyphs: HashMap::new(),
            glyph_cache: raster::GlyphCache::new(),
            native_glyphs: None,
            masks: HashMap::new(),
            mask_bytes: 0,
            groups: Vec::new(),
            cell_scissor: false,
        })
    }
    fn native_glyphs(&mut self) -> Result<&mut crate::glyph::Glyphs, RenderError> {
        if self.native_glyphs.is_none() {
            self.native_glyphs = Some(crate::glyph::Glyphs::new()?);
        }
        Ok(self.native_glyphs.as_mut().expect("initialized above"))
    }

    fn paint_glyph(
        &mut self,
        brush: interp::Brush<'_>,
        bitmap: raster::Glyph,
    ) -> Result<(), RenderError> {
        let r = bitmap.mask.rect();
        let placed = raster::IntRect::new(
            r.x0.saturating_add(bitmap.x),
            r.y0.saturating_add(bitmap.y),
            r.x1.saturating_add(bitmap.x),
            r.y1.saturating_add(bitmap.y),
        );
        self.paint(
            brush,
            |canvas| placed.intersect(&canvas.clip_bounds()),
            |canvas, paint| canvas.fill_mask_at(&bitmap.mask, bitmap.x, bitmap.y, paint),
        )
    }

    fn child(&self, bounds: raster::IntRect) -> Result<Self, RenderError> {
        let depth = self.depth + self.groups.len() + 1;
        if depth > MAX_NESTING
            || u64::from(bounds.width()) * u64::from(bounds.height()) * 4 * (depth as u64 + 1)
                > MAX_LAYER_BYTES
        {
            return Err(limit("rendering layers exceed the memory limit"));
        }
        let mut child = Self::new_at(bounds, true, true)?;
        child.depth = depth;
        Ok(child)
    }

    pub(crate) fn finish(self) -> Result<raster::Pixmap, RenderError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.canvas.finish()),
        }
    }

    fn apply(&mut self, draw: impl FnOnce(&mut Self) -> Result<(), RenderError>) {
        if self.error.is_none() {
            self.error = draw(self).err();
        }
    }

    fn mask(
        &mut self,
        mask: Option<&interp::SoftMask<'_>>,
    ) -> Result<Option<Arc<raster::Mask>>, RenderError> {
        let Some(mask) = mask else { return Ok(None) };
        if let Some(cached) = self.masks.get(&mask.key) {
            return Ok(Some(Arc::clone(cached)));
        }
        let bounds = self.canvas.bounds();
        let mut device = self.child(bounds)?;
        if mask.luminosity {
            device.canvas.clear(raster::Color::rgb(
                mask.backdrop[0],
                mask.backdrop[1],
                mask.backdrop[2],
            ));
        }
        mask.run(&mut device)?;
        let pixmap = device.finish()?;
        let mut coverage = if mask.luminosity {
            raster::Mask::from_luminosity(
                &pixmap,
                mask.backdrop
                    .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8),
            )
        } else {
            raster::Mask::from_alpha(&pixmap)
        };
        coverage.translate(bounds.x0, bounds.y0);
        if let Some(lut) = &mask.transfer {
            coverage.map_values(lut);
        }
        let coverage = Arc::new(coverage);
        if self.mask_bytes + coverage.data().len() <= 64 * 1024 * 1024 {
            self.mask_bytes += coverage.data().len();
            self.masks.insert(mask.key, Arc::clone(&coverage));
        }
        Ok(Some(coverage))
    }

    /// Paints with `brush` through `draw`. `shape` names the device pixels
    /// the drawn shape can reach; a shading brush is scissored to them.
    fn paint(
        &mut self,
        brush: interp::Brush<'_>,
        shape: impl FnOnce(&mut raster::Canvas) -> raster::IntRect,
        draw: impl FnOnce(&mut raster::Canvas, &raster::Paint<'_>),
    ) -> Result<(), RenderError> {
        let mask = self.mask(brush.soft_mask)?;
        let composite = raster::Composite {
            alpha: brush.alpha,
            alpha_is_shape: brush.alpha_is_shape,
            blend_mode: blend(brush.blend),
            soft_mask: mask.as_deref(),
        };
        match brush.paint {
            interp::Paint::Color([r, g, b]) => {
                let mut paint = raster::Paint::solid(raster::Color::rgb(r, g, b));
                paint.composite = composite;
                draw(&mut self.canvas, &paint);
            }
            interp::Paint::Shading(shading) => {
                let matrix = shading.matrix;
                let shape = shape(&mut self.canvas);
                self.shade(&shading.shading, matrix, composite, Some(shape), draw)?;
            }
            interp::Paint::Tiling(tile) => {
                let matrix = tile.matrix;
                let Some(inverse) = matrix.invert() else {
                    return Ok(());
                };
                let bounds = rect(tile.bbox, matrix).round_out();
                if bounds.is_empty() {
                    return Ok(());
                }
                let step_x = tile.x_step.abs();
                let step_y = tile.y_step.abs();
                if !step_x.is_finite() || !step_y.is_finite() || step_x == 0.0 || step_y == 0.0 {
                    return Err(limit("tiling pattern has an invalid step"));
                }
                if (tile.bbox.width() / step_x + 1.0) * (tile.bbox.height() / step_y + 1.0) > 1024.0
                {
                    return Err(limit("tiling pattern overlap exceeds the work limit"));
                }
                let mut image = self.child(bounds)?;
                image.cell_scissor = true;
                tile.run_cell(&mut image)?;
                let pixmap = image.finish()?;
                // Every device pixel a placed copy can reach, in pattern
                // space: the cell pixmap widened by the pixel its truncated
                // placement can move it.
                let reach = Rect::new(
                    f64::from(bounds.x0) - 1.0,
                    f64::from(bounds.y0) - 1.0,
                    f64::from(bounds.x1) + 1.0,
                    f64::from(bounds.y1) + 1.0,
                )
                .transform(&inverse);
                let shader = TileShader {
                    pixmap: &pixmap,
                    inverse,
                    matrix,
                    reach,
                    origin: bounds,
                    step_x,
                    step_y,
                    color: tile.color,
                };
                let mut paint = raster::Paint::shader(&shader);
                paint.composite = composite;
                draw(&mut self.canvas, &paint);
            }
        }
        Ok(())
    }

    /// Paints `shading` under `matrix` (shading space to device) as PyMuPDF
    /// does. The scissor is the clip bounds, cut to `shape` for a pattern
    /// paint and to the shading's bound when a clip is in force (MuPDF's
    /// display list shrinks a clip to what it contains). The background fills
    /// the scissor, the triangles paint the bound within it, and `draw`
    /// composites the result with `composite`.
    fn shade(
        &mut self,
        shading: &interp::Shading,
        matrix: Matrix,
        composite: raster::Composite<'_>,
        shape: Option<raster::IntRect>,
        draw: impl FnOnce(&mut raster::Canvas, &raster::Paint<'_>),
    ) -> Result<(), RenderError> {
        let mut scissor = self.canvas.clip_bounds();
        if let Some(shape) = shape {
            scissor = scissor.intersect(&shape);
        }
        let bound = shading.bound().map(|bound| rect(bound, matrix).round_out());
        if let Some(bound) = bound
            && (shape.is_some() || self.canvas.clip_depth() > 0)
        {
            scissor = scissor.intersect(&bound);
        }
        let clip = match bound {
            Some(bound) => bound.intersect(&scissor),
            None => scissor,
        };
        if clip.is_empty() {
            return Ok(());
        }
        let background = shading.background().map(|c| c.map(|v| (v * 255.0) as u8));
        let area = if background.is_some() { scissor } else { clip };
        let source = match shading.lut() {
            Some(lut) => raster::ShadeSource::Parameter(lut),
            None => raster::ShadeSource::Rgb,
        };
        let mut painter = raster::ShadePainter::new(area, clip, source)?;
        let scissor = Rect::new(
            f64::from(clip.x0),
            f64::from(clip.y0),
            f64::from(clip.x1),
            f64::from(clip.y1),
        );
        shading.triangles(&matrix, scissor, &mut |triangle| {
            painter.triangle(triangle.map(|v| raster::ShadeVertex {
                x: v.x,
                y: v.y,
                value: v.value,
            }));
        });
        let pixmap = painter.finish(background);
        let placement = raster::Transform::translate(f64::from(area.x0), f64::from(area.y0));
        if let Some(shader) =
            raster::PixmapShader::new(&pixmap, &placement, raster::Filter::Nearest, false)
        {
            let mut paint = raster::Paint::shader(&shader);
            paint.composite = composite;
            draw(&mut self.canvas, &paint);
        }
        Ok(())
    }
}

/// Paints copies of one rendered cell the way MuPDF's `fz_draw_end_tile`
/// does: the cell pixmap covers the cell's rounded-out device box, and each
/// copy is placed at the truncated device offset of its step, so a cell edge
/// that ends inside a pixel keeps that pixel's partial coverage.
struct TileShader<'a> {
    pixmap: &'a raster::Pixmap,
    inverse: Matrix,
    matrix: Matrix,
    reach: Rect,
    origin: raster::IntRect,
    step_x: f64,
    step_y: f64,
    color: Option<interp::Rgb>,
}

impl raster::Shader for TileShader<'_> {
    fn shade_row(&self, x: i32, y: i32, out: &mut [[u8; 4]]) {
        for (i, pixel) in out.iter_mut().enumerate() {
            *pixel = [0; 4];
            let px = f64::from(x) + i as f64;
            let p = Point::new(px + 0.5, f64::from(y) + 0.5).transform(&self.inverse);
            let k0 = ((p.x - self.reach.x1) / self.step_x).ceil() as i64;
            let k1 = ((p.x - self.reach.x0) / self.step_x).floor() as i64;
            let j0 = ((p.y - self.reach.y1) / self.step_y).ceil() as i64;
            let j1 = ((p.y - self.reach.y0) / self.step_y).floor() as i64;
            for j in j0..=j1 {
                for k in k0..=k1 {
                    // MuPDF: `fz_pre_translate(ctm, x * xstep, y * ystep)` in
                    // single precision, then the pixmap origin takes the
                    // integer part (60 × 4.1666665 lands at 249, not 250).
                    let kx = k as f32 * self.step_x as f32;
                    let jy = j as f32 * self.step_y as f32;
                    let placed_x = (self.origin.x0 as f32
                        + kx * self.matrix.a as f32
                        + jy * self.matrix.c as f32)
                        .trunc();
                    let placed_y = (self.origin.y0 as f32
                        + kx * self.matrix.b as f32
                        + jy * self.matrix.d as f32)
                        .trunc();
                    let qx = px - f64::from(placed_x);
                    let qy = f64::from(y) - f64::from(placed_y);
                    if qx < 0.0 || qy < 0.0 {
                        continue;
                    }
                    if let Some(mut source) = self.pixmap.pixel(qx as u32, qy as u32) {
                        if let Some(color) = self.color {
                            let opacity = f32::from(source[3]);
                            for (channel, component) in source[..3].iter_mut().zip(color) {
                                *channel = (component.clamp(0.0, 1.0) * opacity).round() as u8;
                            }
                        }
                        let inverse_alpha = 255 - u16::from(source[3]);
                        for (target, value) in pixel.iter_mut().zip(source) {
                            *target = (u16::from(value)
                                + (u16::from(*target) * inverse_alpha + 127) / 255)
                                .min(255) as u8;
                        }
                    }
                }
            }
        }
    }
}

impl interp::Device for RasterDevice {
    fn fill_path(&mut self, source: &interp::Path, event: &interp::FillEvent<'_>) {
        self.apply(|this| {
            let path = path(source);
            let ctm = transform(event.ctm);
            // MuPDF paints a pattern through the fill path as a clip, and a
            // rectangular clip is a whole-pixel scissor.
            let scissor = match event.brush.paint {
                interp::Paint::Color(_) => None,
                _ => this.canvas.fill_scissor(&path, &ctm),
            };
            let rule = rule(event.rule);
            this.paint(
                event.brush,
                |canvas| scissor.unwrap_or_else(|| canvas.fill_bounds(&path, &ctm)),
                |canvas, paint| match scissor {
                    Some(rect) => {
                        canvas.fill_rect(rect.to_rect(), &raster::Transform::IDENTITY, paint)
                    }
                    None => canvas.fill_path(&path, &ctm, rule, paint),
                },
            )
        });
    }

    fn stroke_path(&mut self, source: &interp::Path, event: &interp::StrokeEvent<'_>) {
        self.apply(|this| {
            let path = path(source);
            let ctm = transform(event.ctm);
            let style = stroke(event.style);
            this.paint(
                event.brush,
                |canvas| canvas.stroke_bounds(&path, &ctm, &style),
                |canvas, paint| canvas.stroke_path(&path, &ctm, &style, paint),
            )
        });
    }

    fn clip_path(&mut self, source: &interp::Path, event: &interp::ClipEvent<'_>) {
        self.apply(|this| {
            let path = path(source);
            let ctm = transform(event.ctm);
            if std::mem::take(&mut this.cell_scissor) {
                let scissor = this.canvas.fill_bounds(&path, &ctm);
                this.canvas
                    .push_clip_rect(scissor.to_rect(), &raster::Transform::IDENTITY);
            } else {
                this.canvas.push_clip_path(&path, &ctm, rule(event.rule));
            }
            Ok(())
        });
    }

    fn pop_clip(&mut self) {
        self.canvas.pop_clip();
    }

    fn text(&mut self, run: &interp::TextRun<'_>) {
        self.apply(|this| {
            if !this.draw_text
                || run.font.subtype() == interp::FontSubtype::Type3
                || (run.fill.is_none() && run.stroke.is_none())
            {
                return Ok(());
            }
            for glyph in run.glyphs {
                let key = (run.font.id(), glyph.gid);
                let outline = match this.glyphs.get(&key) {
                    Some(outline) => Arc::clone(outline),
                    None => {
                        let Some(outline) = run.font.glyph_outline(glyph.gid) else {
                            continue;
                        };
                        let outline = Arc::new(glyph_outline(&outline));
                        this.glyphs.insert(key, Arc::clone(&outline));
                        outline
                    }
                };
                if let Some(brush) = run.fill {
                    let ctm = transform(glyph.trm);
                    // Glyphs up to MuPDF's bitmap size come from the glyph
                    // cache at a quantised subpixel position; larger ones
                    // are filled as paths.
                    match this
                        .glyph_cache
                        .glyph(key.0, key.1, &outline.units, &outline.to_em, &ctm)
                    {
                        Some(bitmap) => this.paint_glyph(brush, bitmap)?,
                        None => {
                            let tm = transform(glyph.user_trm);
                            let path = this.native_glyphs()?.outline(key.0, &outline, &tm)?;
                            let ctm = transform(run.ctm);
                            this.paint(
                                brush,
                                |canvas| canvas.fill_bounds(&path, &ctm),
                                |canvas, paint| {
                                    canvas.fill_path(&path, &ctm, raster::FillRule::NonZero, paint)
                                },
                            )?;
                        }
                    }
                }
                if let Some(brush) = run.stroke {
                    let ctm = transform(run.ctm);
                    let style = stroke(run.stroke_style);
                    if style.dash.as_ref().is_none_or(|dash| dash.array.is_empty()) {
                        let trm = transform(glyph.trm);
                        let bitmap = this
                            .native_glyphs()?
                            .stroke(key.0, &outline, &trm, &ctm, &style)?;
                        this.paint_glyph(brush, bitmap)?;
                    } else {
                        let tm = transform(glyph.user_trm);
                        let path = this.native_glyphs()?.outline(key.0, &outline, &tm)?;
                        this.paint(
                            brush,
                            |canvas| canvas.stroke_bounds(&path, &ctm, &style),
                            |canvas, paint| canvas.stroke_path(&path, &ctm, &style, paint),
                        )?;
                    }
                }
            }
            Ok(())
        });
    }

    fn fill_shading(&mut self, event: &interp::ShadingEvent<'_>) {
        self.apply(|this| {
            let matrix = event.matrix;
            let mask = this.mask(event.soft_mask)?;
            let composite = raster::Composite {
                alpha: event.alpha,
                alpha_is_shape: event.alpha_is_shape,
                blend_mode: blend(event.blend),
                soft_mask: mask.as_deref(),
            };
            let bounds = this.canvas.clip_bounds().to_rect();
            this.shade(event.shading, matrix, composite, None, |canvas, paint| {
                canvas.fill_rect(bounds, &raster::Transform::IDENTITY, paint);
            })
        });
    }

    fn fill_image(&mut self, event: &interp::ImageEvent<'_>) {
        self.apply(|this| {
            let pixels = event.image.decode_rgb()?;
            let image = raster::Image::new(
                pixels.width,
                pixels.height,
                raster::ImageFormat::Rgb8,
                &pixels.rgb,
            )?;
            let alpha = pixels
                .alpha
                .as_ref()
                .map(|alpha| raster::MaskImage::new(alpha.width, alpha.height, &alpha.data))
                .transpose()?;
            let mask = this.mask(event.soft_mask)?;
            let ctm = Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 1.0).concat(&event.ctm);
            this.canvas.draw_image(
                &image,
                &transform(ctm),
                &raster::ImageOptions {
                    filter: if event.image.interpolate() {
                        raster::Filter::Bilinear
                    } else {
                        raster::Filter::Nearest
                    },
                    soft_mask: alpha,
                    composite: raster::Composite {
                        alpha: event.alpha,
                        alpha_is_shape: event.alpha_is_shape,
                        blend_mode: blend(event.blend),
                        soft_mask: mask.as_deref(),
                    },
                },
            );
            Ok(())
        });
    }

    fn fill_image_mask(&mut self, event: &interp::ImageMaskEvent<'_>) {
        self.apply(|this| {
            let pixels = event.image.decode_stencil()?;
            let mask = raster::MaskImage::new(pixels.width, pixels.height, &pixels.data)?;
            let ctm = transform(Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 1.0).concat(&event.ctm));
            let filter = if event.image.interpolate() {
                raster::Filter::Bilinear
            } else {
                raster::Filter::Nearest
            };
            this.paint(
                event.brush,
                |canvas| {
                    canvas.fill_bounds(
                        &raster::Path::from_rect(raster::Rect::new(0.0, 0.0, 1.0, 1.0)),
                        &ctm,
                    )
                },
                |canvas, paint| canvas.draw_stencil(&mask, &ctm, filter, paint),
            )
        });
    }

    fn begin_group(&mut self, event: &interp::GroupEvent<'_>) {
        self.apply(|this| {
            let layers = this.depth + this.groups.len() + 1;
            let bytes = u64::from(this.canvas.width()) * u64::from(this.canvas.height()) * 4;
            if layers > MAX_NESTING || bytes * (layers as u64 + 1) > MAX_LAYER_BYTES {
                return Err(limit("transparency groups exceed the memory limit"));
            }
            let mask = this.mask(event.soft_mask)?;
            this.canvas.begin_group(event.isolated, event.knockout)?;
            this.groups.push(Group {
                alpha: event.alpha,
                alpha_is_shape: event.alpha_is_shape,
                blend_mode: blend(event.blend),
                mask,
            });
            Ok(())
        });
    }

    fn end_group(&mut self) {
        self.apply(|this| {
            if let Some(group) = this.groups.pop() {
                this.canvas.pop_group(&raster::Composite {
                    alpha: group.alpha,
                    alpha_is_shape: group.alpha_is_shape,
                    blend_mode: group.blend_mode,
                    soft_mask: group.mask.as_deref(),
                });
            }
            Ok(())
        });
    }

    fn wants_type3_procs(&self) -> bool {
        self.draw_text
    }
}

#[cfg(test)]
mod tests {
    use pdf_core::{Dict, Document, Name, Rect, Stream};
    use pdf_interp::{Interpreter, RunOptions};

    use super::RasterDevice;

    /// Renders `content` with Helvetica as `/F` and returns the number of
    /// glyph bitmaps that were rasterized.
    fn glyph_renders(content: &[u8]) -> u64 {
        let mut doc = Document::new();
        let page = doc
            .insert_blank_page(0, Rect::new(0.0, 0.0, 200.0, 40.0))
            .unwrap();
        let mut font = Dict::new();
        font.insert("Type", Name::from("Font"));
        font.insert("Subtype", Name::from("Type1"));
        font.insert("BaseFont", Name::from("Helvetica"));
        let mut fonts = Dict::new();
        fonts.insert("F", font);
        let mut resources = Dict::new();
        resources.insert("Font", fonts);
        let content = doc.add(Stream::new(Dict::new(), content.to_vec()));
        let mut dict = doc.get(page).unwrap().as_dict().unwrap().clone();
        dict.insert("Contents", content);
        dict.insert("Resources", resources);
        doc.set(page, dict);
        let mut device = RasterDevice::new(200, 40, false, true).unwrap();
        Interpreter::new()
            .run_page(
                &doc,
                &doc.page(0).unwrap(),
                &mut device,
                &RunOptions::default(),
            )
            .unwrap();
        assert!(device.error.is_none());
        device.glyph_cache.renders()
    }

    /// The same glyph at the same size and subpixel phase is rasterized once
    /// per page, however many times it is drawn.
    #[test]
    fn repeated_glyphs_share_one_bitmap() {
        let mut content = b"BT /F 12 Tf ".to_vec();
        for i in 0..40 {
            content.extend_from_slice(format!("1 0 0 1 {} 20 Tm (H) Tj ", 4 * i).as_bytes());
        }
        content.extend_from_slice(b"ET");
        assert_eq!(glyph_renders(&content), 1);
    }

    /// Quarter-pixel phases along the baseline are distinct bitmaps (text
    /// under 24 px), so four phases of one glyph rasterize four times.
    #[test]
    fn subpixel_phases_are_separate_bitmaps() {
        assert_eq!(
            glyph_renders(
                b"BT /F 12 Tf 1 0 0 1 10 20 Tm (H) Tj 1 0 0 1 20.25 20 Tm (H) Tj \
                  1 0 0 1 30.5 20 Tm (H) Tj 1 0 0 1 40.75 20 Tm (H) Tj \
                  1 0 0 1 50 20 Tm (H) Tj 1 0 0 1 60.25 20 Tm (H) Tj ET"
            ),
            4
        );
    }
}
