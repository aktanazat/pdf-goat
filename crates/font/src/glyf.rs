//! TrueType `glyf` outlines: simple glyphs and composites with transforms and point matching.

use crate::error::{FontError, Result};
use crate::outline::{Outline, OutlineBuilder, Rect};
use crate::reader::{Reader, read_i16, read_u16, read_u32};
use crate::sfnt::hmtx_lsb;

const ON_CURVE: u8 = 0x01;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const REPEAT: u8 = 0x08;
const X_SAME_OR_POSITIVE: u8 = 0x10;
const Y_SAME_OR_POSITIVE: u8 = 0x20;

const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const WE_HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
const USE_MY_METRICS: u16 = 0x0200;
const SCALED_COMPONENT_OFFSET: u16 = 0x0800;
const UNSCALED_COMPONENT_OFFSET: u16 = 0x1000;

/// Deepest composite nesting followed.
const MAX_DEPTH: u32 = 16;
/// Most points one glyph may expand to, composites included.
const MAX_POINTS: usize = 1 << 20;
/// Most component references followed for one glyph.
const MAX_COMPONENTS: u32 = 16_384;

#[cfg(test)]
std::thread_local! {
    static OUTLINE_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Byte ranges of the `loca` and `glyf` tables within the font data.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlyfTables {
    pub(crate) loca: (usize, usize),
    pub(crate) glyf: (usize, usize),
    pub(crate) long_offsets: bool,
    pub(crate) num_glyphs: u16,
}

impl GlyfTables {
    /// The raw glyph record of `gid`, empty for glyphs without outline. Offsets past the table
    /// end or out of order are treated as an empty glyph.
    pub(crate) fn glyph_data<'a>(&self, data: &'a [u8], gid: u16) -> Result<&'a [u8]> {
        if gid >= self.num_glyphs {
            return Err(FontError::Missing("glyph id out of range"));
        }
        let loca = data
            .get(self.loca.0..self.loca.0 + self.loca.1)
            .ok_or(FontError::Truncated("loca table"))?;
        let glyf = data
            .get(self.glyf.0..self.glyf.0 + self.glyf.1)
            .ok_or(FontError::Truncated("glyf table"))?;
        let i = usize::from(gid);
        let (start, end) = if self.long_offsets {
            (
                read_u32(loca, 4 * i).map(|v| v as usize),
                read_u32(loca, 4 * i + 4).map(|v| v as usize),
            )
        } else {
            (
                read_u16(loca, 2 * i).map(|v| 2 * usize::from(v)),
                read_u16(loca, 2 * i + 2).map(|v| 2 * usize::from(v)),
            )
        };
        let (Some(start), Some(end)) = (start, end) else {
            return Err(FontError::Truncated("loca table"));
        };
        if start >= end || start >= glyf.len() {
            return Ok(&[]);
        }
        Ok(&glyf[start..end.min(glyf.len())])
    }

    pub(crate) fn outline(&self, data: &[u8], gid: u16) -> Result<Outline> {
        #[cfg(test)]
        OUTLINE_LOADS.with(|loads| loads.set(loads.get() + 1));
        let mut state = LoadState {
            points: 0,
            components: 0,
        };
        let contours = self.load(data, gid, 0, &mut state)?;
        contours.to_outline()
    }

    pub(crate) fn bounds(&self, data: &[u8], gid: u16) -> Result<Option<Rect>> {
        let glyph = self.glyph_data(data, gid)?;
        if glyph.is_empty() {
            return Ok(None);
        }
        let mut r = Reader::new(glyph, "glyf glyph bounds");
        let num_contours = r.i16()?;
        let bounds = Rect {
            x_min: f32::from(r.i16()?),
            y_min: f32::from(r.i16()?),
            x_max: f32::from(r.i16()?),
            y_max: f32::from(r.i16()?),
        };
        if num_contours == 0 {
            return Ok(None);
        }
        if num_contours < 0 {
            return Ok(self.outline(data, gid)?.bounds());
        }
        if bounds.x_min > bounds.x_max || bounds.y_min > bounds.y_max {
            return Err(FontError::Malformed("glyf bounds out of order"));
        }
        let mut num_points = 0usize;
        let mut has_segments = false;
        for _ in 0..num_contours {
            let end = usize::from(r.u16()?) + 1;
            if end < num_points {
                return Err(FontError::Malformed("glyf contour end points out of order"));
            }
            has_segments |= end > num_points + 1;
            num_points = end;
        }
        let instruction_len = usize::from(r.u16()?);
        r.skip(instruction_len)?;
        let mut coordinate_bytes = 0usize;
        let mut points = 0usize;
        while points < num_points {
            let flags = r.u8()?;
            let run = if flags & REPEAT != 0 {
                usize::from(r.u8()?) + 1
            } else {
                1
            };
            let run = run.min(num_points - points);
            // An off-curve singleton draws a degenerate curve; an on-curve singleton is only a hint point.
            has_segments |= flags & ON_CURVE == 0;
            for (short, same) in [(X_SHORT, X_SAME_OR_POSITIVE), (Y_SHORT, Y_SAME_OR_POSITIVE)] {
                coordinate_bytes += run
                    * if flags & short != 0 {
                        1
                    } else if flags & same == 0 {
                        2
                    } else {
                        0
                    };
            }
            points += run;
        }
        r.skip(coordinate_bytes)?;
        Ok(has_segments.then_some(bounds))
    }

    fn load(&self, data: &[u8], gid: u16, depth: u32, state: &mut LoadState) -> Result<Contours> {
        if depth > MAX_DEPTH {
            return Err(FontError::LimitExceeded("composite glyph depth"));
        }
        let glyph = self.glyph_data(data, gid)?;
        if glyph.is_empty() {
            return Ok(Contours::default());
        }
        let mut r = Reader::new(glyph, "glyf glyph header");
        let num_contours = r.i16()?;
        r.skip(8)?;
        if num_contours >= 0 {
            let contours = parse_simple(&mut r, num_contours as usize)?;
            state.points += contours.points.len();
            if state.points > MAX_POINTS {
                return Err(FontError::LimitExceeded("glyph point count"));
            }
            return Ok(contours);
        }
        let mut out = Contours::default();
        loop {
            state.components += 1;
            if state.components > MAX_COMPONENTS {
                return Err(FontError::LimitExceeded("composite glyph components"));
            }
            let flags = r.u16()?;
            let component = r.u16()?;
            let (arg1, arg2) = if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                if flags & ARGS_ARE_XY_VALUES != 0 {
                    (i32::from(r.i16()?), i32::from(r.i16()?))
                } else {
                    (i32::from(r.u16()?), i32::from(r.u16()?))
                }
            } else if flags & ARGS_ARE_XY_VALUES != 0 {
                (i32::from(r.i8()?), i32::from(r.i8()?))
            } else {
                (i32::from(r.u8()?), i32::from(r.u8()?))
            };
            // [xx, xy, yx, yy]: x' = xx*x + yx*y, y' = xy*x + yy*y.
            let mut m = [1.0f32, 0.0, 0.0, 1.0];
            if flags & WE_HAVE_A_SCALE != 0 {
                let s = f2dot14(r.i16()?);
                m = [s, 0.0, 0.0, s];
            } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                m = [f2dot14(r.i16()?), 0.0, 0.0, f2dot14(r.i16()?)];
            } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
                m = [
                    f2dot14(r.i16()?),
                    f2dot14(r.i16()?),
                    f2dot14(r.i16()?),
                    f2dot14(r.i16()?),
                ];
            }
            let mut child = self.load(data, component, depth + 1, state)?;
            for p in &mut child.points {
                let (x, y) = (p.0, p.1);
                p.0 = m[0] * x + m[2] * y;
                p.1 = m[1] * x + m[3] * y;
            }
            let (dx, dy) = if flags & ARGS_ARE_XY_VALUES != 0 {
                let (x, y) = (arg1 as f32, arg2 as f32);
                if flags & SCALED_COMPONENT_OFFSET != 0 && flags & UNSCALED_COMPONENT_OFFSET == 0 {
                    (m[0] * x + m[2] * y, m[1] * x + m[3] * y)
                } else {
                    (x, y)
                }
            } else {
                // Point matching: align child point arg2 with the already placed point arg1.
                let parent = out
                    .points
                    .get(arg1 as usize)
                    .ok_or(FontError::Malformed("composite anchor point"))?;
                let own = child
                    .points
                    .get(arg2 as usize)
                    .ok_or(FontError::Malformed("composite anchor point"))?;
                (parent.0 - own.0, parent.1 - own.1)
            };
            let base = out.points.len();
            out.points
                .extend(child.points.iter().map(|&(x, y, on)| (x + dx, y + dy, on)));
            out.ends.extend(child.ends.iter().map(|e| e + base));
            if flags & MORE_COMPONENTS == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// FreeType's origin for `gid` (its first phantom point) in glyph coordinates: the header
    /// xMin minus the hmtx left side bearing, or the origin of the last component flagged
    /// USE_MY_METRICS. Outlines are drawn shifted by minus this value.
    pub(crate) fn phantom_origin(
        &self,
        data: &[u8],
        hmtx: &[u8],
        num_hmetrics: u16,
        gid: u16,
    ) -> Option<i32> {
        let mut gid = gid;
        let mut origin = None;
        for _ in 0..=MAX_DEPTH {
            let glyph = self.glyph_data(data, gid).ok()?;
            let (Some(x_min), Some(lsb)) = (read_i16(glyph, 2), hmtx_lsb(hmtx, num_hmetrics, gid))
            else {
                return origin;
            };
            origin = Some(i32::from(x_min) - i32::from(lsb));
            match composite_refs(glyph)
                .ok()?
                .iter()
                .rev()
                .find(|c| c.flags & USE_MY_METRICS != 0)
            {
                Some(c) => gid = c.gid,
                None => break,
            }
        }
        origin
    }
}

/// One component reference of a composite glyph record.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ComponentRef {
    /// Byte offset of the component's glyph id within the record.
    pub(crate) gid_offset: usize,
    pub(crate) gid: u16,
    pub(crate) flags: u16,
}

/// Component references of a composite glyph record; empty for simple and empty glyphs.
pub(crate) fn composite_refs(glyph: &[u8]) -> Result<Vec<ComponentRef>> {
    if glyph.is_empty() || read_i16(glyph, 0).ok_or(FontError::Truncated("glyf glyph header"))? >= 0
    {
        return Ok(Vec::new());
    }
    let mut refs = Vec::new();
    let mut pos = 10usize;
    loop {
        if refs.len() >= MAX_COMPONENTS as usize {
            return Err(FontError::LimitExceeded("composite glyph components"));
        }
        let flags = read_u16(glyph, pos).ok_or(FontError::Truncated("composite glyph"))?;
        let gid = read_u16(glyph, pos + 2).ok_or(FontError::Truncated("composite glyph"))?;
        refs.push(ComponentRef {
            gid_offset: pos + 2,
            gid,
            flags,
        });
        pos += 4 + if flags & ARG_1_AND_2_ARE_WORDS != 0 {
            4
        } else {
            2
        };
        pos += if flags & WE_HAVE_A_SCALE != 0 {
            2
        } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
            4
        } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
            8
        } else {
            0
        };
        if flags & MORE_COMPONENTS == 0 {
            break;
        }
    }
    if pos > glyph.len() {
        return Err(FontError::Truncated("composite glyph"));
    }
    Ok(refs)
}

struct LoadState {
    points: usize,
    components: u32,
}

fn f2dot14(v: i16) -> f32 {
    f32::from(v) / 16384.0
}

/// Points of a glyph with each contour's last point index.
#[derive(Default)]
struct Contours {
    points: Vec<(f32, f32, bool)>,
    ends: Vec<usize>,
}

fn parse_simple(r: &mut Reader<'_>, num_contours: usize) -> Result<Contours> {
    let mut ends = Vec::with_capacity(num_contours);
    let mut prev: Option<usize> = None;
    for _ in 0..num_contours {
        let end = usize::from(r.u16()?);
        if prev.is_some_and(|p| end < p) {
            return Err(FontError::Malformed("glyf contour end points out of order"));
        }
        prev = Some(end);
        ends.push(end);
    }
    let Some(last) = prev else {
        return Ok(Contours::default());
    };
    let num_points = last + 1;
    let instruction_len = usize::from(r.u16()?);
    r.skip(instruction_len)?;
    // Each point needs at least one flag byte; reject counts the data cannot hold.
    if num_points > r.remaining() {
        return Err(FontError::Truncated("glyf point data"));
    }
    let mut flags = Vec::with_capacity(num_points);
    while flags.len() < num_points {
        let f = r.u8()?;
        flags.push(f);
        if f & REPEAT != 0 {
            let n = usize::from(r.u8()?);
            for _ in 0..n.min(num_points - flags.len()) {
                flags.push(f);
            }
        }
    }
    let mut xs = Vec::with_capacity(num_points);
    let mut v = 0i32;
    for &f in &flags {
        if f & X_SHORT != 0 {
            let d = i32::from(r.u8()?);
            v += if f & X_SAME_OR_POSITIVE != 0 { d } else { -d };
        } else if f & X_SAME_OR_POSITIVE == 0 {
            v += i32::from(r.i16()?);
        }
        xs.push(v);
    }
    let mut points = Vec::with_capacity(num_points);
    v = 0;
    for (i, &f) in flags.iter().enumerate() {
        if f & Y_SHORT != 0 {
            let d = i32::from(r.u8()?);
            v += if f & Y_SAME_OR_POSITIVE != 0 { d } else { -d };
        } else if f & Y_SAME_OR_POSITIVE == 0 {
            v += i32::from(r.i16()?);
        }
        points.push((xs[i] as f32, v as f32, f & ON_CURVE != 0));
    }
    Ok(Contours { points, ends })
}

impl Contours {
    fn to_outline(&self) -> Result<Outline> {
        let mut b = OutlineBuilder::new();
        let mut start = 0usize;
        for &end in &self.ends {
            if end < start || end >= self.points.len() {
                start = end.saturating_add(1);
                continue;
            }
            emit_contour(&mut b, &self.points[start..=end])?;
            start = end + 1;
        }
        b.finish()
    }
}

fn emit_contour(b: &mut OutlineBuilder, pts: &[(f32, f32, bool)]) -> Result<()> {
    let n = pts.len();
    if n == 0 {
        return Ok(());
    }
    let mid = |a: (f32, f32, bool), c: (f32, f32, bool)| ((a.0 + c.0) / 2.0, (a.1 + c.1) / 2.0);
    // Start at an on-curve point; with none, at the midpoint of the last and first points.
    // The closing segment back to the start is left to Close.
    let (start, first, count) = match pts.iter().position(|p| p.2) {
        Some(i) => ((pts[i].0, pts[i].1), i + 1, n - 1),
        None => (mid(pts[n - 1], pts[0]), 0, n),
    };
    b.move_to(start.0, start.1)?;
    let mut pending: Option<(f32, f32)> = None;
    for k in 0..count {
        let p = pts[(first + k) % n];
        if p.2 {
            match pending.take() {
                Some(c) => b.quad_to(c.0, c.1, p.0, p.1)?,
                None => b.line_to(p.0, p.1)?,
            }
        } else {
            if let Some(c) = pending {
                let m = ((c.0 + p.0) / 2.0, (c.1 + p.1) / 2.0);
                b.quad_to(c.0, c.1, m.0, m.1)?;
            }
            pending = Some((p.0, p.1));
        }
    }
    if let Some(c) = pending {
        b.quad_to(c.0, c.1, start.0, start.1)?;
    }
    b.close()
}

#[cfg(test)]
mod tests {
    use super::{GlyfTables, OUTLINE_LOADS};
    use crate::outline::Rect;

    #[test]
    fn simple_bounds_do_not_load_outlines() {
        let mut data = vec![0, 0, 0, 0, 0, 0, 0, 22];
        data.extend_from_slice(&[
            0, 1, 0, 0, 0, 0, 0, 100, 0, 100, 0, 3, 0, 0, 0x31, 0x33, 0x35, 0x23, 100, 100, 100, 0,
        ]);
        let tables = GlyfTables {
            loca: (0, 8),
            glyf: (8, 22),
            long_offsets: true,
            num_glyphs: 1,
        };
        OUTLINE_LOADS.with(|loads| loads.set(0));
        assert_eq!(
            tables.bounds(&data, 0).expect("valid glyph"),
            Some(Rect {
                x_min: 0.0,
                y_min: 0.0,
                x_max: 100.0,
                y_max: 100.0
            }),
        );
        assert_eq!(
            OUTLINE_LOADS.with(|loads| loads.get()),
            0,
            "bounds must not build an outline"
        );
    }
}
