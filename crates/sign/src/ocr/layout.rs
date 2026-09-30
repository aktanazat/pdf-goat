//! Where each recognized word's invisible text goes.
//!
//! Vision's word boxes span the whole line quad, which is taller than the
//! type, and their ends drift by up to a character. Each line is measured
//! again on the bitmap Vision read, in the line's own frame so that skewed
//! lines work: the baseline is where most ink columns end, the type size
//! follows from how high the ink rises above it, and every boundary between
//! two words moves into the widest blank run of columns near Vision's gap.
//! A word box then runs from 0.9 em above the baseline to 0.215 em below
//! it, the box a PDF reader gives the same word set in a common text face
//! such as Times or Arial.

use pdf_ocr::{Line, Point, Quad};

/// Gray samples below this are ink.
const INK: u8 = 128;
/// A word box's extent above and below the baseline, in em.
const ABOVE: f64 = 0.9;
const BELOW: f64 = 0.215;
/// Ascender and x-height, in em, of the faces documents are set in.
const TALL: f64 = 0.70;
const SHORT: f64 = 0.48;
/// Vision's line quad height, in em.
const QUAD_EM: f64 = 1.15;
/// A row this full of ink is a rule, not text.
const RULE_ROW: f64 = 0.85;
/// A column this full of ink across the text band is a rule.
const RULE_COLUMN: f64 = 0.95;
/// Rows holding this share of the fullest row's ink form the line's core.
const CORE: f64 = 0.35;
/// The band word gaps are looked for in, above and below the baseline, in em.
const BAND_ABOVE: f64 = 0.8;
const BAND_BELOW: f64 = 0.25;
/// How far a word edge may move from Vision's, in em.
const SNAP: f64 = 0.35;
/// Characters that reach the ascender, besides capitals and digits.
const TALL_CHARS: &str = "bdfhklß()[]{}/\\|!?%&@#$£€¥";

/// The 8-bit gray bitmap Vision read, one byte per pixel, rows packed.
pub(super) struct Ink<'a> {
    pub width: usize,
    pub height: usize,
    pub gray: &'a [u8],
}

/// A word's box in bitmap pixels: `origin` is its top-left corner in the
/// text's own orientation, `width` runs along `along` and `height` along
/// `down`, both unit vectors.
#[derive(Clone, Copy, Debug)]
pub(super) struct WordBox {
    pub origin: Point,
    pub along: [f64; 2],
    pub down: [f64; 2],
    pub width: f64,
    pub height: f64,
}

/// One box per word of `line`, `None` for a word Vision gave no usable box.
pub(super) fn place_words(ink: &Ink<'_>, line: &Line) -> Vec<Option<WordBox>> {
    let Some(frame) = Frame::of(&line.quad) else {
        return line
            .words
            .iter()
            .map(|word| word.quad.and_then(|quad| upright(&quad)))
            .collect();
    };
    let spans: Vec<(f64, f64)> = line
        .words
        .iter()
        .filter_map(|word| word.quad.map(|quad| frame.extent(&quad, frame.along)))
        .collect();
    let measured = measure(ink, &frame, &line.text);
    let placed = match &measured {
        Some(measure) => snap(&measure.columns, measure.em, &spans),
        None => spans,
    };
    let mut placed = placed.into_iter();
    line.words
        .iter()
        .map(|word| {
            let quad = word.quad?;
            let span = placed.next()?;
            let across = measured.as_ref().map_or_else(
                || frame.extent(&quad, frame.down),
                |measure| {
                    (
                        measure.base - ABOVE * measure.em,
                        measure.base + BELOW * measure.em,
                    )
                },
            );
            frame.word(span, across)
        })
        .collect()
}

/// A quad's axis-aligned bounds, for a line whose own frame is degenerate.
fn upright(quad: &Quad) -> Option<WordBox> {
    let bounds = quad.bounds();
    (bounds.width > 0.0 && bounds.height > 0.0).then_some(WordBox {
        origin: Point {
            x: bounds.x,
            y: bounds.y,
        },
        along: [1.0, 0.0],
        down: [0.0, 1.0],
        width: bounds.width,
        height: bounds.height,
    })
}

/// A line quad's own frame: `along` follows the text, `down` points from
/// its top to its bottom, and distances are pixels from the top-left corner.
struct Frame {
    origin: Point,
    along: [f64; 2],
    down: [f64; 2],
    length: f64,
    height: f64,
}

fn project(axis: [f64; 2], from: Point, to: Point) -> f64 {
    (to.x - from.x) * axis[0] + (to.y - from.y) * axis[1]
}

impl Frame {
    fn of(quad: &Quad) -> Option<Frame> {
        let Quad {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        } = *quad;
        let x = (top_right.x - top_left.x) + (bottom_right.x - bottom_left.x);
        let y = (top_right.y - top_left.y) + (bottom_right.y - bottom_left.y);
        let norm = x.hypot(y);
        if !(norm.is_finite() && norm > 0.0) {
            return None;
        }
        let along = [x / norm, y / norm];
        let down = [-along[1], along[0]];
        let length =
            project(along, top_left, top_right).max(project(along, bottom_left, bottom_right));
        let height =
            project(down, top_left, bottom_left).max(project(down, top_right, bottom_right));
        (length.is_finite() && height.is_finite() && length > 0.0 && height > 0.0).then_some(
            Frame {
                origin: top_left,
                along,
                down,
                length,
                height,
            },
        )
    }

    /// The range a quad's corners project to on `axis`.
    fn extent(&self, quad: &Quad, axis: [f64; 2]) -> (f64, f64) {
        [
            quad.top_left,
            quad.top_right,
            quad.bottom_right,
            quad.bottom_left,
        ]
        .into_iter()
        .map(|corner| project(axis, self.origin, corner))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), t| {
            (lo.min(t), hi.max(t))
        })
    }

    fn word(&self, (start, end): (f64, f64), (top, bottom): (f64, f64)) -> Option<WordBox> {
        let (width, height) = (end - start, bottom - top);
        (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0).then_some(
            WordBox {
                origin: Point {
                    x: self.origin.x + self.along[0] * start + self.down[0] * top,
                    y: self.origin.y + self.along[1] * start + self.down[1] * top,
                },
                along: self.along,
                down: self.down,
                width,
                height,
            },
        )
    }
}

/// The frame's cells: `rows` across the text and `columns` along it, one
/// pixel each, over the bitmap pixels whose centres fall inside the quad.
struct Grid<'f> {
    frame: &'f Frame,
    rows: usize,
    columns: usize,
    x: (usize, usize),
    y: (usize, usize),
}

impl<'f> Grid<'f> {
    fn new(ink: &Ink<'_>, frame: &'f Frame) -> Option<Grid<'f>> {
        // A line never outgrows the bitmap it was read from; a quad that
        // does is not measured, which also bounds the vectors below.
        let limit = (ink.width + ink.height) as f64;
        if frame.length > limit || frame.height > limit {
            return None;
        }
        let corners = [
            frame.origin,
            frame.at(frame.length, 0.0),
            frame.at(0.0, frame.height),
            frame.at(frame.length, frame.height),
        ];
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for corner in corners {
            x0 = x0.min(corner.x);
            y0 = y0.min(corner.y);
            x1 = x1.max(corner.x);
            y1 = y1.max(corner.y);
        }
        let clamp = |value: f64, size: usize| value.clamp(0.0, size as f64) as usize;
        let x = (clamp(x0.floor(), ink.width), clamp(x1.ceil(), ink.width));
        let y = (clamp(y0.floor(), ink.height), clamp(y1.ceil(), ink.height));
        (x.0 < x.1 && y.0 < y.1).then_some(Grid {
            frame,
            rows: frame.height.ceil() as usize,
            columns: frame.length.ceil() as usize,
            x,
            y,
        })
    }

    /// Calls `visit(row, column, across, inked)` for every bitmap pixel whose
    /// centre lies inside the quad; `across` is its distance below the
    /// quad's top edge.
    fn each(&self, ink: &Ink<'_>, mut visit: impl FnMut(usize, usize, f64, bool)) {
        let Frame {
            origin,
            along,
            down,
            length,
            height,
            ..
        } = *self.frame;
        for y in self.y.0..self.y.1 {
            let Some(samples) = ink
                .gray
                .get(y * ink.width + self.x.0..y * ink.width + self.x.1)
            else {
                return;
            };
            let dy = y as f64 + 0.5 - origin.y;
            for (x, &sample) in (self.x.0..).zip(samples) {
                let dx = x as f64 + 0.5 - origin.x;
                let s = dx * along[0] + dy * along[1];
                let t = dx * down[0] + dy * down[1];
                if !(0.0..length).contains(&s) || !(0.0..height).contains(&t) {
                    continue;
                }
                let row = (t as usize).min(self.rows - 1);
                let column = (s as usize).min(self.columns - 1);
                visit(row, column, t, sample < INK);
            }
        }
    }
}

impl Frame {
    fn at(&self, s: f64, t: f64) -> Point {
        Point {
            x: self.origin.x + self.along[0] * s + self.down[0] * t,
            y: self.origin.y + self.along[1] * s + self.down[1] * t,
        }
    }
}

/// What the ink says about one line: its baseline and em, in pixels from
/// the quad's top edge, and which columns hold text ink near the baseline.
struct Measure {
    base: f64,
    em: f64,
    columns: Vec<bool>,
}

fn measure(ink: &Ink<'_>, frame: &Frame, text: &str) -> Option<Measure> {
    let grid = Grid::new(ink, frame)?;

    // Ink per row; rows almost fully inked are rules and hold no text.
    let mut profile = vec![0.0f64; grid.rows];
    let mut span = vec![0.0f64; grid.rows];
    grid.each(ink, |row, _, _, inked| {
        span[row] += 1.0;
        if inked {
            profile[row] += 1.0;
        }
    });
    let rule: Vec<bool> = profile
        .iter()
        .zip(&span)
        .map(|(&inked, &total)| total > 0.0 && inked / total >= RULE_ROW)
        .collect();
    for (value, &is_rule) in profile.iter_mut().zip(&rule) {
        if is_rule {
            *value = 0.0;
        }
    }
    let fullest = profile.iter().copied().fold(0.0, f64::max);
    if fullest <= 0.0 {
        return None;
    }

    // The line's own ink: the longest run of well-inked rows, grown while
    // rows still hold ink. Ascenders or descenders of neighbouring lines
    // that reach into the quad lie beyond an empty row and drop out.
    let (mut core, mut run) = ((0, 0), None);
    for row in 0..=grid.rows {
        let strong = profile.get(row).is_some_and(|&ink| ink >= CORE * fullest);
        match (strong, run) {
            (true, None) => run = Some(row),
            (false, Some(start)) => {
                if row - start > core.1 - core.0 {
                    core = (start, row);
                }
                run = None;
            }
            _ => {}
        }
    }
    let (mut upper, mut lower) = core;
    while upper > 0 && profile[upper - 1] > 0.0 {
        upper -= 1;
    }
    while lower < grid.rows && profile[lower] > 0.0 {
        lower += 1;
    }

    // Each column's highest and lowest ink inside that window.
    let mut tops = vec![usize::MAX; grid.columns];
    let mut bottoms = vec![0usize; grid.columns];
    grid.each(ink, |row, column, _, inked| {
        if inked && (upper..lower).contains(&row) && !rule[row] {
            tops[column] = tops[column].min(row);
            bottoms[column] = bottoms[column].max(row + 1);
        }
    });

    // The baseline: where most columns end, averaged over the rows beside
    // the most common end.
    let mut ends = vec![0.0f64; grid.rows + 2];
    for &bottom in bottoms.iter().filter(|&&bottom| bottom > 0) {
        ends[bottom] += 1.0;
    }
    let smooth = |row: usize| {
        ends[row]
            + row.checked_sub(1).map_or(0.0, |before| ends[before])
            + ends.get(row + 1).copied().unwrap_or(0.0)
    };
    let mut peak = 0;
    for row in 1..ends.len() {
        if smooth(row) > smooth(peak) {
            peak = row;
        }
    }
    let near = peak.saturating_sub(1)..(peak + 2).min(ends.len());
    let count: f64 = ends[near.clone()].iter().sum();
    if count <= 0.0 {
        return None;
    }
    let base = near.map(|row| ends[row] * row as f64).sum::<f64>() / count;

    // The em: capitals, digits and ascenders reach TALL em above the
    // baseline; a line without them rises only to the x-height.
    let mut body: Vec<f64> = tops
        .iter()
        .zip(&bottoms)
        .filter(|&(_, &bottom)| bottom > 0)
        .map(|(&top, _)| top as f64)
        .filter(|&top| top < base)
        .collect();
    body.sort_by(f64::total_cmp);
    let quad_em = frame.height / QUAD_EM;
    let tall = text
        .chars()
        .any(|c| c.is_uppercase() || c.is_numeric() || TALL_CHARS.contains(c));
    let em = match (body.is_empty(), tall) {
        (true, _) => quad_em,
        (false, true) => (base - percentile(&body, 3.0)) / TALL,
        (false, false) => (base - percentile(&body, 50.0)) / SHORT,
    };
    let em = if (0.5 * quad_em..=2.0 * quad_em).contains(&em) {
        em
    } else {
        quad_em
    };

    // Columns with ink in the text band, less vertical rules.
    let band = base - BAND_ABOVE * em..base + BAND_BELOW * em;
    let mut inked = vec![0u32; grid.columns];
    let mut spanned = vec![0u32; grid.columns];
    grid.each(ink, |_, column, across, is_ink| {
        if band.contains(&across) {
            spanned[column] += 1;
            if is_ink {
                inked[column] += 1;
            }
        }
    });
    let columns = inked
        .iter()
        .zip(&spanned)
        .map(|(&ink, &total)| ink > 0 && f64::from(ink) < RULE_COLUMN * f64::from(total))
        .collect();
    Some(Measure { base, em, columns })
}

/// The `q`th percentile of sorted, non-empty `values`, interpolating
/// linearly between neighbours.
fn percentile(values: &[f64], q: f64) -> f64 {
    let position = q / 100.0 * (values.len() - 1) as f64;
    let below = position.floor() as usize;
    let above = (below + 1).min(values.len() - 1);
    values[below] + (values[above] - values[below]) * (position - below as f64)
}

/// Moves each boundary between neighbouring words into the widest blank run
/// of columns within [`SNAP`] em of Vision's gap (on a tie, the run nearest
/// the gap's middle), pulls the line's outer edges onto its first and last
/// ink, and trims every word to its own ink. A word left without ink keeps
/// Vision's span.
fn snap(columns: &[bool], em: f64, spans: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let tolerance = SNAP * em;
    let mut out = spans.to_vec();
    for (i, pair) in spans.windows(2).enumerate() {
        let (a, b) = (pair[0].1 - tolerance, pair[1].0 + tolerance);
        let low = (a.min(b).floor() as i64).max(0);
        let high = (a.max(b).ceil() as i64).min(columns.len() as i64);
        if low > high {
            continue;
        }
        let (low, high) = (low as usize, high as usize);
        let middle = (pair[0].1 + pair[1].0) / 2.0;
        let mut best: Option<((usize, f64), usize, usize)> = None;
        let mut start = None;
        for x in low..=high {
            let blank = columns.get(x).is_some_and(|&inked| !inked) && x < high;
            match (blank, start) {
                (true, None) => start = Some(x),
                (false, Some(first)) => {
                    let key = (x - first, -(((first + x) as f64) / 2.0 - middle).abs());
                    if best.is_none_or(|(best_key, _, _)| key > best_key) {
                        best = Some((key, first, x));
                    }
                    start = None;
                }
                _ => {}
            }
        }
        if let Some((_, first, end)) = best {
            out[i].1 = first as f64;
            out[i + 1].0 = end as f64;
        }
    }

    let ink: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter_map(|(x, &inked)| inked.then_some(x))
        .collect();
    let first_at = |at: f64| ink.partition_point(|&x| (x as f64) < at);
    if let (Some(&(first, _)), Some(&(_, last))) = (spans.first(), spans.last()) {
        if let Some(&x) = ink.get(first_at(first - tolerance))
            && (x as f64) < out[0].1
        {
            out[0].0 = x as f64;
        }
        let end = first_at(last + tolerance);
        let tail = out.len() - 1;
        if let Some(&x) = end.checked_sub(1).and_then(|index| ink.get(index))
            && (x as f64) >= out[tail].0
        {
            out[tail].1 = x as f64 + 1.0;
        }
    }
    for (span, &vision) in out.iter_mut().zip(spans) {
        let (from, to) = (first_at(span.0), first_at(span.1));
        if from < to {
            *span = (ink[from] as f64, ink[to - 1] as f64 + 1.0);
        }
        if span.1 <= span.0 {
            *span = vision;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pdf_ocr::Word;

    use super::*;

    const WIDTH: usize = 140;
    const BASELINE: usize = 60;

    /// Letters as solid blocks 6 px wide on an 8 px pitch, standing on row
    /// 60: capitals and `b d f` rise 14 px (0.70 em of a 20 px em), the rest
    /// 10 px. Returns the word's ink columns.
    fn draw(gray: &mut [u8], left: usize, letters: &str) -> (f64, f64) {
        let mut x = left;
        for c in letters.chars() {
            let rise = if c.is_uppercase() || "bdf".contains(c) {
                14
            } else {
                10
            };
            for y in BASELINE - rise..BASELINE {
                gray[y * WIDTH + x..y * WIDTH + x + 6].fill(0);
            }
            x += 8;
        }
        (left as f64, (x - 2) as f64)
    }

    fn quad(x0: f64, y0: f64, x1: f64, y1: f64) -> Quad {
        Quad {
            top_left: Point { x: x0, y: y0 },
            top_right: Point { x: x1, y: y0 },
            bottom_right: Point { x: x1, y: y1 },
            bottom_left: Point { x: x0, y: y1 },
        }
    }

    #[test]
    fn word_boxes_follow_the_ink_rather_than_visions_loose_boxes() {
        let mut gray = vec![255u8; WIDTH * 80];
        let words = [("Ab", 20), ("cd", 60), ("Ef", 100)];
        let ink: Vec<(f64, f64)> = words
            .iter()
            .map(|&(text, left)| draw(&mut gray, left, text))
            .collect();
        // Vision's line quad reaches 1.2 em above the baseline and 0.1 em
        // below it, and every word box sits a quarter em to the right.
        let (top, bottom) = (36.0, 62.0);
        let line = Line {
            text: "Ab cd Ef".to_owned(),
            confidence: 1.0,
            quad: quad(10.0, top, 130.0, bottom),
            words: words
                .iter()
                .zip(&ink)
                .map(|(&(text, _), &(left, right))| Word {
                    text: text.to_owned(),
                    quad: Some(quad(left + 5.0, top, right + 5.0, bottom)),
                })
                .collect(),
        };
        let bitmap = Ink {
            width: WIDTH,
            height: 80,
            gray: &gray,
        };
        let placed = place_words(&bitmap, &line);
        assert_eq!(placed.len(), 3);
        for (place, &(left, right)) in placed.iter().zip(&ink) {
            let place = place.expect("every word is placed");
            assert_eq!((place.along, place.down), ([1.0, 0.0], [0.0, 1.0]));
            assert!(
                (place.origin.x - left).abs() < 0.01
                    && (place.origin.x + place.width - right).abs() < 0.01,
                "{place:?} should span the ink {left}..{right}"
            );
            // A 20 px em: 0.9 em above the baseline to 0.215 em below it.
            assert!(
                (place.origin.y - 42.0).abs() < 0.5 && (place.height - 22.3).abs() < 0.5,
                "{place:?} should run from 0.9 em above the baseline to 0.215 em below"
            );
        }
    }
}
