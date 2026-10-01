//! The Vision calls behind [`crate::recognize`] and
//! [`crate::supported_languages`].

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{AnyThread, sel};
use objc2_core_foundation::{CFData, CFIndex, CFMutableData, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSRange, NSString};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedText,
    VNRecognizedTextObservation, VNRectangleObservation, VNRequest, VNRequestTextRecognitionLevel,
};

use crate::{Bitmap, Line, OcrError, OcrOptions, PixelFormat, Point, Quad, Rect, Word};

/// A second-pass line counts as new while recognized lines cover less than
/// this share of it.
const READ_SHARE: f64 = 0.5;
/// Pixels darker than this (0 black, 255 white) are ink.
const INK: u8 = 128;
/// Side, in pixels, of the square cells unread ink is gathered in.
const CELL: usize = 4;
/// Unread ink blobs considered on one image: a skipped line holds one per
/// word.
const MAX_UNREAD: usize = 4096;
/// Slabs of unread ink, and lines holding unknown glyphs, read a second
/// time on one image.
const MAX_REGIONS: usize = 16;
/// A region taller than this many lines is read in slabs this tall.
const SLAB_LINES: f64 = 4.0;
/// A slab wider than this many line heights is read in pieces this wide
/// (see [`pieces`]).
const PIECE_LINES: f64 = 14.0;
/// Line heights each piece reads past its share of a slab on either side.
const PIECE_REACH: f64 = 3.0;
/// Pieces one slab is read in.
const MAX_PIECES: usize = 8;
/// Pieces read on one image, over every round of [`read_unread`].
const MAX_READS: usize = 64;
/// Line heights each round of [`read_unread`] grows its pieces by on every
/// side: Vision can miss a line in one crop and read it in another a few
/// pixels larger.
const ROUND_MARGINS: [f64; 3] = [0.0, 0.25, 0.5];
/// Line heights every piece is read at least across and down: Vision
/// misses a lone figure, such as a count alone in a table cell, more often
/// in a crop it fills much of.
const MIN_READ_LINES: f64 = 4.0;
/// Unread ink filling less of its box than this is a rule or an outline.
const MIN_FILL: f64 = 0.2;
/// Unread ink with more of its cells solid than this is a picture or a
/// dark border rather than strokes of text.
const MAX_SOLID: f64 = 0.6;
/// Share of each side of a box outline that its ink runs along.
const BOX_SIDE: f64 = 0.8;
/// Largest window, in pixels, searched for the mark under one word.
const MAX_MARK_PIXELS: usize = 1 << 20;
/// The Latin recognizer reads a glyph outside its alphabet, such as the yen
/// sign, as a middle dot.
const UNKNOWN_GLYPH: char = '\u{b7}';
/// A recognizer whose alphabet holds those glyphs, Latin letters included.
const WIDE_ALPHABET: [&str; 2] = ["ja-JP", "en-US"];
const NEIGHBOURS: [(isize, isize); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// The whole image as a Vision region of interest.
const WHOLE: CGRect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0));

/// An accurate-level request with language correction on, the settings
/// every call here shares.
fn base_request() -> Retained<VNRecognizeTextRequest> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request
}

/// [`base_request`] reading `options`' languages, which
/// [`check_languages`] has accepted.
fn text_request(options: &OcrOptions) -> Retained<VNRecognizeTextRequest> {
    let request = base_request();
    if !options.languages.is_empty() {
        let codes: Vec<Retained<NSString>> = options
            .languages
            .iter()
            .map(|code| NSString::from_str(code))
            .collect();
        request.setRecognitionLanguages(&NSArray::from_retained_slice(&codes));
    }
    if request.respondsToSelector(sel!(setAutomaticallyDetectsLanguage:)) {
        request.setAutomaticallyDetectsLanguage(options.detect_language);
    }
    request
}

fn supported(request: &VNRecognizeTextRequest) -> Result<Vec<String>, OcrError> {
    // SAFETY: a plain query on a live, fully initialized request; the
    // returned array holds NSStrings as declared.
    let codes = unsafe { request.supportedRecognitionLanguagesAndReturnError() }
        .map_err(|error| OcrError::Vision(describe(&error)))?;
    Ok(codes.iter().map(|code| code.to_string()).collect())
}

pub(crate) fn supported_languages() -> Result<Vec<String>, OcrError> {
    autoreleasepool(|_| supported(&base_request()))
}

/// Vision quietly ignores a code it does not know, which would read the
/// page in its default language instead.
fn check_languages(options: &OcrOptions) -> Result<(), OcrError> {
    if options.languages.is_empty() {
        return Ok(());
    }
    let supported = supported(&base_request())?;
    match options
        .languages
        .iter()
        .find(|code| !supported.contains(code))
    {
        Some(unknown) => Err(OcrError::UnsupportedLanguage {
            language: unknown.clone(),
            supported,
        }),
        None => Ok(()),
    }
}

/// Callers have checked that the bitmap is non-empty and its buffer covers
/// `stride * height` bytes.
pub(crate) fn recognize(bitmap: &Bitmap<'_>, options: &OcrOptions) -> Result<Vec<Line>, OcrError> {
    autoreleasepool(|_| {
        check_languages(options)?;
        let image = cg_image(bitmap)?;
        let no_options = NSDictionary::<VNImageOption, AnyObject>::new();
        // SAFETY: `image` is a valid CGImage that outlives the handler's use
        // of it (every perform below runs synchronously), and `no_options` is
        // an empty dictionary of the declared key and value types.
        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                &image,
                &no_options,
            )
        };
        let size = Size {
            width: f64::from(bitmap.width),
            height: f64::from(bitmap.height),
        };

        let request = text_request(options);
        perform(&handler, &[&request])?;
        let mut lines = read_lines(&request, WHOLE, size);
        read_unread(bitmap, &mut lines, size, |region| {
            let retry = text_request(options);
            let roi = size.normalized(region);
            // SAFETY: `read_unread` asks only for regions inside the image,
            // so `roi` lies inside the unit square, as Vision requires.
            unsafe { retry.setRegionOfInterest(roi) };
            perform(&handler, &[&retry])?;
            Ok(read_lines(&retry, roi, size))
        })?;
        if lines.iter().any(|line| line.text.contains(UNKNOWN_GLYPH))
            && supported(&base_request())?
                .iter()
                .any(|code| code == WIDE_ALPHABET[0])
        {
            read_unknown_glyphs(&handler, &mut lines, size)?;
        }
        drop_box_marks(bitmap, &mut lines);
        Ok(lines)
    })
}

fn perform(handler: &VNImageRequestHandler, requests: &[&VNRequest]) -> Result<(), OcrError> {
    handler
        .performRequests_error(&NSArray::from_slice(requests))
        .map_err(|error| OcrError::Vision(describe(&error)))
}

fn read_lines(request: &VNRecognizeTextRequest, roi: CGRect, size: Size) -> Vec<Line> {
    request.results().map_or_else(Vec::new, |observations| {
        observations
            .iter()
            .filter_map(|observation| line(&observation, roi, size))
            .collect()
    })
}

/// Reads again the ink `lines` leave unread, slab by slab and each slab in
/// pieces, and adds what it finds (see [`add_found`]). `read` reads one
/// region, which lies inside the image: [`unread_regions`] clamps every
/// slab to it, each piece lies inside its slab, and [`at_least`] and
/// [`grow`] keep each piece inside it while they widen it to at least
/// [`MIN_READ_LINES`] across and down. Each later round reads the ink the
/// earlier ones still left unread, in pieces grown a little more (see
/// [`ROUND_MARGINS`]), even when the first round found nothing: Vision can
/// miss a lone figure in a table cell in every crop of one round. A word
/// is kept only where no word was read before.
fn read_unread(
    bitmap: &Bitmap<'_>,
    lines: &mut Vec<Line>,
    size: Size,
    mut read: impl FnMut(Rect) -> Result<Vec<Line>, OcrError>,
) -> Result<(), OcrError> {
    let line_height = typical_line_height(lines).unwrap_or(size.height / 80.0);
    let ink = InkGrid::new(bitmap);
    let mut found: Vec<Line> = Vec::new();
    let mut reads = 0;
    'rounds: for margin in ROUND_MARGINS.map(|lines| lines * line_height) {
        let slabs = unread_regions(&ink, lines.iter().chain(&found), line_height, size);
        for piece in slabs.into_iter().flat_map(|slab| pieces(slab, line_height)) {
            if reads == MAX_READS {
                break 'rounds;
            }
            reads += 1;
            let crop = at_least(piece.roi, MIN_READ_LINES * line_height, size);
            for line in read(grow(crop, margin, size))? {
                let fresh = retain_words(line, |at| {
                    piece.owns(at.x) && !read_at(at, lines.iter().chain(&found))
                });
                found.extend(fresh);
            }
        }
    }
    add_found(lines, found);
    Ok(())
}

/// Adds to `lines`, in reading order, the lines `found` makes (see
/// [`join_fragments`]), except one `lines` mostly cover already. Each goes
/// in after the nearest line above it in its columns, so they go in last to
/// first: a later one on the same row then lands after an earlier one.
fn add_found(lines: &mut Vec<Line>, found: Vec<Line>) {
    for line in join_fragments(found).into_iter().rev() {
        if covered_share(line.quad.bounds(), lines) < READ_SHARE {
            insert_in_reading_order(lines, line);
        }
    }
}

/// The ink of `ink`, the image's, that the recognized lines leave unread,
/// as slabs to read again. A blob of dark cells counts when its ink is at
/// least a quarter line tall and wide, measured to the pixel, the size of a
/// word in lower case, and looks like text: it fills at least [`MIN_FILL`]
/// of its cells' box (a table's rules or a field's outline fill less) and
/// at most [`MAX_SOLID`] of its cells are solid (a photo or a dark border
/// is solid). Blobs grow by a line on every side and merge where they meet,
/// so a skipped line is gathered whole rather than word by word, across the
/// gap a comma leaves. A region counts when it gathers two blobs or more,
/// or one at least half a line tall, as a lone figure in a table cell is.
/// Dust on a scan makes lone blobs smaller than that, and every region
/// costs reads, so a lone word in lower case the first pass skipped stays
/// skipped. Each region is cut between its lines into slabs (see
/// [`InkGrid::slabs`]). Top to bottom, clamped to the image.
fn unread_regions<'a>(
    ink: &InkGrid,
    read: impl IntoIterator<Item = &'a Line>,
    line_height: f64,
    size: Size,
) -> Vec<Rect> {
    let mut unread = ink.clone();
    for line in read {
        unread.clear(line.quad.bounds());
    }
    let blobs = unread.blobs(MAX_UNREAD, |blob, fill, solid| {
        blob.height >= line_height / 4.0
            && blob.width >= line_height / 4.0
            && fill >= MIN_FILL
            && solid <= MAX_SOLID
    });
    // Each region, with the blobs it gathers and the tallest one's height.
    let mut regions: Vec<(Rect, usize, f64)> = Vec::new();
    for blob in blobs {
        let (mut region, mut count, mut tallest) = (grow(blob, line_height, size), 1, blob.height);
        while let Some(index) = regions
            .iter()
            .position(|&(other, ..)| overlap(region, other) > 0.0)
        {
            let (other, other_count, other_tallest) = regions.swap_remove(index);
            region = union(region, other);
            count += other_count;
            tallest = tallest.max(other_tallest);
        }
        if region.width > 0.0 && region.height > 0.0 {
            regions.push((region, count, tallest));
        }
    }
    let mut regions: Vec<Rect> = regions
        .into_iter()
        .filter(|&(_, count, tallest)| count > 1 || tallest >= line_height / 2.0)
        .map(|(region, ..)| region)
        .collect();
    regions.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    let mut slabs = Vec::new();
    for region in regions {
        if slabs.len() >= MAX_REGIONS {
            break;
        }
        ink.slabs(&unread, region, line_height, size, &mut slabs);
    }
    slabs.truncate(MAX_REGIONS);
    slabs
}

/// Which [`CELL`]-pixel squares of an image hold ink, and where in each: a
/// cell's low [`CELL`] bits mark its pixel rows that hold ink and its next
/// [`CELL`] bits its pixel columns, so a blob of cells measures its ink to
/// the pixel. A cell without ink is 0.
#[derive(Clone)]
struct InkGrid {
    cells: Vec<u8>,
    columns: usize,
    rows: usize,
}

const _: () = assert!(2 * CELL <= u8::BITS as usize, "a cell's marks fit a byte");

/// The first and last pixel line inside a cell that a mark of [`InkGrid`]
/// sets.
fn marked_span(mark: u8) -> (usize, usize) {
    let first = mark.trailing_zeros() as usize;
    let last = (u8::BITS - 1 - mark.leading_zeros()) as usize;
    (first, last)
}

impl InkGrid {
    fn new(bitmap: &Bitmap<'_>) -> Self {
        let width = bitmap.width as usize;
        let columns = width.div_ceil(CELL);
        let rows = (bitmap.height as usize).div_ceil(CELL);
        let bytes = bitmap.format.bytes_per_pixel();
        let mut cells = vec![0u8; columns * rows];
        for (y, row) in bitmap
            .data
            .chunks(bitmap.stride)
            .take(bitmap.height as usize)
            .enumerate()
        {
            let Some(row) = row.get(..width * bytes) else {
                continue;
            };
            let start = y / CELL * columns;
            let row_mark = 1u8 << (y % CELL);
            for (cell, pixels) in cells[start..start + columns]
                .iter_mut()
                .zip(row.chunks(CELL * bytes))
            {
                let column_mark = pixels
                    .chunks_exact(bytes)
                    .enumerate()
                    .filter(|(_, pixel)| is_ink(pixel))
                    .fold(0u8, |mark, (column, _)| mark | 1 << column);
                if column_mark != 0 {
                    *cell |= row_mark | column_mark << CELL;
                }
            }
        }
        Self {
            cells,
            columns,
            rows,
        }
    }

    /// Forgets the ink inside `rect`, in pixels.
    fn clear(&mut self, rect: Rect) {
        let (x0, x1) = cell_span(rect.x, rect.x + rect.width, self.columns);
        let (y0, y1) = cell_span(rect.y, rect.y + rect.height, self.rows);
        for row in y0..y1 {
            self.cells[row * self.columns + x0..row * self.columns + x1].fill(0);
        }
    }

    /// Up to `limit` blobs of 8-connected ink cells, each the box of its ink
    /// in pixels, that `keep` accepts given that box, the share of its
    /// cells' box its cells fill, and the share of them whose four
    /// neighbours are ink too.
    fn blobs(&self, limit: usize, keep: impl Fn(Rect, f64, f64) -> bool) -> Vec<Rect> {
        let columns = self.columns;
        let inked = |cell: usize| self.cells[cell] != 0;
        let mut unvisited: Vec<bool> = self.cells.iter().map(|&mark| mark != 0).collect();
        let mut blobs = Vec::new();
        let mut stack = Vec::new();
        let mut next = 0;
        while let Some(offset) = unvisited[next..].iter().position(|&cell| cell) {
            let start = next + offset;
            next = start + 1;
            unvisited[start] = false;
            stack.push(start);
            let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
            let (mut count, mut solid) = (0usize, 0usize);
            while let Some(cell) = stack.pop() {
                let (x, y) = (cell % columns, cell / columns);
                let mark = self.cells[cell];
                let (first_row, last_row) = marked_span(mark & ((1 << CELL) - 1));
                let (first_column, last_column) = marked_span(mark >> CELL);
                left = left.min(x * CELL + first_column);
                right = right.max(x * CELL + last_column);
                top = top.min(y * CELL + first_row);
                bottom = bottom.max(y * CELL + last_row);
                count += 1;
                if x > 0
                    && y > 0
                    && x + 1 < columns
                    && y + 1 < self.rows
                    && inked(cell - 1)
                    && inked(cell + 1)
                    && inked(cell - columns)
                    && inked(cell + columns)
                {
                    solid += 1;
                }
                for (dx, dy) in NEIGHBOURS {
                    let (Some(nx), Some(ny)) = (x.checked_add_signed(dx), y.checked_add_signed(dy))
                    else {
                        continue;
                    };
                    if nx < columns && ny < self.rows && unvisited[ny * columns + nx] {
                        unvisited[ny * columns + nx] = false;
                        stack.push(ny * columns + nx);
                    }
                }
            }
            let across = right / CELL + 1 - left / CELL;
            let down = bottom / CELL + 1 - top / CELL;
            let blob = Rect {
                x: left as f64,
                y: top as f64,
                width: (right + 1 - left) as f64,
                height: (bottom + 1 - top) as f64,
            };
            let fill = count as f64 / (across * down) as f64;
            if keep(blob, fill, solid as f64 / count as f64) {
                blobs.push(blob);
                if blobs.len() == limit {
                    break;
                }
            }
        }
        blobs
    }

    /// Pushes `region` onto `slabs` as slabs at most [`SLAB_LINES`] lines
    /// tall, cut on the rows of the image's ink, `self`, with the least ink:
    /// the outer cuts within a line above and below the `unread` ink in the
    /// region, the inner ones within a line of where even slabs would cut.
    /// So every cut lies between two lines of text, and each slab reaches a
    /// quarter line past its cuts, which keeps each line whole in one slab.
    fn slabs(
        &self,
        unread: &InkGrid,
        region: Rect,
        line_height: f64,
        size: Size,
        slabs: &mut Vec<Rect>,
    ) {
        let (x0, x1) = cell_span(region.x, region.x + region.width, self.columns);
        let (y0, y1) = cell_span(region.y, region.y + region.height, self.rows);
        let ink = |grid: &InkGrid, row: usize| {
            grid.cells[row * grid.columns + x0..row * grid.columns + x1]
                .iter()
                .filter(|&&cell| cell != 0)
                .count()
        };
        let Some(first) = (y0..y1).find(|&row| ink(unread, row) > 0) else {
            return;
        };
        let last = (first..y1)
            .rev()
            .find(|&row| ink(unread, row) > 0)
            .unwrap_or(first);
        let quietest = |from: f64, to: f64, ideal: f64| {
            let ideal_row = (ideal / CELL as f64) as usize;
            let (first, last) = cell_span(from, to, self.rows);
            (first..last)
                .min_by_key(|&row| (ink(self, row), row.abs_diff(ideal_row)))
                .map_or(ideal, |row| (row * CELL) as f64 + CELL as f64 / 2.0)
        };
        let (ink_top, ink_bottom) = ((first * CELL) as f64, ((last + 1) * CELL) as f64);
        let top = quietest(ink_top - line_height, ink_top, ink_top);
        let bottom = quietest(ink_bottom, ink_bottom + line_height, ink_bottom);
        let count =
            (((bottom - top) / (line_height * SLAB_LINES)).ceil() as usize).clamp(1, MAX_REGIONS);
        let overlap = line_height / 4.0;
        let mut from = top;
        for k in 1..count {
            let ideal = top + (bottom - top) * k as f64 / count as f64;
            let cut = quietest(ideal - line_height, ideal + line_height, ideal).clamp(from, bottom);
            slabs.push(band(region, from - overlap, cut + overlap, size));
            from = cut;
        }
        slabs.push(band(region, from - overlap, bottom + overlap, size));
    }
}

/// The image from `top` to `bottom` across `region`'s columns.
fn band(region: Rect, top: f64, bottom: f64, size: Size) -> Rect {
    let top = top.max(0.0);
    let bottom = bottom.min(size.height);
    Rect {
        x: region.x,
        y: top,
        width: region.width,
        height: (bottom - top).max(0.0),
    }
}

/// Part of a slab read on its own: Vision reads `roi`, and the piece keeps
/// the words whose centre lies from `from` to `to` across the image.
#[derive(Debug, Clone, Copy)]
struct Piece {
    roi: Rect,
    from: f64,
    to: f64,
}

impl Piece {
    /// Whether the piece keeps a word centred at `x` across the image.
    fn owns(&self, x: f64) -> bool {
        self.from <= x && x < self.to
    }
}

/// `slab` as pieces to read one at a time, each at most [`PIECE_LINES`]
/// line heights wide. macOS compiles Vision's text recognizer the first
/// time a program uses it and caches the result for that program. A
/// damaged cache entry sends the lines of one width class to the wrong
/// network, and Vision then drops every line in that class, such as the
/// longest lines on a page of small type. Pieces are narrower than any
/// class seen damaged. Each piece keeps the words whose centre lies in its
/// share of the slab and reads [`PIECE_REACH`] line heights past that share
/// on either side, so a word the share's edge cuts is whole in the piece
/// that keeps it.
fn pieces(slab: Rect, line_height: f64) -> Vec<Piece> {
    let count = ((slab.width / (line_height * PIECE_LINES)).ceil() as usize).clamp(1, MAX_PIECES);
    let edge = |index: usize| slab.x + slab.width * index as f64 / count as f64;
    let reach = line_height * PIECE_REACH;
    let right = slab.x + slab.width;
    (0..count)
        .map(|index| {
            let (from, to) = (edge(index), edge(index + 1));
            let (left, end) = ((from - reach).max(slab.x), (to + reach).min(right));
            Piece {
                roi: Rect {
                    x: left,
                    y: slab.y,
                    width: (end - left).max(0.0),
                    height: slab.height,
                },
                from: if index == 0 { f64::NEG_INFINITY } else { from },
                to: if index + 1 == count {
                    f64::INFINITY
                } else {
                    to
                },
            }
        })
        .collect()
}

/// `line` holding only the words whose centre `keep` accepts, its text and
/// box made of theirs; a word Vision placed no box for is centred on its
/// line. `None` when it keeps none.
fn retain_words(mut line: Line, keep: impl Fn(Point) -> bool) -> Option<Line> {
    let bounds = line.quad.bounds();
    let count = line.words.len();
    line.words
        .retain(|word| keep(centre(word.quad.map_or(bounds, |quad| quad.bounds()))));
    let (first, last) = (line.words.first()?.quad, line.words.last()?.quad);
    if line.words.len() < count {
        line.text = joined(&line.words);
        if let (Some(first), Some(last)) = (first, last) {
            line.quad = Quad {
                top_left: first.top_left,
                top_right: last.top_right,
                bottom_right: last.bottom_right,
                bottom_left: first.bottom_left,
            };
        }
    }
    Some(line)
}

/// Whether a word of `lines` covers `point`; a word Vision placed no box
/// for covers its line's box.
fn read_at<'a>(point: Point, lines: impl IntoIterator<Item = &'a Line>) -> bool {
    lines.into_iter().any(|line| {
        let bounds = line.quad.bounds();
        line.words.iter().any(|word| {
            let rect = word.quad.map_or(bounds, |quad| quad.bounds());
            (rect.x..rect.x + rect.width).contains(&point.x)
                && (rect.y..rect.y + rect.height).contains(&point.y)
        })
    })
}

fn centre(rect: Rect) -> Point {
    Point {
        x: rect.x + rect.width / 2.0,
        y: rect.y + rect.height / 2.0,
    }
}

/// The lines the second pass's fragments make, in reading order: rows top
/// to bottom, each left to right. A fragment is on the row of the first
/// fragment above it whose height holds its middle, and it continues the
/// line to its left on that row when the gap between them is at most half
/// the shorter one's height: a space between words, not a gutter.
fn join_fragments(mut fragments: Vec<Line>) -> Vec<Line> {
    let middle = |line: &Line| centre(line.quad.bounds()).y;
    fragments.sort_by(|a, b| middle(a).total_cmp(&middle(b)));
    let mut rows: Vec<Vec<Line>> = Vec::new();
    for fragment in fragments {
        let holds = |row: &[Line]| {
            let first = row[0].quad.bounds();
            (first.y..=first.y + first.height).contains(&middle(&fragment))
        };
        match rows.last_mut() {
            Some(row) if holds(row) => row.push(fragment),
            _ => rows.push(vec![fragment]),
        }
    }
    let mut joined = Vec::new();
    for mut row in rows {
        row.sort_by(|a, b| a.quad.bounds().x.total_cmp(&b.quad.bounds().x));
        let mut current: Option<Line> = None;
        for fragment in row {
            match current.as_mut() {
                Some(line) if continues(line.quad.bounds(), fragment.quad.bounds()) => {
                    line.text.push(' ');
                    line.text.push_str(&fragment.text);
                    line.words.extend(fragment.words);
                    line.quad.top_right = fragment.quad.top_right;
                    line.quad.bottom_right = fragment.quad.bottom_right;
                    line.confidence = line.confidence.min(fragment.confidence);
                }
                _ => joined.extend(current.replace(fragment)),
            }
        }
        joined.extend(current);
    }
    joined
}

/// Whether a fragment at `next` continues a line at `line` on its row: it
/// reaches further right, and the gap between them is at most half the
/// shorter one's height.
fn continues(line: Rect, next: Rect) -> bool {
    let end = line.x + line.width;
    next.x - end <= line.height.min(next.height) / 2.0 && next.x + next.width > end
}

/// Vision reads a form's empty box as a bullet or a dash. A word with no
/// letter or digit whose ink outlines a box is that mark, not text; it is
/// dropped, and so is a line it leaves empty.
fn drop_box_marks(bitmap: &Bitmap<'_>, lines: &mut Vec<Line>) {
    for line in lines.iter_mut() {
        let count = line.words.len();
        line.words.retain(|word| {
            word.text.chars().any(char::is_alphanumeric)
                || !word
                    .quad
                    .is_some_and(|quad| is_box_outline(bitmap, quad.bounds()))
        });
        if line.words.len() != count {
            line.text = joined(&line.words);
        }
    }
    lines.retain(|line| !line.text.is_empty());
}

/// `words` as a line's text, a space between each two.
fn joined(words: &[Word]) -> String {
    words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the ink shape with the most pixels in `rect` outlines a box:
/// ink along at least [`BOX_SIDE`] of each side and in each corner of its
/// bounds, and less than half its middle inked. A round bullet leaves its
/// corners empty and a filled square its middle full.
fn is_box_outline(bitmap: &Bitmap<'_>, rect: Rect) -> bool {
    let (width, height) = (bitmap.width as usize, bitmap.height as usize);
    let clamp = |value: f64, limit: usize| (value.max(0.0) as usize).min(limit);
    let pad = rect.height;
    let (x0, x1) = (
        clamp(rect.x - pad, width),
        clamp(rect.x + rect.width + pad, width),
    );
    let (y0, y1) = (
        clamp(rect.y - pad, height),
        clamp(rect.y + rect.height + pad, height),
    );
    let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if w == 0 || h == 0 || w * h > MAX_MARK_PIXELS {
        return false;
    }
    let bytes = bitmap.format.bytes_per_pixel();
    let ink: Vec<bool> = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| y * bitmap.stride + x * bytes))
        .map(|at| bitmap.data.get(at..at + bytes).is_some_and(is_ink))
        .collect();
    let inside_x = clamp(rect.x, width).saturating_sub(x0)..clamp(rect.x + rect.width, width) - x0;
    let inside_y =
        clamp(rect.y, height).saturating_sub(y0)..clamp(rect.y + rect.height, height) - y0;
    let mut label = vec![0u32; w * h];
    // (label, pixels inside `rect`, [left, top, right, bottom], touches the window's edge)
    let mut best: Option<(u32, usize, [usize; 4], bool)> = None;
    let mut next_label = 0;
    let mut stack = Vec::new();
    for y in inside_y.clone() {
        for x in inside_x.clone() {
            let start = y * w + x;
            if !ink[start] || label[start] != 0 {
                continue;
            }
            next_label += 1;
            label[start] = next_label;
            stack.push(start);
            let (mut inside, mut bounds, mut edge) = (0, [x, y, x, y], false);
            while let Some(at) = stack.pop() {
                let (px, py) = (at % w, at / w);
                inside += usize::from(inside_x.contains(&px) && inside_y.contains(&py));
                bounds = [
                    bounds[0].min(px),
                    bounds[1].min(py),
                    bounds[2].max(px),
                    bounds[3].max(py),
                ];
                edge |= px == 0 || py == 0 || px + 1 == w || py + 1 == h;
                for (dx, dy) in NEIGHBOURS {
                    let (Some(nx), Some(ny)) =
                        (px.checked_add_signed(dx), py.checked_add_signed(dy))
                    else {
                        continue;
                    };
                    if nx < w && ny < h && ink[ny * w + nx] && label[ny * w + nx] == 0 {
                        label[ny * w + nx] = next_label;
                        stack.push(ny * w + nx);
                    }
                }
            }
            if best.is_none_or(|(_, most, _, _)| inside > most) {
                best = Some((next_label, inside, bounds, edge));
            }
        }
    }
    let Some((id, _, [left, top, right, bottom], false)) = best else {
        return false;
    };
    let (across, down) = (right + 1 - left, bottom + 1 - top);
    if across < 6 || down < 6 || across > 2 * down || down > 2 * across {
        return false;
    }
    let member = |x: usize, y: usize| label[y * w + x] == id;
    let band = (across.min(down) / 6).max(1);
    let corner = (across.min(down) / 10).max(1);
    let runs_along = |hits: usize, length: usize| hits as f64 >= BOX_SIDE * length as f64;
    let top_side = (left..=right)
        .filter(|&x| (top..top + band).any(|y| member(x, y)))
        .count();
    let bottom_side = (left..=right)
        .filter(|&x| (bottom + 1 - band..=bottom).any(|y| member(x, y)))
        .count();
    let left_side = (top..=bottom)
        .filter(|&y| (left..left + band).any(|x| member(x, y)))
        .count();
    let right_side = (top..=bottom)
        .filter(|&y| (right + 1 - band..=right).any(|x| member(x, y)))
        .count();
    let inked = |xs: std::ops::Range<usize>, ys: std::ops::Range<usize>| {
        ys.into_iter().any(|y| xs.clone().any(|x| member(x, y)))
    };
    let corners = inked(left..left + corner, top..top + corner)
        && inked(right + 1 - corner..right + 1, top..top + corner)
        && inked(left..left + corner, bottom + 1 - corner..bottom + 1)
        && inked(
            right + 1 - corner..right + 1,
            bottom + 1 - corner..bottom + 1,
        );
    let (middle_x, middle_y) = (left + band..right + 1 - band, top + band..bottom + 1 - band);
    let middle = middle_y
        .clone()
        .map(|y| middle_x.clone().filter(|&x| member(x, y)).count())
        .sum::<usize>();
    runs_along(top_side, across)
        && runs_along(bottom_side, across)
        && runs_along(left_side, down)
        && runs_along(right_side, down)
        && corners
        && 2 * middle < middle_x.len() * middle_y.len()
}

fn is_ink(pixel: &[u8]) -> bool {
    match *pixel {
        [gray] => gray < INK,
        [r, g, b, a] => a >= 128 && u16::from(r) + u16::from(g) + u16::from(b) < 3 * u16::from(INK),
        _ => false,
    }
}

/// The cells from `from` to `to` pixels along an axis `cells` long.
fn cell_span(from: f64, to: f64, cells: usize) -> (usize, usize) {
    let first = ((from / CELL as f64).floor().max(0.0) as usize).min(cells);
    let last = ((to / CELL as f64).ceil().max(0.0) as usize).min(cells);
    (first, last.max(first))
}

/// The median height of the recognized lines, measured across each line so
/// a turned line counts its height rather than its length.
fn typical_line_height(lines: &[Line]) -> Option<f64> {
    let mut heights: Vec<f64> = lines
        .iter()
        .map(|line| {
            let quad = &line.quad;
            (quad.bottom_left.x - quad.top_left.x).hypot(quad.bottom_left.y - quad.top_left.y)
        })
        .filter(|height| *height > 0.0)
        .collect();
    heights.sort_by(f64::total_cmp);
    heights.get(heights.len() / 2).copied()
}

/// `rect` grown by `margin` on every side, clamped to the image.
fn grow(rect: Rect, margin: f64, size: Size) -> Rect {
    let x0 = (rect.x - margin).max(0.0);
    let y0 = (rect.y - margin).max(0.0);
    let x1 = (rect.x + rect.width + margin).min(size.width);
    let y1 = (rect.y + rect.height + margin).min(size.height);
    Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0.0),
        height: (y1 - y0).max(0.0),
    }
}

/// `rect` widened about its centre to at least `side` across and down, and
/// moved inside the image where it would reach past an edge; no wider than
/// the image.
fn at_least(rect: Rect, side: f64, size: Size) -> Rect {
    let span = |start: f64, length: f64, limit: f64| {
        let wide = length.max(side).min(limit);
        let start = (start - (wide - length) / 2.0).clamp(0.0, limit - wide);
        (start, wide)
    };
    let (x, width) = span(rect.x, rect.width, size.width);
    let (y, height) = span(rect.y, rect.height, size.height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Reads every line holding [`UNKNOWN_GLYPH`] again with [`WIDE_ALPHABET`],
/// and gives a word that reading when it differs only where the dots were,
/// so every other character and every box stays as first read.
fn read_unknown_glyphs(
    handler: &VNImageRequestHandler,
    lines: &mut [Line],
    size: Size,
) -> Result<(), OcrError> {
    let codes: Vec<Retained<NSString>> = WIDE_ALPHABET
        .iter()
        .map(|code| NSString::from_str(code))
        .collect();
    let languages = NSArray::from_retained_slice(&codes);
    for line in lines
        .iter_mut()
        .filter(|line| line.text.contains(UNKNOWN_GLYPH))
        .take(MAX_REGIONS)
    {
        let bounds = line.quad.bounds();
        let region = grow(bounds, bounds.height / 2.0, size);
        if region.width <= 0.0 || region.height <= 0.0 {
            continue;
        }
        let request = base_request();
        request.setRecognitionLanguages(&languages);
        let roi = size.normalized(region);
        // SAFETY: `grow` clamps the region to the image, so `roi` lies
        // inside the unit square.
        unsafe { request.setRegionOfInterest(roi) };
        perform(handler, &[&request])?;
        let readings: Vec<String> = read_lines(&request, roi, size)
            .iter()
            .flat_map(|reading| reading.text.split_whitespace().map(fold_wide))
            .collect();
        let mut changed = false;
        for word in &mut line.words {
            if let Some(reading) = readings
                .iter()
                .find(|reading| fills_unknown(&word.text, reading))
            {
                word.text.clone_from(reading);
                changed = true;
            }
        }
        if changed {
            line.text = joined(&line.words);
        }
    }
    Ok(())
}

/// Whether `reading` is `word` with every changed character an unknown
/// glyph read as something that is not itself a dot.
fn fills_unknown(word: &str, reading: &str) -> bool {
    word != reading
        && word.chars().count() == reading.chars().count()
        && word.chars().zip(reading.chars()).all(|(first, second)| {
            first == second
                || (first == UNKNOWN_GLYPH
                    && !second.is_whitespace()
                    && !matches!(
                        second,
                        '.' | '\u{b7}'
                            | '\u{2022}'
                            | '\u{2219}'
                            | '\u{22c5}'
                            | '\u{30fb}'
                            | '\u{ff65}'
                    ))
        })
}

/// Fullwidth forms, which the Japanese recognizer prefers, as their
/// ordinary characters.
fn fold_wide(text: &str) -> String {
    text.chars()
        .map(|c| match u32::from(c) {
            wide @ 0xff01..=0xff5e => char::from_u32(wide - 0xfee0).unwrap_or(c),
            0xffe0 => '\u{a2}',
            0xffe1 => '\u{a3}',
            0xffe5 => '\u{a5}',
            0xffe6 => '\u{20a9}',
            _ => c,
        })
        .collect()
}

/// The share of `rect` that `lines` cover, from 0 to 1.
fn covered_share(rect: Rect, lines: &[Line]) -> f64 {
    let area = rect.width * rect.height;
    if area <= 0.0 {
        return 1.0;
    }
    let covered: f64 = lines
        .iter()
        .map(|line| overlap(rect, line.quad.bounds()))
        .sum();
    (covered / area).min(1.0)
}

fn overlap(a: Rect, b: Rect) -> f64 {
    let width = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let height = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    width.max(0.0) * height.max(0.0)
}

fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    Rect {
        x: x0,
        y: y0,
        width: (a.x + a.width).max(b.x + b.width) - x0,
        height: (a.y + a.height).max(b.y + b.height) - y0,
    }
}

/// Puts a second-pass line where a reader meets it: after the nearest line
/// above it that shares its columns, else before the nearest such line
/// below, else last.
fn insert_in_reading_order(lines: &mut Vec<Line>, line: Line) {
    let rect = line.quad.bounds();
    let middle = |r: Rect| r.y + r.height / 2.0;
    let centre = middle(rect);
    let same_column = |r: Rect| r.x < rect.x + rect.width && rect.x < r.x + r.width;
    let mut above: Option<(usize, f64)> = None;
    let mut below: Option<(usize, f64)> = None;
    for (index, other) in lines.iter().enumerate() {
        let bounds = other.quad.bounds();
        if !same_column(bounds) {
            continue;
        }
        let y = middle(bounds);
        if y < centre {
            if above.is_none_or(|(_, best)| y > best) {
                above = Some((index + 1, y));
            }
        } else if below.is_none_or(|(_, best)| y < best) {
            below = Some((index, y));
        }
    }
    let at = above.or(below).map_or(lines.len(), |(index, _)| index);
    lines.insert(at, line);
}

fn describe(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

fn cg_image(bitmap: &Bitmap<'_>) -> Result<CFRetained<CGImage>, OcrError> {
    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    let (space, alpha, stride, provider) = match bitmap.format {
        PixelFormat::Gray8 => {
            let data = CFData::from_bytes(&bitmap.data[..bitmap.stride * height]);
            (
                CGColorSpace::new_device_gray(),
                CGImageAlphaInfo::None,
                bitmap.stride,
                CGDataProvider::with_cf_data(Some(&data)),
            )
        }
        PixelFormat::Rgba8 | PixelFormat::Rgba8Premultiplied => {
            let data = flatten_on_white(bitmap)?;
            (
                CGColorSpace::new_device_rgb(),
                CGImageAlphaInfo::NoneSkipLast,
                width * 4,
                CGDataProvider::with_cf_data(Some(&data)),
            )
        }
    };
    let space = space.ok_or_else(|| OcrError::Vision("no device color space".into()))?;
    let provider = provider
        .ok_or_else(|| OcrError::Vision("could not wrap the bitmap for Core Graphics".into()))?;
    let bits_per_pixel = 8 * bitmap.format.bytes_per_pixel();
    // SAFETY: `decode` is null, which CGImageCreate accepts as "no decode
    // array". The provider owns a copy of exactly `stride * height` bytes,
    // matching the width, stride and sample layout passed alongside it.
    let image = unsafe {
        CGImage::new(
            width,
            height,
            8,
            bits_per_pixel,
            stride,
            Some(&space),
            CGBitmapInfo(alpha.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    };
    image.ok_or_else(|| OcrError::Vision("Core Graphics rejected the bitmap".into()))
}

/// Tightly packed RGBX of the bitmap composited over white paper. Vision
/// reads color and ignores alpha, so a transparent background would
/// otherwise read as black behind black text.
fn flatten_on_white(bitmap: &Bitmap<'_>) -> Result<CFRetained<CFMutableData>, OcrError> {
    let too_large = || OcrError::InvalidBitmap("the image is too large".into());
    let row_bytes = bitmap.width as usize * 4;
    let row_length = CFIndex::try_from(row_bytes).map_err(|_| too_large())?;
    let capacity = CFIndex::try_from(bitmap.height)
        .ok()
        .and_then(|height| row_length.checked_mul(height))
        .ok_or_else(too_large)?;
    let out = CFMutableData::new(None, capacity)
        .ok_or_else(|| OcrError::Vision("could not allocate the flattened image".into()))?;
    let premultiplied = bitmap.format == PixelFormat::Rgba8Premultiplied;
    let mut flat_row = Vec::with_capacity(row_bytes);
    for row in bitmap
        .data
        .chunks(bitmap.stride)
        .take(bitmap.height as usize)
    {
        flat_row.clear();
        for pixel in row[..row_bytes].as_chunks::<4>().0 {
            let a = u32::from(pixel[3]);
            for &c in &pixel[..3] {
                let c = u32::from(c);
                let covered = if premultiplied {
                    c.min(a)
                } else {
                    (c * a + 127) / 255
                };
                flat_row.push((covered + 255 - a) as u8);
            }
            flat_row.push(255);
        }
        // SAFETY: `flat_row` is a live buffer of exactly `row_length` bytes,
        // and `out` is a valid mutable CFData we own.
        unsafe { CFMutableData::append_bytes(Some(&out), flat_row.as_ptr(), row_length) };
    }
    Ok(out)
}

/// The bitmap's size in pixels, mapping Vision's coordinates (normalized
/// to a region of interest, origin bottom-left) to top-left pixels.
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

impl Size {
    fn point(self, point: CGPoint, roi: CGRect) -> Point {
        Point {
            x: (roi.origin.x + point.x * roi.size.width) * self.width,
            y: (1.0 - roi.origin.y - point.y * roi.size.height) * self.height,
        }
    }

    fn quad(self, observation: &VNRectangleObservation, roi: CGRect) -> Quad {
        // SAFETY: plain CGPoint property getters on a live observation
        // Vision returned.
        let (top_left, top_right, bottom_right, bottom_left) = unsafe {
            (
                observation.topLeft(),
                observation.topRight(),
                observation.bottomRight(),
                observation.bottomLeft(),
            )
        };
        Quad {
            top_left: self.point(top_left, roi),
            top_right: self.point(top_right, roi),
            bottom_right: self.point(bottom_right, roi),
            bottom_left: self.point(bottom_left, roi),
        }
    }

    /// A pixel box inside the image as a Vision region of interest.
    fn normalized(self, rect: Rect) -> CGRect {
        CGRect::new(
            CGPoint::new(
                rect.x / self.width,
                1.0 - (rect.y + rect.height) / self.height,
            ),
            CGSize::new(rect.width / self.width, rect.height / self.height),
        )
    }
}

fn line(observation: &VNRecognizedTextObservation, roi: CGRect, size: Size) -> Option<Line> {
    let candidate = observation.topCandidates(1).firstObject()?;
    let text = candidate.string().to_string();
    let words = words(&candidate, &text, roi, size);
    Some(Line {
        confidence: candidate.confidence(),
        quad: size.quad(observation, roi),
        text,
        words,
    })
}

/// The whitespace-separated words of `text`, each placed through
/// `boundingBoxForRange:error:` on its UTF-16 range.
fn words(candidate: &VNRecognizedText, text: &str, roi: CGRect, size: Size) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let mut offset = 0usize;
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_whitespace() {
            if !current.is_empty() {
                let range = NSRange::new(start, offset - start);
                // SAFETY: `range` lies inside the candidate's own string: it
                // was measured in UTF-16 units over that string's characters.
                let placed = unsafe { candidate.boundingBoxForRange_error(range) };
                words.push(Word {
                    text: std::mem::take(&mut current),
                    quad: placed.ok().map(|observation| size.quad(&observation, roi)),
                });
            }
        } else {
            if current.is_empty() {
                start = offset;
            }
            current.push(c);
        }
        offset += c.len_utf16();
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: usize = 400;
    const HEIGHT: usize = 200;

    fn fill(pixels: &mut [u8], x: std::ops::Range<usize>, y: std::ops::Range<usize>) {
        for row in y {
            pixels[row * WIDTH + x.start..row * WIDTH + x.end].fill(0);
        }
    }

    fn read_line(x0: f64, y0: f64, x1: f64, y1: f64) -> Line {
        let point = |x, y| Point { x, y };
        Line {
            text: "read".to_owned(),
            confidence: 1.0,
            quad: Quad {
                top_left: point(x0, y0),
                top_right: point(x1, y0),
                bottom_right: point(x1, y1),
                bottom_left: point(x0, y1),
            },
            words: Vec::new(),
        }
    }

    /// A line of `words` from `left` along `top`, 10 px tall, 6 px a letter
    /// and 4 px between words, as Vision returns one.
    fn typed(words: &[&str], left: f64, top: f64) -> Line {
        let mut x = left;
        let words = words
            .iter()
            .map(|text| {
                let right = x + 6.0 * text.chars().count() as f64;
                let word = Word {
                    text: (*text).to_owned(),
                    quad: Some(read_line(x, top, right, top + 10.0).quad),
                };
                x = right + 4.0;
                word
            })
            .collect();
        line_of(words).expect("a line has words")
    }

    /// A line holding `words`, its box from the first word's to the last's.
    fn line_of(words: Vec<Word>) -> Option<Line> {
        let first = words.first()?.quad?;
        let last = words.last()?.quad?;
        Some(Line {
            text: joined(&words),
            confidence: 0.9,
            quad: Quad {
                top_left: first.top_left,
                top_right: last.top_right,
                bottom_right: last.bottom_right,
                bottom_left: first.bottom_left,
            },
            words,
        })
    }

    #[test]
    fn only_unread_ink_the_size_of_text_is_read_again() {
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        // A line the first pass read, 24 px tall with its quad.
        fill(&mut pixels, 20..200, 20..40);
        // A lone digit it skipped, a table rule, and a picture five lines tall.
        fill(&mut pixels, 250..262, 60..76);
        fill(&mut pixels, 20..240, 84..86);
        fill(&mut pixels, 300..360, 90..200);
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let size = Size {
            width: WIDTH as f64,
            height: HEIGHT as f64,
        };
        let regions = unread_regions(
            &InkGrid::new(&bitmap),
            &[read_line(18.0, 18.0, 202.0, 42.0)],
            24.0,
            size,
        );
        assert_eq!(
            regions.len(),
            1,
            "only the digit is unread text: {regions:?}"
        );
        let region = regions[0];
        assert!(
            region.x <= 250.0
                && region.x + region.width >= 262.0
                && region.y <= 60.0
                && region.y + region.height >= 76.0,
            "the region holds the digit: {region:?}"
        );
    }

    #[test]
    fn dust_on_a_scan_is_not_read_again() {
        // The first pass read the line at the top. Below it lie specks of
        // dust, as on a fax or a photocopy: forty of two pixels, each across
        // the corner of four grid cells, and three of nine pixels, under
        // half a line tall and far apart.
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        fill(&mut pixels, 20..200, 20..40);
        for y in (63..HEIGHT).step_by(40) {
            for x in (23..WIDTH).step_by(40) {
                fill(&mut pixels, x..x + 2, y..y + 2);
            }
        }
        for (x, y) in [(80, 120), (200, 160), (320, 120)] {
            fill(&mut pixels, x..x + 9, y..y + 9);
        }
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let size = Size {
            width: WIDTH as f64,
            height: HEIGHT as f64,
        };
        let mut lines = vec![read_line(18.0, 18.0, 202.0, 42.0)];
        let mut reads = Vec::new();
        read_unread(&bitmap, &mut lines, size, |region| {
            reads.push(region);
            Ok(Vec::new())
        })
        .expect("reading finds nothing");
        assert!(reads.is_empty(), "dust is not read: {reads:?}");
    }

    #[test]
    fn a_dotted_word_takes_only_a_reading_that_fills_its_dots() {
        assert!(fills_unknown("·3,800.", &fold_wide("￥3,800．")));
        assert!(fills_unknown("·3,800.", "¥3,800."));
        // A real middle dot read again as a dot stays, and so does a word
        // the second reading changes anywhere else.
        assert!(!fills_unknown("col·lecció", "col・lecció"));
        assert!(!fills_unknown("·3,800.", "¥3,300."));
        assert!(!fills_unknown("·3,800.", "¥3,800"));
        assert!(!fills_unknown("·3,800.", "·3,800."));
    }

    /// A `w` by `h` px outline at (`x`, `y`) with sides `stroke` px thick.
    fn ring(pixels: &mut [u8], x: usize, y: usize, w: usize, h: usize, stroke: usize) {
        fill(pixels, x..x + w, y..y + stroke);
        fill(pixels, x..x + w, y + h - stroke..y + h);
        fill(pixels, x..x + stroke, y..y + h);
        fill(pixels, x + w - stroke..x + w, y..y + h);
    }

    #[test]
    fn a_skipped_paragraph_is_read_in_slabs_that_keep_each_line_whole() {
        const TALL: usize = 460;
        let mut pixels = vec![255u8; WIDTH * TALL];
        // The line the first pass read, 24 px tall with its quad.
        fill(&mut pixels, 20..200, 20..40);
        // Eight lines of letter-like rings 30 px apart that it skipped.
        let tops: Vec<usize> = (0..8).map(|line| 80 + 30 * line).collect();
        for &top in &tops {
            for left in (20..360).step_by(16) {
                ring(&mut pixels, left, top, 12, 20, 4);
            }
        }
        // A form field's outline below them, which is not text.
        ring(&mut pixels, 150, 360, 200, 60, 2);
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: TALL as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let size = Size {
            width: WIDTH as f64,
            height: TALL as f64,
        };
        let slabs = unread_regions(
            &InkGrid::new(&bitmap),
            &[read_line(18.0, 18.0, 202.0, 42.0)],
            24.0,
            size,
        );
        assert!(slabs.len() > 1, "the paragraph is cut: {slabs:?}");
        for slab in &slabs {
            assert!(
                slab.height <= 24.0 * (SLAB_LINES + 2.5) && slab.y + slab.height <= 340.0,
                "slabs are a few lines tall and leave the outline alone: {slabs:?}"
            );
        }
        for top in tops {
            let (top, bottom) = (top as f64, (top + 20) as f64);
            assert!(
                slabs
                    .iter()
                    .any(|slab| slab.y <= top && slab.y + slab.height >= bottom),
                "the line at {top} is whole in one slab: {slabs:?}"
            );
        }
    }

    #[test]
    fn a_line_skipped_between_read_lines_is_read_again_whole() {
        // Three lines 30 px apart, each a row of words. Most words are as
        // short as lower case without ascenders; every fourth reaches 6 px
        // above and below them. The first pass read the outer lines, its
        // boxes spanning their tallest words, and skipped the middle one, as
        // Vision does with some lines of a dense paragraph.
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        for top in [20, 50, 80] {
            for (index, left) in (20..380).step_by(30).enumerate() {
                if index % 4 == 0 {
                    ring(&mut pixels, left, top, 20, 24, 3);
                } else {
                    ring(&mut pixels, left, top + 6, 20, 12, 3);
                }
            }
        }
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let size = Size {
            width: WIDTH as f64,
            height: HEIGHT as f64,
        };
        let read = [
            read_line(18.0, 19.0, 372.0, 45.0),
            read_line(18.0, 79.0, 372.0, 105.0),
        ];
        let slabs = unread_regions(&InkGrid::new(&bitmap), &read, 26.0, size);
        assert!(
            slabs.iter().any(|slab| slab.x <= 20.0
                && slab.x + slab.width >= 370.0
                && slab.y <= 50.0
                && slab.y + slab.height >= 74.0),
            "the skipped line is whole in one region, not cut word by word: {slabs:?}"
        );
    }

    #[test]
    fn a_wide_slab_is_read_in_narrow_pieces_that_each_hold_whole_the_words_they_keep() {
        // A full line of 7 pt type runs about 67 line heights, a width a
        // damaged recognizer cache drops. The narrowest width class seen
        // damaged starts near 26 line heights, measured by a line's own
        // height, which can be a sixth less than the typical line's.
        let line_height = 10.0;
        let slab = Rect {
            x: 40.0,
            y: 100.0,
            width: 670.0,
            height: 50.0,
        };
        let pieces = pieces(slab, line_height);
        for piece in &pieces {
            assert!(
                piece.roi.width < 21.0 * line_height
                    && piece.roi.x >= slab.x
                    && piece.roi.x + piece.roi.width <= slab.x + slab.width
                    && (piece.roi.y - slab.y).abs() < 1e-9
                    && (piece.roi.height - slab.height).abs() < 1e-9,
                "each piece is a narrow part of the slab: {pieces:?}"
            );
        }
        // A word up to six line heights wide, wherever it sits, is kept by
        // exactly one piece, which reads it whole.
        let width = 6.0 * line_height;
        let mut left = slab.x;
        while left + width <= slab.x + slab.width {
            let keepers: Vec<&Piece> = pieces
                .iter()
                .filter(|piece| piece.owns(left + width / 2.0))
                .collect();
            assert_eq!(
                keepers.len(),
                1,
                "one piece keeps the word at {left}: {pieces:?}"
            );
            let roi = keepers[0].roi;
            assert!(
                roi.x <= left && left + width <= roi.x + roi.width,
                "the word at {left} is whole in the piece that keeps it: {pieces:?}"
            );
            left += 1.0;
        }
    }

    #[test]
    fn lines_read_in_pieces_come_back_whole_with_each_word_once() {
        // Two lines across a slab wider than one piece. Each piece reads the
        // words it sees whole, so a word near a cut comes back from two.
        let rows = [
            typed(
                &[
                    "Every", "archive", "begins", "with", "a", "promise", "that", "the",
                ],
                0.0,
                20.0,
            ),
            typed(
                &[
                    "paper", "will", "outlast", "the", "people", "who", "wrote", "it.",
                ],
                0.0,
                32.0,
            ),
        ];
        let slab = Rect {
            x: 0.0,
            y: 15.0,
            width: 270.0,
            height: 30.0,
        };
        let pieces = pieces(slab, 10.0);
        assert!(pieces.len() > 1, "the slab is cut: {pieces:?}");
        let mut fragments = Vec::new();
        for piece in &pieces {
            for row in &rows {
                let seen = row
                    .words
                    .iter()
                    .filter(|word| {
                        let bounds = word.quad.map_or(row.quad.bounds(), |quad| quad.bounds());
                        piece.roi.x <= bounds.x
                            && bounds.x + bounds.width <= piece.roi.x + piece.roi.width
                    })
                    .cloned()
                    .collect();
                fragments.extend(
                    line_of(seen).and_then(|line| retain_words(line, |at| piece.owns(at.x))),
                );
            }
        }
        let lines = join_fragments(fragments);
        assert_eq!(lines.len(), 2, "one line a row: {lines:?}");
        for (line, row) in lines.iter().zip(&rows) {
            assert_eq!(
                (&line.text, &line.words, line.quad),
                (&row.text, &row.words, row.quad),
                "each line comes back whole, each word once and in order"
            );
        }
    }

    #[test]
    fn a_crop_read_again_adds_only_unread_words_in_reading_order() {
        // The first pass read the lines above and below. The first round
        // read the start and end of the line between them; the second
        // round's crop of its middle reached into both and cut the words at
        // its edges, and a note sits past a gutter on the same row.
        let above = typed(
            &[
                "Clerks", "who", "prepare", "records", "for", "the", "new", "system", "are",
                "asked",
            ],
            0.0,
            0.0,
        );
        let below = typed(
            &[
                "cabinet",
                "until",
                "the",
                "corrected",
                "images",
                "arrive.",
                "Most",
                "batches",
            ],
            0.0,
            40.0,
        );
        let mut lines = vec![above.clone(), below.clone()];
        let mut found = vec![
            typed(&["has", "not", "merged"], 0.0, 20.0),
            typed(&["fails", "any"], 196.0, 20.0),
        ];
        let middle = typed(
            &["ot", "merged", "two", "sheets", "into", "one", "fai"],
            28.0,
            20.0,
        );
        let fresh = retain_words(middle, |at| !read_at(at, lines.iter().chain(&found)));
        found.extend(fresh);
        found.push(typed(&["Note"], 260.0, 20.0));
        add_found(&mut lines, found);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                above.text.as_str(),
                "has not merged two sheets into one fails any",
                "Note",
                below.text.as_str()
            ]
        );
    }

    #[test]
    fn a_line_missed_in_its_first_crop_is_read_in_a_larger_one() {
        // The first pass read the line at the top and skipped a short one
        // below it, as it can a lone figure in a table. Vision misses the
        // short line in the first crop holding it and reads it in a larger
        // one, so the later rounds run although the first found nothing.
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        fill(&mut pixels, 20..200, 20..40);
        for left in (20..128).step_by(16) {
            ring(&mut pixels, left, 80, 12, 20, 4);
        }
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let size = Size {
            width: WIDTH as f64,
            height: HEIGHT as f64,
        };
        let mut lines = vec![read_line(18.0, 18.0, 202.0, 42.0)];
        let mut skipped = read_line(18.0, 78.0, 130.0, 102.0);
        skipped.text = "skipped".to_owned();
        skipped.words = vec![Word {
            text: skipped.text.clone(),
            quad: Some(skipped.quad),
        }];
        let middle = centre(skipped.quad.bounds());
        let area = |rect: Rect| rect.width * rect.height;
        let mut first_crop: Option<Rect> = None;
        read_unread(&bitmap, &mut lines, size, |region| {
            let holds = (region.x..region.x + region.width).contains(&middle.x)
                && (region.y..region.y + region.height).contains(&middle.y);
            Ok(match (holds, first_crop) {
                (false, _) => Vec::new(),
                (true, None) => {
                    first_crop = Some(region);
                    Vec::new()
                }
                (true, Some(first)) if area(region) > area(first) => vec![skipped.clone()],
                (true, Some(_)) => Vec::new(),
            })
        })
        .expect("reading finds the line");
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["read", "skipped"]
        );
    }

    #[test]
    fn a_lone_figure_is_read_in_a_crop_it_fills_little_of() {
        // The first pass read the line at the top and skipped a lone figure
        // below it, as it did a 3 alone in a table cell. Vision missed that 3
        // in most crops under four lines across and down and read it in
        // nearly every larger one, so the stub reads the figure only in a
        // crop four lines (96 px) across and down.
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        fill(&mut pixels, 20..200, 20..40);
        ring(&mut pixels, 250, 80, 12, 20, 3);
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let size = Size {
            width: WIDTH as f64,
            height: HEIGHT as f64,
        };
        let mut lines = vec![read_line(18.0, 18.0, 202.0, 42.0)];
        let mut figure = read_line(248.0, 78.0, 264.0, 102.0);
        figure.text = "3".to_owned();
        figure.words = vec![Word {
            text: figure.text.clone(),
            quad: Some(figure.quad),
        }];
        let middle = centre(figure.quad.bounds());
        read_unread(&bitmap, &mut lines, size, |region| {
            let holds = (region.x..region.x + region.width).contains(&middle.x)
                && (region.y..region.y + region.height).contains(&middle.y);
            Ok(if holds && region.width >= 96.0 && region.height >= 96.0 {
                vec![figure.clone()]
            } else {
                Vec::new()
            })
        })
        .expect("reading finds the figure");
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["read", "3"]
        );
    }

    #[test]
    fn a_box_read_as_a_bullet_is_dropped_and_a_real_bullet_kept() {
        let mut pixels = vec![255u8; WIDTH * HEIGHT];
        // An empty checkbox, a round bullet, a box read as a letter, and a
        // checkbox alone on its line.
        ring(&mut pixels, 20, 20, 40, 40, 4);
        for y in 20..60 {
            for x in 100..140 {
                if (x as f64 - 119.5).hypot(y as f64 - 39.5) <= 20.0 {
                    pixels[y * WIDTH + x] = 0;
                }
            }
        }
        ring(&mut pixels, 180, 20, 40, 40, 4);
        ring(&mut pixels, 20, 100, 40, 40, 4);
        let bitmap = Bitmap {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            stride: WIDTH,
            format: PixelFormat::Gray8,
            data: &pixels,
        };
        let word = |text: &str, x: f64, y: f64| Word {
            text: text.to_owned(),
            quad: Some(read_line(x, y, x + 44.0, y + 44.0).quad),
        };
        let mut lines = vec![
            Line {
                text: "• • O".to_owned(),
                words: vec![
                    word("•", 18.0, 18.0),
                    word("•", 98.0, 18.0),
                    word("O", 178.0, 18.0),
                ],
                ..read_line(18.0, 18.0, 222.0, 62.0)
            },
            Line {
                text: "_".to_owned(),
                words: vec![word("_", 18.0, 98.0)],
                ..read_line(18.0, 98.0, 62.0, 142.0)
            },
        ];
        drop_box_marks(&bitmap, &mut lines);
        assert_eq!(lines.len(), 1, "a line of box marks alone goes: {lines:?}");
        assert_eq!(lines[0].text, "• O");
        assert_eq!(lines[0].words.len(), 2);
    }
}
