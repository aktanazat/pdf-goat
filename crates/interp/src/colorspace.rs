//! Colour spaces and their conversion to sRGB, following what PyMuPDF's
//! MuPDF (colour management on) paints:
//!
//! - DeviceGray and DeviceRGB pass through; ICCBased spaces with 1 or 3
//!   components are treated as those.
//! - DeviceCMYK (and 4-component ICCBased) goes through MuPDF's default CMYK
//!   profile, reproduced by a sampled table ([`crate::cmyk_table`]).
//! - CalGray, CalRGB and Lab use their colorimetry: to CIE XYZ, Bradford
//!   adapted to D50 (Lab is D50 already), then to sRGB.
//! - Indexed, Separation and DeviceN go through their base or alternate
//!   space.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use pdf_core::{Dict, Document, ObjRef, Object};

use crate::cmyk_table::{CMYK_GRID, CMYK_TO_RGB};
use crate::device::Rgb;
use crate::function::Function;
use crate::image::ColorFamily;

/// Most components a colour may have.
pub(crate) const MAX_COLORANTS: usize = 32;
const MAX_DEPTH: usize = 8;

pub(crate) type ColorSpaceCache = RefCell<HashMap<ObjRef, Arc<ColorSpace>>>;

#[derive(Debug)]
pub(crate) struct ColorSpace {
    /// The PDF family name: `DeviceGray`, `ICCBased`, `Indexed`, ...
    pub(crate) name: &'static str,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Gray,
    Rgb,
    Cmyk,
    CalGray {
        white: [f64; 3],
        gamma: f64,
    },
    CalRgb {
        white: [f64; 3],
        gamma: [f64; 3],
        matrix: [f64; 9],
    },
    Lab {
        range: [f64; 4],
    },
    Indexed {
        hival: usize,
        palette: Vec<Rgb>,
    },
    Separation {
        alternate: Arc<ColorSpace>,
        tint: Tint,
    },
    DeviceN {
        n: usize,
        alternate: Arc<ColorSpace>,
        tint: Tint,
    },
    Pattern {
        base: Option<Arc<ColorSpace>>,
    },
}

/// A tint transform: one function, or one single-output function per
/// alternate component.
#[derive(Debug)]
struct Tint {
    functions: Vec<Function>,
}

impl Tint {
    fn eval(&self, input: &[f64], out: &mut [f64]) {
        if let [single] = self.functions.as_slice() {
            single.eval(input, out);
        } else {
            for (slot, function) in out.iter_mut().zip(&self.functions) {
                let mut one = [0.0];
                function.eval(input, &mut one);
                *slot = one[0];
            }
        }
    }
}

static GRAY: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "DeviceGray",
        kind: Kind::Gray,
    })
});
static RGB: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "DeviceRGB",
        kind: Kind::Rgb,
    })
});
static CMYK: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "DeviceCMYK",
        kind: Kind::Cmyk,
    })
});
static PATTERN: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "Pattern",
        kind: Kind::Pattern { base: None },
    })
});

impl ColorSpace {
    pub(crate) fn gray() -> Arc<ColorSpace> {
        GRAY.clone()
    }

    pub(crate) fn rgb() -> Arc<ColorSpace> {
        RGB.clone()
    }

    pub(crate) fn cmyk() -> Arc<ColorSpace> {
        CMYK.clone()
    }

    /// Colour components (a pattern space: its base's, 0 without one).
    pub(crate) fn components(&self) -> usize {
        match &self.kind {
            Kind::Gray | Kind::CalGray { .. } | Kind::Indexed { .. } | Kind::Separation { .. } => 1,
            Kind::Rgb | Kind::CalRgb { .. } | Kind::Lab { .. } => 3,
            Kind::Cmyk => 4,
            Kind::DeviceN { n, .. } => *n,
            Kind::Pattern { base } => base.as_ref().map_or(0, |b| b.components()),
        }
    }

    pub(crate) fn is_pattern(&self) -> bool {
        matches!(self.kind, Kind::Pattern { .. })
    }

    /// The underlying space of an uncoloured-pattern space.
    pub(crate) fn pattern_base(&self) -> Option<&Arc<ColorSpace>> {
        match &self.kind {
            Kind::Pattern { base } => base.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn is_indexed(&self) -> bool {
        matches!(self.kind, Kind::Indexed { .. })
    }

    pub(crate) fn family(&self) -> ColorFamily {
        match &self.kind {
            Kind::Gray | Kind::CalGray { .. } => ColorFamily::Gray,
            Kind::Rgb | Kind::CalRgb { .. } => ColorFamily::Rgb,
            Kind::Cmyk => ColorFamily::Cmyk,
            Kind::Lab { .. } => ColorFamily::Lab,
            Kind::Indexed { .. } => ColorFamily::Indexed,
            Kind::Separation { .. } => ColorFamily::Separation,
            Kind::DeviceN { .. } => ColorFamily::DeviceN,
            Kind::Pattern { base } => base.as_ref().map_or(ColorFamily::Rgb, |b| b.family()),
        }
    }

    /// The colour `cs`/`CS` selects (ISO 32000 8.6.8 and MuPDF).
    pub(crate) fn initial_color(&self) -> Vec<f32> {
        match &self.kind {
            Kind::Cmyk => vec![0.0, 0.0, 0.0, 1.0],
            Kind::Separation { .. } | Kind::DeviceN { .. } => vec![1.0; self.components()],
            Kind::Lab { range } => {
                vec![
                    0.0,
                    (0.0f64).clamp(range[0], range[1]) as f32,
                    (0.0f64).clamp(range[2], range[3]) as f32,
                ]
            }
            _ => vec![0.0; self.components()],
        }
    }

    /// The default /Decode array of an image in this space.
    pub(crate) fn default_decode(&self, bpc: u8) -> Vec<[f64; 2]> {
        match &self.kind {
            Kind::Indexed { .. } => vec![[0.0, f64::from((1u32 << bpc.min(16)) - 1)]],
            Kind::Lab { range } => vec![[0.0, 100.0], [range[0], range[1]], [range[2], range[3]]],
            _ => vec![[0.0, 1.0]; self.components()],
        }
    }

    /// Converts component values (Lab: L*, a*, b*; Indexed: the index) to
    /// sRGB in 0..=1.
    pub(crate) fn to_rgb(&self, values: &[f32]) -> Rgb {
        self.to_rgb_depth(values, 0)
    }

    fn to_rgb_depth(&self, v: &[f32], depth: usize) -> Rgb {
        let at = |i: usize| v.get(i).copied().unwrap_or(0.0);
        let unit = |x: f32| if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
        match &self.kind {
            Kind::Gray => {
                let g = unit(at(0));
                [g, g, g]
            }
            Kind::Rgb => [unit(at(0)), unit(at(1)), unit(at(2))],
            Kind::Cmyk => cmyk_to_rgb([unit(at(0)), unit(at(1)), unit(at(2)), unit(at(3))]),
            Kind::CalGray { white, gamma } => {
                let a = f64::from(unit(at(0))).powf(*gamma);
                xyz_to_srgb(adapt_to_d50(
                    [white[0] * a, white[1] * a, white[2] * a],
                    *white,
                ))
            }
            Kind::CalRgb {
                white,
                gamma,
                matrix,
            } => {
                let a = f64::from(unit(at(0))).powf(gamma[0]);
                let b = f64::from(unit(at(1))).powf(gamma[1]);
                let c = f64::from(unit(at(2))).powf(gamma[2]);
                let xyz = [
                    matrix[0] * a + matrix[3] * b + matrix[6] * c,
                    matrix[1] * a + matrix[4] * b + matrix[7] * c,
                    matrix[2] * a + matrix[5] * b + matrix[8] * c,
                ];
                xyz_to_srgb(adapt_to_d50(xyz, *white))
            }
            Kind::Lab { range } => {
                let l = f64::from(at(0)).clamp(0.0, 100.0);
                let a = f64::from(at(1)).clamp(range[0], range[1]);
                let b = f64::from(at(2)).clamp(range[2], range[3]);
                lab_to_rgb(l, a, b)
            }
            Kind::Indexed { palette, hival, .. } => {
                let index = if at(0).is_nan() {
                    0
                } else {
                    at(0).round().clamp(0.0, *hival as f32) as usize
                };
                palette.get(index).copied().unwrap_or([0.0; 3])
            }
            Kind::Separation { alternate, tint }
            | Kind::DeviceN {
                alternate, tint, ..
            } => {
                if depth > MAX_DEPTH {
                    return [0.0; 3];
                }
                let n = self.components().min(MAX_COLORANTS);
                let mut input = [0.0f64; MAX_COLORANTS];
                for (i, slot) in input.iter_mut().enumerate().take(n) {
                    *slot = f64::from(at(i));
                }
                let mut out = [0.0f64; MAX_COLORANTS];
                let m = alternate.components().clamp(1, MAX_COLORANTS);
                tint.eval(&input[..n], &mut out[..m]);
                let mut alt = [0.0f32; MAX_COLORANTS];
                for (slot, value) in alt.iter_mut().zip(&out[..m]) {
                    *slot = *value as f32;
                }
                alternate.to_rgb_depth(&alt[..m], depth + 1)
            }
            Kind::Pattern { base } => match base {
                Some(base) if depth <= MAX_DEPTH => base.to_rgb_depth(v, depth + 1),
                _ => [0.0; 3],
            },
        }
    }

    /// Loads a colour space operand or /ColorSpace value. Names are looked
    /// up in `resources` /ColorSpace when they are not a family name.
    pub(crate) fn load(
        doc: &Document,
        object: &Object,
        resources: Option<&Dict>,
        cache: &ColorSpaceCache,
    ) -> Option<Arc<ColorSpace>> {
        load(doc, object, resources, cache, 0)
    }
}

fn load(
    doc: &Document,
    object: &Object,
    resources: Option<&Dict>,
    cache: &ColorSpaceCache,
    depth: usize,
) -> Option<Arc<ColorSpace>> {
    if depth > MAX_DEPTH {
        return None;
    }
    match object {
        Object::Name(name) => {
            if let Some(space) = family_name(name.as_bytes()) {
                return Some(space);
            }
            let spaces = doc.resolve_dict(resources?.get(b"ColorSpace")?).ok()??;
            let value = spaces.get(name.as_bytes())?;
            load(doc, value, None, cache, depth + 1)
        }
        Object::Reference(id) => {
            if let Some(hit) = cache.borrow().get(id) {
                return Some(hit.clone());
            }
            let resolved = doc.resolve(object).ok()?;
            let space = load(doc, &resolved, resources, cache, depth + 1)?;
            cache.borrow_mut().insert(*id, space.clone());
            Some(space)
        }
        Object::Array(items) => load_array(doc, items, resources, cache, depth),
        _ => None,
    }
}

fn family_name(name: &[u8]) -> Option<Arc<ColorSpace>> {
    Some(match name {
        b"DeviceGray" | b"G" | b"CalGray" => ColorSpace::gray(),
        b"DeviceRGB" | b"RGB" | b"CalRGB" => ColorSpace::rgb(),
        b"DeviceCMYK" | b"CMYK" | b"CalCMYK" => ColorSpace::cmyk(),
        b"Pattern" => PATTERN.clone(),
        _ => return None,
    })
}

fn load_array(
    doc: &Document,
    items: &[Object],
    resources: Option<&Dict>,
    cache: &ColorSpaceCache,
    depth: usize,
) -> Option<Arc<ColorSpace>> {
    let family = doc.resolve_name(items.first()?).ok()??;
    let family = family.as_bytes();
    let dict_at = |i: usize| {
        items
            .get(i)
            .and_then(|o| doc.resolve_dict(o).ok().flatten())
    };
    let numbers = |dict: &Dict, key: &[u8]| -> Option<Vec<f64>> {
        let array = doc.resolve_array(dict.get(key)?).ok()??;
        array
            .iter()
            .map(|o| doc.resolve_f64(o).ok().flatten())
            .collect()
    };
    let white = |dict: &Dict| -> [f64; 3] {
        match numbers(dict, b"WhitePoint").as_deref() {
            Some([x, y, z]) if *y > 0.0 => [*x, *y, *z],
            _ => D50,
        }
    };
    let space = match family {
        b"DeviceGray" | b"G" | b"DeviceRGB" | b"RGB" | b"DeviceCMYK" | b"CMYK" => {
            return family_name(family);
        }
        b"Pattern" if items.len() == 1 => return family_name(family),
        b"CalGray" => {
            let dict = dict_at(1).unwrap_or_default();
            let gamma = dict
                .get(b"Gamma")
                .and_then(|o| doc.resolve_f64(o).ok().flatten())
                .filter(|g| *g > 0.0);
            ColorSpace {
                name: "CalGray",
                kind: Kind::CalGray {
                    white: white(&dict),
                    gamma: gamma.unwrap_or(1.0),
                },
            }
        }
        b"CalRGB" => {
            let dict = dict_at(1).unwrap_or_default();
            let gamma = match numbers(&dict, b"Gamma").as_deref() {
                Some([r, g, b]) if *r > 0.0 && *g > 0.0 && *b > 0.0 => [*r, *g, *b],
                _ => [1.0; 3],
            };
            let matrix = match numbers(&dict, b"Matrix") {
                Some(m) if m.len() == 9 => [m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8]],
                _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            };
            ColorSpace {
                name: "CalRGB",
                kind: Kind::CalRgb {
                    white: white(&dict),
                    gamma,
                    matrix,
                },
            }
        }
        b"Lab" => {
            let dict = dict_at(1).unwrap_or_default();
            let range = match numbers(&dict, b"Range").as_deref() {
                Some([a0, a1, b0, b1]) if a0 <= a1 && b0 <= b1 => [*a0, *a1, *b0, *b1],
                _ => [-100.0, 100.0, -100.0, 100.0],
            };
            ColorSpace {
                name: "Lab",
                kind: Kind::Lab { range },
            }
        }
        b"ICCBased" => {
            let stream = doc.resolve_stream(items.get(1)?).ok()??;
            let n = stream
                .dict
                .get(b"N")
                .and_then(|o| doc.resolve_i64(o).ok().flatten())
                .unwrap_or(0);
            let by_n = match n {
                1 => Some(ColorSpace::gray()),
                3 => Some(ColorSpace::rgb()),
                4 => Some(ColorSpace::cmyk()),
                _ => None,
            };
            let base = match by_n {
                Some(space) => space,
                None => {
                    let alternate = stream.dict.get(b"Alternate")?;
                    load(doc, alternate, resources, cache, depth + 1)?
                }
            };
            ColorSpace {
                name: "ICCBased",
                kind: iccbased_kind(&base)?,
            }
        }
        b"Indexed" | b"I" => {
            let base = load(doc, items.get(1)?, resources, cache, depth + 1)?;
            if base.is_pattern() || base.is_indexed() {
                return None;
            }
            let hival = doc.resolve_i64(items.get(2)?).ok()??.clamp(0, 255) as usize;
            let lookup = match doc.resolve(items.get(3)?).ok()? {
                Object::String(s) => s.bytes,
                Object::Stream(stream) => doc.decode_stream(&stream).ok()?.data,
                _ => return None,
            };
            let n = base.components().max(1);
            let decode = base.default_decode(8);
            let mut palette = Vec::with_capacity(hival + 1);
            let mut comps = [0.0f32; MAX_COLORANTS];
            for entry in 0..=hival {
                for (c, slot) in comps.iter_mut().enumerate().take(n.min(MAX_COLORANTS)) {
                    let byte = lookup.get(entry * n + c).copied().unwrap_or(0);
                    let [d0, d1] = decode.get(c).copied().unwrap_or([0.0, 1.0]);
                    *slot = (d0 + f64::from(byte) / 255.0 * (d1 - d0)) as f32;
                }
                palette.push(base.to_rgb(&comps[..n.min(MAX_COLORANTS)]));
            }
            ColorSpace {
                name: "Indexed",
                kind: Kind::Indexed { hival, palette },
            }
        }
        b"Separation" => {
            let alternate = load(doc, items.get(2)?, resources, cache, depth + 1)?;
            if alternate.is_pattern() {
                return None;
            }
            let tint = load_tint(doc, items.get(3)?)?;
            ColorSpace {
                name: "Separation",
                kind: Kind::Separation { alternate, tint },
            }
        }
        b"DeviceN" => {
            let names = doc.resolve_array(items.get(1)?).ok()??;
            let n = names.len();
            if n == 0 || n > MAX_COLORANTS {
                return None;
            }
            let alternate = load(doc, items.get(2)?, resources, cache, depth + 1)?;
            if alternate.is_pattern() {
                return None;
            }
            let tint = load_tint(doc, items.get(3)?)?;
            ColorSpace {
                name: "DeviceN",
                kind: Kind::DeviceN { n, alternate, tint },
            }
        }
        b"Pattern" => {
            let base = match items.get(1) {
                Some(object) => Some(load(doc, object, resources, cache, depth + 1)?),
                None => None,
            };
            ColorSpace {
                name: "Pattern",
                kind: Kind::Pattern { base },
            }
        }
        _ => return None,
    };
    Some(Arc::new(space))
}

/// An ICCBased space behaves as its device equivalent.
fn iccbased_kind(base: &ColorSpace) -> Option<Kind> {
    Some(match &base.kind {
        Kind::Gray | Kind::CalGray { .. } => Kind::Gray,
        Kind::Rgb | Kind::CalRgb { .. } => Kind::Rgb,
        Kind::Cmyk => Kind::Cmyk,
        Kind::Lab { range } => Kind::Lab { range: *range },
        _ => return None,
    })
}

fn load_tint(doc: &Document, object: &Object) -> Option<Tint> {
    let resolved = doc.resolve(object).ok()?;
    let functions = match &resolved {
        Object::Array(items) => {
            let mut functions = Vec::with_capacity(items.len());
            for item in items.iter().take(MAX_COLORANTS) {
                functions.push(Function::load(doc, item)?);
            }
            functions
        }
        _ => vec![Function::load(doc, object)?],
    };
    (!functions.is_empty()).then_some(Tint { functions })
}

// ----- conversions -----------------------------------------------------------

const D50: [f64; 3] = [0.9642, 1.0, 0.8249];

/// MuPDF's default CMYK → sRGB: quadrilinear interpolation in the sampled
/// grid.
pub(crate) fn cmyk_to_rgb(cmyk: [f32; 4]) -> Rgb {
    let steps = (CMYK_GRID - 1) as f32;
    let mut base = [0usize; 4];
    let mut frac = [0.0f32; 4];
    for i in 0..4 {
        let x = cmyk[i].clamp(0.0, 1.0) * steps;
        let lower = x.floor().min(steps - 1.0);
        base[i] = lower as usize;
        frac[i] = x - lower;
    }
    let mut rgb = [0.0f32; 3];
    for corner in 0..16usize {
        let mut weight = 1.0f32;
        let mut index = 0usize;
        for i in 0..4 {
            let high = (corner >> (3 - i)) & 1;
            weight *= if high == 1 { frac[i] } else { 1.0 - frac[i] };
            index = index * CMYK_GRID + base[i] + high;
        }
        if weight == 0.0 {
            continue;
        }
        let at = index * 3;
        for (c, slot) in rgb.iter_mut().enumerate() {
            *slot += weight * f32::from(CMYK_TO_RGB[at + c]);
        }
    }
    rgb.map(|v| (v / 255.0).clamp(0.0, 1.0))
}

/// Bradford chromatic adaptation from `white` to D50.
fn adapt_to_d50(xyz: [f64; 3], white: [f64; 3]) -> [f64; 3] {
    const M: [[f64; 3]; 3] = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    const M_INV: [[f64; 3]; 3] = [
        [0.9869929, -0.1470543, 0.1599627],
        [0.4323053, 0.5183603, 0.0492912],
        [-0.0085287, 0.0400428, 0.9684867],
    ];
    let mul = |m: &[[f64; 3]; 3], v: [f64; 3]| {
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    };
    let src = mul(&M, white);
    let dst = mul(&M, D50);
    if src.iter().any(|v| v.abs() < 1e-9) {
        return xyz;
    }
    let cone = mul(&M, xyz);
    let scaled = [
        cone[0] * dst[0] / src[0],
        cone[1] * dst[1] / src[1],
        cone[2] * dst[2] / src[2],
    ];
    mul(&M_INV, scaled)
}

/// D50 XYZ → sRGB (D50-adapted sRGB matrix), gamma encoded.
fn xyz_to_srgb(xyz: [f64; 3]) -> Rgb {
    let [x, y, z] = xyz;
    let r = 3.1338561 * x - 1.6168667 * y - 0.4906146 * z;
    let g = -0.9787684 * x + 1.9161415 * y + 0.0334540 * z;
    let b = 0.0719453 * x - 0.2289914 * y + 1.4052427 * z;
    [srgb_encode(r), srgb_encode(g), srgb_encode(b)]
}

fn srgb_encode(v: f64) -> f32 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    let e = if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    e as f32
}

/// CIE L*a*b* (D50) → sRGB.
fn lab_to_rgb(l: f64, a: f64, b: f64) -> Rgb {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let finv = |t: f64| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    xyz_to_srgb([D50[0] * finv(fx), D50[1] * finv(fy), D50[2] * finv(fz)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(rgb: Rgb) -> [u8; 3] {
        rgb.map(|v| (v * 255.0).round() as u8)
    }

    #[test]
    fn cmyk_matches_mupdf_default_profile_at_grid_points() {
        // Values PyMuPDF 1.27 paints for these fills.
        assert_eq!(bytes(cmyk_to_rgb([1.0, 0.0, 0.0, 0.0])), [0, 173, 239]);
        assert_eq!(bytes(cmyk_to_rgb([0.0, 0.0, 0.0, 1.0])), [34, 31, 31]);
        assert_eq!(bytes(cmyk_to_rgb([0.0, 1.0, 1.0, 0.0])), [237, 28, 36]);
        assert_eq!(bytes(cmyk_to_rgb([0.0, 0.0, 0.0, 0.0])), [255, 255, 255]);
    }

    #[test]
    fn lab_converts_through_d50_to_srgb() {
        // PyMuPDF paints Lab (50, 20, -30) as (131, 107, 170).
        let rgb = bytes(lab_to_rgb(50.0, 20.0, -30.0));
        for (got, want) in rgb.iter().zip([131u8, 107, 170]) {
            assert!(got.abs_diff(want) <= 3, "{rgb:?}");
        }
    }
}
