//! Function, axial, radial, triangle and patch-mesh shadings.
use crate::colorspace::{ColorSpace, ColorSpaceCache, MAX_COLORANTS};
use crate::device::Rgb;
use crate::function::Function;
use pdf_core::{Dict, Document, Matrix, Object, Point, Rect};
use std::sync::Arc;

const MAX_TRIANGLES: usize = 1_000_000;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshTriangle {
    pub points: [Point; 3],
    pub colors: [Rgb; 3],
}
#[derive(Debug)]
enum Field {
    Function {
        inverse: Matrix,
        domain: Rect,
        functions: Vec<Function>,
        space: Arc<ColorSpace>,
    },
    Axial {
        coords: [f64; 4],
        extend: [bool; 2],
        colors: Box<[Rgb; 256]>,
    },
    Radial {
        coords: [f64; 6],
        extend: [bool; 2],
        colors: Box<[Rgb; 256]>,
    },
    Mesh,
}
#[derive(Debug)]
pub struct Shading {
    shading_type: u8,
    bbox: Option<Rect>,
    background: Option<Rgb>,
    mesh: Vec<MeshTriangle>,
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
    pub fn mesh(&self) -> &[MeshTriangle] {
        &self.mesh
    }
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
            Field::Mesh => None,
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
            space.to_rgb(
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
        let mut mesh = Vec::new();
        let field = match shading_type {
            1 => {
                let domain = numbers(d, b"Domain", [0.0, 1.0, 0.0, 1.0]);
                let inverse = d
                    .get_array(b"Matrix")
                    .and_then(Matrix::from_array)
                    .unwrap_or(Matrix::IDENTITY)
                    .invert()?;
                Field::Function {
                    inverse,
                    domain: Rect::new(domain[0], domain[2], domain[1], domain[3]),
                    functions,
                    space,
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
                let colors = Box::new(std::array::from_fn(|i| {
                    evaluate(
                        &space,
                        &functions,
                        &[domain[0] + (domain[1] - domain[0]) * i as f64 / 255.0],
                    )
                }));
                if shading_type == 2 {
                    Field::Axial {
                        coords: numbers(d, b"Coords", [0.0; 4]),
                        extend,
                        colors,
                    }
                } else {
                    Field::Radial {
                        coords: numbers(d, b"Coords", [0.0; 6]),
                        extend,
                        colors,
                    }
                }
            }
            4..=7 => {
                let stream = object.as_stream()?;
                let bytes = doc.decode_stream(stream).ok()?.data;
                let mut reader = MeshReader::new(&bytes, d, &space, &functions);
                match shading_type {
                    4 => reader.triangles(&mut mesh),
                    5 => reader.lattice(
                        &mut mesh,
                        d.get_i64(b"VerticesPerRow").unwrap_or(2).clamp(2, 65536) as usize,
                    ),
                    _ => reader.patches(&mut mesh, shading_type),
                }?;
                Field::Mesh
            }
            _ => return None,
        };
        Some(Shading {
            shading_type,
            bbox,
            background,
            mesh,
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
    space.to_rgb(&values[..space.components()])
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
    fn sample(&mut self, n: usize, range: [f64; 2]) -> Option<f64> {
        Some(range[0] + f64::from(self.read(n)?) / ((1u64 << n) - 1) as f64 * (range[1] - range[0]))
    }
}
#[derive(Clone, Copy, Default)]
struct Vertex {
    point: Point,
    color: Rgb,
}
struct MeshReader<'a> {
    bits: Bits<'a>,
    flag: usize,
    coord: usize,
    component: usize,
    ranges: Vec<[f64; 2]>,
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
                *r = [p[0].as_f64().unwrap_or(0.0), p[1].as_f64().unwrap_or(1.0)];
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
    fn point(&mut self) -> Option<Point> {
        Some(Point::new(
            self.bits.sample(self.coord, self.ranges[0])?,
            self.bits.sample(self.coord, self.ranges[1])?,
        ))
    }
    fn components(&mut self) -> Option<[f64; MAX_COLORANTS]> {
        let mut c = [0.0; MAX_COLORANTS];
        for (i, v) in c.iter_mut().take(self.n).enumerate() {
            *v = self.bits.sample(self.component, self.ranges[i + 2])?;
        }
        Some(c)
    }
    fn color(&self, c: &[f64]) -> Rgb {
        if self.functions.is_empty() {
            let mut values = [0.0; MAX_COLORANTS];
            for (a, b) in values.iter_mut().zip(c) {
                *a = *b as f32;
            }
            self.space.to_rgb(&values[..self.n])
        } else {
            evaluate(self.space, self.functions, &c[..1])
        }
    }
    fn vertex(&mut self) -> Option<Vertex> {
        let point = self.point()?;
        let c = self.components()?;
        Some(Vertex {
            point,
            color: self.color(&c),
        })
    }
    fn triangles(&mut self, out: &mut Vec<MeshTriangle>) -> Option<()> {
        let mut prev: Option<[Vertex; 3]> = None;
        while let Some(flag) = self.bits.read(self.flag) {
            let Some(v) = self.vertex() else {
                break;
            };
            let tri = match (flag, prev) {
                (1, Some(p)) => [p[1], p[2], v],
                (2, Some(p)) => [p[0], p[2], v],
                _ => {
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
            };
            add_triangle(out, tri)?;
            prev = Some(tri);
        }
        Some(())
    }
    fn lattice(&mut self, out: &mut Vec<MeshTriangle>, count: usize) -> Option<()> {
        let mut prev = Vec::new();
        loop {
            let row: Vec<Vertex> = (0..count).map_while(|_| self.vertex()).collect();
            if row.len() != count {
                break;
            }
            if prev.len() == count {
                for i in 0..count - 1 {
                    add_triangle(out, [prev[i], prev[i + 1], row[i + 1]])?;
                    add_triangle(out, [prev[i], row[i + 1], row[i]])?;
                }
            }
            prev = row;
        }
        Some(())
    }
    fn patches(&mut self, out: &mut Vec<MeshTriangle>, kind: u8) -> Option<()> {
        let mut previous: Option<([Point; 16], [[f64; MAX_COLORANTS]; 4])> = None;
        'patch: while let Some(flag) = self.bits.read(self.flag) {
            let mut points = [Point::new(0.0, 0.0); 16];
            let mut colors = [[0.0; MAX_COLORANTS]; 4];
            let start = if flag == 0 { 0 } else { 4 };
            for p in points
                .iter_mut()
                .take(if kind == 6 { 12 } else { 16 })
                .skip(start)
            {
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
                let Some((p, c)) = previous else {
                    continue;
                };
                if flag > 3 {
                    continue;
                }
                let first = flag as usize * 3;
                for i in 0..4 {
                    points[i] = p[(first + i) % 12];
                }
                colors[0] = c[flag as usize];
                colors[1] = c[(flag as usize + 1) % 4];
            }
            let grid = patch_grid(points, kind);
            // MuPDF's fixed three subdivisions in each direction: an 8×8 mesh.
            let mut rows = [[Vertex::default(); 9]; 2];
            for y in 0..=8 {
                for (x, vertex) in rows[y % 2].iter_mut().enumerate() {
                    let u = x as f64 / 8.0;
                    let v = y as f64 / 8.0;
                    let bu = basis(u);
                    let bv = basis(v);
                    let mut point = Point::new(0.0, 0.0);
                    for (j, row) in grid.iter().enumerate() {
                        for (i, p) in row.iter().enumerate() {
                            let w = bu[i] * bv[j];
                            point.x += p.x * w;
                            point.y += p.y * w;
                        }
                    }
                    let mut c = [0.0; MAX_COLORANTS];
                    for (i, c) in c.iter_mut().take(self.n).enumerate() {
                        *c = colors[0][i] * (1.0 - u) * (1.0 - v)
                            + colors[1][i] * u * (1.0 - v)
                            + colors[2][i] * u * v
                            + colors[3][i] * (1.0 - u) * v;
                    }
                    *vertex = Vertex {
                        point,
                        color: self.color(&c),
                    };
                }
                if y != 0 {
                    for x in 0..8 {
                        let a = rows[(y - 1) % 2][x];
                        let b = rows[(y - 1) % 2][x + 1];
                        let c = rows[y % 2][x + 1];
                        let d = rows[y % 2][x];
                        add_triangle(out, [a, b, c])?;
                        add_triangle(out, [a, c, d])?;
                    }
                }
            }
            previous = Some((points, colors));
        }
        Some(())
    }
}
fn add_triangle(out: &mut Vec<MeshTriangle>, v: [Vertex; 3]) -> Option<()> {
    if out.len() >= MAX_TRIANGLES {
        return None;
    }
    out.push(MeshTriangle {
        points: v.map(|v| v.point),
        colors: v.map(|v| v.color),
    });
    Some(())
}
fn basis(t: f64) -> [f64; 4] {
    let s = 1.0 - t;
    [s * s * s, 3.0 * s * s * t, 3.0 * s * t * t, t * t * t]
}
fn patch_grid(p: [Point; 16], kind: u8) -> [[Point; 4]; 4] {
    let mut g = [
        [p[0], p[1], p[2], p[3]],
        [p[11], p[12], p[13], p[4]],
        [p[10], p[15], p[14], p[5]],
        [p[9], p[8], p[7], p[6]],
    ];
    if kind == 6 {
        g[1][1] = interior([
            g[0][0], g[0][1], g[1][0], g[0][3], g[3][0], g[3][1], g[1][3], g[3][3],
        ]);
        g[1][2] = interior([
            g[0][3], g[0][2], g[1][3], g[0][0], g[3][3], g[3][2], g[1][0], g[3][0],
        ]);
        g[2][1] = interior([
            g[3][0], g[3][1], g[2][0], g[3][3], g[0][0], g[0][1], g[2][3], g[0][3],
        ]);
        g[2][2] = interior([
            g[3][3], g[3][2], g[2][3], g[3][0], g[0][3], g[0][2], g[2][0], g[0][0],
        ]);
    }
    g
}
fn interior(p: [Point; 8]) -> Point {
    let weights = [-4.0, 6.0, 6.0, -2.0, -2.0, 3.0, 3.0, -1.0];
    let mut v = Point::new(0.0, 0.0);
    for (p, w) in p.into_iter().zip(weights) {
        v.x += p.x * w / 9.0;
        v.y += p.y * w / 9.0;
    }
    v
}
