//! Glyph bitmaps for text: an exact-area scanline rasterizer, MuPDF's
//! subpixel placement, and a bitmap cache keyed the way MuPDF keys it.
//!
//! The rasterizer is a port of FreeType's "smooth" renderer: project
//! FreeType, file `src/smooth/ftgrays.c`, version 2.13.3, used under
//! FreeType's GPL-2.0-or-later licence option. Placement, the cache key and
//! the size limits follow MuPDF 1.27.2 `source/fitz/draw-glyph.c`
//! (`fz_subpixel_adjust`, `fz_render_glyph`) and `source/fitz/font.c`
//! (`do_ft_render_glyph`), AGPL-3.0.
//!
//! Outline points are quantised to FreeType's 26.6 fixed point and then
//! upscaled to 1/256 pixel, so a glyph bitmap matches what MuPDF gets from
//! FreeType for the same outline and matrix.

use std::collections::HashMap;
use std::sync::Arc;

use crate::geom::{IntRect, Point, Transform};
use crate::mask::Mask;
use crate::path::{Path, PathEl};
use crate::pixmap::check_size;
use crate::raster::FillRule;

/// Subpixel precision: coordinates are in 1/256 pixel (`PIXEL_BITS` 8).
const PIXEL_BITS: u32 = 8;
const ONE_PIXEL: i64 = 1 << PIXEL_BITS;
const ONE_PIXEL_I32: i32 = ONE_PIXEL as i32;

/// Largest glyph scale (geometric mean, pixels per em) drawn from a bitmap;
/// bigger glyphs are filled as paths (MuPDF `MAX_GLYPH_SIZE`).
pub const MAX_GLYPH_SIZE: f32 = 256.0;

/// Bytes of cached bitmaps before the cache is dropped (MuPDF
/// `MAX_CACHE_SIZE`).
const MAX_CACHE_BYTES: usize = 1 << 24;

/// FreeType matrices are 16.16: `do_ft_render_glyph` refuses larger scales.
const MAX_FT_SCALE: f64 = 512.0;

#[derive(Clone, Copy, Default)]
struct Cell {
    x: i32,
    cover: i32,
    area: i32,
    next: u32,
}

/// `cells[0]`: the sentinel every row list ends with (`cell_null`).
const NULL_CELL: u32 = 0;

#[derive(Clone, Copy)]
enum El {
    Move(i64, i64),
    Line(i64, i64),
    Quad(i64, i64, i64, i64),
    Cubic(i64, i64, i64, i64, i64, i64),
    Close,
}

/// Scratch state of one rasterization (`gray_TWorker`), reused across glyphs.
#[derive(Default)]
struct Worker {
    /// `cells[0]` is the null sentinel, `cells[1..=rows]` the row heads.
    cells: Vec<Cell>,
    els: Vec<El>,
    min_ex: i32,
    max_ex: i32,
    min_ey: i32,
    max_ey: i32,
    cell: u32,
    x: i64,
    y: i64,
}

#[inline]
fn trunc(v: i64) -> i32 {
    (v >> PIXEL_BITS) as i32
}

#[inline]
fn fract(v: i64) -> i32 {
    (v & (ONE_PIXEL - 1)) as i32
}

/// `FT_UDIV`: `a / b` for non-negative `a` through the precomputed
/// reciprocal `b_r = 0xFFFFFFFF / b`.
#[inline]
fn udiv(a: i64, b_r: i64) -> i32 {
    ((a as u64).wrapping_mul(b_r as u64) >> 32) as i32
}

/// `LEFT_SHIFT`: a shift that wraps instead of overflowing.
#[inline]
fn left_shift(a: i64, b: u32) -> i64 {
    a.wrapping_shl(b)
}

/// `FT_FILL_RULE`: cell area to a coverage byte.
#[inline]
fn fill_rule(area: i32, fill: i32) -> u8 {
    let mut coverage = area >> (PIXEL_BITS * 2 + 1 - 8);
    if coverage & fill != 0 {
        coverage = !coverage;
    }
    if coverage > 255 && fill & i32::MIN != 0 {
        coverage = 255;
    }
    coverage as u8
}

/// Device coordinate to 26.6 fixed point, rounded as `FT_MulFix` rounds.
#[inline]
fn to_26_6(v: f64) -> i64 {
    (v * 64.0).round() as i64
}

impl Worker {
    fn reset(&mut self, width: i32, height: i32) {
        self.min_ex = 0;
        self.max_ex = width;
        self.min_ey = 0;
        self.max_ey = height;
        self.cells.clear();
        self.cells.push(Cell {
            x: i32::MAX,
            cover: 0,
            area: 0,
            next: NULL_CELL,
        });
        self.cells.resize(1 + height as usize, Cell::default());
        self.cell = NULL_CELL;
        self.x = 0;
        self.y = 0;
    }

    /// `gray_set_cell`: makes `(ex, ey)` the current cell, creating it in
    /// its sorted row list when new. Cells left of the box collapse onto
    /// `min_ex - 1`; cells right of it or outside its rows go to the null
    /// sentinel.
    fn set_cell(&mut self, ex: i32, ey: i32) {
        let row = ey - self.min_ey;
        if row < 0 || row >= self.max_ey - self.min_ey || ex >= self.max_ex {
            self.cell = NULL_CELL;
            return;
        }
        let ex = ex.max(self.min_ex - 1);
        let mut prev = 1 + row as usize;
        loop {
            let cell = self.cells[prev].next;
            let c = self.cells[cell as usize];
            if c.x > ex {
                let new = self.cells.len() as u32;
                self.cells.push(Cell {
                    x: ex,
                    cover: 0,
                    area: 0,
                    next: cell,
                });
                self.cells[prev].next = new;
                self.cell = new;
                return;
            }
            if c.x == ex {
                self.cell = cell;
                return;
            }
            prev = cell as usize;
        }
    }

    /// `FT_INTEGRATE`: adds a crossing of height `a` at horizontal position
    /// `b` (twice the pixel-relative x, in 1/256 pixel) to the current cell.
    #[inline]
    fn integrate(&mut self, a: i32, b: i32) {
        let c = &mut self.cells[self.cell as usize];
        c.cover = c.cover.wrapping_add(a);
        c.area = c.area.wrapping_add(a.wrapping_mul(b));
    }

    fn move_to(&mut self, x: i64, y: i64) {
        self.set_cell(trunc(x), trunc(y));
        self.x = x;
        self.y = y;
    }

    /// `gray_render_line` (the 64-bit variant).
    fn render_line(&mut self, to_x: i64, to_y: i64) {
        let mut ey1 = trunc(self.y);
        let ey2 = trunc(to_y);
        if (ey1 >= self.max_ey && ey2 >= self.max_ey) || (ey1 < self.min_ey && ey2 < self.min_ey) {
            self.x = to_x;
            self.y = to_y;
            return;
        }
        let mut ex1 = trunc(self.x);
        let ex2 = trunc(to_x);
        let mut fx1 = fract(self.x);
        let mut fy1 = fract(self.y);
        let dx = to_x - self.x;
        let dy = to_y - self.y;
        if ex1 == ex2 && ey1 == ey2 {
            // Inside one cell: only the final integration below.
        } else if dy == 0 {
            self.set_cell(ex2, ey2);
            self.x = to_x;
            self.y = to_y;
            return;
        } else if dx == 0 {
            if dy > 0 {
                loop {
                    self.integrate(ONE_PIXEL_I32 - fy1, fx1 * 2);
                    fy1 = 0;
                    ey1 += 1;
                    self.set_cell(ex1, ey1);
                    if ey1 == ey2 {
                        break;
                    }
                }
            } else {
                loop {
                    self.integrate(-fy1, fx1 * 2);
                    fy1 = ONE_PIXEL_I32;
                    ey1 -= 1;
                    self.set_cell(ex1, ey1);
                    if ey1 == ey2 {
                        break;
                    }
                }
            }
        } else {
            let mut prod = dx * i64::from(fy1) - dy * i64::from(fx1);
            let dx_r = if ex1 != ex2 { 0xFFFF_FFFF_i64 / dx } else { 0 };
            let dy_r = if ey1 != ey2 { 0xFFFF_FFFF_i64 / dy } else { 0 };
            loop {
                if prod - dx * ONE_PIXEL > 0 && prod <= 0 {
                    // Left.
                    let fy2 = udiv(-prod, -dx_r);
                    prod -= dy * ONE_PIXEL;
                    self.integrate(fy2 - fy1, fx1);
                    fx1 = ONE_PIXEL_I32;
                    fy1 = fy2;
                    ex1 -= 1;
                } else if prod - dx * ONE_PIXEL + dy * ONE_PIXEL > 0 && prod - dx * ONE_PIXEL <= 0 {
                    // Up.
                    prod -= dx * ONE_PIXEL;
                    let fx2 = udiv(-prod, dy_r);
                    self.integrate(ONE_PIXEL_I32 - fy1, fx1 + fx2);
                    fx1 = fx2;
                    fy1 = 0;
                    ey1 += 1;
                } else if prod + dy * ONE_PIXEL >= 0 && prod - dx * ONE_PIXEL + dy * ONE_PIXEL <= 0
                {
                    // Right.
                    prod += dy * ONE_PIXEL;
                    let fy2 = udiv(prod, dx_r);
                    self.integrate(fy2 - fy1, fx1 + ONE_PIXEL_I32);
                    fx1 = 0;
                    fy1 = fy2;
                    ex1 += 1;
                } else {
                    // Down.
                    let fx2 = udiv(prod, -dy_r);
                    prod += dx * ONE_PIXEL;
                    self.integrate(-fy1, fx1 + fx2);
                    fx1 = fx2;
                    fy1 = ONE_PIXEL_I32;
                    ey1 -= 1;
                }
                self.set_cell(ex1, ey1);
                if ex1 == ex2 && ey1 == ey2 {
                    break;
                }
            }
        }
        let fx2 = fract(to_x);
        let fy2 = fract(to_y);
        self.integrate(fy2 - fy1, fx1 + fx2);
        self.x = to_x;
        self.y = to_y;
    }

    /// `gray_render_conic`: a forward-differencing walk of the quadratic.
    fn render_conic(&mut self, cx: i64, cy: i64, tx: i64, ty: i64) {
        let (p0x, p0y) = (self.x, self.y);
        if (trunc(p0y) >= self.max_ey && trunc(cy) >= self.max_ey && trunc(ty) >= self.max_ey)
            || (trunc(p0y) < self.min_ey && trunc(cy) < self.min_ey && trunc(ty) < self.min_ey)
        {
            self.x = tx;
            self.y = ty;
            return;
        }
        let bx = cx - p0x;
        let by = cy - p0y;
        let ax = tx - cx - bx;
        let ay = ty - cy - by;
        let mut dx = ax.abs().max(ay.abs());
        if dx <= ONE_PIXEL / 4 {
            self.render_line(tx, ty);
            return;
        }
        let mut shift: u32 = 16;
        loop {
            dx >>= 2;
            shift -= 1;
            if dx <= ONE_PIXEL / 4 {
                break;
            }
        }
        let mut count = 0x10000_u32 >> shift;
        let mut rx = left_shift(ax, shift + shift);
        let mut ry = left_shift(ay, shift + shift);
        let mut qx = left_shift(bx, shift + 17) + rx;
        let mut qy = left_shift(by, shift + 17) + ry;
        rx = rx.wrapping_mul(2);
        ry = ry.wrapping_mul(2);
        let mut px = left_shift(p0x, 32);
        let mut py = left_shift(p0y, 32);
        loop {
            px = px.wrapping_add(qx);
            py = py.wrapping_add(qy);
            qx = qx.wrapping_add(rx);
            qy = qy.wrapping_add(ry);
            self.render_line(px >> 32, py >> 32);
            count -= 1;
            if count == 0 {
                break;
            }
        }
    }

    /// `gray_render_cubic`: bisection until each piece is flat enough.
    fn render_cubic(&mut self, c1x: i64, c1y: i64, c2x: i64, c2y: i64, tx: i64, ty: i64) {
        let mut stack = [(0_i64, 0_i64); 16 * 3 + 1];
        stack[0] = (tx, ty);
        stack[1] = (c2x, c2y);
        stack[2] = (c1x, c1y);
        stack[3] = (self.x, self.y);
        let rows = |p: (i64, i64)| trunc(p.1);
        if stack[..4].iter().all(|&p| rows(p) >= self.max_ey)
            || stack[..4].iter().all(|&p| rows(p) < self.min_ey)
        {
            self.x = tx;
            self.y = ty;
            return;
        }
        let mut arc = 0usize;
        loop {
            let a = &stack[arc..arc + 4];
            if (2 * a[0].0 - 3 * a[1].0 + a[3].0).abs() > ONE_PIXEL / 2
                || (2 * a[0].1 - 3 * a[1].1 + a[3].1).abs() > ONE_PIXEL / 2
                || (a[0].0 - 3 * a[2].0 + 2 * a[3].0).abs() > ONE_PIXEL / 2
                || (a[0].1 - 3 * a[2].1 + 2 * a[3].1).abs() > ONE_PIXEL / 2
            {
                split_cubic(&mut stack[arc..arc + 7]);
                arc += 3;
                continue;
            }
            let (x, y) = stack[arc];
            self.render_line(x, y);
            if arc == 0 {
                return;
            }
            arc -= 3;
        }
    }

    /// Feeds the quantised outline (`FT_Outline_Decompose` with each
    /// contour closed back to its start).
    fn decompose(&mut self) {
        let els = std::mem::take(&mut self.els);
        let mut start = (0_i64, 0_i64);
        let mut open = false;
        for el in &els {
            match *el {
                El::Move(x, y) => {
                    if open && (self.x, self.y) != start {
                        self.render_line(start.0, start.1);
                    }
                    self.move_to(x, y);
                    start = (x, y);
                    open = true;
                }
                El::Line(x, y) => self.render_line(x, y),
                El::Quad(cx, cy, x, y) => self.render_conic(cx, cy, x, y),
                El::Cubic(c1x, c1y, c2x, c2y, x, y) => self.render_cubic(c1x, c1y, c2x, c2y, x, y),
                El::Close => {
                    if open && (self.x, self.y) != start {
                        self.render_line(start.0, start.1);
                    }
                    open = false;
                }
            }
        }
        if open && (self.x, self.y) != start {
            self.render_line(start.0, start.1);
        }
        self.els = els;
    }

    /// `gray_sweep`: accumulates each row's cells into coverage bytes.
    fn sweep(&self, rule: FillRule, data: &mut [u8]) {
        let fill = match rule {
            FillRule::NonZero => i32::MIN,
            FillRule::EvenOdd => 0x100,
        };
        let width = self.max_ex as usize;
        for (row, line) in data.chunks_exact_mut(width).enumerate() {
            let mut cell = self.cells[1 + row].next;
            let mut x = self.min_ex;
            let mut cover: i32 = 0;
            while cell != NULL_CELL {
                let c = self.cells[cell as usize];
                if cover != 0 && c.x > x {
                    line[x as usize..c.x as usize].fill(fill_rule(cover, fill));
                }
                cover = cover.wrapping_add(c.cover.wrapping_mul(ONE_PIXEL_I32 * 2));
                let area = cover.wrapping_sub(c.area);
                if area != 0 && c.x >= self.min_ex {
                    line[c.x as usize] = fill_rule(area, fill);
                }
                x = c.x + 1;
                cell = c.next;
            }
            if cover != 0 {
                line[x as usize..].fill(fill_rule(cover, fill));
            }
        }
    }

    /// Rasterizes `path` into `out`, each point mapped to 26.6 device
    /// coordinates by `map`; `out`'s rectangle becomes the pixel box of
    /// the outline's control box. False when the box is too large for a
    /// mask.
    fn rasterize(
        &mut self,
        path: &Path,
        map: &dyn Fn(Point) -> (i64, i64),
        rule: FillRule,
        out: &mut Mask,
    ) -> bool {
        self.els.clear();
        let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        let mut pt = |p: Point| {
            let (x, y) = map(p);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            (x, y)
        };
        let els = path.elements();
        // A TrueType on-point implied between two off-points is their
        // midpoint. FreeType first translates the outline into its positive
        // bitmap box, then truncates that midpoint. Before translation this
        // is floor division, not Rust's signed division towards zero.
        let mut start: Option<(i64, i64)> = None;
        for (i, el) in els.iter().enumerate() {
            self.els.push(match *el {
                PathEl::MoveTo(p) => {
                    start = None;
                    if let Some(PathEl::QuadTo(first, _)) = els.get(i + 1) {
                        let end = els[i + 1..]
                            .iter()
                            .position(|e| matches!(e, PathEl::Close | PathEl::MoveTo(_)))
                            .map_or(els.len(), |n| i + 1 + n);
                        if let Some(PathEl::QuadTo(last, back)) = els.get(end - 1)
                            && *back == p
                            && p.x == (first.x + last.x) * 0.5
                            && p.y == (first.y + last.y) * 0.5
                        {
                            let (fx, fy) = pt(*first);
                            let (lx, ly) = pt(*last);
                            start = Some(((fx + lx) >> 1, (fy + ly) >> 1));
                        }
                    }
                    let (x, y) = start.unwrap_or_else(|| pt(p));
                    El::Move(x, y)
                }
                PathEl::LineTo(p) => {
                    let (x, y) = pt(p);
                    El::Line(x, y)
                }
                PathEl::QuadTo(c, p) => {
                    let (cx, cy) = pt(c);
                    let (x, y) = match els.get(i + 1) {
                        Some(PathEl::QuadTo(next, _))
                            if p.x == (c.x + next.x) * 0.5 && p.y == (c.y + next.y) * 0.5 =>
                        {
                            let (nx, ny) = pt(*next);
                            ((cx + nx) >> 1, (cy + ny) >> 1)
                        }
                        Some(PathEl::Close | PathEl::MoveTo(_)) | None if start.is_some() => {
                            start.take().unwrap_or_default()
                        }
                        _ => pt(p),
                    };
                    El::Quad(cx, cy, x, y)
                }
                PathEl::CubicTo(c1, c2, p) => {
                    let (c1x, c1y) = pt(c1);
                    let (c2x, c2y) = pt(c2);
                    let (x, y) = pt(p);
                    El::Cubic(c1x, c1y, c2x, c2y, x, y)
                }
                PathEl::Close => El::Close,
            });
        }
        if x0 > x1 {
            out.reset(IntRect::EMPTY);
            return true;
        }
        // `ft_glyphslot_preset_bitmap`: floor the minimum, ceil the maximum.
        let px0 = x0 >> 6;
        let py0 = y0 >> 6;
        let px1 = (x1 + 63) >> 6;
        let py1 = (y1 + 63) >> 6;
        let (Ok(bx0), Ok(by0), Ok(bx1), Ok(by1)) = (
            i32::try_from(px0),
            i32::try_from(py0),
            i32::try_from(px1),
            i32::try_from(py1),
        ) else {
            return false;
        };
        let rect = IntRect::new(bx0, by0, bx1, by1);
        if rect.is_empty() {
            out.reset(IntRect::EMPTY);
            return true;
        }
        if check_size(rect.width(), rect.height()).is_err() {
            return false;
        }
        let (ox, oy) = (px0 << 6, py0 << 6);
        for el in &mut self.els {
            let up = |v: i64, o: i64| (v - o) * (ONE_PIXEL >> 6);
            *el = match *el {
                El::Move(x, y) => El::Move(up(x, ox), up(y, oy)),
                El::Line(x, y) => El::Line(up(x, ox), up(y, oy)),
                El::Quad(cx, cy, x, y) => El::Quad(up(cx, ox), up(cy, oy), up(x, ox), up(y, oy)),
                El::Cubic(c1x, c1y, c2x, c2y, x, y) => El::Cubic(
                    up(c1x, ox),
                    up(c1y, oy),
                    up(c2x, ox),
                    up(c2y, oy),
                    up(x, ox),
                    up(y, oy),
                ),
                El::Close => El::Close,
            };
        }
        self.reset(rect.width() as i32, rect.height() as i32);
        self.decompose();
        out.reset(rect);
        self.sweep(rule, out.data_mut());
        true
    }
}

/// `gray_split_cubic`: de Casteljau split at the midpoint, in place.
fn split_cubic(base: &mut [(i64, i64)]) {
    base[6] = base[3];
    let mut a = base[0].0 + base[1].0;
    let mut b = base[1].0 + base[2].0;
    let mut c = base[2].0 + base[3].0;
    base[5].0 = c >> 1;
    c += b;
    base[4].0 = c >> 2;
    base[1].0 = a >> 1;
    a += b;
    base[2].0 = a >> 2;
    base[3].0 = (a + c) >> 3;
    a = base[0].1 + base[1].1;
    b = base[1].1 + base[2].1;
    c = base[2].1 + base[3].1;
    base[5].1 = c >> 1;
    c += b;
    base[4].1 = c >> 2;
    base[1].1 = a >> 1;
    a += b;
    base[2].1 = a >> 2;
    base[3].1 = (a + c) >> 3;
}

/// Exact-area coverage of `path` through `t` (nonzero or even-odd), over
/// the pixel box of the transformed outline. `None` when the box is too
/// large for a mask.
pub fn rasterize_outline(path: &Path, t: &Transform, rule: FillRule) -> Option<Mask> {
    let mut worker = Worker::default();
    let mut out = Mask::default();
    let map = |p: Point| {
        let q = t.apply(p);
        (to_26_6(q.x), to_26_6(q.y))
    };
    worker.rasterize(path, &map, rule, &mut out).then_some(out)
}

/// `FT_MulFix`: a 16.16 product rounded to nearest, halves away from zero.
#[inline]
fn mul_fix(a: i64, b: i64) -> i64 {
    let ab = a * b;
    (ab + 0x8000 - i64::from(ab < 0)) >> 16
}

/// How FreeType turns font units into the 26.6 device coordinates MuPDF
/// rasterizes: `FT_Set_Char_Size(65536, 65536, 72, 72)` scales units to
/// 1024 pixels per em through the face's `x_scale`, then `FT_Set_Transform`
/// applies `trm · 64` as a 16.16 matrix and the subpixel offset, every
/// product rounded as `FT_MulFix` rounds. Reproducing the chain makes the
/// bitmap match MuPDF's to the last coverage level.
struct FtChain {
    x_scale: i64,
    xx: i64,
    xy: i64,
    yx: i64,
    yy: i64,
    dx: i64,
    dy: i64,
}

impl FtChain {
    /// The chain for a font whose `to_em` matrix is a plain scale by
    /// `1 / units_per_em` (with any substitute-width stretch along x, which
    /// MuPDF folds into `trm` as `fz_adjust_ft_glyph_width` does). A skewed
    /// or fractional-em matrix has no FreeType equivalent here.
    fn new(to_em: &Transform, trm: &Transform) -> Option<FtChain> {
        if to_em.b != 0.0 || to_em.c != 0.0 || to_em.e != 0.0 || to_em.f != 0.0 || to_em.d <= 0.0 {
            return None;
        }
        let upem = (1.0 / to_em.d).round();
        if ((1.0 / to_em.d) - upem).abs() > 1e-6 || !(16.0..=16384.0).contains(&upem) {
            return None;
        }
        // A substitute's width stretch rides along in `trm`'s first row.
        let stretch = (to_em.a * upem) as f32;
        let upem = upem as i64;
        // `FT_DivFix(65536, units_per_EM)`: 1024 pixels per em in 26.6.
        let x_scale = ((65536_i64 << 16) + upem / 2) / upem;
        let fixed = |v: f32| (v * 64.0) as i64;
        Some(FtChain {
            x_scale,
            xx: fixed(trm.a as f32 * stretch),
            yx: fixed(trm.b as f32 * stretch),
            xy: fixed(trm.c as f32),
            yy: fixed(trm.d as f32),
            dx: fixed(trm.e as f32),
            dy: fixed(trm.f as f32),
        })
    }

    #[inline]
    fn map(&self, p: Point) -> (i64, i64) {
        let ux = p.x.round() as i64;
        let uy = p.y.round() as i64;
        let sx = mul_fix(ux, self.x_scale);
        let sy = mul_fix(uy, self.x_scale);
        (
            mul_fix(sx, self.xx) + mul_fix(sy, self.xy) + self.dx,
            mul_fix(sx, self.yx) + mul_fix(sy, self.yy) + self.dy,
        )
    }
}

/// Where a glyph lands: its matrix with the translation split into a
/// whole-pixel origin and a quantised subpixel part (`fz_subpixel_adjust`).
pub struct GlyphPlacement {
    pub subpix: Transform,
    qe: u8,
    qf: u8,
    pub x: i32,
    pub y: i32,
    pub size: f32,
}

pub fn glyph_placement(t: &Transform) -> GlyphPlacement {
    let (a, b, c, d) = (t.a as f32, t.b as f32, t.c as f32, t.d as f32);
    let size = (a * d - b * c).abs().sqrt();
    // Subpixel positions along the writing direction: more of them for
    // small text, none once the glyph is big enough not to need them.
    let (q, r) = if size >= 48.0 {
        (0, 0.5_f32)
    } else if size >= 24.0 {
        (128, 0.25)
    } else {
        (192, 0.125)
    };
    // Across it, fewer still.
    let (qmin, rmin) = if size >= 8.0 {
        (0, 0.5_f32)
    } else if size >= 4.0 {
        (128, 0.25)
    } else {
        (192, 0.125)
    };
    let (mut hq, mut hr, mut vq, mut vr) = (q, r, q, r);
    if a == 0.0 && d == 0.0 {
        hq = qmin;
        hr = rmin;
    }
    if b == 0.0 && c == 0.0 {
        vq = qmin;
        vr = rmin;
    }
    let e = t.e as f32 + hr;
    let pix_e = e.floor();
    let f = t.f as f32 + vr;
    let pix_f = f.floor();
    let qe = (((e - pix_e) * 256.0) as i32 & hq) as u8;
    let qf = (((f - pix_f) * 256.0) as i32 & vq) as u8;
    GlyphPlacement {
        subpix: Transform::new(
            t.a,
            t.b,
            t.c,
            t.d,
            f64::from(qe) / 256.0,
            f64::from(qf) / 256.0,
        ),
        qe,
        qf,
        x: pix_e as i32,
        y: pix_f as i32,
        size,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    font: u64,
    gid: u32,
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: u8,
    f: u8,
}

/// A rendered glyph: coverage relative to the glyph origin's pixel, which
/// is `(x, y)` on the device.
pub struct Glyph {
    pub mask: Arc<Mask>,
    pub x: i32,
    pub y: i32,
}

/// Glyph bitmaps shared between the glyphs of a page (`fz_glyph_cache`).
#[derive(Default)]
pub struct GlyphCache {
    entries: HashMap<GlyphKey, Arc<Mask>>,
    bytes: usize,
    renders: u64,
    worker: Worker,
}

impl GlyphCache {
    pub fn new() -> GlyphCache {
        GlyphCache::default()
    }

    /// Number of bitmaps rasterized so far (cache misses).
    pub fn renders(&self) -> u64 {
        self.renders
    }

    /// The bitmap of `outline` (glyph `gid` of `font`, in font units that
    /// `to_em` maps to one-em glyph space) under `trm`, rendered at one of
    /// MuPDF's quantised subpixel positions and placed at a whole pixel.
    /// `None` when the glyph is too large for a bitmap: fill the outline
    /// as a path instead.
    pub fn glyph(
        &mut self,
        font: u64,
        gid: u32,
        outline: &Path,
        to_em: &Transform,
        trm: &Transform,
    ) -> Option<Glyph> {
        let placement = glyph_placement(trm);
        if placement.size > MAX_GLYPH_SIZE
            || [trm.a, trm.b, trm.c, trm.d]
                .iter()
                .any(|v| v.is_nan() || v.abs() > MAX_FT_SCALE)
        {
            return None;
        }
        let m = &placement.subpix;
        let key = GlyphKey {
            font,
            gid,
            a: (m.a as f32 * 65536.0) as i32,
            b: (m.b as f32 * 65536.0) as i32,
            c: (m.c as f32 * 65536.0) as i32,
            d: (m.d as f32 * 65536.0) as i32,
            e: placement.qe,
            f: placement.qf,
        };
        if let Some(mask) = self.entries.get(&key) {
            return Some(Glyph {
                mask: Arc::clone(mask),
                x: placement.x,
                y: placement.y,
            });
        }
        let mut mask = Mask::default();
        let rendered = match FtChain::new(to_em, m) {
            Some(chain) => {
                self.worker
                    .rasterize(outline, &|p| chain.map(p), FillRule::NonZero, &mut mask)
            }
            None => {
                let t = to_em.then(m);
                self.worker.rasterize(
                    outline,
                    &|p| {
                        let q = t.apply(p);
                        (to_26_6(q.x), to_26_6(q.y))
                    },
                    FillRule::NonZero,
                    &mut mask,
                )
            }
        };
        if !rendered {
            return None;
        }
        self.renders += 1;
        let mask = Arc::new(mask);
        let rect = mask.rect();
        if rect.width() < 256 && rect.height() < 256 {
            let bytes = mask.data().len();
            if self.bytes + bytes > MAX_CACHE_BYTES {
                self.entries.clear();
                self.bytes = 0;
            }
            self.bytes += bytes;
            self.entries.insert(key, Arc::clone(&mask));
        }
        Some(Glyph {
            mask,
            x: placement.x,
            y: placement.y,
        })
    }
}
