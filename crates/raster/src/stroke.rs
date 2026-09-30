//! Stroking: turns a path into filled pieces.
//!
//! Every segment becomes a quad, every outer join and cap its own polygon,
//! all with the same orientation, so a nonzero fill of the pieces is their
//! union. At a join the two segment quads are cut along the line from the
//! vertex to the inner corner, so the pieces tile instead of overlapping
//! (overlap would count twice in anti-aliased edge pixels). Stroking happens
//! in user space and the pieces are then mapped to device space, so the pen
//! follows non-uniform transforms.

use std::f64::consts::PI;

use crate::geom::{Point, Transform};
use crate::path::{Path, Polyline, PolylineCollector, flatten};
use crate::raster::{DEVICE_TOLERANCE, Edges};

/// Shape at the open ends of a stroked subpath (PDF `J`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    /// Projecting square cap: extends half the line width past the end.
    Square,
}

/// Shape at corners of a stroked subpath (PDF `j`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineJoin {
    /// Miter join; becomes a bevel when `1 / sin(φ / 2)` exceeds the miter
    /// limit, `φ` being the angle between the two segments.
    #[default]
    Miter,
    Round,
    Bevel,
}

/// Dash pattern (PDF `d`): alternating on and off lengths in user space and
/// the distance into the pattern at which each subpath starts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dash {
    pub array: Vec<f64>,
    pub phase: f64,
}

/// Stroke parameters in user space.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    /// Line width; `0` (or any non-positive or non-finite value) draws a
    /// one-device-pixel hairline.
    pub width: f64,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f64,
    /// `None`, an empty array, an all-zero array, or one with a negative or
    /// non-finite entry strokes solid.
    pub dash: Option<Dash>,
}

impl Default for Stroke {
    fn default() -> Stroke {
        Stroke {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
        }
    }
}

/// Dash output above this many pieces is drawn solid instead.
const MAX_DASH_PIECES: f64 = 200_000.0;
const MAX_ARC_STEPS: usize = 256;

/// Adds the stroke outline of `path` under `transform` to `edges`.
pub(crate) fn stroke_edges(path: &Path, transform: &Transform, stroke: &Stroke, edges: &mut Edges) {
    let hairline = !(stroke.width.is_finite() && stroke.width > 0.0);
    let scale = transform.max_scale();
    let user_tol = if scale.is_finite() && scale > 1e-12 {
        DEVICE_TOLERANCE / scale
    } else {
        DEVICE_TOLERANCE
    };
    let mut collector = PolylineCollector::default();
    flatten(path, &Transform::IDENTITY, user_tol, &mut collector);
    let mut lines = collector.lines;
    lines.retain(|l| l.has_segments);
    if let Some(pattern) = stroke.dash.as_ref().and_then(normalize_dash) {
        lines = apply_dash(
            &lines,
            &pattern,
            stroke.dash.as_ref().map_or(0.0, |d| d.phase),
        );
    }
    let (hw, tol, piece_transform) = if hairline {
        for l in &mut lines {
            for p in &mut l.points {
                *p = transform.apply(*p);
            }
            l.zero_dir = transform.apply_vector(l.zero_dir);
        }
        (0.5, DEVICE_TOLERANCE, Transform::IDENTITY)
    } else {
        (0.5 * stroke.width, user_tol, *transform)
    };
    let miter_limit = if stroke.miter_limit.is_finite() {
        stroke.miter_limit.max(1.0)
    } else {
        10.0
    };
    let mut stroker = Stroker {
        hw,
        tol,
        cap: stroke.cap,
        join: stroke.join,
        miter_limit,
        transform: piece_transform,
        edges,
        scratch: Vec::new(),
        pts: Vec::new(),
        smooth: Vec::new(),
        cuts: Vec::new(),
    };
    let eps = tol * 1e-4;
    for l in &lines {
        stroker.polyline(l, eps);
    }
}

/// Even-length, positive-total dash array, or `None` for a solid stroke.
fn normalize_dash(dash: &Dash) -> Option<Vec<f64>> {
    let a = &dash.array;
    if a.is_empty() || a.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return None;
    }
    let mut out = a.clone();
    if out.len() % 2 == 1 {
        out.extend_from_slice(a);
    }
    let total: f64 = out.iter().sum();
    (total > 0.0 && total.is_finite()).then_some(out)
}

fn apply_dash(lines: &[Polyline], pattern: &[f64], phase: f64) -> Vec<Polyline> {
    let total: f64 = pattern.iter().sum();
    let length: f64 = lines.iter().map(polyline_length).sum();
    if length / total * pattern.len() as f64 > MAX_DASH_PIECES {
        return lines.to_vec();
    }
    let phase = if phase.is_finite() {
        phase.rem_euclid(total)
    } else {
        0.0
    };
    let mut out = Vec::new();
    for l in lines {
        dash_one(l, pattern, phase, &mut out);
    }
    out
}

fn polyline_length(l: &Polyline) -> f64 {
    let mut len: f64 = l.points.windows(2).map(|w| (w[1] - w[0]).length()).sum();
    if l.closed
        && let (Some(&first), Some(&last)) = (l.points.first(), l.points.last())
    {
        len += (first - last).length();
    }
    len
}

fn dash_one(l: &Polyline, pattern: &[f64], phase: f64, out: &mut Vec<Polyline>) {
    let n = pattern.len();
    let mut idx = 0;
    let mut remaining = pattern[0];
    let mut skip = phase;
    while skip > 0.0 {
        if skip >= remaining {
            skip -= remaining;
            idx = (idx + 1) % n;
            remaining = pattern[idx];
        } else {
            remaining -= skip;
            skip = 0.0;
        }
    }
    let on_at_start = idx % 2 == 0;
    let mut on = on_at_start;
    let first_out = out.len();
    let mut piece = Polyline::default();
    if on {
        piece.points.push(l.points[0]);
        piece.smooth.push(false);
    }
    let count = l.points.len();
    let segs = if l.closed {
        count
    } else {
        count.saturating_sub(1)
    };
    for i in 0..segs {
        let a = l.points[i];
        let j = (i + 1) % count;
        let b = l.points[j];
        let seg = b - a;
        let len = seg.length();
        let dir = if len > 0.0 {
            seg * (1.0 / len)
        } else {
            Point::new(1.0, 0.0)
        };
        let mut t = 0.0;
        while len - t > remaining {
            t += remaining;
            let q = a + seg * (t / len);
            if on {
                piece.points.push(q);
                piece.smooth.push(false);
                piece.zero_dir = dir;
                piece.has_segments = true;
                out.push(std::mem::take(&mut piece));
            } else {
                piece.points.push(q);
                piece.smooth.push(false);
                piece.zero_dir = dir;
            }
            on = !on;
            idx = (idx + 1) % n;
            remaining = pattern[idx];
        }
        remaining -= len - t;
        if on {
            piece.points.push(b);
            piece
                .smooth
                .push(l.smooth.get(j).copied().unwrap_or(false) && j != 0);
            piece.zero_dir = dir;
            piece.has_segments = true;
        }
    }
    if on && piece.has_segments {
        if l.closed && on_at_start && out.len() > first_out {
            // The dash running through the start point continues into the
            // first dash: join them instead of capping both.
            let first = std::mem::take(&mut out[first_out]);
            piece.points.extend_from_slice(&first.points[1..]);
            piece.smooth.extend_from_slice(&first.smooth[1..]);
            out[first_out] = piece;
        } else if l.closed && on_at_start {
            let mut whole = l.clone();
            whole.zero_dir = piece.zero_dir;
            out.push(whole);
        } else {
            out.push(piece);
        }
    }
}

struct Stroker<'a> {
    hw: f64,
    tol: f64,
    cap: LineCap,
    join: LineJoin,
    miter_limit: f64,
    transform: Transform,
    edges: &'a mut Edges,
    scratch: Vec<Point>,
    pts: Vec<Point>,
    smooth: Vec<bool>,
    /// Inner corner of the join at each vertex, when the quads are cut there.
    cuts: Vec<Option<Cut>>,
}

/// Where the inner offset lines of two joined segments meet, on the left
/// (`left`) or right side of the path.
#[derive(Clone, Copy)]
struct Cut {
    q: Point,
    left: bool,
}

/// The inner corner of the join at `cur`, when it lies within the near half
/// of both segments (so cuts from the two ends of a segment cannot cross).
/// Straight continuations need no cut and reversals have no inner corner.
fn inner_corner(prev: Point, cur: Point, next: Point, hw: f64) -> Option<Cut> {
    let (v0, v1) = (cur - prev, next - cur);
    let (d0, d1) = (unit(v0), unit(v1));
    let cross = d0.cross(d1);
    let k = 1.0 + d0.dot(d1);
    if cross == 0.0 || k <= 1e-9 {
        return None;
    }
    // Distance from the vertex back along each segment: hw * tan(θ / 2).
    let t = hw * cross.abs() / k;
    if t > 0.5 * v0.length().min(v1.length()) {
        return None;
    }
    let left_turn = cross > 0.0;
    let side = if left_turn { hw } else { -hw };
    Some(Cut {
        q: cur + left(d0) * side - d0 * t,
        left: left_turn,
    })
}

fn left(d: Point) -> Point {
    Point::new(-d.y, d.x)
}

fn rotate(v: Point, angle: f64) -> Point {
    let (s, c) = angle.sin_cos();
    Point::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

impl Stroker<'_> {
    fn polyline(&mut self, l: &Polyline, eps: f64) {
        self.pts.clear();
        self.smooth.clear();
        for (i, &p) in l.points.iter().enumerate() {
            if !p.is_finite() {
                continue;
            }
            let smooth = l.smooth.get(i).copied().unwrap_or(false);
            match self.pts.last() {
                Some(&last) if (p - last).length() <= eps => {
                    if let Some(s) = self.smooth.last_mut() {
                        *s = *s && smooth;
                    }
                }
                _ => {
                    self.pts.push(p);
                    self.smooth.push(smooth);
                }
            }
        }
        let closed = l.closed;
        if closed && self.pts.len() > 1 {
            let first = self.pts[0];
            if let Some(&last) = self.pts.last()
                && (first - last).length() <= eps
            {
                self.pts.pop();
                self.smooth.pop();
            }
        }
        match self.pts.len() {
            0 => {}
            1 => self.dot(self.pts[0], l.zero_dir),
            _ => self.segments(closed),
        }
    }

    fn segments(&mut self, closed: bool) {
        let count = self.pts.len();
        let nseg = if closed { count } else { count - 1 };
        self.cuts.clear();
        for j in 0..count {
            let cut = if closed || (j > 0 && j + 1 < count) {
                let prev = self.pts[(j + count - 1) % count];
                let next = self.pts[(j + 1) % count];
                inner_corner(prev, self.pts[j], next, self.hw)
            } else {
                None
            };
            self.cuts.push(cut);
        }
        let mut first_dir = Point::default();
        let mut last_dir = Point::default();
        let mut piece = std::mem::take(&mut self.scratch);
        for i in 0..nseg {
            let j = (i + 1) % count;
            let a = self.pts[i];
            let b = self.pts[j];
            let d = unit(b - a);
            if i == 0 {
                first_dir = d;
            }
            last_dir = d;
            let n = left(d) * self.hw;
            // Left side from a to b, then right side back from b to a.
            piece.clear();
            match self.cuts[i] {
                Some(Cut { q, left: true }) => piece.extend([a, q]),
                _ => piece.push(a + n),
            }
            match self.cuts[j] {
                Some(Cut { q, left: true }) => piece.extend([q, b, b - n]),
                Some(Cut { q, left: false }) => piece.extend([b + n, b, q]),
                None => piece.extend([b + n, b - n]),
            }
            match self.cuts[i] {
                Some(Cut { q, left: false }) => piece.extend([q, a]),
                _ => piece.push(a - n),
            }
            self.emit(&piece);
        }
        self.scratch = piece;
        let joins = if closed { 0..count } else { 1..count - 1 };
        for j in joins {
            let prev = self.pts[(j + count - 1) % count];
            let cur = self.pts[j];
            let next = self.pts[(j + 1) % count];
            let smooth = self.smooth[j];
            self.join_at(cur, unit(cur - prev), unit(next - cur), smooth);
        }
        if !closed {
            let start = self.pts[0];
            let end = self.pts[count - 1];
            self.cap_at(start, -first_dir);
            self.cap_at(end, last_dir);
        }
    }

    fn join_at(&mut self, p: Point, d0: Point, d1: Point, smooth: bool) {
        let cross = d0.cross(d1);
        let dot = d0.dot(d1);
        if cross.abs() <= 1e-12 && dot > 0.0 {
            return;
        }
        // The outer side is right of the path for a left turn.
        let side = if cross > 0.0 { -1.0 } else { 1.0 };
        let n0 = left(d0) * (self.hw * side);
        let n1 = left(d1) * (self.hw * side);
        let join = if smooth { LineJoin::Round } else { self.join };
        match join {
            LineJoin::Bevel => self.emit(&[p, p + n0, p + n1]),
            LineJoin::Miter => {
                let k = 1.0 + dot;
                if k > 1e-12 && k * self.miter_limit * self.miter_limit >= 2.0 {
                    let tip = p + (n0 + n1) * (1.0 / k);
                    self.emit(&[p, p + n0, tip, p + n1]);
                } else {
                    self.emit(&[p, p + n0, p + n1]);
                }
            }
            LineJoin::Round => {
                let angle = n0.cross(n1).atan2(n0.dot(n1)).abs() * -side;
                self.scratch.clear();
                self.scratch.push(p);
                self.scratch.push(p + n0);
                self.push_arc(p, n0, angle);
                let pts = std::mem::take(&mut self.scratch);
                self.emit(&pts);
                self.scratch = pts;
            }
        }
    }

    /// Cap at `p` bulging towards the outward direction `d`.
    fn cap_at(&mut self, p: Point, d: Point) {
        let n = left(d) * self.hw;
        match self.cap {
            LineCap::Butt => {}
            LineCap::Square => {
                let e = d * self.hw;
                self.emit(&[p + n, p + n + e, p - n + e, p - n]);
            }
            LineCap::Round => {
                self.scratch.clear();
                self.scratch.push(p + n);
                self.push_arc(p, n, -PI);
                let pts = std::mem::take(&mut self.scratch);
                self.emit(&pts);
                self.scratch = pts;
            }
        }
    }

    /// Zero-length subpath: a round or square cap on both sides.
    fn dot(&mut self, p: Point, dir: Point) {
        let d = if dir.length() > 0.0 {
            unit(dir)
        } else {
            Point::new(1.0, 0.0)
        };
        match self.cap {
            LineCap::Butt => {}
            LineCap::Square => {
                let e = d * self.hw;
                let n = left(d) * self.hw;
                self.emit(&[p + n + e, p - n + e, p - n - e, p + n - e]);
            }
            LineCap::Round => {
                let v = Point::new(self.hw, 0.0);
                self.scratch.clear();
                self.scratch.push(p + v);
                self.push_arc(p, v, -2.0 * PI);
                self.scratch.pop();
                let pts = std::mem::take(&mut self.scratch);
                self.emit(&pts);
                self.scratch = pts;
            }
        }
    }

    /// Pushes points of the arc that rotates `v` around `c` by `angle`,
    /// excluding the start point.
    fn push_arc(&mut self, c: Point, v: Point, angle: f64) {
        let r = self.hw;
        let step = if self.tol < r {
            2.0 * (1.0 - self.tol / r).acos()
        } else {
            PI / 2.0
        };
        let steps = if step > 0.0 {
            ((angle.abs() / step).ceil() as usize).clamp(1, MAX_ARC_STEPS)
        } else {
            MAX_ARC_STEPS
        };
        for k in 1..=steps {
            self.scratch
                .push(c + rotate(v, angle * k as f64 / steps as f64));
        }
    }

    /// Adds one piece with negative orientation, mapped to device space.
    fn emit(&mut self, pts: &[Point]) {
        let mut area = 0.0;
        for (i, &a) in pts.iter().enumerate() {
            let b = pts[(i + 1) % pts.len()];
            area += a.cross(b);
        }
        self.edges.add_polygon(pts, &self.transform, area > 0.0);
    }
}

fn unit(v: Point) -> Point {
    let len = v.length();
    if len > 0.0 {
        v * (1.0 / len)
    } else {
        Point::new(1.0, 0.0)
    }
}
