//! Colour spaces and their conversion to sRGB, following what PyMuPDF's
//! MuPDF (colour management on) paints:
//!
//! - DeviceRGB passes through; an ICCBased space whose profile is MuPDF's
//!   own sRGB profile does too.
//! - DeviceGray, DeviceCMYK and Lab go through MuPDF's default ICC
//!   profiles; CalGray and CalRGB through the profile MuPDF synthesises
//!   from their dictionaries; other ICCBased spaces through their embedded
//!   profile. Every link is built by [`crate::icc`] the way Little-CMS
//!   builds it for MuPDF: 16-bit for fills, 8-bit for image samples.
//! - Indexed, Separation and DeviceN go through their base or alternate
//!   space.
//! - A fill's rendering intent and black point compensation
//!   ([`ColorParams`]) pick the link; a page's or form's `DefaultGray`,
//!   `DefaultRGB`, `DefaultCMYK` and the document's output intent
//!   ([`DefaultSpaces`]) replace the device spaces.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use pdf_core::{Dict, Document, ObjRef, Object};

use crate::device::Rgb;
use crate::function::Function;
use crate::icc::{self, Intent, Profile, Transform};
use crate::image::ColorFamily;

/// Most components a colour may have.
pub(crate) const MAX_COLORANTS: usize = 32;
const MAX_DEPTH: usize = 8;

pub(crate) type ColorSpaceCache = RefCell<HashMap<ObjRef, Arc<ColorSpace>>>;

/// MuPDF's `fz_color_params` as colour conversion sees them: the rendering
/// intent and black point compensation of a fill, stroke or image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ColorParams {
    pub(crate) intent: Intent,
    pub(crate) bpc: bool,
}

impl ColorParams {
    /// `fz_default_color_params`: relative colorimetric with compensation.
    pub(crate) const DEFAULT: ColorParams = ColorParams {
        intent: Intent::RelativeColorimetric,
        bpc: true,
    };
}

impl Default for ColorParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug)]
pub(crate) struct ColorSpace {
    /// The PDF family name: `DeviceGray`, `ICCBased`, `Indexed`, ...
    pub(crate) name: &'static str,
    kind: Kind,
    /// One of MuPDF's device spaces (`FZ_COLORSPACE_IS_DEVICE`): what a
    /// page's default colour spaces replace.
    device: bool,
}

#[derive(Debug)]
enum Kind {
    Gray,
    Rgb,
    Cmyk,
    Icc {
        n: usize,
        /// Set for Lab profiles: the a*/b* clamp range.
        lab: Option<[f64; 4]>,
        profile: Arc<Profile>,
        /// The 16-bit link with the default colour parameters.
        link: Arc<Transform>,
    },
    Lab {
        range: [f64; 4],
    },
    Indexed {
        hival: usize,
        base: Arc<ColorSpace>,
        lookup: Vec<u8>,
        /// The entries through `base` with the default colour parameters.
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
        device: true,
    })
});
static RGB: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "DeviceRGB",
        kind: Kind::Rgb,
        device: true,
    })
});
static CMYK: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "DeviceCMYK",
        kind: Kind::Cmyk,
        device: true,
    })
});
static SRGB: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "DeviceRGB",
        kind: Kind::Rgb,
        device: false,
    })
});
static PATTERN: LazyLock<Arc<ColorSpace>> = LazyLock::new(|| {
    Arc::new(ColorSpace {
        name: "Pattern",
        kind: Kind::Pattern { base: None },
        device: false,
    })
});

static GRAY_LINK: LazyLock<Arc<Transform>> =
    LazyLock::new(|| device_link(&icc::DEVICE_GRAY, false));
static CMYK_LINK: LazyLock<Arc<Transform>> =
    LazyLock::new(|| device_link(&icc::DEVICE_CMYK, false));
static LAB_LINK: LazyLock<Arc<Transform>> = LazyLock::new(|| device_link(&icc::DEVICE_LAB, false));
static GRAY_LINK8: LazyLock<Arc<Transform>> =
    LazyLock::new(|| device_link(&icc::DEVICE_GRAY, true));
static CMYK_LINK8: LazyLock<Arc<Transform>> =
    LazyLock::new(|| device_link(&icc::DEVICE_CMYK, true));
static LAB_LINK8: LazyLock<Arc<Transform>> = LazyLock::new(|| device_link(&icc::DEVICE_LAB, true));

/// A link to device RGB with MuPDF's default colour parameters: relative
/// colorimetric intent with black point compensation.
fn device_link(profile: &Arc<Profile>, bits8: bool) -> Arc<Transform> {
    icc::link_to_rgb(profile, Intent::RelativeColorimetric, true, bits8)
        .expect("MuPDF's bundled profiles link to device RGB")
}

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

    /// RGB that is already converted: painted as is, never replaced by a
    /// page's DefaultRGB.
    pub(crate) fn srgb() -> Arc<ColorSpace> {
        SRGB.clone()
    }

    /// The ICC profile a colour in this space goes through, when it has one.
    fn profile(&self) -> Option<&Arc<Profile>> {
        match &self.kind {
            Kind::Gray => Some(&icc::DEVICE_GRAY),
            Kind::Rgb => Some(&icc::DEVICE_RGB),
            Kind::Cmyk => Some(&icc::DEVICE_CMYK),
            Kind::Lab { .. } => Some(&icc::DEVICE_LAB),
            Kind::Icc { profile, .. } => Some(profile),
            _ => None,
        }
    }

    /// The profile a colour leaves through towards `dest`: inside a
    /// luminosity soft mask (`FZ_RI_IN_SOFTMASK`) every gray, RGB or CMYK
    /// space goes through MuPDF's PostScript profile for its family.
    fn source_profile(&self, dest: Dest<'_>) -> Option<&Arc<Profile>> {
        if !dest.soft_mask {
            return self.profile();
        }
        match self.family() {
            ColorFamily::Gray => Some(&icc::PS_GRAY),
            ColorFamily::Rgb => Some(&icc::PS_RGB),
            ColorFamily::Cmyk => Some(&icc::PS_CMYK),
            _ => self.profile(),
        }
    }

    fn lab_range(&self) -> Option<[f64; 4]> {
        match &self.kind {
            Kind::Lab { range } => Some(*range),
            Kind::Icc { lab, .. } => *lab,
            _ => None,
        }
    }

    /// The link to device RGB for `params`, 16-bit or 8-bit: the one built
    /// at load for the defaults, else the cached link for that intent.
    fn link(&self, params: ColorParams, bits8: bool) -> Option<Arc<Transform>> {
        if params == ColorParams::DEFAULT {
            let link = match (&self.kind, bits8) {
                (Kind::Gray, false) => &*GRAY_LINK,
                (Kind::Gray, true) => &*GRAY_LINK8,
                (Kind::Cmyk, false) => &*CMYK_LINK,
                (Kind::Cmyk, true) => &*CMYK_LINK8,
                (Kind::Lab { .. }, false) => &*LAB_LINK,
                (Kind::Lab { .. }, true) => &*LAB_LINK8,
                (Kind::Icc { link, .. }, false) => link,
                (Kind::Icc { profile, .. }, true) => {
                    return icc::link_to_rgb(profile, params.intent, params.bpc, true);
                }
                _ => return None,
            };
            return Some(Arc::clone(link));
        }
        icc::link_to_rgb(self.profile()?, params.intent, params.bpc, bits8)
    }

    /// The link into `dest` for `params`: the device RGB links for the
    /// page, the cached link between the two profiles otherwise.
    fn link_to(&self, params: ColorParams, dest: Dest<'_>, bits8: bool) -> Option<Arc<Transform>> {
        match dest.profile {
            None => self.link(params, bits8),
            Some(dst) => icc::link(
                self.source_profile(dest)?,
                dst,
                params.intent,
                params.bpc,
                bits8,
            ),
        }
    }

    /// Colour components (a pattern space: its base's, 0 without one).
    pub(crate) fn components(&self) -> usize {
        match &self.kind {
            Kind::Gray | Kind::Indexed { .. } | Kind::Separation { .. } => 1,
            Kind::Rgb | Kind::Lab { .. } => 3,
            Kind::Cmyk => 4,
            Kind::Icc { n, .. } => *n,
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
            Kind::Gray => ColorFamily::Gray,
            Kind::Rgb => ColorFamily::Rgb,
            Kind::Cmyk => ColorFamily::Cmyk,
            Kind::Lab { .. } | Kind::Icc { lab: Some(_), .. } => ColorFamily::Lab,
            Kind::Icc { n: 1, .. } => ColorFamily::Gray,
            Kind::Icc { n: 4, .. } => ColorFamily::Cmyk,
            Kind::Icc { .. } => ColorFamily::Rgb,
            Kind::Indexed { .. } => ColorFamily::Indexed,
            Kind::Separation { .. } => ColorFamily::Separation,
            Kind::DeviceN { .. } => ColorFamily::DeviceN,
            Kind::Pattern { base } => base.as_ref().map_or(ColorFamily::Rgb, |b| b.family()),
        }
    }

    /// The colour `cs`/`CS` selects (ISO 32000 8.6.8 and MuPDF).
    pub(crate) fn initial_color(&self) -> Vec<f32> {
        match &self.kind {
            Kind::Cmyk | Kind::Icc { n: 4, .. } => vec![0.0, 0.0, 0.0, 1.0],
            Kind::Separation { .. } | Kind::DeviceN { .. } => vec![1.0; self.components()],
            Kind::Lab { range }
            | Kind::Icc {
                lab: Some(range), ..
            } => {
                vec![
                    0.0,
                    (0.0f64).clamp(range[0], range[1]) as f32,
                    (0.0f64).clamp(range[2], range[3]) as f32,
                ]
            }
            _ => vec![0.0; self.components()],
        }
    }

    /// The default /Decode array of an image in this space. Lab samples
    /// decode to the 8-bit Lab encoding (`pdf_load_image_imp`: 0..100 and
    /// -128..127 whatever the space's /Range).
    pub(crate) fn default_decode(&self, bpc: u8) -> Vec<[f64; 2]> {
        match &self.kind {
            Kind::Indexed { .. } => vec![[0.0, f64::from((1u32 << bpc.min(16)) - 1)]],
            Kind::Lab { .. } | Kind::Icc { lab: Some(_), .. } => {
                vec![[0.0, 100.0], [-128.0, 127.0], [-128.0, 127.0]]
            }
            _ => vec![[0.0, 1.0]; self.components()],
        }
    }

    /// Converts component values (Lab: L*, a*, b*; Indexed: the index) to
    /// sRGB in 0..=1 with the default colour parameters.
    pub(crate) fn to_rgb(&self, values: &[f32]) -> Rgb {
        self.to_rgb_depth(values, ColorParams::DEFAULT, 0)
    }

    /// [`ColorSpace::to_rgb`] with a fill's rendering intent and black
    /// point compensation (`fz_convert_color` with its `fz_color_params`).
    pub(crate) fn to_rgb_with(&self, values: &[f32], params: ColorParams) -> Rgb {
        self.to_rgb_depth(values, params, 0)
    }

    fn to_rgb_depth(&self, v: &[f32], params: ColorParams, depth: usize) -> Rgb {
        let at = |i: usize| v.get(i).copied().unwrap_or(0.0);
        let unit = |x: f32| if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
        match &self.kind {
            Kind::Rgb => [unit(at(0)), unit(at(1)), unit(at(2))],
            Kind::Gray | Kind::Cmyk | Kind::Lab { .. } | Kind::Icc { .. } => {
                let Some(link) = self.link(params, false) else {
                    return [0.0; 3];
                };
                if let Some(range) = self.lab_range() {
                    return link.convert(&lab_components(v, &range));
                }
                let n = self.components().min(4);
                let mut comps = [0.0f32; 4];
                for (slot, value) in comps.iter_mut().zip(v).take(n) {
                    *slot = unit(*value);
                }
                link.convert(&comps[..n])
            }
            Kind::Indexed {
                palette,
                hival,
                base,
                lookup,
            } => {
                let index = if at(0).is_nan() {
                    0
                } else {
                    at(0).round().clamp(0.0, *hival as f32) as usize
                };
                if params == ColorParams::DEFAULT || depth > MAX_DEPTH {
                    return palette.get(index).copied().unwrap_or([0.0; 3]);
                }
                let (comps, n) = indexed_entry(base, lookup, index);
                base.to_rgb_depth(&comps[..n], params, depth + 1)
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
                alternate.to_rgb_depth(&alt[..m], params, depth + 1)
            }
            Kind::Pattern { base } => match base {
                Some(base) if depth <= MAX_DEPTH => base.to_rgb_depth(v, params, depth + 1),
                _ => [0.0; 3],
            },
        }
    }

    /// Component values as the bytes of an 8-bit pixmap in this space
    /// (`fz_unpack_tile` after `fz_decode_tile`): Lab in its 8-bit
    /// encoding, Indexed the index. Returns the count of bytes written.
    pub(crate) fn sample_bytes(&self, values: &[f32], out: &mut [u8; MAX_COLORANTS]) -> usize {
        let at = |i: usize| values.get(i).copied().unwrap_or(0.0);
        let unit = |x: f32| {
            if x.is_nan() {
                0
            } else {
                (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
            }
        };
        if let Kind::Indexed { hival, .. } = &self.kind {
            let top = (*hival).min(255) as f32;
            out[0] = if at(0).is_nan() {
                0
            } else {
                at(0).round().clamp(0.0, top) as u8
            };
            return 1;
        }
        if self.lab_range().is_some() {
            let l = at(0);
            out[0] = if l.is_nan() {
                0
            } else {
                (l.clamp(0.0, 100.0) * 2.55 + 0.5) as u8
            };
            for (k, slot) in out.iter_mut().enumerate().take(3).skip(1) {
                let v = at(k);
                *slot = if v.is_nan() {
                    0
                } else {
                    (v + 128.0).round().clamp(0.0, 255.0) as u8
                };
            }
            return 3;
        }
        let n = self.components().clamp(1, MAX_COLORANTS);
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = unit(at(i));
        }
        n
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

/// Where a conversion lands.
#[derive(Clone, Copy)]
struct Dest<'a> {
    /// None: the page's device RGB, through the links built for it.
    profile: Option<&'a Arc<Profile>>,
    /// DeviceGray paints a CMYK destination as K alone (PDF 1.7 6.3).
    cmyk: bool,
    /// `FZ_RI_IN_SOFTMASK`: see [`ColorSpace::source_profile`].
    soft_mask: bool,
}

impl Dest<'_> {
    fn page() -> Dest<'static> {
        Dest {
            profile: None,
            cmyk: false,
            soft_mask: false,
        }
    }

    fn is_page(self) -> bool {
        self.profile.is_none()
    }

    /// The destination of an Indexed base or a tint alternate: MuPDF
    /// clears the soft mask flag before it converts them.
    fn inner(self) -> Self {
        Dest {
            soft_mask: false,
            ..self
        }
    }
}

/// A link's source profile, destination profile (none: the page) and
/// colour parameters.
type LinkId = (u64, Option<u64>, ColorParams);

/// One pixmap's conversion (`fz_convert_pixmap_samples`): 8-bit samples
/// through the 8-bit links with the device spaces replaced by the page's
/// defaults, each link looked up once per pixmap rather than per pixel.
pub(crate) struct PixmapConversion<'a> {
    defaults: &'a DefaultSpaces,
    links: Vec<(LinkId, Option<Arc<Transform>>)>,
}

impl<'a> PixmapConversion<'a> {
    pub(crate) fn new(defaults: &'a DefaultSpaces) -> Self {
        Self {
            defaults,
            links: Vec::new(),
        }
    }

    /// Samples of `space` (Lab in its 8-bit encoding, Indexed the index)
    /// as page RGB. Fewer bytes than components read as zero.
    pub(crate) fn rgb(&mut self, space: &ColorSpace, bytes: &[u8], params: ColorParams) -> [u8; 3] {
        let mut out = [0u8; MAX_COLORANTS];
        self.convert(space, bytes, params, Dest::page(), &mut out, 0);
        [out[0], out[1], out[2]]
    }

    fn link(
        &mut self,
        space: &ColorSpace,
        params: ColorParams,
        dest: Dest<'_>,
    ) -> Option<&Transform> {
        let id = (
            space.source_profile(dest)?.id(),
            dest.profile.map(|p| p.id()),
            params,
        );
        let at = match self.links.iter().position(|(key, _)| *key == id) {
            Some(at) => at,
            None => {
                self.links.push((id, space.link_to(params, dest, true)));
                self.links.len() - 1
            }
        };
        self.links[at].1.as_deref()
    }

    /// 8-bit samples of `space` into `dest`. Returns the count written.
    fn convert(
        &mut self,
        space: &ColorSpace,
        bytes: &[u8],
        params: ColorParams,
        dest: Dest<'_>,
        out: &mut [u8; MAX_COLORANTS],
        depth: usize,
    ) -> usize {
        let at = |i: usize| bytes.get(i).copied().unwrap_or(0);
        if depth > MAX_DEPTH {
            return 0;
        }
        let defaults = self.defaults;
        match &space.kind {
            Kind::Rgb if dest.is_page() => {
                out[..3].copy_from_slice(&[at(0), at(1), at(2)]);
                3
            }
            Kind::Gray if space.device && dest.cmyk => {
                out[..4].copy_from_slice(&[0, 0, 0, 255 - at(0)]);
                4
            }
            Kind::Gray | Kind::Rgb | Kind::Cmyk | Kind::Lab { .. } | Kind::Icc { .. } => {
                let n = space.components().min(4);
                let mut input = [0u8; 4];
                for (i, slot) in input.iter_mut().enumerate().take(n) {
                    *slot = at(i);
                }
                let Some(link) = self.link(space, params, dest) else {
                    return 0;
                };
                let m = link.outputs().min(MAX_COLORANTS);
                link.eval8(&input[..n], &mut out[..m]);
                m
            }
            Kind::Indexed {
                base,
                lookup,
                hival,
                ..
            } => {
                // fz_convert_indexed_pixmap_to_base: the lookup bytes are
                // the samples of a pixmap in the base space.
                let n = base.components().clamp(1, MAX_COLORANTS);
                let index = usize::from(at(0)).min(*hival);
                let mut entry = [0u8; MAX_COLORANTS];
                for (c, slot) in entry.iter_mut().enumerate().take(n) {
                    *slot = lookup.get(index * n + c).copied().unwrap_or(0);
                }
                let base = defaults.substitute(base);
                self.convert(base, &entry[..n], params, dest.inner(), out, depth + 1)
            }
            Kind::Separation { alternate, tint }
            | Kind::DeviceN {
                alternate, tint, ..
            } => {
                // fz_convert_separation_pixmap_to_base: the tint output
                // truncated to bytes, Lab in its 8-bit encoding.
                let n = space.components().min(MAX_COLORANTS);
                let mut input = [0.0f64; MAX_COLORANTS];
                for (i, slot) in input.iter_mut().enumerate().take(n) {
                    *slot = f64::from(at(i)) / 255.0;
                }
                let mut tinted = [0.0f64; MAX_COLORANTS];
                let m = alternate.components().clamp(1, MAX_COLORANTS);
                tint.eval(&input[..n], &mut tinted[..m]);
                let mut alt = [0u8; MAX_COLORANTS];
                let lab = alternate.lab_range().is_some();
                for (k, (slot, value)) in alt.iter_mut().zip(&tinted[..m]).enumerate() {
                    *slot = match (lab, k) {
                        (true, 0) => (value / 100.0 * 255.0) as u8,
                        (true, _) => (value + 128.0) as u8,
                        _ => (value * 255.0) as u8,
                    };
                }
                let alternate = defaults.substitute(alternate);
                self.convert(alternate, &alt[..m], params, dest.inner(), out, depth + 1)
            }
            Kind::Pattern { .. } => 0,
        }
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
                .filter(|g| *g > 0.0)
                .unwrap_or(1.0);
            match cal_space("CalGray", white(&dict), &[gamma as f32], None) {
                Some(space) => space,
                None => return Some(ColorSpace::gray()),
            }
        }
        b"CalRGB" => {
            let dict = dict_at(1).unwrap_or_default();
            let gamma = match numbers(&dict, b"Gamma").as_deref() {
                Some([r, g, b]) if *r > 0.0 && *g > 0.0 && *b > 0.0 => {
                    [*r as f32, *g as f32, *b as f32]
                }
                _ => [1.0; 3],
            };
            let matrix = match numbers(&dict, b"Matrix") {
                Some(m) if m.len() == 9 => {
                    let mut out = [0.0f32; 9];
                    for (slot, value) in out.iter_mut().zip(&m) {
                        *slot = *value as f32;
                    }
                    out
                }
                _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            };
            match cal_space("CalRGB", white(&dict), &gamma, Some(matrix)) {
                Some(space) => space,
                None => return Some(ColorSpace::rgb()),
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
                device: false,
            }
        }
        b"ICCBased" => {
            let stream_object = items.get(1)?;
            let stream_ref = match stream_object {
                Object::Reference(id) => Some(*id),
                _ => None,
            };
            if let Some(id) = stream_ref
                && let Some(hit) = cache.borrow().get(&id)
            {
                return Some(hit.clone());
            }
            let stream = doc.resolve_stream(stream_object).ok()??;
            let n = stream
                .dict
                .get(b"N")
                .and_then(|o| doc.resolve_i64(o).ok().flatten())
                .unwrap_or(0);
            // MuPDF keeps an embedded profile unless it has more channels
            // than /N claims; a broken profile falls back to /N, then
            // /Alternate.
            let embedded = doc
                .decode_stream(&stream)
                .ok()
                .and_then(|data| icc::Profile::parse(&data.data))
                .filter(|profile| n <= 0 || profile.components() as i64 <= n)
                .and_then(|profile| {
                    let lab = profile.is_lab().then_some([-128.0, 127.0, -128.0, 127.0]);
                    icc_space("ICCBased", &profile, lab)
                });
            let space = match embedded {
                Some(space) => space,
                None => {
                    let base = match n {
                        1 => ColorSpace::gray(),
                        3 => ColorSpace::rgb(),
                        4 => ColorSpace::cmyk(),
                        _ => load(
                            doc,
                            stream.dict.get(b"Alternate")?,
                            resources,
                            cache,
                            depth + 1,
                        )?,
                    };
                    ColorSpace {
                        name: "ICCBased",
                        kind: iccbased_kind(&base)?,
                        device: base.device,
                    }
                }
            };
            let space = Arc::new(space);
            if let Some(id) = stream_ref {
                cache.borrow_mut().insert(id, space.clone());
            }
            return Some(space);
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
            let n = base.components().clamp(1, MAX_COLORANTS);
            let mut palette = Vec::with_capacity(hival + 1);
            for entry in 0..=hival {
                let (comps, n) = indexed_entry(&base, &lookup, entry);
                palette.push(base.to_rgb(&comps[..n]));
            }
            let _ = n;
            ColorSpace {
                name: "Indexed",
                kind: Kind::Indexed {
                    hival,
                    base,
                    lookup,
                    palette,
                },
                device: false,
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
                device: false,
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
                device: false,
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
                device: false,
            }
        }
        _ => return None,
    };
    Some(Arc::new(space))
}

/// An ICCBased space behaves as its device equivalent.
fn iccbased_kind(base: &ColorSpace) -> Option<Kind> {
    Some(match &base.kind {
        Kind::Gray => Kind::Gray,
        Kind::Rgb => Kind::Rgb,
        Kind::Cmyk => Kind::Cmyk,
        Kind::Lab { range } => Kind::Lab { range: *range },
        Kind::Icc {
            n,
            lab,
            profile,
            link,
        } => Kind::Icc {
            n: *n,
            lab: *lab,
            profile: profile.clone(),
            link: link.clone(),
        },
        _ => return None,
    })
}

/// One Indexed lookup entry as the base's components (8-bit samples through
/// the base's default /Decode).
fn indexed_entry(base: &ColorSpace, lookup: &[u8], index: usize) -> ([f32; MAX_COLORANTS], usize) {
    let n = base.components().clamp(1, MAX_COLORANTS);
    let decode = base.default_decode(8);
    let mut comps = [0.0f32; MAX_COLORANTS];
    for (c, slot) in comps.iter_mut().enumerate().take(n) {
        let byte = lookup.get(index * n + c).copied().unwrap_or(0);
        let [d0, d1] = decode.get(c).copied().unwrap_or([0.0, 1.0]);
        *slot = (d0 + f64::from(byte) / 255.0 * (d1 - d0)) as f32;
    }
    (comps, n)
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

// ----- default colour spaces -------------------------------------------------

static NEXT_DEFAULTS_KEY: AtomicU64 = AtomicU64::new(1);

static NO_DEFAULTS: LazyLock<Arc<DefaultSpaces>> = LazyLock::new(|| {
    Arc::new(DefaultSpaces {
        gray: None,
        rgb: None,
        cmyk: None,
        key: 0,
    })
});

/// `fz_default_colorspaces`: what a page or form paints DeviceGray,
/// DeviceRGB and DeviceCMYK through, from its Resources `/ColorSpace`
/// `DefaultGray`, `DefaultRGB` and `DefaultCMYK` entries and the document's
/// output intent. `None` keeps the device space.
#[derive(Debug)]
pub(crate) struct DefaultSpaces {
    gray: Option<Arc<ColorSpace>>,
    rgb: Option<Arc<ColorSpace>>,
    cmyk: Option<Arc<ColorSpace>>,
    key: u64,
}

impl DefaultSpaces {
    /// No substitutions: the device spaces paint as themselves.
    pub(crate) fn none() -> Arc<DefaultSpaces> {
        NO_DEFAULTS.clone()
    }

    /// Identifies the set, for caches of images converted through it; 0 is
    /// the empty set.
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    /// `pdf_load_default_colorspaces`: a page's defaults, with the output
    /// intent standing in for whichever device space the page left alone.
    pub(crate) fn for_page(
        doc: &Document,
        resources: &Dict,
        output_intent: Option<&Arc<ColorSpace>>,
        cache: &ColorSpaceCache,
    ) -> Arc<DefaultSpaces> {
        let mut spaces = DefaultSpaces {
            gray: None,
            rgb: None,
            cmyk: None,
            key: 0,
        };
        spaces.load(doc, resources, cache);
        if let Some(oi) = output_intent {
            // fz_set_default_output_intent
            let slot = match oi.family() {
                ColorFamily::Gray => &mut spaces.gray,
                ColorFamily::Rgb => &mut spaces.rgb,
                ColorFamily::Cmyk => &mut spaces.cmyk,
                _ => &mut None,
            };
            if slot.is_none() {
                *slot = Some(oi.clone());
            }
        }
        Self::finish(spaces)
    }

    /// `pdf_update_default_colorspaces`: a form's own Resources override the
    /// set it inherits.
    pub(crate) fn update(
        parent: &Arc<DefaultSpaces>,
        doc: &Document,
        resources: &Dict,
        cache: &ColorSpaceCache,
    ) -> Arc<DefaultSpaces> {
        let mut spaces = DefaultSpaces {
            gray: parent.gray.clone(),
            rgb: parent.rgb.clone(),
            cmyk: parent.cmyk.clone(),
            key: 0,
        };
        if !spaces.load(doc, resources, cache) {
            return parent.clone();
        }
        Self::finish(spaces)
    }

    /// Reads the `Default*` entries; whether any was present.
    fn load(&mut self, doc: &Document, resources: &Dict, cache: &ColorSpaceCache) -> bool {
        let Some(dict) = resources
            .get(b"ColorSpace")
            .and_then(|o| doc.resolve_dict(o).ok().flatten())
        else {
            return false;
        };
        let gray = set_default(
            &mut self.gray,
            doc,
            &dict,
            b"DefaultGray",
            ColorFamily::Gray,
            1,
            cache,
        );
        let rgb = set_default(
            &mut self.rgb,
            doc,
            &dict,
            b"DefaultRGB",
            ColorFamily::Rgb,
            3,
            cache,
        );
        let cmyk = set_default(
            &mut self.cmyk,
            doc,
            &dict,
            b"DefaultCMYK",
            ColorFamily::Cmyk,
            4,
            cache,
        );
        gray || rgb || cmyk
    }

    fn finish(mut spaces: DefaultSpaces) -> Arc<DefaultSpaces> {
        if spaces.gray.is_none() && spaces.rgb.is_none() && spaces.cmyk.is_none() {
            return Self::none();
        }
        spaces.key = NEXT_DEFAULTS_KEY.fetch_add(1, Ordering::Relaxed);
        Arc::new(spaces)
    }

    /// `fz_default_colorspace`: a device space becomes its default; every
    /// other space is painted as itself.
    pub(crate) fn substitute<'a>(&'a self, space: &'a ColorSpace) -> &'a ColorSpace {
        if !space.device {
            return space;
        }
        let replacement = match &space.kind {
            Kind::Gray => &self.gray,
            Kind::Rgb => &self.rgb,
            Kind::Cmyk => &self.cmyk,
            _ => &None,
        };
        replacement.as_deref().unwrap_or(space)
    }
}

/// `fz_set_default_gray` and friends: the entry is taken only when it is a
/// space of the right family and component count; a device space resets
/// the slot.
fn set_default(
    slot: &mut Option<Arc<ColorSpace>>,
    doc: &Document,
    dict: &Dict,
    key: &[u8],
    family: ColorFamily,
    n: usize,
    cache: &ColorSpaceCache,
) -> bool {
    let Some(object) = dict.get(key) else {
        return false;
    };
    let Some(space) = ColorSpace::load(doc, object, None, cache) else {
        return false;
    };
    if space.is_pattern() || space.family() != family || space.components() != n {
        return false;
    }
    *slot = (!space.device).then_some(space);
    true
}

/// `pdf_document_output_intent`: the first `/OutputIntents` entry's
/// `/DestOutputProfile` as an ICCBased space. MuPDF reads it without an
/// Alternate and needs `/N`; a profile that falls back to a device space
/// changes nothing.
pub(crate) fn output_intent(doc: &Document, cache: &ColorSpaceCache) -> Option<Arc<ColorSpace>> {
    let catalog = doc.catalog().ok()?;
    let intents = doc.resolve_array(catalog.get(b"OutputIntents")?).ok()??;
    let first = doc.resolve_dict(intents.first()?).ok()??;
    let profile = first.get(b"DestOutputProfile")?;
    let stream = doc.resolve_stream(profile).ok()??;
    let n = stream
        .dict
        .get(b"N")
        .and_then(|o| doc.resolve_i64(o).ok().flatten())
        .unwrap_or(0);
    if n <= 0 {
        return None;
    }
    let array = Object::Array(vec![Object::name("ICCBased"), profile.clone()]);
    let space = ColorSpace::load(doc, &array, None, cache)?;
    (!space.device).then_some(space)
}

// ----- conversions -----------------------------------------------------------

const D50: [f64; 3] = [0.9642, 1.0, 0.8249];

/// Lab components clamped the way MuPDF clamps them before the link.
fn lab_components(v: &[f32], range: &[f64; 4]) -> [f32; 3] {
    let at = |i: usize| f64::from(v.get(i).copied().unwrap_or(0.0));
    [
        at(0).clamp(0.0, 100.0) as f32,
        at(1).clamp(range[0], range[1]) as f32,
        at(2).clamp(range[2], range[3]) as f32,
    ]
}

/// A CalGray/CalRGB space as MuPDF builds it: an ICC profile generated
/// from the dictionary, linked like any other profile.
fn cal_space(
    name: &'static str,
    white: [f64; 3],
    gamma: &[f32],
    matrix: Option<[f32; 9]>,
) -> Option<ColorSpace> {
    let profile = icc::cal_profile(white.map(|w| w as f32), gamma, matrix)?;
    icc_space(name, &profile, None)
}

/// A colour space over an ICC profile. A profile that links to device RGB
/// as identity is plain RGB, as in MuPDF.
fn icc_space(
    name: &'static str,
    profile: &Arc<Profile>,
    lab: Option<[f64; 4]>,
) -> Option<ColorSpace> {
    let link = icc::link_to_rgb(profile, Intent::RelativeColorimetric, true, false)?;
    let n = profile.components();
    let kind = if link.is_identity() && n == 3 && lab.is_none() {
        Kind::Rgb
    } else {
        Kind::Icc {
            n,
            lab,
            profile: profile.clone(),
            link,
        }
    };
    Some(ColorSpace {
        name,
        kind,
        device: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(rgb: Rgb) -> [u8; 3] {
        rgb.map(|v| (v * 255.0).round() as u8)
    }

    fn within_one(got: [u8; 3], want: [u8; 3]) -> bool {
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1)
    }

    #[test]
    fn device_cmyk_fills_match_pymupdf() {
        // Values PyMuPDF 1.27 paints for these fills at 72 dpi.
        let cmyk = ColorSpace::cmyk();
        for (input, want) in [
            ([1.0, 0.0, 0.0, 0.0], [0, 173, 239]),
            ([0.0, 0.0, 0.0, 1.0], [34, 31, 31]),
            ([0.0, 1.0, 1.0, 0.0], [237, 28, 36]),
            ([0.5, 0.0, 0.0, 0.0], [109, 207, 246]),
            ([0.3, 0.6, 0.1, 0.2], [150, 101, 140]),
            ([0.0, 0.0, 0.0, 0.0], [255, 255, 255]),
        ] {
            let got = bytes(cmyk.to_rgb(&input));
            assert!(
                within_one(got, want),
                "cmyk {input:?}: got {got:?}, want {want:?}"
            );
        }
    }

    #[test]
    fn device_gray_fills_match_pymupdf() {
        let gray = ColorSpace::gray();
        for (input, want) in [
            (0.2f32, [51, 51, 50]),
            (100.0 / 255.0, [99, 100, 99]),
            (1.0, [255, 255, 255]),
        ] {
            let got = bytes(gray.to_rgb(&[input]));
            assert!(
                within_one(got, want),
                "gray {input}: got {got:?}, want {want:?}"
            );
        }
    }

    #[test]
    fn lab_fills_match_pymupdf() {
        let lab = ColorSpace {
            name: "Lab",
            kind: Kind::Lab {
                range: [-100.0, 100.0, -100.0, 100.0],
            },
            device: false,
        };
        let got = bytes(lab.to_rgb(&[50.0, 20.0, -30.0]));
        assert!(within_one(got, [131, 107, 170]), "{got:?}");
        let white = bytes(lab.to_rgb(&[100.0, 0.0, 0.0]));
        assert!(white.iter().all(|&v| v >= 253), "{white:?}");
    }

    #[test]
    fn cal_spaces_keep_white_and_black() {
        let gray = cal_space("CalGray", D50, &[1.0], None).expect("CalGray profile");
        assert_eq!(bytes(gray.to_rgb(&[1.0])), [255, 255, 255]);
        assert_eq!(bytes(gray.to_rgb(&[0.0])), [0, 0, 0]);
        let srgb_matrix = [
            0.4124, 0.2126, 0.0193, 0.3576, 0.7152, 0.1192, 0.1805, 0.0722, 0.9505,
        ];
        let rgb = cal_space("CalRGB", [0.9505, 1.0, 1.089], &[2.2; 3], Some(srgb_matrix))
            .expect("CalRGB profile");
        assert_eq!(bytes(rgb.to_rgb(&[1.0, 1.0, 1.0])), [255, 255, 255]);
        assert_eq!(bytes(rgb.to_rgb(&[0.0, 0.0, 0.0])), [0, 0, 0]);
    }

    #[test]
    fn rendering_intent_and_compensation_change_cmyk_fills() {
        // PyMuPDF 1.27 at 72 dpi: the same CMYK fill under `/Perceptual ri`
        // and under `/UseBlackPtComp /OFF`, which lcms2 links differently.
        let cmyk = ColorSpace::cmyk();
        let dark = [0.0, 0.0, 0.0, 1.0];
        let default = bytes(cmyk.to_rgb(&dark));
        let perceptual = bytes(cmyk.to_rgb_with(
            &dark,
            ColorParams {
                intent: Intent::Perceptual,
                bpc: true,
            },
        ));
        let no_bpc = bytes(cmyk.to_rgb_with(
            &dark,
            ColorParams {
                intent: Intent::RelativeColorimetric,
                bpc: false,
            },
        ));
        assert!(within_one(default, [34, 31, 31]), "{default:?}");
        assert!(within_one(no_bpc, [55, 52, 53]), "{no_bpc:?}");
        assert!(within_one(perceptual, [43, 40, 41]), "{perceptual:?}");
    }

    #[test]
    fn gray_image_bytes_match_gray_fills() {
        // An 8-bit DeviceGray sample converts like the fill of the same
        // value, through the 33-point link.
        let gray = ColorSpace::gray();
        let defaults = DefaultSpaces::none();
        let mut pixmap = PixmapConversion::new(&defaults);
        for sample in [0u8, 1, 51, 100, 128, 200, 254, 255] {
            let image = pixmap.rgb(&gray, &[sample], ColorParams::DEFAULT);
            let fill = bytes(gray.to_rgb(&[f32::from(sample) / 255.0]));
            assert!(
                within_one(image, fill),
                "{sample}: image {image:?} fill {fill:?}"
            );
        }
    }

    #[test]
    fn default_spaces_replace_only_device_spaces() {
        let srgb_matrix = [
            0.4124, 0.2126, 0.0193, 0.3576, 0.7152, 0.1192, 0.1805, 0.0722, 0.9505,
        ];
        let cal = Arc::new(
            cal_space("CalRGB", [0.9505, 1.0, 1.089], &[1.0; 3], Some(srgb_matrix))
                .expect("CalRGB profile"),
        );
        let spaces = DefaultSpaces {
            gray: None,
            rgb: Some(cal.clone()),
            cmyk: None,
            key: 1,
        };
        let device = ColorSpace::rgb();
        assert!(std::ptr::eq(spaces.substitute(&device), &*cal));
        let converted = ColorSpace::srgb();
        assert!(std::ptr::eq(spaces.substitute(&converted), &*converted));
        let gray = ColorSpace::gray();
        assert!(std::ptr::eq(spaces.substitute(&gray), &*gray));
        // Linear CalRGB mid-grey is lighter than the sRGB byte it replaces.
        let mid = bytes(spaces.substitute(&device).to_rgb(&[0.5, 0.5, 0.5]));
        assert!(mid[0] > 180, "{mid:?}");
    }
}
