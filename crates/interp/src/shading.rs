//! Function, axial, radial, triangle and patch-mesh shadings.
//!
//! Loading decodes a shading dictionary into shading-space geometry and
//! colour samples. [`Shading::triangles`] then produces the Gouraud
//! triangles MuPDF paints for it, in MuPDF's order and single-precision
//! arithmetic, so a raster that paints them the same way matches PyMuPDF.
//! [`Shading::sample`] evaluates the shading analytically for consumers that
//! want a colour at a point rather than a raster.
use crate::colorspace::{ColorSpace, ColorSpaceCache, MAX_COLORANTS};
use crate::device::Rgb;
use crate::function::Function;
use crate::image::ColorFamily;
use pdf_core::{Dict, Document, Matrix, Object, Point, Rect};
use std::sync::Arc;

const MAX_TRIANGLES: usize = 1_000_000;
const MAX_PATCHES: usize = 65_536;
/// Grid divisions of a function-based shading (MuPDF's `FUNSEGS`).
const FUNSEGS: usize = 256;
/// How far axial and radial shadings extend (MuPDF's `HUGENUM`).
const HUGENUM: f32 = 32000.0;
/// Patch subdivision levels in each direction (MuPDF's `SUBDIV`).
const SUBDIV: u32 = 3;

/// A Gouraud triangle corner in device space. `value` holds the function
/// parameter scaled to `0..=255` when [`Shading::lut`] is `Some`, otherwise
/// RGB scaled to `0..=255`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadeVertex {
    pub x: f32,
    pub y: f32,
    pub value: [f32; 3],
}

type Lut = Box<[[u8; 3]; 256]>;

/// A mesh vertex in shading space with its prepared value.
#[derive(Clone, Copy, Debug)]
struct MeshVertex {
    point: [f32; 2],
    value: [f32; 3],
}

/// A Coons or tensor patch: the control points in stream order and the
/// corner colours as raw components.
#[derive(Clone, Debug)]
struct Patch {
    points: [[f32; 2]; 16],
    colors: [[f32; MAX_COLORANTS]; 4],
}

#[derive(Debug)]
enum Field {
    Function {
        matrix: Matrix,
        inverse: Matrix,
        domain: Rect,
        functions: Vec<Function>,
        space: Arc<ColorSpace>,
        /// RGB × 255 at the `(FUNSEGS + 1)²` grid points, row by row.
        samples: Vec<[f32; 3]>,
    },
    Axial {
        coords: [f64; 4],
        extend: [bool; 2],
        colors: Box<[Rgb; 256]>,
        lut: Lut,
    },
    Radial {
        coords: [f64; 6],
        extend: [bool; 2],
        colors: Box<[Rgb; 256]>,
        lut: Lut,
    },
    Mesh {
        kind: u8,
        /// The Decode ranges of the coordinates.
        extent: Rect,
        triangles: Vec<[MeshVertex; 3]>,
        patches: Vec<Patch>,
        space: Arc<ColorSpace>,
        components: usize,
        /// Decode range of the function parameter, with its lookup table.
        function: Option<([f32; 2], Lut)>,
    },
}
#[derive(Debug)]
pub struct Shading {
    shading_type: u8,
    bbox: Option<Rect>,
    background: Option<Rgb>,
    field: Field,
}

impl Shading {
    pub fn shading_type(&self) -> u8 {
        self.shading_type
    }
    pub fn bbox(&self) -> Option<Rect> {
        self.bbox
    }
    pub fn background(&self) -> Option<Rgb> {
        self.background
    }
    /// The shading-space rectangle outside which nothing is painted, as
    /// MuPDF bounds it: `None` when unbounded. Axial and radial shadings are
    /// bounded only by `BBox`; the others also by their domain or decode
    /// ranges.
    pub fn bound(&self) -> Option<Rect> {
        let extent = match &self.field {
            Field::Function { matrix, domain, .. } => Some(domain.transform(matrix)),
            Field::Axial { .. } | Field::Radial { .. } => None,
            Field::Mesh { extent, .. } => Some(*extent),
        };
        match (extent, self.bbox) {
            (Some(extent), Some(bbox)) => Some(extent.intersect(&bbox)),
            (extent, bbox) => extent.or(bbox),
        }
    }
    /// The colour table indexed by the parameter byte that
    /// [`Shading::triangles`] interpolates, or `None` when the triangles carry
    /// RGB.
    pub fn lut(&self) -> Option<&[[u8; 3]; 256]> {
        match &self.field {
            Field::Axial { lut, .. } | Field::Radial { lut, .. } => Some(lut),
            Field::Mesh { function, .. } => function.as_ref().map(|(_, lut)| &**lut),
            Field::Function { .. } => None,
        }
    }
    /// Emits the triangles that paint this shading under `ctm` (shading
    /// space → device) within the device rectangle `scissor`, following
    /// MuPDF's decomposition: axial and radial shadings become quads and
    /// annuli whose corners carry the parameter, function-based shadings a
    /// 256 × 256 grid, and patches an 8 × 8 midpoint subdivision.
    pub fn triangles(&self, ctm: &Matrix, scissor: Rect, emit: &mut dyn FnMut([ShadeVertex; 3])) {
        let ctm = M32::from(ctm);
        match &self.field {
            Field::Function {
                matrix, samples, ..
            } => function_grid(&M32::from(matrix).concat(&ctm), samples, emit),
            Field::Axial { coords, extend, .. } => axial(coords, *extend, &ctm, scissor, emit),
            Field::Radial { coords, extend, .. } => radial(coords, *extend, &ctm, emit),
            Field::Mesh {
                kind,
                triangles,
                patches,
                space,
                components,
                function,
                ..
            } => {
                for tri in triangles {
                    emit(tri.map(|v| {
                        let [x, y] = ctm.point(v.point);
                        ShadeVertex {
                            x,
                            y,
                            value: v.value,
                        }
                    }));
                }
                let prepare = |c: &[f32; MAX_COLORANTS]| {
                    prepare_value(c, *components, space, function.as_ref().map(|(r, _)| *r))
                };
                for patch in patches {
                    draw_patch(patch, *kind, &ctm, &prepare, emit);
                }
            }
        }
    }
    /// The colour at `point` in shading space: `None` where nothing is
    /// painted (mesh shadings are never sampled this way).
    pub fn sample(&self, point: Point) -> Option<Rgb> {
        if self.bbox.is_some_and(|b| !b.contains(point)) {
            return None;
        }
        let color = match &self.field {
            Field::Function {
                inverse,
                domain,
                functions,
                space,
                ..
            } => {
                let p = point.transform(inverse);
                domain
                    .contains(p)
                    .then(|| evaluate(space, functions, &[p.x, p.y]))
            }
            Field::Axial {
                coords: [x0, y0, x1, y1],
                extend,
                colors,
                ..
            } => {
                let dx = x1 - x0;
                let dy = y1 - y0;
                let length = dx * dx + dy * dy;
                if length == 0.0 {
                    None
                } else {
                    gradient(
                        ((point.x - x0) * dx + (point.y - y0) * dy) / length,
                        *extend,
                        colors,
                    )
                }
            }
            Field::Radial {
                coords: [x0, y0, r0, x1, y1, r1],
                extend,
                colors,
                ..
            } => {
                let dx = x1 - x0;
                let dy = y1 - y0;
                let dr = r1 - r0;
                let px = point.x - x0;
                let py = point.y - y0;
                let a = dx * dx + dy * dy - dr * dr;
                let b = -2.0 * (px * dx + py * dy + r0 * dr);
                let c = px * px + py * py - r0 * r0;
                let mut roots = [f64::NAN; 2];
                if a.abs() < 1e-12 {
                    if b != 0.0 {
                        roots[0] = -c / b;
                    }
                } else {
                    let det = b * b - 4.0 * a * c;
                    if det >= 0.0 {
                        roots = [(-b - det.sqrt()) / (2.0 * a), (-b + det.sqrt()) / (2.0 * a)];
                    }
                }
                roots
                    .into_iter()
                    .filter(|t| {
                        t.is_finite()
                            && r0 + t * dr >= 0.0
                            && (*t >= 0.0 || extend[0])
                            && (*t <= 1.0 || extend[1])
                    })
                    .max_by(f64::total_cmp)
                    .and_then(|t| gradient(t, *extend, colors))
            }
            Field::Mesh { .. } => None,
        };
        color.or(self.background)
    }
    pub(crate) fn load(
        doc: &Document,
        object: &Object,
        resources: &Dict,
        cache: &ColorSpaceCache,
    ) -> Option<Shading> {
        let object = doc.resolve(object).ok()?;
        let d = object
            .as_dict()
            .or_else(|| object.as_stream().map(|s| &s.dict))?;
        let shading_type = d.get_i64(b"ShadingType")? as u8;
        let space = ColorSpace::load(doc, d.get(b"ColorSpace")?, Some(resources), cache)?;
        let bbox = d.get_array(b"BBox").and_then(Rect::from_array);
        let background = d.get_array(b"Background").map(|a| {
            to_rgb(
                &space,
                &a.iter()
                    .map(|o| o.as_f64().unwrap_or(0.0) as f32)
                    .collect::<Vec<_>>(),
            )
        });
        let functions = match d
            .get(b"Function")
            .map(|o| doc.resolve(o))
            .transpose()
            .ok()?
        {
            Some(Object::Array(a)) => a
                .iter()
                .map(|o| Function::load(doc, o))
                .collect::<Option<Vec<_>>>()?,
            Some(o) if !o.is_null() => vec![Function::load(doc, &o)?],
            _ => Vec::new(),
        };
        if shading_type <= 3 && functions.is_empty() {
            return None;
        }
        if functions.len() > 1 && functions.len() != space.components() {
            return None;
        }
        let field = match shading_type {
            1 => {
                let domain = numbers(d, b"Domain", [0.0, 1.0, 0.0, 1.0]);
                let matrix = d
                    .get_array(b"Matrix")
                    .and_then(Matrix::from_array)
                    .unwrap_or(Matrix::IDENTITY);
                let inverse = matrix.invert()?;
                let samples = function_samples(&space, &functions, domain);
                Field::Function {
                    matrix,
                    inverse,
                    domain: Rect::new(domain[0], domain[2], domain[1], domain[3]),
                    functions,
                    space,
                    samples,
                }
            }
            2 | 3 => {
                let domain = numbers(d, b"Domain", [0.0, 1.0]);
                let extend = std::array::from_fn(|i| {
                    d.get_array(b"Extend")
                        .and_then(|a| a.get(i))
                        .and_then(Object::as_bool)
                        .unwrap_or(false)
                });
                let (colors, lut) =
                    sample_function(&space, &functions, [domain[0] as f32, domain[1] as f32]);
                if shading_type == 2 {
                    Field::Axial {
                        coords: numbers(d, b"Coords", [0.0; 4]),
                        extend,
                        colors,
                        lut,
                    }
                } else {
                    Field::Radial {
                        coords: numbers(d, b"Coords", [0.0; 6]),
                        extend,
                        colors,
                        lut,
                    }
                }
            }
            4..=7 => {
                let stream = object.as_stream()?;
                let bytes = doc.decode_stream(stream).ok()?.data;
                let mut reader = MeshReader::new(&bytes, d, &space, &functions);
                let mut triangles = Vec::new();
                let mut patches = Vec::new();
                match shading_type {
                    4 => reader.triangles(&mut triangles),
                    5 => reader.lattice(
                        &mut triangles,
                        d.get_i64(b"VerticesPerRow").unwrap_or(2).clamp(2, 65536) as usize,
                    ),
                    6 => reader.patches(&mut patches, 12),
                    _ => reader.patches(&mut patches, 16),
                }?;
                let function = (!functions.is_empty()).then(|| {
                    let range = reader.ranges[2];
                    (range, sample_function(&space, &functions, range).1)
                });
                let [x, y] = [reader.ranges[0], reader.ranges[1]];
                Field::Mesh {
                    kind: shading_type,
                    extent: Rect::new(
                        f64::from(x[0].min(x[1])),
                        f64::from(y[0].min(y[1])),
                        f64::from(x[0].max(x[1])),
                        f64::from(y[0].max(y[1])),
                    ),
                    triangles,
                    patches,
                    components: reader.n,
                    space,
                    function,
                }
            }
            _ => return None,
        };
        Some(Shading {
            shading_type,
            bbox,
            background,
            field,
        })
    }
}
fn numbers<const N: usize>(d: &Dict, key: &[u8], default: [f64; N]) -> [f64; N] {
    std::array::from_fn(|i| {
        d.get_array(key)
            .and_then(|a| a.get(i))
            .and_then(Object::as_f64)
            .unwrap_or(default[i])
    })
}
/// Converts a shading colour to RGB as MuPDF's draw device does: Indexed by
/// `index × 255` clamped to hival (so a mesh's undecoded index picks the last
/// palette entry unless it is zero), every other space through the normal
/// conversion.
fn to_rgb(space: &ColorSpace, values: &[f32]) -> Rgb {
    match space.family() {
        ColorFamily::Indexed => {
            let first = values.first().copied().unwrap_or(0.0);
            let index = if first.is_nan() { 0.0 } else { first * 255.0 };
            space.to_rgb(&[index.clamp(0.0, 1.0e9).trunc()])
        }
        _ => space.to_rgb(values),
    }
}
/// Evaluates the shading function(s) at `input` and converts the result to
/// RGB. Outputs are rounded to single precision first, as MuPDF computes
/// them.
fn evaluate(space: &ColorSpace, functions: &[Function], input: &[f64]) -> Rgb {
    let mut out = [0.0; MAX_COLORANTS];
    if let [f] = functions {
        f.eval(input, &mut out[..space.components()]);
    } else {
        for (slot, f) in out.iter_mut().zip(functions) {
            f.eval(input, std::slice::from_mut(slot));
        }
    }
    let values = out.map(|v| v as f32);
    to_rgb(space, &values[..space.components()])
}
/// Samples the function over `[t0, t1]` at MuPDF's 256 points: the exact
/// colours for [`Shading::sample`] and the byte table for painting, whose
/// entries truncate `255 × colour` as MuPDF's `clut` does.
fn sample_function(
    space: &ColorSpace,
    functions: &[Function],
    [t0, t1]: [f32; 2],
) -> (Box<[Rgb; 256]>, Lut) {
    let colors: Box<[Rgb; 256]> = Box::new(std::array::from_fn(|i| {
        let t = t0 + (i as f32 / 255.0) * (t1 - t0);
        evaluate(space, functions, &[f64::from(t)])
    }));
    let lut = Box::new(colors.map(bytes));
    (colors, lut)
}
/// `255 × colour` truncated to bytes.
fn bytes(rgb: Rgb) -> [u8; 3] {
    rgb.map(|c| (c * 255.0) as u8)
}
fn gradient(t: f64, extend: [bool; 2], colors: &[Rgb; 256]) -> Option<Rgb> {
    if (t < 0.0 && !extend[0]) || (t > 1.0 && !extend[1]) {
        return None;
    }
    let p = t.clamp(0.0, 1.0) * 255.0;
    let i = p as usize;
    let f = (p - i as f64) as f32;
    Some(std::array::from_fn(|c| {
        colors[i][c] * (1.0 - f) + colors[(i + 1).min(255)][c] * f
    }))
}
/// RGB × 255 at the `(FUNSEGS + 1)²` grid points of a function-based
/// shading's domain `[x0, x1] × [y0, y1]`, row by row.
fn function_samples(
    space: &ColorSpace,
    functions: &[Function],
    [x0, x1, y0, y1]: [f64; 4],
) -> Vec<[f32; 3]> {
    let (x0, x1, y0, y1) = (x0 as f32, x1 as f32, y0 as f32, y1 as f32);
    let mut samples = Vec::with_capacity((FUNSEGS + 1) * (FUNSEGS + 1));
    for yy in 0..=FUNSEGS {
        let y = y0 + (y1 - y0) * yy as f32 / FUNSEGS as f32;
        for xx in 0..=FUNSEGS {
            let x = x0 + (x1 - x0) * xx as f32 / FUNSEGS as f32;
            let rgb = evaluate(space, functions, &[f64::from(x), f64::from(y)]);
            samples.push(rgb.map(|c| c * 255.0));
        }
    }
    samples
}
/// The value a mesh vertex carries: the parameter mapped to `0..=255` over
/// its decode range, or the colour converted to RGB × 255.
fn prepare_value(
    c: &[f32; MAX_COLORANTS],
    components: usize,
    space: &ColorSpace,
    range: Option<[f32; 2]>,
) -> [f32; 3] {
    match range {
        Some([c0, c1]) => {
            let f = if c1 == c0 {
                0.0
            } else {
                (c[0] - c0) / (c1 - c0)
            };
            [f * 255.0, 0.0, 0.0]
        }
        None => to_rgb(space, &c[..components]).map(|v| v * 255.0),
    }
}

/// A single-precision affine matrix, so device geometry rounds as MuPDF's
/// does.
#[derive(Clone, Copy)]
struct M32 {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}
impl M32 {
    fn from(m: &Matrix) -> M32 {
        M32 {
            a: m.a as f32,
            b: m.b as f32,
            c: m.c as f32,
            d: m.d as f32,
            e: m.e as f32,
            f: m.f as f32,
        }
    }
    /// `self` then `other`.
    fn concat(&self, other: &M32) -> M32 {
        M32 {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }
    fn point(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [
            x * self.a + y * self.c + self.e,
            x * self.b + y * self.d + self.f,
        ]
    }
    fn vector(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [x * self.a + y * self.c, x * self.b + y * self.d]
    }
    fn expansion(&self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}
fn vertex([x, y]: [f32; 2], t: f32) -> ShadeVertex {
    ShadeVertex {
        x,
        y,
        value: [t * 255.0, 0.0, 0.0],
    }
}
/// A quad `v0 v1 v2 v3` (corners in order) as MuPDF's two triangles.
fn quad(v: [ShadeVertex; 4], emit: &mut dyn FnMut([ShadeVertex; 3])) {
    emit([v[0], v[1], v[3]]);
    emit([v[3], v[2], v[1]]);
}
fn on_circle([x, y]: [f32; 2], r: f32, theta: f32) -> [f32; 2] {
    [x + theta.cos() * r, y + theta.sin() * r]
}
fn axial(
    coords: &[f64; 4],
    extend: [bool; 2],
    ctm: &M32,
    scissor: Rect,
    emit: &mut dyn FnMut([ShadeVertex; 3]),
) {
    let c = coords.map(|v| v as f32);
    let dir = ctm.vector([c[1] - c[3], c[2] - c[0]]);
    let p0 = ctm.point([c[0], c[1]]);
    let p1 = ctm.point([c[2], c[3]]);
    let theta = dir[1].atan2(dir[0]);
    let (sx0, sy0, sx1, sy1) = (
        scissor.x0 as f32,
        scissor.y0 as f32,
        scissor.x1 as f32,
        scissor.y1 as f32,
    );
    let mut x = p0[0] - sx0;
    let mut y = p0[1] - sy0;
    for candidate in [sx1 - p0[0], p0[0] - sx1, sx1 - p1[0]] {
        if x < candidate {
            x = candidate;
        }
    }
    for candidate in [sy1 - p0[1], p0[1] - sy1, sy1 - p1[1]] {
        if y < candidate {
            y = candidate;
        }
    }
    let mut r = x + y;
    let v0 = on_circle(p0, r, theta);
    let v1 = on_circle(p1, r, theta);
    let v2 = [2.0 * p0[0] - v0[0], 2.0 * p0[1] - v0[1]];
    let v3 = [2.0 * p1[0] - v1[0], 2.0 * p1[1] - v1[1]];
    quad(
        [
            vertex(v0, 0.0),
            vertex(v2, 0.0),
            vertex(v3, 1.0),
            vertex(v1, 1.0),
        ],
        emit,
    );
    if extend[0] || extend[1] {
        let d = (p1[0] - p0[0]).abs().max((p1[1] - p0[1]).abs());
        if d != 0.0 {
            r /= d;
        }
    }
    let dx = (p1[0] - p0[0]) * r;
    let dy = (p1[1] - p0[1]) * r;
    if extend[0] {
        let e0 = [v0[0] - dx, v0[1] - dy];
        let e1 = [v2[0] - dx, v2[1] - dy];
        quad(
            [
                vertex(e0, 0.0),
                vertex(v0, 0.0),
                vertex(v2, 0.0),
                vertex(e1, 0.0),
            ],
            emit,
        );
    }
    if extend[1] {
        let e0 = [v1[0] + dx, v1[1] + dy];
        let e1 = [v3[0] + dx, v3[1] + dy];
        quad(
            [
                vertex(e0, 1.0),
                vertex(v1, 1.0),
                vertex(v3, 1.0),
                vertex(e1, 1.0),
            ],
            emit,
        );
    }
}
#[allow(clippy::too_many_arguments)]
fn annulus(
    ctm: &M32,
    p0: [f32; 2],
    r0: f32,
    c0: f32,
    p1: [f32; 2],
    r1: f32,
    c1: f32,
    count: i32,
    emit: &mut dyn FnMut([ShadeVertex; 3]),
) {
    let theta = (p1[1] - p0[1]).atan2(p1[0] - p0[0]);
    let step = std::f32::consts::PI / count as f32;
    let mut a = 0.0;
    for i in 1..=count {
        let b = i as f32 * step;
        let t0 = vertex(ctm.point(on_circle(p0, r0, theta + a)), c0);
        let t1 = vertex(ctm.point(on_circle(p0, r0, theta + b)), c0);
        let t2 = vertex(ctm.point(on_circle(p1, r1, theta + a)), c1);
        let t3 = vertex(ctm.point(on_circle(p1, r1, theta + b)), c1);
        let b0 = vertex(ctm.point(on_circle(p0, r0, theta - a)), c0);
        let b1 = vertex(ctm.point(on_circle(p0, r0, theta - b)), c0);
        let b2 = vertex(ctm.point(on_circle(p1, r1, theta - a)), c1);
        let b3 = vertex(ctm.point(on_circle(p1, r1, theta - b)), c1);
        quad([t0, t2, t3, t1], emit);
        quad([b0, b2, b3, b1], emit);
        a = b;
    }
}
fn radial(coords: &[f64; 6], extend: [bool; 2], ctm: &M32, emit: &mut dyn FnMut([ShadeVertex; 3])) {
    let c = coords.map(|v| v as f32);
    let p0 = [c[0], c[1]];
    let r0 = c[2];
    let p1 = [c[3], c[4]];
    let r1 = c[5];
    // Segments per half circle.
    let count = ((4.0 * (ctm.expansion() * r0.max(r1)).sqrt()) as i32).clamp(3, 1024);
    if extend[0] {
        let rs = if r0 < r1 { r0 / (r0 - r1) } else { -HUGENUM };
        let e = [p0[0] + (p1[0] - p0[0]) * rs, p0[1] + (p1[1] - p0[1]) * rs];
        let er = r0 + (r1 - r0) * rs;
        annulus(ctm, e, er, 0.0, p0, r0, 0.0, count, emit);
    }
    annulus(ctm, p0, r0, 0.0, p1, r1, 1.0, count, emit);
    if extend[1] {
        let rs = if r0 > r1 { r1 / (r1 - r0) } else { -HUGENUM };
        let e = [p1[0] + (p0[0] - p1[0]) * rs, p1[1] + (p0[1] - p1[1]) * rs];
        let er = r1 + (r0 - r1) * rs;
        annulus(ctm, p1, r1, 1.0, e, er, 1.0, count, emit);
    }
}
/// The quads of a function-based shading's sample grid; `ctm` maps the
/// domain to device space and `samples` holds RGB × 255 per grid point.
fn function_grid(ctm: &M32, samples: &[[f32; 3]], emit: &mut dyn FnMut([ShadeVertex; 3])) {
    let at = |xx: usize, yy: usize| -> ShadeVertex {
        let [x, y] = ctm.point([xx as f32 / FUNSEGS as f32, yy as f32 / FUNSEGS as f32]);
        ShadeVertex {
            x,
            y,
            value: samples
                .get(yy * (FUNSEGS + 1) + xx)
                .copied()
                .unwrap_or([0.0; 3]),
        }
    };
    for yy in 0..FUNSEGS {
        let mut v = [at(0, yy), at(0, yy + 1)];
        for xx in 0..FUNSEGS {
            let vn = [at(xx + 1, yy), at(xx + 1, yy + 1)];
            quad([v[0], vn[0], vn[1], v[1]], emit);
            v = vn;
        }
    }
}

/// A patch's control points as a 4 × 4 pole grid with corner colours.
#[derive(Clone, Copy)]
struct Tensor {
    pole: [[[f32; 2]; 4]; 4],
    color: [[f32; MAX_COLORANTS]; 4],
}
fn tensor_interior(p: [[f32; 2]; 8]) -> [f32; 2] {
    let [a, b, c, d, e, f, g, h] = p;
    std::array::from_fn(|i| {
        (-4.0 * a[i] + 6.0 * (b[i] + c[i]) - 2.0 * (d[i] + e[i]) + 3.0 * (f[i] + g[i]) - h[i]) / 9.0
    })
}
fn make_tensor(pt: &[[f32; 2]; 16], kind: u8, color: [[f32; MAX_COLORANTS]; 4]) -> Tensor {
    let mut p = [[[0.0; 2]; 4]; 4];
    p[0][0] = pt[0];
    p[0][1] = pt[1];
    p[0][2] = pt[2];
    p[0][3] = pt[3];
    p[1][3] = pt[4];
    p[2][3] = pt[5];
    p[3][3] = pt[6];
    p[3][2] = pt[7];
    p[3][1] = pt[8];
    p[3][0] = pt[9];
    p[2][0] = pt[10];
    p[1][0] = pt[11];
    if kind == 6 {
        p[1][1] = tensor_interior([
            p[0][0], p[0][1], p[1][0], p[0][3], p[3][0], p[3][1], p[1][3], p[3][3],
        ]);
        p[1][2] = tensor_interior([
            p[0][3], p[0][2], p[1][3], p[0][0], p[3][3], p[3][2], p[1][0], p[3][0],
        ]);
        p[2][1] = tensor_interior([
            p[3][0], p[3][1], p[2][0], p[3][3], p[0][0], p[0][1], p[2][3], p[0][3],
        ]);
        p[2][2] = tensor_interior([
            p[3][3], p[3][2], p[2][3], p[3][0], p[0][3], p[0][2], p[2][0], p[0][0],
        ]);
    } else {
        p[1][1] = pt[12];
        p[1][2] = pt[13];
        p[2][2] = pt[14];
        p[2][1] = pt[15];
    }
    Tensor { pole: p, color }
}
fn mid(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}
fn midcolor(a: &[f32; MAX_COLORANTS], b: &[f32; MAX_COLORANTS]) -> [f32; MAX_COLORANTS] {
    std::array::from_fn(|i| (a[i] + b[i]) * 0.5)
}
/// De Casteljau split of the cubic `pole[0..4]` at its midpoint.
fn split_curve(pole: [[f32; 2]; 4]) -> ([[f32; 2]; 4], [[f32; 2]; 4]) {
    let p12 = mid(pole[1], pole[2]);
    let q01 = mid(pole[0], pole[1]);
    let q12 = mid(pole[2], pole[3]);
    let q02 = mid(q01, p12);
    let q11 = mid(p12, q12);
    let m = mid(q02, q11);
    ([pole[0], q01, q02, m], [m, q11, q12, pole[3]])
}
/// Splits the curves that run along the second pole index (MuPDF's
/// `split_patch`).
fn split_patch(p: &Tensor) -> (Tensor, Tensor) {
    let mut s0 = *p;
    let mut s1 = *p;
    for i in 0..4 {
        let (a, b) = split_curve(p.pole[i]);
        s0.pole[i] = a;
        s1.pole[i] = b;
    }
    s0.color[1] = midcolor(&p.color[0], &p.color[1]);
    s0.color[2] = midcolor(&p.color[2], &p.color[3]);
    s1.color[0] = s0.color[1];
    s1.color[3] = s0.color[2];
    (s0, s1)
}
/// Splits the curves that run along the first pole index (MuPDF's
/// `split_stripe`).
fn split_stripe(p: &Tensor) -> (Tensor, Tensor) {
    let mut s0 = *p;
    let mut s1 = *p;
    for j in 0..4 {
        let column = [p.pole[0][j], p.pole[1][j], p.pole[2][j], p.pole[3][j]];
        let (a, b) = split_curve(column);
        for i in 0..4 {
            s0.pole[i][j] = a[i];
            s1.pole[i][j] = b[i];
        }
    }
    s0.color[2] = midcolor(&p.color[1], &p.color[2]);
    s0.color[3] = midcolor(&p.color[0], &p.color[3]);
    s1.color[0] = s0.color[3];
    s1.color[1] = s0.color[2];
    (s0, s1)
}
type Prepare<'a> = dyn Fn(&[f32; MAX_COLORANTS]) -> [f32; 3] + 'a;
fn triangulate(p: &Tensor, prepare: &Prepare<'_>, emit: &mut dyn FnMut([ShadeVertex; 3])) {
    let corner = |[x, y]: [f32; 2], c: &[f32; MAX_COLORANTS]| ShadeVertex {
        x,
        y,
        value: prepare(c),
    };
    quad(
        [
            corner(p.pole[0][0], &p.color[0]),
            corner(p.pole[0][3], &p.color[1]),
            corner(p.pole[3][3], &p.color[2]),
            corner(p.pole[3][0], &p.color[3]),
        ],
        emit,
    );
}
fn draw_stripe(
    p: &Tensor,
    depth: u32,
    prepare: &Prepare<'_>,
    emit: &mut dyn FnMut([ShadeVertex; 3]),
) {
    let (s0, s1) = split_stripe(p);
    if depth <= 1 {
        triangulate(&s1, prepare, emit);
        triangulate(&s0, prepare, emit);
    } else {
        draw_stripe(&s1, depth - 1, prepare, emit);
        draw_stripe(&s0, depth - 1, prepare, emit);
    }
}
fn subdivide(
    p: &Tensor,
    depth: u32,
    prepare: &Prepare<'_>,
    emit: &mut dyn FnMut([ShadeVertex; 3]),
) {
    let (s0, s1) = split_patch(p);
    if depth <= 1 {
        draw_stripe(&s0, SUBDIV, prepare, emit);
        draw_stripe(&s1, SUBDIV, prepare, emit);
    } else {
        subdivide(&s0, depth - 1, prepare, emit);
        subdivide(&s1, depth - 1, prepare, emit);
    }
}
fn draw_patch(
    patch: &Patch,
    kind: u8,
    ctm: &M32,
    prepare: &Prepare<'_>,
    emit: &mut dyn FnMut([ShadeVertex; 3]),
) {
    let points = patch.points.map(|p| ctm.point(p));
    let tensor = make_tensor(&points, kind, patch.colors);
    subdivide(&tensor, SUBDIV, prepare, emit);
}

struct Bits<'a> {
    data: &'a [u8],
    bit: usize,
}
impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        if n == 0 || n > 32 || self.bit.checked_add(n)? > self.data.len() * 8 {
            return None;
        }
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u32::from((self.data[self.bit / 8] >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Some(v)
    }
    /// A sample decoded over `range` as MuPDF's `read_sample` computes it.
    fn sample(&mut self, n: usize, [min, max]: [f32; 2]) -> Option<f32> {
        let bitscale = 1.0 / (2f32.powi(n as i32) - 1.0);
        Some(min + self.read(n)? as f32 * (max - min) * bitscale)
    }
}
struct MeshReader<'a> {
    bits: Bits<'a>,
    flag: usize,
    coord: usize,
    component: usize,
    ranges: Vec<[f32; 2]>,
    space: &'a ColorSpace,
    functions: &'a [Function],
    n: usize,
}
impl<'a> MeshReader<'a> {
    fn new(data: &'a [u8], d: &Dict, space: &'a ColorSpace, functions: &'a [Function]) -> Self {
        let n = if functions.is_empty() {
            space.components()
        } else {
            1
        };
        let mut ranges = vec![[0.0, 1.0]; n + 2];
        if let Some(a) = d.get_array(b"Decode") {
            for (r, p) in ranges.iter_mut().zip(a.as_chunks::<2>().0) {
                *r = [
                    p[0].as_f64().unwrap_or(0.0) as f32,
                    p[1].as_f64().unwrap_or(1.0) as f32,
                ];
            }
        }
        let flag = d.get_i64(b"BitsPerFlag").unwrap_or(8) as usize;
        let coord = d.get_i64(b"BitsPerCoordinate").unwrap_or(8) as usize;
        let component = d.get_i64(b"BitsPerComponent").unwrap_or(8) as usize;
        Self {
            bits: Bits { data, bit: 0 },
            flag: if matches!(flag, 2 | 4 | 8) { flag } else { 8 },
            coord: if matches!(coord, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
                coord
            } else {
                8
            },
            component: if matches!(component, 1 | 2 | 4 | 8 | 12 | 16) {
                component
            } else {
                8
            },
            ranges,
            space,
            functions,
            n,
        }
    }
    fn point(&mut self) -> Option<[f32; 2]> {
        Some([
            self.bits.sample(self.coord, self.ranges[0])?,
            self.bits.sample(self.coord, self.ranges[1])?,
        ])
    }
    fn components(&mut self) -> Option<[f32; MAX_COLORANTS]> {
        let mut c = [0.0; MAX_COLORANTS];
        for (i, v) in c.iter_mut().take(self.n).enumerate() {
            *v = self.bits.sample(self.component, self.ranges[i + 2])?;
        }
        Some(c)
    }
    fn vertex(&mut self) -> Option<MeshVertex> {
        let point = self.point()?;
        let c = self.components()?;
        let range = (!self.functions.is_empty()).then(|| self.ranges[2]);
        Some(MeshVertex {
            point,
            value: prepare_value(&c, self.n, self.space, range),
        })
    }
    fn triangles(&mut self, out: &mut Vec<[MeshVertex; 3]>) -> Option<()> {
        let mut prev: Option<[MeshVertex; 3]> = None;
        while let Some(flag) = self.bits.read(self.flag) {
            let Some(v) = self.vertex() else {
                break;
            };
            let tri = match (flag, prev) {
                (1, Some(p)) => [p[1], p[2], v],
                (2, Some(p)) => [p[0], p[2], v],
                (0, _) | (1 | 2, None) => {
                    if self.bits.read(self.flag).is_none() {
                        break;
                    }
                    let Some(b) = self.vertex() else {
                        break;
                    };
                    if self.bits.read(self.flag).is_none() {
                        break;
                    }
                    let Some(c) = self.vertex() else {
                        break;
                    };
                    [v, b, c]
                }
                _ => continue,
            };
            add_triangle(out, tri)?;
            prev = Some(tri);
        }
        Some(())
    }
    fn lattice(&mut self, out: &mut Vec<[MeshVertex; 3]>, count: usize) -> Option<()> {
        let mut prev = Vec::new();
        loop {
            let row: Vec<MeshVertex> = (0..count).map_while(|_| self.vertex()).collect();
            if row.len() != count {
                break;
            }
            if prev.len() == count {
                for i in 0..count - 1 {
                    // MuPDF's quad split of (prev[i], prev[i+1], row[i+1], row[i]).
                    add_triangle(out, [prev[i], prev[i + 1], row[i]])?;
                    add_triangle(out, [row[i], row[i + 1], prev[i + 1]])?;
                }
            }
            prev = row;
        }
        Some(())
    }
    /// Reads patches with `count` control points each (12 for Coons, 16
    /// for tensor).
    fn patches(&mut self, out: &mut Vec<Patch>, count: usize) -> Option<()> {
        let mut previous: Option<Patch> = None;
        'patch: while let Some(flag) = self.bits.read(self.flag) {
            let mut points = [[0.0; 2]; 16];
            let mut colors = [[0.0; MAX_COLORANTS]; 4];
            let start = if flag == 0 { 0 } else { 4 };
            for p in points.iter_mut().take(count).skip(start) {
                let Some(point) = self.point() else {
                    break 'patch;
                };
                *p = point;
            }
            for c in colors.iter_mut().skip(if flag == 0 { 0 } else { 2 }) {
                let Some(color) = self.components() else {
                    break 'patch;
                };
                *c = color;
            }
            if flag != 0 {
                let Some(p) = &previous else {
                    continue;
                };
                if flag > 3 {
                    continue;
                }
                let first = flag as usize * 3;
                for (i, point) in points.iter_mut().take(4).enumerate() {
                    *point = p.points[(first + i) % 12];
                }
                colors[0] = p.colors[flag as usize];
                colors[1] = p.colors[(flag as usize + 1) % 4];
            }
            if out.len() >= MAX_PATCHES {
                return None;
            }
            let patch = Patch { points, colors };
            out.push(patch.clone());
            previous = Some(patch);
        }
        Some(())
    }
}
fn add_triangle(out: &mut Vec<[MeshVertex; 3]>, tri: [MeshVertex; 3]) -> Option<()> {
    if out.len() >= MAX_TRIANGLES {
        return None;
    }
    out.push(tri);
    Some(())
}
