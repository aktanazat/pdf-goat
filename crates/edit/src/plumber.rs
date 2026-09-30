//! pdfplumber's page objects and `extract_tables()`.
//!
//! The chars, lines, rects and curves come from a pdf-interp run the way
//! pdfminer.six builds them (`PDFLayoutAnalyzer.paint_path`,
//! `render_char`), in pdfplumber's top-left coordinates. The table finder
//! is `pdfplumber.table` with the default settings (`lines` strategies,
//! snap and join tolerance 3, intersection tolerance 3, edge min length 3
//! after a prefilter of 1) and `extract_text(x_tolerance=3, y_tolerance=3)`
//! for the cell text.

use std::collections::{HashMap, HashSet};

use pdf_core::{Dict, Document, Matrix, Object, Page, Point, Rect};
use pdf_interp::{
    Device, FillEvent, FontSubtype, GlyphText, Path, PathEl, PdfFont, Provenance, RunOptions,
    StrokeEvent, TextRun,
};

use crate::EditError;

const SNAP_TOLERANCE: f64 = 3.0;
const JOIN_TOLERANCE: f64 = 3.0;
const INTERSECTION_TOLERANCE: f64 = 3.0;
const EDGE_MIN_LENGTH: f64 = 3.0;
const EDGE_MIN_LENGTH_PREFILTER: f64 = 1.0;
const X_TOLERANCE: f64 = 3.0;
const Y_TOLERANCE: f64 = 3.0;

/// A pdfplumber `char`.
#[derive(Clone, Debug)]
pub struct Char {
    pub text: String,
    pub x0: f64,
    pub top: f64,
    pub x1: f64,
    pub bottom: f64,
    pub upright: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Orientation {
    Horizontal,
    Vertical,
}

/// A pdfplumber edge: the four bbox keys and the orientation are all the
/// table finder reads.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Edge {
    x0: f64,
    top: f64,
    x1: f64,
    bottom: f64,
    orientation: Orientation,
}

impl Edge {
    fn length(&self) -> f64 {
        match self.orientation {
            Orientation::Horizontal => self.x1 - self.x0,
            Orientation::Vertical => self.bottom - self.top,
        }
    }

    fn bbox(&self) -> [u64; 4] {
        [
            self.x0.to_bits(),
            self.top.to_bits(),
            self.x1.to_bits(),
            self.bottom.to_bits(),
        ]
    }
}

/// A pdfminer path object in pdfplumber coordinates.
#[derive(Clone, Debug)]
enum Shape {
    Line {
        x0: f64,
        top: f64,
        x1: f64,
        bottom: f64,
    },
    Rect {
        x0: f64,
        top: f64,
        x1: f64,
        bottom: f64,
    },
    Curve {
        pts: Vec<(f64, f64)>,
    },
}

/// pdfplumber's objects for one page.
#[derive(Debug, Default)]
pub struct PlumberPage {
    pub chars: Vec<Char>,
    shapes: Vec<Shape>,
}

/// `(x0, top, x1, bottom)` of a table cell.
type Cell = (f64, f64, f64, f64);

/// Runs the page through pdf-interp and builds pdfplumber's objects.
pub fn plumber_page(doc: &Document, page: &Page) -> Result<PlumberPage, EditError> {
    // pdfminer: coordinates relative to the (raw) MediaBox with /Rotate
    // folded in; pdfplumber: `top = height - y1`.
    let rotation = page.rotation();
    let media = page.media_box();
    let (mx0, my0, mx1, my1) = (media.x0, media.y0, media.x1, media.y1);
    let pdfminer_ctm = match rotation {
        90 => Matrix::new(0.0, -1.0, 1.0, 0.0, -my0, mx1),
        180 => Matrix::new(-1.0, 0.0, 0.0, -1.0, mx1, my1),
        270 => Matrix::new(0.0, 1.0, -1.0, 0.0, my1, -mx0),
        _ => Matrix::new(1.0, 0.0, 0.0, 1.0, -mx0, -my0),
    };
    // pdfplumber's mediabox: normalized, swapped for 90/270, y inverted.
    let (w, h) = (mx1 - mx0, my1 - my0);
    let height = if matches!(rotation, 90 | 270) { w } else { h };
    let inverse = pdf_interp::page_transform(page)
        .invert()
        .unwrap_or(Matrix::IDENTITY);
    let mut device = PlumberDevice {
        doc,
        ctm: pdfminer_ctm,
        height,
        page: PlumberPage::default(),
        painted: HashSet::new(),
        descents: HashMap::new(),
    };
    let options = RunOptions {
        transform: inverse,
        annotations: false,
        ..RunOptions::default()
    };
    pdf_interp::run_page_contents(doc, page, &mut device, &options)?;
    Ok(device.page)
}

struct PlumberDevice<'a> {
    doc: &'a Document,
    ctm: Matrix,
    height: f64,
    page: PlumberPage,
    painted: HashSet<Provenance>,
    descents: HashMap<u64, f64>,
}

impl PlumberDevice<'_> {
    /// pdfplumber `point2coord` of a pdfminer point.
    fn to_plumber(&self, p: Point) -> (f64, f64) {
        (p.x, self.height - p.y)
    }

    fn paint(&mut self, path: &Path, ctm: &Matrix, source: &Provenance) {
        if !self.painted.insert(source.clone()) {
            return;
        }
        let full = ctm.concat(&self.ctm);
        // Split at every moveto, as `paint_path` recurses per subpath.
        let mut subpaths: Vec<Vec<(char, Point)>> = Vec::new();
        for el in path.elements() {
            match *el {
                PathEl::MoveTo(p) => subpaths.push(vec![('m', p.transform(&full))]),
                PathEl::LineTo(p) => {
                    if let Some(sub) = subpaths.last_mut() {
                        sub.push(('l', p.transform(&full)));
                    }
                }
                PathEl::CurveTo(_, _, p) => {
                    if let Some(sub) = subpaths.last_mut() {
                        sub.push(('c', p.transform(&full)));
                    }
                }
                PathEl::Close => {
                    if let Some(sub) = subpaths.last_mut()
                        && let Some(&(_, start)) = sub.first()
                    {
                        sub.push(('h', start));
                    }
                }
            }
        }
        let single = subpaths.len() == 1;
        for sub in subpaths {
            if sub.len() < 2 && !single {
                continue;
            }
            let shape: String = sub.iter().map(|(op, _)| *op).collect();
            let pts: Vec<Point> = sub.iter().map(|(_, p)| *p).collect();
            let object = if shape == "mlh" || shape == "ml" {
                let (a, b) = (pts[0], pts[1]);
                Shape::Line {
                    x0: a.x.min(b.x),
                    top: self.height - a.y.max(b.y),
                    x1: a.x.max(b.x),
                    bottom: self.height - a.y.min(b.y),
                }
            } else if shape == "mlllh" || shape == "mllll" {
                let [p0, p1, p2, p3, p4] = [pts[0], pts[1], pts[2], pts[3], pts[4]];
                let closed = p0 == p4;
                let square = (p0.x == p1.x && p1.y == p2.y && p2.x == p3.x && p3.y == p0.y)
                    || (p0.y == p1.y && p1.x == p2.x && p2.y == p3.y && p3.x == p0.x);
                if closed && square {
                    Shape::Rect {
                        x0: p0.x.min(p2.x),
                        top: self.height - p0.y.max(p2.y),
                        x1: p0.x.max(p2.x),
                        bottom: self.height - p0.y.min(p2.y),
                    }
                } else {
                    Shape::Curve {
                        pts: pts.iter().map(|p| self.to_plumber(*p)).collect(),
                    }
                }
            } else {
                Shape::Curve {
                    pts: pts.iter().map(|p| self.to_plumber(*p)).collect(),
                }
            };
            self.page.shapes.push(object);
        }
    }

    /// pdfminer's `font.get_descent()`: the AFM value for the standard
    /// names it knows, else /FontDescriptor /Descent (0 when absent), in em.
    fn descent(&mut self, font: &PdfFont) -> f64 {
        if let Some(value) = self.descents.get(&font.id()) {
            return *value;
        }
        let value = pdfminer_descent(self.doc, font);
        self.descents.insert(font.id(), value);
        value
    }
}

/// pdfminer's `FONT_METRICS` descents (AFM `Descender`), per 1000 em.
fn afm_descent(base_font: &str) -> Option<f64> {
    Some(match base_font {
        "Courier"
        | "Courier-Bold"
        | "Courier-BoldOblique"
        | "Courier-Oblique"
        | "CourierNew"
        | "CourierNew,Bold"
        | "CourierNew,BoldItalic"
        | "CourierNew,Italic" => -194.0,
        "Helvetica"
        | "Helvetica-Bold"
        | "Helvetica-BoldOblique"
        | "Helvetica-Oblique"
        | "Arial"
        | "Arial,Bold"
        | "Arial,BoldItalic"
        | "Arial,Italic" => -207.0,
        "Times-Roman"
        | "Times-Bold"
        | "Times-BoldItalic"
        | "Times-Italic"
        | "TimesNewRoman"
        | "TimesNewRoman,Bold"
        | "TimesNewRoman,BoldItalic"
        | "TimesNewRoman,Italic" => -217.0,
        "Symbol" | "ZapfDingbats" => 0.0,
        _ => return None,
    })
}

fn pdfminer_descent(doc: &Document, font: &PdfFont) -> f64 {
    if font.subtype() != FontSubtype::Type3
        && let Some(descent) = afm_descent(font.base_font())
    {
        return descent * 0.001;
    }
    let Some(dict) = font
        .object()
        .and_then(|id| doc.get(id).ok())
        .and_then(|o| o.as_dict().cloned())
    else {
        return 0.0;
    };
    if font.subtype() == FontSubtype::Type3 {
        let bbox = dict
            .get(b"FontBBox")
            .and_then(|o| doc.resolve_array(o).ok().flatten())
            .and_then(|items| items.get(1).and_then(Object::as_f64))
            .unwrap_or(0.0);
        let matrix = dict
            .get(b"FontMatrix")
            .and_then(|o| doc.resolve_array(o).ok().flatten())
            .and_then(|items| Matrix::from_array(&items))
            .unwrap_or(Matrix::new(0.001, 0.0, 0.0, 0.001, 0.0, 0.0));
        return bbox * (matrix.b + matrix.d);
    }
    let descriptor = font_descriptor(doc, &dict);
    descriptor
        .and_then(|d| {
            d.get(b"Descent")
                .and_then(|o| doc.resolve_f64(o).ok().flatten())
        })
        .unwrap_or(0.0)
        * 0.001
}

fn font_descriptor(doc: &Document, font: &Dict) -> Option<Dict> {
    if font.get_name(b"Subtype") == Some(b"Type0") {
        let descendants = doc.resolve_array(font.get(b"DescendantFonts")?).ok()??;
        let child = doc.resolve_dict(descendants.first()?).ok()??;
        return doc.resolve_dict(child.get(b"FontDescriptor")?).ok()?;
    }
    doc.resolve_dict(font.get(b"FontDescriptor")?).ok()?
}

impl Device for PlumberDevice<'_> {
    fn fill_path(&mut self, path: &Path, event: &FillEvent<'_>) {
        self.paint(path, &event.ctm, event.source);
    }

    fn stroke_path(&mut self, path: &Path, event: &StrokeEvent<'_>) {
        self.paint(path, &event.ctm, event.source);
    }

    fn text(&mut self, run: &TextRun<'_>) {
        let descent = self.descent(run.font);
        for glyph in run.glyphs {
            // pdfminer: the glyph box (0, descent + rise) .. (adv, descent + rise + size)
            // through Tm x CTM is trm applied to (0, d) and (w, d + 1).
            let m = glyph.trm.concat(&self.ctm);
            let a = Point::new(0.0, descent).transform(&m);
            let b = Point::new(glyph.width, descent + 1.0).transform(&m);
            let upright = m.a * m.d > 0.0 && m.b * m.c <= 0.0;
            let text = glyph_text(glyph.unicode, glyph.cid);
            self.page.chars.push(Char {
                text,
                x0: a.x.min(b.x),
                top: self.height - a.y.max(b.y),
                x1: a.x.max(b.x),
                bottom: self.height - a.y.min(b.y),
                upright,
            });
        }
    }
}

fn glyph_text(unicode: GlyphText, cid: u32) -> String {
    let chars = unicode.as_slice();
    if chars.is_empty() || chars.iter().all(|c| *c == '\u{FFFD}') {
        return format!("(cid:{cid})");
    }
    chars.iter().collect()
}

pub(crate) struct Table {
    pub rect: Rect,
    pub widths: Vec<f64>,
    pub heights: Vec<f64>,
    pub cells: Vec<TableCell>,
}

pub(crate) struct TableCell {
    pub rect: Rect,
    pub row: usize,
    pub column: usize,
    pub row_span: usize,
    pub column_span: usize,
}

impl PlumberPage {
    /// `page.edges`: every line, rect side, and curve segment.
    fn edges(&self) -> Vec<Edge> {
        let mut edges = Vec::new();
        for shape in &self.shapes {
            match shape {
                Shape::Line {
                    x0,
                    top,
                    x1,
                    bottom,
                } => edges.push(Edge {
                    x0: *x0,
                    top: *top,
                    x1: *x1,
                    bottom: *bottom,
                    orientation: if top == bottom {
                        Orientation::Horizontal
                    } else {
                        Orientation::Vertical
                    },
                }),
                Shape::Rect {
                    x0,
                    top,
                    x1,
                    bottom,
                } => {
                    edges.push(Edge {
                        x0: *x0,
                        top: *top,
                        x1: *x1,
                        bottom: *top,
                        orientation: Orientation::Horizontal,
                    });
                    edges.push(Edge {
                        x0: *x0,
                        top: *bottom,
                        x1: *x1,
                        bottom: *bottom,
                        orientation: Orientation::Horizontal,
                    });
                    edges.push(Edge {
                        x0: *x0,
                        top: *top,
                        x1: *x0,
                        bottom: *bottom,
                        orientation: Orientation::Vertical,
                    });
                    edges.push(Edge {
                        x0: *x1,
                        top: *top,
                        x1: *x1,
                        bottom: *bottom,
                        orientation: Orientation::Vertical,
                    });
                }
                Shape::Curve { pts } => {
                    for pair in pts.windows(2) {
                        let (p0, p1) = (pair[0], pair[1]);
                        let orientation = if p0.0 == p1.0 {
                            Some(Orientation::Vertical)
                        } else if p0.1 == p1.1 {
                            Some(Orientation::Horizontal)
                        } else {
                            None
                        };
                        // A slanted curve edge has no orientation; the
                        // `lines` strategy filters it out.
                        if let Some(orientation) = orientation {
                            edges.push(Edge {
                                x0: p0.0.min(p1.0),
                                top: p0.1.min(p1.1),
                                x1: p0.0.max(p1.0),
                                bottom: p0.1.max(p1.1),
                                orientation,
                            });
                        }
                    }
                }
            }
        }
        edges
    }

    /// `page.extract_tables()` with the default settings: rows of cell
    /// text, `None` where the grid has no cell.
    pub fn extract_tables(&self) -> Vec<Vec<Vec<Option<String>>>> {
        self.grids()
            .iter()
            .map(|table| self.extract_table(table))
            .collect()
    }

    pub(crate) fn table_layouts(&self) -> Vec<Table> {
        self.grids()
            .iter()
            .map(|cells| {
                let x0 = cells.iter().map(|c| c.0).fold(f64::INFINITY, f64::min);
                let y0 = cells.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
                let x1 = cells.iter().map(|c| c.2).fold(f64::NEG_INFINITY, f64::max);
                let y1 = cells.iter().map(|c| c.3).fold(f64::NEG_INFINITY, f64::max);
                let mut columns = cells.iter().flat_map(|c| [c.0, c.2]).collect::<Vec<_>>();
                columns.sort_by(f64::total_cmp);
                columns.dedup();
                let mut rows = cells.iter().flat_map(|c| [c.1, c.3]).collect::<Vec<_>>();
                rows.sort_by(f64::total_cmp);
                rows.dedup();
                Table {
                    rect: Rect::new(x0, y0, x1, y1),
                    widths: columns.windows(2).map(|c| c[1] - c[0]).collect(),
                    heights: rows.windows(2).map(|r| r[1] - r[0]).collect(),
                    cells: cells
                        .iter()
                        .map(|c| {
                            let column = columns.partition_point(|x| *x < c.0);
                            let row = rows.partition_point(|y| *y < c.1);
                            TableCell {
                                rect: Rect::new(c.0, c.1, c.2, c.3),
                                row,
                                column,
                                row_span: rows.partition_point(|y| *y < c.3) - row,
                                column_span: columns.partition_point(|x| *x < c.2) - column,
                            }
                        })
                        .collect(),
                }
            })
            .collect()
    }

    fn grids(&self) -> Vec<Vec<Cell>> {
        let all = self.edges();
        let filtered = |orientation| {
            all.iter()
                .filter(|e| {
                    e.orientation == orientation
                        && e.length() > 0.0
                        && e.length() >= EDGE_MIN_LENGTH_PREFILTER
                })
                .copied()
                .collect::<Vec<Edge>>()
        };
        let mut edges = filtered(Orientation::Vertical);
        edges.extend(filtered(Orientation::Horizontal));
        let edges = merge_edges(edges);
        let edges: Vec<Edge> = edges
            .into_iter()
            .filter(|e| e.length() > 0.0 && e.length() >= EDGE_MIN_LENGTH)
            .collect();
        let intersections = edges_to_intersections(&edges);
        let cells = intersections_to_cells(&intersections);
        cells_to_tables(&cells)
    }

    fn extract_table(&self, cells: &[Cell]) -> Vec<Vec<Option<String>>> {
        let mut rows = Vec::new();
        let mut sorted: Vec<Cell> = cells.to_vec();
        sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.total_cmp(&b.0)));
        let mut xs: Vec<f64> = cells.iter().map(|c| c.0).collect();
        xs.sort_by(f64::total_cmp);
        xs.dedup();
        let mut i = 0;
        while i < sorted.len() {
            let top = sorted[i].1;
            let mut j = i;
            while j < sorted.len() && sorted[j].1 == top {
                j += 1;
            }
            let row_cells: Vec<Option<Cell>> = xs
                .iter()
                .map(|x| sorted[i..j].iter().rev().find(|c| c.0 == *x).copied())
                .collect();
            rows.push(row_cells);
            i = j;
        }
        let in_bbox = |ch: &Char, bbox: Cell| {
            let v_mid = (ch.top + ch.bottom) / 2.0;
            let h_mid = (ch.x0 + ch.x1) / 2.0;
            h_mid >= bbox.0 && h_mid < bbox.2 && v_mid >= bbox.1 && v_mid < bbox.3
        };
        rows.iter()
            .map(|row| {
                let present = row.iter().flatten();
                let bbox = (
                    present.clone().map(|c| c.0).fold(f64::INFINITY, f64::min),
                    present.clone().map(|c| c.1).fold(f64::INFINITY, f64::min),
                    present
                        .clone()
                        .map(|c| c.2)
                        .fold(f64::NEG_INFINITY, f64::max),
                    present.map(|c| c.3).fold(f64::NEG_INFINITY, f64::max),
                );
                let row_chars: Vec<&Char> =
                    self.chars.iter().filter(|c| in_bbox(c, bbox)).collect();
                row.iter()
                    .map(|cell| {
                        let cell = (*cell)?;
                        let cell_chars: Vec<&Char> = row_chars
                            .iter()
                            .filter(|c| in_bbox(c, cell))
                            .copied()
                            .collect();
                        Some(if cell_chars.is_empty() {
                            String::new()
                        } else {
                            extract_text(&cell_chars)
                        })
                    })
                    .collect()
            })
            .collect()
    }
}

/// pdfplumber `cluster_list` over distinct values, then the objects
/// grouped by their value's cluster, original order kept within a cluster.
fn cluster_objects<T>(items: &[T], key: impl Fn(&T) -> f64, tolerance: f64) -> Vec<Vec<usize>> {
    let mut values: Vec<f64> = items.iter().map(&key).collect();
    values.sort_by(f64::total_cmp);
    values.dedup();
    let mut cluster_of: HashMap<u64, usize> = HashMap::new();
    let mut cluster = 0;
    let mut last: Option<f64> = None;
    for value in values {
        if let Some(prev) = last
            && (tolerance == 0.0 || value > prev + tolerance)
        {
            cluster += 1;
        }
        cluster_of.insert(value.to_bits(), cluster);
        last = Some(value);
    }
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); cluster + 1];
    for (index, item) in items.iter().enumerate() {
        if let Some(group) =
            groups.get_mut(cluster_of.get(&key(item).to_bits()).copied().unwrap_or(0))
        {
            group.push(index);
        }
    }
    groups.retain(|group| !group.is_empty());
    groups
}

/// `snap_edges` then `join_edge_group` per infinite line.
fn merge_edges(edges: Vec<Edge>) -> Vec<Edge> {
    let mut snapped = Vec::new();
    for orientation in [Orientation::Vertical, Orientation::Horizontal] {
        let group: Vec<Edge> = edges
            .iter()
            .filter(|e| e.orientation == orientation)
            .copied()
            .collect();
        let clusters = cluster_objects(
            &group,
            |e| {
                if orientation == Orientation::Vertical {
                    e.x0
                } else {
                    e.top
                }
            },
            SNAP_TOLERANCE,
        );
        for cluster in clusters {
            let avg = cluster
                .iter()
                .map(|&i| {
                    if orientation == Orientation::Vertical {
                        group[i].x0
                    } else {
                        group[i].top
                    }
                })
                .sum::<f64>()
                / cluster.len() as f64;
            for &i in &cluster {
                let mut e = group[i];
                if orientation == Orientation::Vertical {
                    let shift = avg - e.x0;
                    e.x0 += shift;
                    e.x1 += shift;
                } else {
                    let shift = avg - e.top;
                    e.top += shift;
                    e.bottom += shift;
                }
                snapped.push(e);
            }
        }
    }
    // Group by ("h", top) / ("v", x0): "h" sorts before "v".
    let group_key = |e: &Edge| match e.orientation {
        Orientation::Horizontal => (0u8, e.top),
        Orientation::Vertical => (1u8, e.x0),
    };
    snapped.sort_by(|a, b| {
        let (ka, kb) = (group_key(a), group_key(b));
        ka.0.cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
    });
    let mut out = Vec::new();
    let mut i = 0;
    while i < snapped.len() {
        let key = group_key(&snapped[i]);
        let mut j = i;
        while j < snapped.len() && group_key(&snapped[j]) == key {
            j += 1;
        }
        let mut items: Vec<Edge> = snapped[i..j].to_vec();
        let horizontal = snapped[i].orientation == Orientation::Horizontal;
        items.sort_by(|a, b| {
            if horizontal {
                a.x0.total_cmp(&b.x0)
            } else {
                a.top.total_cmp(&b.top)
            }
        });
        let mut joined: Vec<Edge> = vec![items[0]];
        for e in &items[1..] {
            let last = joined.len() - 1;
            let (e_min, e_max, last_max) = if horizontal {
                (e.x0, e.x1, joined[last].x1)
            } else {
                (e.top, e.bottom, joined[last].bottom)
            };
            if e_min <= last_max + JOIN_TOLERANCE {
                if e_max > last_max {
                    if horizontal {
                        joined[last].x1 = e_max;
                    } else {
                        joined[last].bottom = e_max;
                    }
                }
            } else {
                joined.push(*e);
            }
        }
        out.extend(joined);
        i = j;
    }
    out
}

type Vertex = (u64, u64);

#[derive(Default)]
struct TouchingEdges {
    vertical: Vec<[u64; 4]>,
    horizontal: Vec<[u64; 4]>,
}

struct Intersections {
    /// Sorted (x0, top) points.
    points: Vec<(f64, f64)>,
    /// Per point: the v edges and h edges through it, as bboxes.
    edges: HashMap<Vertex, TouchingEdges>,
}

fn edges_to_intersections(edges: &[Edge]) -> Intersections {
    let mut v_edges: Vec<Edge> = edges
        .iter()
        .filter(|e| e.orientation == Orientation::Vertical)
        .copied()
        .collect();
    let mut h_edges: Vec<Edge> = edges
        .iter()
        .filter(|e| e.orientation == Orientation::Horizontal)
        .copied()
        .collect();
    v_edges.sort_by(|a, b| a.x0.total_cmp(&b.x0).then(a.top.total_cmp(&b.top)));
    h_edges.sort_by(|a, b| a.top.total_cmp(&b.top).then(a.x0.total_cmp(&b.x0)));
    let mut map: HashMap<Vertex, TouchingEdges> = HashMap::new();
    let mut points = Vec::new();
    for v in &v_edges {
        for h in &h_edges {
            if v.top <= h.top + INTERSECTION_TOLERANCE
                && v.bottom >= h.top - INTERSECTION_TOLERANCE
                && v.x0 >= h.x0 - INTERSECTION_TOLERANCE
                && v.x0 <= h.x1 + INTERSECTION_TOLERANCE
            {
                let vertex = (v.x0.to_bits(), h.top.to_bits());
                let entry = map.entry(vertex).or_insert_with(|| {
                    points.push((v.x0, h.top));
                    TouchingEdges::default()
                });
                entry.vertical.push(v.bbox());
                entry.horizontal.push(h.bbox());
            }
        }
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    Intersections { points, edges: map }
}

fn intersections_to_cells(intersections: &Intersections) -> Vec<Cell> {
    let key = |p: (f64, f64)| (p.0.to_bits(), p.1.to_bits());
    let connects = |p1: (f64, f64), p2: (f64, f64)| -> bool {
        let (Some(a), Some(b)) = (
            intersections.edges.get(&key(p1)),
            intersections.edges.get(&key(p2)),
        ) else {
            return false;
        };
        if p1.0 == p2.0 && a.vertical.iter().any(|e| b.vertical.contains(e)) {
            return true;
        }
        p1.1 == p2.1 && a.horizontal.iter().any(|e| b.horizontal.contains(e))
    };
    let points = &intersections.points;
    let n = points.len();
    let mut cells = Vec::new();
    for i in 0..n {
        if i + 1 == n {
            break;
        }
        let pt = points[i];
        let rest = &points[i + 1..];
        let below: Vec<(f64, f64)> = rest.iter().filter(|p| p.0 == pt.0).copied().collect();
        let right: Vec<(f64, f64)> = rest.iter().filter(|p| p.1 == pt.1).copied().collect();
        'search: for below_pt in &below {
            if !connects(pt, *below_pt) {
                continue;
            }
            for right_pt in &right {
                if !connects(pt, *right_pt) {
                    continue;
                }
                let bottom_right = (right_pt.0, below_pt.1);
                if intersections.edges.contains_key(&key(bottom_right))
                    && connects(bottom_right, *right_pt)
                    && connects(bottom_right, *below_pt)
                {
                    cells.push((pt.0, pt.1, bottom_right.0, bottom_right.1));
                    break 'search;
                }
            }
        }
    }
    cells
}

fn cells_to_tables(cells: &[Cell]) -> Vec<Vec<Cell>> {
    let corners = |c: &Cell| {
        [(c.0, c.1), (c.0, c.3), (c.2, c.1), (c.2, c.3)].map(|p| (p.0.to_bits(), p.1.to_bits()))
    };
    let mut remaining: Vec<Cell> = cells.to_vec();
    let mut current_corners: HashSet<Vertex> = HashSet::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut tables: Vec<Vec<Cell>> = Vec::new();
    while !remaining.is_empty() {
        let initial = current.len();
        for cell in remaining.clone() {
            let cell_corners = corners(&cell);
            let take =
                current.is_empty() || cell_corners.iter().any(|c| current_corners.contains(c));
            if take {
                current_corners.extend(cell_corners);
                current.push(cell);
                if let Some(pos) = remaining.iter().position(|c| *c == cell) {
                    remaining.remove(pos);
                }
            }
        }
        if current.len() == initial {
            tables.push(std::mem::take(&mut current));
            current_corners.clear();
        }
    }
    if !current.is_empty() {
        tables.push(current);
    }
    let table_key = |t: &Vec<Cell>| {
        t.iter()
            .map(|c| (c.1, c.0))
            .fold((f64::INFINITY, f64::INFINITY), |m, k| {
                if k.0 < m.0 || (k.0 == m.0 && k.1 < m.1) {
                    k
                } else {
                    m
                }
            })
    };
    tables.sort_by(|a, b| {
        let (ka, kb) = (table_key(a), table_key(b));
        ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
    });
    tables.into_iter().filter(|t| t.len() > 1).collect()
}

/// pdfplumber `extract_text(chars, x_tolerance=3, y_tolerance=3)`: words
/// per line, lines top to bottom, joined by spaces and newlines.
pub fn extract_text(chars: &[&Char]) -> String {
    #[derive(Clone)]
    struct Word {
        text: String,
        top: f64,
    }
    let mut words: Vec<Word> = Vec::new();
    // groupby upright over consecutive chars
    let mut i = 0;
    while i < chars.len() {
        let upright = chars[i].upright;
        let mut j = i;
        while j < chars.len() && chars[j].upright == upright {
            j += 1;
        }
        let group = &chars[i..j];
        // iter_chars_to_lines: cluster by top (upright) or x0 (rotated).
        let clusters = cluster_objects(
            group,
            |c| if upright { c.top } else { c.x0 },
            if upright { Y_TOLERANCE } else { X_TOLERANCE },
        );
        for cluster in clusters {
            let mut line: Vec<&Char> = cluster.iter().map(|&k| group[k]).collect();
            if upright {
                line.sort_by(|a, b| a.x0.total_cmp(&b.x0));
            } else {
                line.sort_by(|a, b| a.top.total_cmp(&b.top).then(a.bottom.total_cmp(&b.bottom)));
            }
            let mut current: Vec<&Char> = Vec::new();
            let flush = |current: &mut Vec<&Char>, words: &mut Vec<Word>| {
                if current.is_empty() {
                    return;
                }
                let text = current
                    .iter()
                    .map(|c| expand_ligatures(&c.text))
                    .collect::<String>();
                let top = current.iter().map(|c| c.top).fold(f64::INFINITY, f64::min);
                words.push(Word { text, top });
                current.clear();
            };
            for ch in line {
                if !ch.text.is_empty() && ch.text.chars().all(char::is_whitespace) {
                    flush(&mut current, &mut words);
                } else if ch.text.is_empty() {
                    flush(&mut current, &mut words);
                    current.push(ch);
                    flush(&mut current, &mut words);
                } else if let Some(prev) = current.last().copied()
                    && begins_new_word(prev, ch, upright)
                {
                    flush(&mut current, &mut words);
                    current.push(ch);
                } else {
                    current.push(ch);
                }
            }
            flush(&mut current, &mut words);
        }
        i = j;
    }
    let lines = cluster_objects(&words, |w| w.top, Y_TOLERANCE);
    lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|&k| words[k].text.as_str())
                .collect::<Vec<&str>>()
                .join(" ")
        })
        .collect::<Vec<String>>()
        .join("\n")
}

fn begins_new_word(prev: &Char, curr: &Char, upright: bool) -> bool {
    let (x, y, ax, bx, cx, ay, cy) = if upright {
        (
            X_TOLERANCE,
            Y_TOLERANCE,
            prev.x0,
            prev.x1,
            curr.x0,
            prev.top,
            curr.top,
        )
    } else {
        (
            Y_TOLERANCE,
            X_TOLERANCE,
            prev.top,
            prev.bottom,
            curr.top,
            prev.x0,
            curr.x0,
        )
    };
    cx < ax || cx > bx + x || (cy - ay).abs() > y
}

fn expand_ligatures(text: &str) -> String {
    match text {
        "\u{fb00}" => "ff".to_owned(),
        "\u{fb03}" => "ffi".to_owned(),
        "\u{fb04}" => "ffl".to_owned(),
        "\u{fb01}" => "fi".to_owned(),
        "\u{fb02}" => "fl".to_owned(),
        "\u{fb06}" | "\u{fb05}" => "st".to_owned(),
        other => other.to_owned(),
    }
}
