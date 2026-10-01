//! FreeType's glyph stroker and hinted large-outline path.
//!
//! Loading and transform arithmetic follow MuPDF 1.27.2 `font.c`
//! (`do_render_ft_stroked_glyph`, `fz_outline_ft_glyph`), AGPL-3.0.
//! Outline decomposition is ported from FreeType 2.13.2 `ftoutln.c`,
//! Copyright 1996–2023 David Turner, Robert Wilhelm, and Werner Lemberg,
//! used under its GPL-2.0-or-later option.
//! FreeType is bundled at build time; ordinary filled glyphs continue to use
//! pdf-raster's Rust smooth rasterizer. Each face borrows its existing font
//! program through an Arc, without copying or reparsing it for each glyph.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use freetype::face::LoadFlag;
use freetype::{Face, Library, StrokerLineCap, StrokerLineJoin, Vector};
use pdf_font::Font;
use pdf_raster::{Glyph, IntRect, Mask, Path, PathBuilder, PathEl, Point, Stroke, Transform};

use crate::RenderError;

#[derive(Clone)]
struct FontBytes(Arc<Font>);
impl Borrow<[u8]> for FontBytes {
    fn borrow(&self) -> &[u8] {
        self.0.data()
    }
}

fn error(e: freetype::Error) -> RenderError {
    pdf_interp::InterpError::Limit(format!("FreeType glyph rendering: {e}")).into()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct StrokeKey {
    font: u64,
    gid: u16,
    matrix: [i64; 6],
    radius: i64,
    cap: u32,
    join: u32,
    miter: i64,
}

pub(crate) struct Glyphs {
    library: Library,
    faces: HashMap<u64, Face<FontBytes>>,
    outlines: HashMap<(u64, u16), Arc<Path>>,
    strokes: HashMap<StrokeKey, Arc<Mask>>,
    bytes: usize,
}

impl Glyphs {
    pub fn new() -> Result<Self, RenderError> {
        Ok(Self {
            library: Library::init().map_err(error)?,
            faces: HashMap::new(),
            outlines: HashMap::new(),
            strokes: HashMap::new(),
            bytes: 0,
        })
    }

    fn face(&mut self, id: u64, font: &Arc<Font>) -> Result<&Face<FontBytes>, RenderError> {
        match self.faces.entry(id) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let face = self
                    .library
                    .new_memory_face2(FontBytes(Arc::clone(font)), font.face_index() as isize)
                    .map_err(error)?;
                face.set_char_size(65536, 65536, 72, 72).map_err(error)?;
                Ok(entry.insert(face))
            }
        }
    }

    pub fn outline(
        &mut self,
        id: u64,
        source: &crate::device::Outline,
        trm: &Transform,
    ) -> Result<Path, RenderError> {
        let (font, gid, to_em) = (&source.font, source.gid, &source.to_em);
        let raw = match self.outlines.get(&(id, gid)) {
            Some(path) => Arc::clone(path),
            None => {
                let face = self.face(id, font)?;
                face.load_glyph(u32::from(gid), LoadFlag::IGNORE_TRANSFORM)
                    .map_err(error)?;
                let path = match face.glyph().outline() {
                    Some(outline) => decompose(&outline),
                    None => PathBuilder::new().finish(),
                };
                let path = Arc::new(path);
                self.outlines.insert((id, gid), Arc::clone(&path));
                path
            }
        };
        let m = adjusted(font, to_em, trm);
        // fz_concat(fz_scale(1/65536, 1/65536), trm), in float.
        let map = |p: Point| -> [f32; 2] {
            let (x, y) = (p.x as f32, p.y as f32);
            [
                x.mul_add(m[0] / 65536.0, y * (m[2] / 65536.0)) + m[4],
                x.mul_add(m[1] / 65536.0, y * (m[3] / 65536.0)) + m[5],
            ]
        };
        let mut path = PathBuilder::new();
        for el in raw.elements() {
            match *el {
                PathEl::MoveTo(p) => {
                    let p = map(p);
                    path.move_to(p[0].into(), p[1].into());
                }
                PathEl::LineTo(p) => {
                    let p = map(p);
                    path.line_to(p[0].into(), p[1].into());
                }
                PathEl::QuadTo(c, p) => {
                    let (c, p) = (map(c), map(p));
                    path.quad_to(c[0].into(), c[1].into(), p[0].into(), p[1].into());
                }
                PathEl::CubicTo(a, b, p) => {
                    let (a, b, p) = (map(a), map(b), map(p));
                    path.cubic_to(
                        a[0].into(),
                        a[1].into(),
                        b[0].into(),
                        b[1].into(),
                        p[0].into(),
                        p[1].into(),
                    );
                }
                PathEl::Close => {
                    path.close();
                }
            }
        }
        Ok(path.finish())
    }

    pub fn stroke(
        &mut self,
        id: u64,
        source: &crate::device::Outline,
        trm: &Transform,
        ctm: &Transform,
        style: &Stroke,
    ) -> Result<Glyph, RenderError> {
        let placement = pdf_raster::glyph_placement(trm);
        let m = adjusted(&source.font, &source.to_em, &placement.subpix);
        let fixed = m.map(|v| (v * 64.0) as i64);
        let expansion = (ctm.a as f32)
            .mul_add(ctm.d as f32, -(ctm.b as f32 * ctm.c as f32))
            .abs()
            .sqrt();
        let radius = (style.width as f32 * expansion * 64.0 / 2.0) as i64;
        let cap = match style.cap {
            pdf_raster::LineCap::Butt => StrokerLineCap::Butt,
            pdf_raster::LineCap::Round => StrokerLineCap::Round,
            pdf_raster::LineCap::Square => StrokerLineCap::Square,
        };
        let join = match style.join {
            pdf_raster::LineJoin::Miter => StrokerLineJoin::MiterFixed,
            pdf_raster::LineJoin::Round => StrokerLineJoin::Round,
            pdf_raster::LineJoin::Bevel => StrokerLineJoin::Bevel,
        };
        let miter = (style.miter_limit as f32 * 65536.0) as i64;
        let key = StrokeKey {
            font: id,
            gid: source.gid,
            matrix: fixed,
            radius,
            cap: cap as u32,
            join: join as u32,
            miter,
        };
        if let Some(mask) = self.strokes.get(&key) {
            return Ok(Glyph {
                mask: Arc::clone(mask),
                x: placement.x,
                y: placement.y,
            });
        }
        let face = self.face(id, &source.font)?;
        let mut matrix = freetype::Matrix {
            xx: fixed[0] as _,
            yx: fixed[1] as _,
            xy: fixed[2] as _,
            yy: fixed[3] as _,
        };
        let mut delta = Vector {
            x: fixed[4] as _,
            y: fixed[5] as _,
        };
        face.set_transform(&mut matrix, &mut delta);
        face.load_glyph(
            u32::from(source.gid),
            LoadFlag::NO_BITMAP | LoadFlag::NO_HINTING,
        )
        .map_err(error)?;
        let glyph = face.glyph().get_glyph().map_err(error)?;
        let stroker = self.library.new_stroker().map_err(error)?;
        stroker.set(radius as _, cap, join, miter as _);
        let stroked = glyph.stroke(&stroker).map_err(error)?;
        let bitmap = stroked
            .to_bitmap(freetype::RenderMode::Normal, None)
            .map_err(error)?;
        let data = bitmap.bitmap();
        let (width, height) = (data.width(), data.rows());
        let rect = IntRect::new(
            bitmap.left(),
            bitmap.top() - height,
            bitmap.left() + width,
            bitmap.top(),
        );
        let pixels = u64::from(rect.width()) * u64::from(rect.height());
        if pixels > pdf_raster::MAX_PIXELS {
            return Err(pdf_interp::InterpError::Limit(
                "stroked glyph exceeds the pixel limit".to_owned(),
            )
            .into());
        }
        let mut coverage = Vec::with_capacity(pixels as usize);
        let pitch = data.pitch().unsigned_abs() as usize;
        for y in 0..height as usize {
            let source_y = if data.pitch() >= 0 {
                height as usize - y - 1
            } else {
                y
            };
            coverage.extend_from_slice(
                &data.buffer()[source_y * pitch..source_y * pitch + width as usize],
            );
        }
        let mask = Arc::new(Mask::from_data(rect, coverage, 0)?);
        if mask.data().len() <= 1 << 24 {
            if self.bytes + mask.data().len() > 1 << 24 {
                self.strokes.clear();
                self.bytes = 0;
            }
            self.bytes += mask.data().len();
            self.strokes.insert(key, Arc::clone(&mask));
        }
        Ok(Glyph {
            mask,
            x: placement.x,
            y: placement.y,
        })
    }
}

fn adjusted(font: &Font, to_em: &Transform, trm: &Transform) -> [f32; 6] {
    let stretch = (to_em.a * f64::from(font.units_per_em())) as f32;
    [
        trm.a as f32 * stretch,
        trm.b as f32 * stretch,
        trm.c as f32,
        trm.d as f32,
        trm.e as f32,
        trm.f as f32,
    ]
}

/// FT_Outline_Decompose, preserving integer implied points before mapping.
fn decompose(outline: &freetype::Outline<'_>) -> Path {
    let mut path = PathBuilder::new();
    let mut first = 0;
    let midpoint = |a: Vector, b: Vector| Vector {
        x: (a.x + b.x) / 2,
        y: (a.y + b.y) / 2,
    };
    for &last in outline.contours() {
        let end = last as usize;
        let points = &outline.points()[first..=end];
        let tags = &outline.tags()[first..=end];
        let mut limit = points.len();
        let (start, mut i) = if tags[0] & 3 == 0 {
            if tags[limit - 1] & 3 == 1 {
                limit -= 1;
                (points[limit], 0)
            } else {
                (midpoint(points[0], points[limit - 1]), 0)
            }
        } else {
            (points[0], 1)
        };
        path.move_to(start.x as f64, start.y as f64);
        while i < limit {
            let p = points[i];
            match tags[i] & 3 {
                1 => {
                    path.line_to(p.x as f64, p.y as f64);
                    i += 1;
                }
                0 => {
                    i += 1;
                    let mut control = p;
                    loop {
                        if i == limit {
                            path.quad_to(
                                control.x as f64,
                                control.y as f64,
                                start.x as f64,
                                start.y as f64,
                            );
                            break;
                        }
                        let next = points[i];
                        if tags[i] & 3 == 1 {
                            path.quad_to(
                                control.x as f64,
                                control.y as f64,
                                next.x as f64,
                                next.y as f64,
                            );
                            i += 1;
                            break;
                        }
                        let mid = midpoint(control, next);
                        path.quad_to(
                            control.x as f64,
                            control.y as f64,
                            mid.x as f64,
                            mid.y as f64,
                        );
                        control = next;
                        i += 1;
                    }
                }
                _ => {
                    let b = points[i + 1];
                    i += 2;
                    let p2 = if i < limit {
                        let p2 = points[i];
                        i += 1;
                        p2
                    } else {
                        start
                    };
                    path.cubic_to(
                        p.x as f64,
                        p.y as f64,
                        b.x as f64,
                        b.y as f64,
                        p2.x as f64,
                        p2.y as f64,
                    );
                }
            }
        }
        path.close();
        first = end + 1;
    }
    path.finish()
}
