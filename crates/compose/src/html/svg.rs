//! SVG raster appearance and searchable labels from the same resolved glyph layout.

use std::collections::HashMap;
use std::sync::Arc;

use resvg::{tiny_skia, usvg};
use tiny_skia::{Mask, Path, PathBuilder, Transform};
use url::Url;

use super::assets::Assets;
use super::fonts::{FaceId, Fonts, system_fonts};
use super::image::{Image, Pixels};
use super::shaping::Unicode;

pub struct Glyph {
    pub face: FaceId,
    pub gid: u16,
    pub unicode: Unicode,
    /// PDF em coordinates to the image's unit square, with its origin at the top left.
    pub transform: Transform,
}

fn tree(bytes: &[u8], assets: &Assets, base: &Url, depth: u8) -> Option<usvg::Tree> {
    if depth >= 16 {
        return None;
    }
    let resolver = usvg::ImageHrefResolver {
        resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
        resolve_string: Box::new(move |href, _options| {
            let asset = assets.fetch(base, href)?;
            let bytes = &asset.bytes;
            match pdf_codec::detect_format(bytes) {
                Some(pdf_codec::ImageFormat::Png) => {
                    Some(usvg::ImageKind::PNG(Arc::new(bytes.clone())))
                }
                Some(pdf_codec::ImageFormat::Jpeg) => {
                    Some(usvg::ImageKind::JPEG(Arc::new(bytes.clone())))
                }
                Some(pdf_codec::ImageFormat::WebP) => {
                    Some(usvg::ImageKind::WEBP(Arc::new(bytes.clone())))
                }
                _ if bytes.starts_with(b"GIF8") => {
                    Some(usvg::ImageKind::GIF(Arc::new(bytes.clone())))
                }
                _ => tree(bytes, assets, &asset.url, depth + 1).map(usvg::ImageKind::SVG),
            }
        }),
    };
    let options = usvg::Options {
        fontdb: system_fonts(),
        image_href_resolver: resolver,
        ..usvg::Options::default()
    };
    usvg::Tree::from_data(bytes, &options).ok()
}

pub fn rasterize(bytes: &[u8], assets: &Assets, base: &Url, fonts: &mut Fonts) -> Option<Image> {
    let tree = tree(bytes, assets, base, 0)?;
    let size = tree.size();
    let width = (size.width() * 2.0).ceil() as u32;
    let height = (size.height() * 2.0).ceil() as u32;
    let count = width.checked_mul(height)?;
    if count > 16_777_216 {
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    let transform =
        Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut labels = Labels {
        fonts,
        glyphs: Vec::new(),
        outlines: HashMap::new(),
        width,
        height,
        raster: transform,
        normalized: Transform::from_scale(1.0 / size.width(), 1.0 / size.height()),
    };
    let scope = Scope {
        database: tree.fontdb(),
        outer: Transform::identity(),
        mask: None,
        mask_bytes: 0,
    };
    labels.group(tree.root(), scope, 0)?;
    let mut samples = Vec::with_capacity(count as usize * 3);
    let mut alpha = Vec::with_capacity(count as usize);
    for pixel in pixmap.pixels() {
        let pixel = pixel.demultiply();
        samples.extend_from_slice(&[pixel.red(), pixel.green(), pixel.blue()]);
        alpha.push(pixel.alpha());
    }
    let alpha = alpha.iter().any(|&a| a != 255).then_some(alpha);
    Some(Image {
        width,
        height,
        intrinsic: (size.width(), size.height()),
        pixels: Pixels::Raw {
            samples,
            gray: false,
            alpha,
        },
        text: labels.glyphs,
    })
}

struct Labels<'a> {
    fonts: &'a mut Fonts,
    glyphs: Vec<Glyph>,
    outlines: HashMap<(FaceId, u16), Option<Path>>,
    width: u32,
    height: u32,
    raster: Transform,
    normalized: Transform,
}

#[derive(Clone, Copy)]
struct Scope<'a> {
    database: &'a fontdb::Database,
    outer: Transform,
    mask: Option<&'a Mask>,
    mask_bytes: usize,
}

impl Labels<'_> {
    fn group(&mut self, group: &usvg::Group, scope: Scope<'_>, depth: u16) -> Option<()> {
        if depth >= 256 {
            return None;
        }
        if group.opacity().get() == 0.0 {
            return Some(());
        }
        let has_mask = group.clip_path().is_some() || group.mask().is_some();
        let mask_bytes = scope.mask_bytes
            + if has_mask {
                self.width as usize * self.height as usize
            } else {
                0
            };
        if mask_bytes > 32 * 1024 * 1024 {
            return None;
        }
        let transform = self
            .raster
            .pre_concat(scope.outer)
            .pre_concat(group.abs_transform());
        let mut own = None;
        if let Some(clip) = group.clip_path() {
            own = Some(clip_mask(clip, transform, self.width, self.height)?);
        }
        if let Some(mask) = group.mask() {
            let mask = image_mask(mask, transform, self.width, self.height)?;
            if let Some(own) = &mut own {
                intersect(own, &mask)
            } else {
                own = Some(mask)
            }
        }
        if let (Some(own), Some(parent)) = (&mut own, scope.mask) {
            intersect(own, parent);
        }
        if own
            .as_ref()
            .is_some_and(|mask| mask.data().iter().all(|&alpha| alpha == 0))
        {
            return Some(());
        }
        let scope = Scope {
            mask: own.as_ref().or(scope.mask),
            mask_bytes,
            ..scope
        };
        for node in group.children() {
            match node {
                usvg::Node::Group(child) => self.group(child, scope, depth + 1)?,
                usvg::Node::Text(text) => self.text(text, scope.database, scope.outer, scope.mask),
                usvg::Node::Image(image) if image.is_visible() => {
                    if let usvg::ImageKind::SVG(tree) = image.kind() {
                        let nested = Scope {
                            database: tree.fontdb(),
                            outer: scope.outer.pre_concat(image.abs_transform()),
                            ..scope
                        };
                        self.group(tree.root(), nested, depth + 1)?;
                    }
                }
                _ => {}
            }
        }
        Some(())
    }

    fn text(
        &mut self,
        text: &usvg::Text,
        database: &fontdb::Database,
        outer: Transform,
        mask: Option<&Mask>,
    ) {
        let world = outer.pre_concat(text.abs_transform());
        for span in text.layouted() {
            let fill = span
                .fill
                .as_ref()
                .is_some_and(|fill| fill.opacity().get() > 0.0 && painted(fill.paint()));
            let stroke = span
                .stroke
                .as_ref()
                .filter(|stroke| stroke.opacity().get() > 0.0 && painted(stroke.paint()));
            if !span.visible || (!fill && stroke.is_none()) {
                continue;
            }
            for glyph in &span.positioned_glyphs {
                let Ok(gid) = u16::try_from(glyph.id.0) else {
                    continue;
                };
                let Some(face) = self.fonts.load_database(database, glyph.font) else {
                    continue;
                };
                let font = &self.fonts.faces[face];
                let outline = self
                    .outlines
                    .entry((face, gid))
                    .or_insert_with(|| outline(&font.font, gid));
                let local = glyph.outline_transform();
                let raster = self.raster.pre_concat(world);
                if !visible(
                    outline.as_ref(),
                    local,
                    raster,
                    mask,
                    (self.width, self.height),
                    fill,
                    stroke,
                ) {
                    continue;
                }
                self.glyphs.push(Glyph {
                    face,
                    gid,
                    unicode: Unicode::from_text(&glyph.text),
                    transform: self
                        .normalized
                        .pre_concat(world)
                        .pre_concat(local)
                        .pre_scale(font.units_per_em, font.units_per_em),
                });
            }
        }
    }
}

fn painted(paint: &usvg::Paint) -> bool {
    match paint {
        usvg::Paint::Color(_) => true,
        usvg::Paint::LinearGradient(gradient) => gradient
            .stops()
            .iter()
            .any(|stop| stop.opacity().get() > 0.0),
        usvg::Paint::RadialGradient(gradient) => gradient
            .stops()
            .iter()
            .any(|stop| stop.opacity().get() > 0.0),
        usvg::Paint::Pattern(pattern) => {
            pattern.root().opacity().get() > 0.0 && pattern.root().has_children()
        }
    }
}

fn outline(font: &pdf_font::Font, gid: u16) -> Option<Path> {
    use pdf_font::PathOp;
    let mut path = PathBuilder::new();
    for op in font.outline(gid).ok()?.ops() {
        match *op {
            PathOp::MoveTo(x, y) => path.move_to(x, y),
            PathOp::LineTo(x, y) => path.line_to(x, y),
            PathOp::QuadTo(cx, cy, x, y) => path.quad_to(cx, cy, x, y),
            PathOp::CurveTo(ax, ay, bx, by, x, y) => path.cubic_to(ax, ay, bx, by, x, y),
            PathOp::Close => path.close(),
        }
    }
    path.finish()
}

/// Coverage is used only to exclude invisible glyphs. Search geometry remains the
/// shaper's exact affine transform and the embedded font's own metrics.
fn visible(
    path: Option<&Path>,
    local: Transform,
    raster: Transform,
    mask: Option<&Mask>,
    size: (u32, u32),
    fill: bool,
    stroke: Option<&usvg::Stroke>,
) -> bool {
    let transform = raster.pre_concat(local);
    let Some(path) = path else {
        // Spaces have no contour. Keep them only when their actual origin is in view.
        let (x, y) = (transform.tx.floor() as i64, transform.ty.floor() as i64);
        return x >= 0
            && y >= 0
            && x < i64::from(size.0)
            && y < i64::from(size.1)
            && mask.is_none_or(|mask| mask.data()[y as usize * size.0 as usize + x as usize] > 0);
    };
    let Some(bounds) = path.bounds().transform(transform) else {
        return false;
    };
    if mask.is_none()
        && bounds.left() >= 0.0
        && bounds.top() >= 0.0
        && bounds.right() <= size.0 as f32
        && bounds.bottom() <= size.1 as f32
    {
        return true;
    }
    let stroke_path = stroke.and_then(|stroke| {
        path.clone()
            .transform(local)?
            .stroke(&stroke.to_tiny_skia(), 1.0)
    });
    let bounds = stroke_path
        .as_ref()
        .and_then(|path| path.bounds().transform(raster))
        .and_then(|stroke| bounds.join(&stroke))
        .unwrap_or(bounds);
    let left = bounds.left().floor().max(0.0) as u32;
    let top = bounds.top().floor().max(0.0) as u32;
    let right = bounds.right().ceil().min(size.0 as f32).max(0.0) as u32;
    let bottom = bounds.bottom().ceil().min(size.1 as f32).max(0.0) as u32;
    let Some(width) = right.checked_sub(left).filter(|&n| n > 0) else {
        return false;
    };
    let Some(height) = bottom.checked_sub(top).filter(|&n| n > 0) else {
        return false;
    };
    let Some(mut coverage) = Mask::new(width, height) else {
        return false;
    };
    let offset = Transform::from_translate(-(left as f32), -(top as f32));
    if fill {
        coverage.fill_path(
            path,
            tiny_skia::FillRule::Winding,
            true,
            offset.pre_concat(transform),
        );
    }
    if let Some(path) = stroke_path {
        coverage.fill_path(
            &path,
            tiny_skia::FillRule::Winding,
            true,
            offset.pre_concat(raster),
        );
    }
    coverage.data().iter().enumerate().any(|(index, &alpha)| {
        alpha > 0
            && mask.is_none_or(|mask| {
                let x = left as usize + index % width as usize;
                let y = top as usize + index / width as usize;
                mask.data()[y * size.0 as usize + x] > 0
            })
    })
}

fn intersect(left: &mut Mask, right: &Mask) {
    for (left, &right) in left.data_mut().iter_mut().zip(right.data()) {
        *left = ((u16::from(*left) * u16::from(right) + 127) / 255) as u8;
    }
}

/// resvg's node entry point moves its bounding box to the origin. Cancel that
/// translation so masks use the same SVG canvas coordinates as the glyphs.
fn render_children(group: &usvg::Group, transform: Transform, pixmap: &mut tiny_skia::Pixmap) {
    for node in group.children() {
        if let Some(bounds) = node.abs_layer_bounding_box() {
            resvg::render_node(
                node,
                transform.pre_translate(bounds.x(), bounds.y()),
                &mut pixmap.as_mut(),
            );
        }
    }
}

fn clip_mask(
    mut clip: &usvg::ClipPath,
    transform: Transform,
    width: u32,
    height: u32,
) -> Option<Mask> {
    let mut combined = Mask::new(width, height)?;
    combined.data_mut().fill(255);
    for _ in 0..64 {
        let mut pixels = tiny_skia::Pixmap::new(width, height)?;
        render_children(
            clip.root(),
            transform.pre_concat(clip.transform()),
            &mut pixels,
        );
        intersect(
            &mut combined,
            &Mask::from_pixmap(pixels.as_ref(), tiny_skia::MaskType::Alpha),
        );
        let Some(next) = clip.clip_path() else {
            return Some(combined);
        };
        clip = next;
    }
    None
}

fn image_mask(
    mut mask: &usvg::Mask,
    transform: Transform,
    width: u32,
    height: u32,
) -> Option<Mask> {
    let mut combined = Mask::new(width, height)?;
    combined.data_mut().fill(255);
    for _ in 0..64 {
        let mut pixels = tiny_skia::Pixmap::new(width, height)?;
        render_children(mask.root(), transform, &mut pixels);
        let kind = match mask.kind() {
            usvg::MaskType::Alpha => tiny_skia::MaskType::Alpha,
            usvg::MaskType::Luminance => tiny_skia::MaskType::Luminance,
        };
        let mut current = Mask::from_pixmap(pixels.as_ref(), kind);
        current.intersect_path(
            &PathBuilder::from_rect(mask.rect().to_rect()),
            tiny_skia::FillRule::Winding,
            true,
            transform,
        );
        intersect(&mut combined, &current);
        let Some(next) = mask.mask() else {
            return Some(combined);
        };
        mask = next;
    }
    None
}
