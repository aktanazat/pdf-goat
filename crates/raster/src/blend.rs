//! PDF blend modes and row compositing on premultiplied RGBA8.
//!
//! Normal-mode integer arithmetic is ported from MuPDF's
//! `source/fitz/draw-paint.c` and `draw-blend.c` (Artifex, AGPL-3.0).
//! Solid spans, pixel spans and non-isolated groups have distinct rounding.

use crate::pixmap::mul255;

/// A byte as a weight in `0..=256` (`FZ_EXPAND`).
#[inline]
pub(crate) fn expand(a: u8) -> u32 {
    let a = u32::from(a);
    a + (a >> 7)
}

/// The product of two weights in `0..=256` (`FZ_COMBINE`).
#[inline]
pub(crate) fn combine(a: u32, b: u32) -> u32 {
    (a * b) >> 8
}

/// `d` moved towards `s` by the weight `m` in `0..=256` (`FZ_BLEND`).
#[inline]
pub(crate) fn lerp(s: u8, d: u8, m: u32) -> u8 {
    let (s, d) = (i32::from(s), i32::from(d));
    (((s - d) * m as i32 + (d << 8)) >> 8) as u8
}

/// Solid-colour alpha through coverage (`fz_paint_span_with_color`).
#[inline]
pub(crate) fn weight(cov: u8, alpha: u8) -> u32 {
    let ma = expand(cov);
    if alpha == 255 {
        ma
    } else {
        combine(ma, expand(alpha))
    }
}

/// `template_span_with_color_N_general`, with straight colour.
#[inline]
fn solid_pixel(d: &mut [u8; 4], color: [u8; 3], ma: u32) {
    if ma == 256 {
        *d = [color[0], color[1], color[2], 255];
        return;
    }
    for k in 0..3 {
        d[k] = lerp(color[k], d[k], ma);
    }
    d[3] = lerp(255, d[3], ma);
}

/// `template_span_N_general` / `template_span_N_with_alpha_general`.
/// An opaque destination stays opaque, like MuPDF's alpha-less page.
#[inline]
pub(crate) fn over_pixel(d: &mut [u8; 4], s: [u8; 4], alpha: u8) {
    let opaque = d[3] == 255;
    if alpha == 255 {
        let t = expand(s[3]);
        if t == 0 {
            return;
        }
        let t = 256 - t;
        if t == 0 {
            *d = s;
            return;
        }
        for k in 0..3 {
            d[k] = (u32::from(s[k]) + combine(u32::from(d[k]), t)).min(255) as u8;
        }
        if !opaque {
            d[3] = (u32::from(s[3]) + combine(u32::from(d[3]), t)).min(255) as u8;
        }
    } else {
        let a = expand(alpha);
        let masa = combine(u32::from(s[3]), a);
        let t = expand((255 - masa) as u8);
        for k in 0..3 {
            d[k] = (combine(u32::from(s[k]), a) + combine(u32::from(d[k]), t)).min(255) as u8;
        }
        if !opaque {
            d[3] = (masa + combine(u32::from(d[3]), t)).min(255) as u8;
        }
    }
}

/// [`over_pixel`] on a contribution-alpha plane (not page opacity).
#[inline]
pub(crate) fn over_alpha(d: u8, s: u8, alpha: u8) -> u8 {
    if alpha == 255 {
        let t = expand(s);
        if t == 0 {
            return d;
        }
        (u32::from(s) + combine(u32::from(d), 256 - t)).min(255) as u8
    } else {
        let masa = combine(u32::from(s), expand(alpha));
        (masa + combine(u32::from(d), expand((255 - masa) as u8))).min(255) as u8
    }
}

/// `fz_blend_separable_nonisolated`, Normal mode: `s` includes the
/// backdrop `d`; `ha` is the group's contribution alpha.
#[inline]
fn nonisolated_pixel(d: &mut [u8; 4], s: [u8; 4], ha: u8, alpha: u8) {
    let haa = mul255(u32::from(ha), u32::from(alpha));
    let sa = u32::from(s[3]);
    if haa == 0 || sa == 0 {
        return;
    }
    let invsa = 255 * 256 / sa;
    let ba = u32::from(d[3]);
    if ba == 0 {
        for k in 0..3 {
            d[k] = mul255((u32::from(s[k]) * invsa) >> 8, haa) as u8;
        }
        d[3] = haa as u8;
        return;
    }
    let invba = 255 * 256 / ba;
    let scale = ((512 * ba + u32::from(ha)) / (u32::from(ha) * 2)) as i32 - expand(d[3]) as i32;
    let bahaa = mul255(ba, haa);
    let ra0 = ba - bahaa;
    let ra = ra0 + haa;
    d[3] = ra as u8;
    for k in 0..3 {
        let sc = ((u32::from(s[k]) * invsa) >> 8) as i32;
        let bc = ((u32::from(d[k]) * invba) >> 8) as i32;
        let sc = (sc + (((sc - bc) * scale) >> 8)).clamp(0, 255) as u32;
        let mut rc = sc;
        if bahaa != 255 {
            rc = mul255(bahaa, rc);
        }
        if ba != 255 {
            rc += mul255(mul255(255 - ba, haa), sc);
        }
        if ra0 != 0 {
            rc += mul255(ra0, bc as u32);
        }
        d[k] = rc.min(ra) as u8;
    }
}

/// PDF blend modes (ISO 32000-2, 11.3.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    /// Maps a PDF `/BM` name; `Compatible` is `Normal`. Unknown names give `None`.
    pub fn from_pdf_name(name: &str) -> Option<BlendMode> {
        Some(match name {
            "Normal" | "Compatible" => BlendMode::Normal,
            "Multiply" => BlendMode::Multiply,
            "Screen" => BlendMode::Screen,
            "Overlay" => BlendMode::Overlay,
            "Darken" => BlendMode::Darken,
            "Lighten" => BlendMode::Lighten,
            "ColorDodge" => BlendMode::ColorDodge,
            "ColorBurn" => BlendMode::ColorBurn,
            "HardLight" => BlendMode::HardLight,
            "SoftLight" => BlendMode::SoftLight,
            "Difference" => BlendMode::Difference,
            "Exclusion" => BlendMode::Exclusion,
            "Hue" => BlendMode::Hue,
            "Saturation" => BlendMode::Saturation,
            "Color" => BlendMode::Color,
            "Luminosity" => BlendMode::Luminosity,
            _ => return None,
        })
    }

    /// True for modes that act on each colour channel independently.
    pub fn is_separable(self) -> bool {
        !matches!(
            self,
            BlendMode::Hue | BlendMode::Saturation | BlendMode::Color | BlendMode::Luminosity
        )
    }
}

/// Source colours for one row.
#[derive(Clone, Copy)]
pub(crate) enum SrcRow<'a> {
    /// One straight colour and constant alpha for every pixel.
    Solid { color: [u8; 3], alpha: u8 },
    /// One premultiplied colour per pixel, 4 bytes each.
    Pixels(&'a [u8]),
    /// A non-isolated group's pixels and contribution-alpha plane.
    Backdrop { pixels: &'a [u8], alpha: &'a [u8] },
}

#[inline]
fn pixel(p: &[u8], i: usize) -> [u8; 4] {
    [p[4 * i], p[4 * i + 1], p[4 * i + 2], p[4 * i + 3]]
}

impl SrcRow<'_> {
    /// One pixel without allocating a temporary row.
    pub(crate) fn at(&self, i: usize) -> SrcRow<'_> {
        match *self {
            SrcRow::Solid { color, alpha } => SrcRow::Solid { color, alpha },
            SrcRow::Pixels(p) => SrcRow::Pixels(&p[4 * i..4 * i + 4]),
            SrcRow::Backdrop { pixels, alpha } => SrcRow::Backdrop {
                pixels: &pixels[4 * i..4 * i + 4],
                alpha: &alpha[i..i + 1],
            },
        }
    }

    #[inline]
    pub(crate) fn premultiplied(&self, i: usize, dst: [u8; 4]) -> [u8; 4] {
        match *self {
            SrcRow::Solid { color, alpha } => {
                let a = u32::from(alpha);
                [
                    mul255(u32::from(color[0]), a) as u8,
                    mul255(u32::from(color[1]), a) as u8,
                    mul255(u32::from(color[2]), a) as u8,
                    alpha,
                ]
            }
            SrcRow::Pixels(p) => pixel(p, i),
            SrcRow::Backdrop { pixels, alpha } => {
                let a = alpha[i];
                let p = pixel(pixels, i);
                let mut out = [0; 4];
                for k in 0..3 {
                    let backdrop = mul255(u32::from(dst[k]), 255 - u32::from(a)) as u8;
                    out[k] = p[k].saturating_sub(backdrop).min(a);
                }
                out[3] = a;
                out
            }
        }
    }

    #[inline]
    pub(crate) fn over_alpha(&self, i: usize, cov: u8, d: u8) -> u8 {
        match *self {
            SrcRow::Solid { alpha, .. } => lerp(255, d, weight(cov, alpha)),
            SrcRow::Pixels(p) => over_alpha(d, p[4 * i + 3], cov),
            SrcRow::Backdrop { alpha, .. } => over_alpha(d, alpha[i], cov),
        }
    }
}

/// Composites `src` scaled by per-pixel coverage `cov` onto `dst`
/// (`4 * cov.len()` bytes).
pub(crate) fn blend_row(dst: &mut [u8], src: SrcRow<'_>, cov: &[u8], mode: BlendMode) {
    let dst = dst.as_chunks_mut::<4>().0;
    if mode == BlendMode::Normal {
        match src {
            SrcRow::Solid { color, alpha } => {
                for (d, &c) in dst.iter_mut().zip(cov) {
                    let ma = weight(c, alpha);
                    if ma != 0 {
                        solid_pixel(d, color, ma);
                    }
                }
            }
            SrcRow::Pixels(p) => {
                for ((d, s), &c) in dst.iter_mut().zip(p.as_chunks::<4>().0).zip(cov) {
                    if c != 0 {
                        over_pixel(d, *s, c);
                    }
                }
            }
            SrcRow::Backdrop { pixels, alpha } => {
                for (((d, s), &ha), &c) in dst
                    .iter_mut()
                    .zip(pixels.as_chunks::<4>().0)
                    .zip(alpha)
                    .zip(cov)
                {
                    if c != 0 {
                        nonisolated_pixel(d, *s, ha, c);
                    }
                }
            }
        }
        return;
    }
    for (i, (d, &c)) in dst.iter_mut().zip(cov).enumerate() {
        if c == 0 {
            continue;
        }
        let s = src.premultiplied(i, *d);
        if s[3] == 0 {
            continue;
        }
        blend_pixel(d, scale(s, u32::from(c)), mode);
    }
}

#[inline]
pub(crate) fn scale(s: [u8; 4], c: u32) -> [u8; 4] {
    if c == 255 {
        s
    } else {
        s.map(|v| mul255(u32::from(v), c) as u8)
    }
}

/// General PDF compositing formula for one pixel:
/// `co = (1 - ab) cs + (1 - as) cb + as ab B(Cb, Cs)`, `ao = as + ab - as ab`.
pub(crate) fn blend_pixel(d: &mut [u8; 4], s: [u8; 4], mode: BlendMode) {
    if s[3] == 0 {
        return;
    }
    if mode == BlendMode::Normal {
        over_pixel(d, s, 255);
        return;
    }
    let sa = f32::from(s[3]) / 255.0;
    let da = f32::from(d[3]) / 255.0;
    let sp = [s[0], s[1], s[2]].map(|v| f32::from(v) / 255.0);
    let dp = [d[0], d[1], d[2]].map(|v| f32::from(v) / 255.0);
    if da == 0.0 {
        *d = s;
        return;
    }
    let cs = sp.map(|v| (v / sa).min(1.0));
    let cb = dp.map(|v| (v / da).min(1.0));
    let b = blend_colors(cb, cs, mode);
    let ao = sa + da - sa * da;
    for k in 0..3 {
        let co = (1.0 - da) * sp[k] + (1.0 - sa) * dp[k] + sa * da * b[k];
        d[k] = to_byte(co.min(ao));
    }
    d[3] = to_byte(ao);
}

/// MuPDF's `fz_blend_knockout`: `s` was painted against the group's
/// initial backdrop; replace the previous sibling according to shape.
pub(crate) fn knockout_pixel(d: &mut [u8; 4], s: [u8; 4], shape: u8) {
    if shape == 0 {
        return;
    }
    if d[3] == 0 && shape == 255 {
        *d = s;
        return;
    }
    let (sa, ba, ha) = (u32::from(s[3]), u32::from(d[3]), u32::from(shape));
    let invsa = (255_u32 * 256).checked_div(sa).unwrap_or(0);
    let invba = (255_u32 * 256).checked_div(ba).unwrap_or(0);
    let ra = mul255(ha, sa) + mul255(255 - ha, ba);
    for k in 0..3 {
        let sc = (u32::from(s[k]) * invsa) >> 8;
        let bc = (u32::from(d[k]) * invba) >> 8;
        let rc = mul255(255 - ha, bc) + mul255(ha, sc);
        d[k] = mul255(ra, rc) as u8;
    }
    d[3] = ra as u8;
}

fn to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// `B(Cb, Cs)` on straight colours in `0..=1`.
pub(crate) fn blend_colors(cb: [f32; 3], cs: [f32; 3], mode: BlendMode) -> [f32; 3] {
    match mode {
        BlendMode::Hue => set_lum(set_sat(cs, sat(cb)), lum(cb)),
        BlendMode::Saturation => set_lum(set_sat(cb, sat(cs)), lum(cb)),
        BlendMode::Color => set_lum(cs, lum(cb)),
        BlendMode::Luminosity => set_lum(cb, lum(cs)),
        _ => [0, 1, 2].map(|k| separable(cb[k], cs[k], mode)),
    }
}

fn separable(cb: f32, cs: f32, mode: BlendMode) -> f32 {
    match mode {
        BlendMode::Multiply => cb * cs,
        BlendMode::Screen => cb + cs - cb * cs,
        BlendMode::Overlay => hard_light(cs, cb),
        BlendMode::Darken => cb.min(cs),
        BlendMode::Lighten => cb.max(cs),
        BlendMode::ColorDodge => {
            if cb <= 0.0 {
                0.0
            } else if cs >= 1.0 {
                1.0
            } else {
                (cb / (1.0 - cs)).min(1.0)
            }
        }
        BlendMode::ColorBurn => {
            if cb >= 1.0 {
                1.0
            } else if cs <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - cb) / cs).min(1.0)
            }
        }
        BlendMode::HardLight => hard_light(cb, cs),
        BlendMode::SoftLight => {
            if cs <= 0.5 {
                cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
            } else {
                let d = if cb <= 0.25 {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                } else {
                    cb.sqrt()
                };
                cb + (2.0 * cs - 1.0) * (d - cb)
            }
        }
        BlendMode::Difference => (cb - cs).abs(),
        BlendMode::Exclusion => cb + cs - 2.0 * cb * cs,
        _ => cs,
    }
}

fn hard_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        cb * 2.0 * cs
    } else {
        let t = 2.0 * cs - 1.0;
        cb + t - cb * t
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut out = c;
    if n < 0.0 && l - n > 0.0 {
        out = out.map(|v| l + (v - l) * l / (l - n));
    }
    if x > 1.0 && x - l > 0.0 {
        out = out.map(|v| l + (v - l) * (1.0 - l) / (x - l));
    }
    out
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color(c.map(|v| v + d))
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&i, &j| c[i].total_cmp(&c[j]));
    let [min_i, mid_i, max_i] = idx;
    let mut out = [0.0; 3];
    if c[max_i] > c[min_i] {
        out[mid_i] = (c[mid_i] - c[min_i]) * s / (c[max_i] - c[min_i]);
        out[max_i] = s;
    }
    out
}
