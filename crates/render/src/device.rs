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

fn rect(r: Rect) -> raster::Rect {
    raster::Rect::new(r.x0, r.y0, r.x1, r.y1)
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
    post: Matrix,
    draw_text: bool,
    depth: usize,
    error: Option<RenderError>,
    glyphs: HashMap<(u64, u32), Arc<raster::Path>>,
    masks: HashMap<u64, Arc<raster::Mask>>,
    mask_bytes: usize,
    groups: Vec<Group>,
}

impl RasterDevice {
    pub(crate) fn new(
        width: u32,
        height: u32,
        alpha: bool,
        draw_text: bool,
    ) -> Result<Self, RenderError> {
        let mut canvas = raster::Canvas::new(width, height)?;
        if !alpha {
            canvas.clear(raster::Color::WHITE);
        }
        Ok(Self {
            canvas,
            post: Matrix::IDENTITY,
            draw_text,
            depth: 0,
            error: None,
            glyphs: HashMap::new(),
            masks: HashMap::new(),
            mask_bytes: 0,
            groups: Vec::new(),
        })
    }

    fn child(&self, width: u32, height: u32, post: Matrix) -> Result<Self, RenderError> {
        let depth = self.depth + self.groups.len() + 1;
        if depth > MAX_NESTING
            || u64::from(width) * u64::from(height) * 4 * (depth as u64 + 1) > MAX_LAYER_BYTES
        {
            return Err(limit("rendering layers exceed the memory limit"));
        }
        let mut child = Self::new(width, height, true, true)?;
        child.depth = depth;
        child.post = post;
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
        let mut device = self.child(self.canvas.width(), self.canvas.height(), self.post)?;
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

    fn paint(
        &mut self,
        brush: interp::Brush<'_>,
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
                let matrix = shading.matrix.concat(&self.post);
                if shading.shading.mesh().is_empty() {
                    if let Some(inverse) = matrix.invert() {
                        let shader = raster::FnShader(|x, y| {
                            shading_color(&shading.shading, Point::new(x, y).transform(&inverse))
                        });
                        let mut paint = raster::Paint::shader(&shader);
                        paint.composite = composite;
                        draw(&mut self.canvas, &paint);
                    }
                } else {
                    let mut image =
                        self.child(self.canvas.width(), self.canvas.height(), Matrix::IDENTITY)?;
                    image.mesh(&shading.shading, matrix);
                    let pixmap = image.finish()?;
                    if let Some(shader) = raster::PixmapShader::new(
                        &pixmap,
                        &raster::Transform::IDENTITY,
                        raster::Filter::Nearest,
                        false,
                    ) {
                        let mut paint = raster::Paint::shader(&shader);
                        paint.composite = composite;
                        draw(&mut self.canvas, &paint);
                    }
                }
            }
            interp::Paint::Tiling(tile) => {
                let matrix = tile.matrix.concat(&self.post);
                let Some(inverse) = matrix.invert() else {
                    return Ok(());
                };
                let bounds = rect(tile.bbox.transform(&matrix)).round_out();
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
                let origin = Matrix::translate(-f64::from(bounds.x0), -f64::from(bounds.y0));
                let mut image =
                    self.child(bounds.width(), bounds.height(), self.post.concat(&origin))?;
                tile.run_cell(&mut image)?;
                let pixmap = image.finish()?;
                let shader = TileShader {
                    pixmap: &pixmap,
                    inverse,
                    matrix,
                    bbox: tile.bbox,
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

    fn mesh(&mut self, shading: &interp::Shading, matrix: Matrix) {
        let depth = self.canvas.clip_depth();
        if let Some(bbox) = shading.bbox() {
            self.canvas.push_clip_rect(rect(bbox), &transform(matrix));
        }
        if let Some([r, g, b]) = shading.background() {
            self.canvas.fill_rect(
                self.canvas.clip_bounds().to_rect(),
                &raster::Transform::IDENTITY,
                &raster::Paint::solid(raster::Color::rgb(r, g, b)),
            );
        }
        for triangle in shading.mesh() {
            let points = triangle.points.map(|p| {
                let p = p.transform(&matrix);
                raster::Point::new(p.x, p.y)
            });
            self.canvas.fill_mesh_triangle(points, triangle.colors);
        }
        self.canvas.restore_clip_depth(depth);
    }
}

fn shading_color(shading: &interp::Shading, point: Point) -> raster::Color {
    if shading.bbox().is_some_and(|bbox| !bbox.contains(point)) {
        return raster::Color::TRANSPARENT;
    }
    match shading.sample(point).or_else(|| shading.background()) {
        Some([r, g, b]) => raster::Color::rgb(r, g, b),
        None => raster::Color::TRANSPARENT,
    }
}

struct TileShader<'a> {
    pixmap: &'a raster::Pixmap,
    inverse: Matrix,
    matrix: Matrix,
    bbox: Rect,
    origin: raster::IntRect,
    step_x: f64,
    step_y: f64,
    color: Option<interp::Rgb>,
}

impl raster::Shader for TileShader<'_> {
    fn shade_row(&self, x: i32, y: i32, out: &mut [[u8; 4]]) {
        for (i, pixel) in out.iter_mut().enumerate() {
            *pixel = [0; 4];
            let p = Point::new(f64::from(x) + i as f64 + 0.5, f64::from(y) + 0.5)
                .transform(&self.inverse);
            let k0 = ((p.x - self.bbox.x1) / self.step_x).ceil() as i64;
            let k1 = ((p.x - self.bbox.x0) / self.step_x).floor() as i64;
            let j0 = ((p.y - self.bbox.y1) / self.step_y).ceil() as i64;
            let j1 = ((p.y - self.bbox.y0) / self.step_y).floor() as i64;
            for j in j0..=j1 {
                for k in k0..=k1 {
                    let q = Point::new(p.x - k as f64 * self.step_x, p.y - j as f64 * self.step_y)
                        .transform(&self.matrix);
                    let qx = (q.x - f64::from(self.origin.x0)).floor();
                    let qy = (q.y - f64::from(self.origin.y0)).floor();
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
            let ctm = transform(event.ctm.concat(&this.post));
            this.paint(event.brush, |canvas, paint| {
                canvas.fill_path(
                    &path,
                    &ctm,
                    raster::FillStyle {
                        rule: rule(event.rule),
                        thin_line: true,
                    },
                    paint,
                )
            })
        });
    }

    fn stroke_path(&mut self, source: &interp::Path, event: &interp::StrokeEvent<'_>) {
        self.apply(|this| {
            let path = path(source);
            let ctm = transform(event.ctm.concat(&this.post));
            let style = stroke(event.style);
            this.paint(event.brush, |canvas, paint| {
                canvas.stroke_path(&path, &ctm, &style, paint)
            })
        });
    }

    fn clip_path(&mut self, source: &interp::Path, event: &interp::ClipEvent<'_>) {
        self.apply(|this| {
            this.canvas.push_clip_path(
                &path(source),
                &transform(event.ctm.concat(&this.post)),
                rule(event.rule),
            );
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
                        let Some(source) = run.font.glyph_path(glyph.gid) else {
                            continue;
                        };
                        let outline = Arc::new(path(&source));
                        this.glyphs.insert(key, Arc::clone(&outline));
                        outline
                    }
                };
                if let Some(brush) = run.fill {
                    let ctm = transform(glyph.trm.concat(&this.post));
                    this.paint(brush, |canvas, paint| {
                        canvas.fill_path(&outline, &ctm, raster::FillStyle::default(), paint)
                    })?;
                }
                if let Some(brush) = run.stroke
                    && let Some(inverse) = run.ctm.invert()
                {
                    let user_outline = outline.transform(&transform(glyph.trm.concat(&inverse)));
                    let ctm = transform(run.ctm.concat(&this.post));
                    let style = stroke(run.stroke_style);
                    this.paint(brush, |canvas, paint| {
                        canvas.stroke_path(&user_outline, &ctm, &style, paint)
                    })?;
                }
            }
            Ok(())
        });
    }

    fn fill_shading(&mut self, event: &interp::ShadingEvent<'_>) {
        self.apply(|this| {
            let matrix = event.matrix.concat(&this.post);
            let mask = this.mask(event.soft_mask)?;
            let composite = raster::Composite {
                alpha: event.alpha,
                alpha_is_shape: event.alpha_is_shape,
                blend_mode: blend(event.blend),
                soft_mask: mask.as_deref(),
            };
            let bounds = this.canvas.clip_bounds().to_rect();
            if event.shading.mesh().is_empty() {
                if let Some(inverse) = matrix.invert() {
                    let shader = raster::FnShader(|x, y| {
                        shading_color(event.shading, Point::new(x, y).transform(&inverse))
                    });
                    let mut paint = raster::Paint::shader(&shader);
                    paint.composite = composite;
                    this.canvas
                        .fill_rect(bounds, &raster::Transform::IDENTITY, &paint);
                }
            } else {
                let mut image =
                    this.child(this.canvas.width(), this.canvas.height(), Matrix::IDENTITY)?;
                image.mesh(event.shading, matrix);
                let pixmap = image.finish()?;
                if let Some(shader) = raster::PixmapShader::new(
                    &pixmap,
                    &raster::Transform::IDENTITY,
                    raster::Filter::Nearest,
                    false,
                ) {
                    let mut paint = raster::Paint::shader(&shader);
                    paint.composite = composite;
                    this.canvas
                        .fill_rect(bounds, &raster::Transform::IDENTITY, &paint);
                }
            }
            Ok(())
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
            let ctm = Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 1.0)
                .concat(&event.ctm)
                .concat(&this.post);
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
            let ctm = transform(
                Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 1.0)
                    .concat(&event.ctm)
                    .concat(&this.post),
            );
            let filter = if event.image.interpolate() {
                raster::Filter::Bilinear
            } else {
                raster::Filter::Nearest
            };
            this.paint(event.brush, |canvas, paint| {
                canvas.draw_stencil(&mask, &ctm, filter, paint)
            })
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
