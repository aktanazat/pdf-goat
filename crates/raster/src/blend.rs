//! PDF blend modes and row compositing on premultiplied RGBA8.

use crate::pixmap::mul255;

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
    /// One premultiplied colour for every pixel.
    Solid([u8; 4]),
    /// One premultiplied colour per pixel, 4 bytes each.
    Pixels(&'a [u8]),
}

impl SrcRow<'_> {
    #[inline]
    pub(crate) fn get(&self, i: usize) -> [u8; 4] {
        match self {
            SrcRow::Solid(c) => *c,
            SrcRow::Pixels(p) => [p[4 * i], p[4 * i + 1], p[4 * i + 2], p[4 * i + 3]],
        }
    }
}

/// Composites `src` scaled by per-pixel coverage `cov` onto `dst`
/// (`4 * cov.len()` bytes).
pub(crate) fn blend_row(dst: &mut [u8], src: SrcRow<'_>, cov: &[u8], mode: BlendMode) {
    if mode == BlendMode::Normal {
        source_over_row(dst, src, cov);
        return;
    }
    for (i, (d, &c)) in dst.as_chunks_mut::<4>().0.iter_mut().zip(cov).enumerate() {
        if c == 0 {
            continue;
        }
        let s = src.get(i);
        if s[3] == 0 {
            continue;
        }
        let s = scale(s, u32::from(c));
        blend_pixel(d, s, mode);
    }
}

#[inline]
fn scale(s: [u8; 4], c: u32) -> [u8; 4] {
    if c == 255 {
        s
    } else {
        s.map(|v| mul255(u32::from(v), c) as u8)
    }
}

fn source_over_row(dst: &mut [u8], src: SrcRow<'_>, cov: &[u8]) {
    for (i, (d, &c)) in dst.as_chunks_mut::<4>().0.iter_mut().zip(cov).enumerate() {
        if c == 0 {
            continue;
        }
        let s = scale(src.get(i), u32::from(c));
        match s[3] {
            0 => {}
            255 => *d = s,
            sa => {
                let inv = 255 - u32::from(sa);
                for k in 0..4 {
                    d[k] = (u32::from(s[k]) + mul255(u32::from(d[k]), inv)).min(255) as u8;
                }
            }
        }
    }
}

/// General PDF compositing formula for one pixel:
/// `co = (1 - ab) cs + (1 - as) cb + as ab B(Cb, Cs)`, `ao = as + ab - as ab`.
pub(crate) fn blend_pixel(d: &mut [u8], s: [u8; 4], mode: BlendMode) {
    if s[3] == 0 {
        return;
    }
    if mode == BlendMode::Normal {
        let inv = 255 - u32::from(s[3]);
        for k in 0..4 {
            d[k] = (u32::from(s[k]) + mul255(u32::from(d[k]), inv)).min(255) as u8;
        }
        return;
    }
    let sa = f32::from(s[3]) / 255.0;
    let da = f32::from(d[3]) / 255.0;
    let sp = [s[0], s[1], s[2]].map(|v| f32::from(v) / 255.0);
    let dp = [d[0], d[1], d[2]].map(|v| f32::from(v) / 255.0);
    if da == 0.0 {
        d.copy_from_slice(&s);
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

/// A knockout element blends with the initial backdrop, then replaces the
/// previous result by its shape, not by its opacity. `s` already includes
/// shape coverage, so subtract the initial backdrop's unpainted share.
pub(crate) fn knockout_pixel(
    d: &mut [u8],
    initial: [u8; 4],
    s: [u8; 4],
    shape: u8,
    mode: BlendMode,
) {
    let mut result = initial;
    blend_pixel(&mut result, s, mode);
    let keep = i32::from(255 - shape);
    for k in 0..4 {
        let delta = keep * (i32::from(d[k]) - i32::from(initial[k]));
        let correction = (delta + 127 * delta.signum()) / 255;
        d[k] = (i32::from(result[k]) + correction).clamp(0, 255) as u8;
    }
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
