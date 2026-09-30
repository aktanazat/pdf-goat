//! Continuous CSS layout with line-safe pagination and table-header repetition.

use std::borrow::Cow;
use std::ops::Range;
use std::rc::Rc;

use goat_common::GoatError;
use unicode_bidi::{BidiInfo, Level};
use unicode_linebreak::{BreakOpportunity, linebreaks};
use unicode_script::{Script, UnicodeScript};

use super::boxes::{Block, Inline, Kind, Link, Marks, SharedStyle};
use super::fonts::{FaceId, Fonts};
use super::image::{ImageId, Images};
use super::position::{FloatArea, Overlay, Positioned};
use super::style::{
    BorderStyle, Break, Color, Decoration, Display, Len, PageStyle, Position, Style, TextAlign,
    Transform, VAlign,
};

pub use super::shaping::Glyph;
use super::shaping::{self, Unicode};

#[derive(Clone)]
pub enum Item {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
        radius: f32,
    },
    Text {
        x: f32,
        y: f32,
        width: f32,
        face: FaceId,
        size: f32,
        color: Color,
        glyphs: Vec<Glyph>,
    },
    Image {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        id: ImageId,
    },
    Link {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        target: Link,
    },
    Anchor {
        x: f32,
        y: f32,
        name: String,
    },
    Bookmark {
        x: f32,
        y: f32,
        level: u8,
        title: String,
    },
}

impl Item {
    pub fn translate(&mut self, dx: f32, dy: f32) {
        let (x, y) = match self {
            Item::Rect { x, y, .. }
            | Item::Text { x, y, .. }
            | Item::Image { x, y, .. }
            | Item::Link { x, y, .. }
            | Item::Anchor { x, y, .. }
            | Item::Bookmark { x, y, .. } => (x, y),
        };
        *x += dx;
        *y += dy;
    }
}

pub struct Unit {
    pub top: f32,
    pub bottom: f32,
    pub start: f32,
    pub forced: bool,
    pub avoid: bool,
    pub items: Vec<Item>,
}

pub struct Deco {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub background: Color,
    pub radius: f32,
    pub border: [f32; 4],
    pub colors: [Color; 4],
    pub styles: [BorderStyle; 4],
}

impl Deco {
    pub fn translate(&mut self, x: f32, y: f32) {
        self.x += x;
        self.y += y;
    }
}

impl Deco {
    pub fn items(&self, top: f32, bottom: f32) -> Vec<Item> {
        let y = self.y.max(top);
        let end = (self.y + self.h).min(bottom);
        if end <= y {
            return Vec::new();
        }
        let mut items = Vec::new();
        if self.background.visible() {
            items.push(Item::Rect {
                x: self.x,
                y,
                w: self.w,
                h: end - y,
                color: self.background,
                radius: self.radius,
            });
        }
        let mut border = self.border;
        if self.y < top {
            border[0] = 0.0;
        }
        if self.y + self.h > bottom {
            border[2] = 0.0;
        }
        borders(
            &mut items,
            [self.x, y, self.w, end - y],
            border,
            self.colors,
            self.styles,
        );
        items
    }
}

pub struct Canvas {
    pub units: Vec<Unit>,
    pub decos: Vec<Deco>,
    pub overlays: Vec<Overlay>,
    pub fixed: Vec<Item>,
    pub repeats: Vec<Repeat>,
    pub background: Color,
    pub height: f32,
}

pub struct Page {
    pub units: Range<usize>,
    pub start: f32,
    pub end: f32,
    pub header: f32,
    pub repeats: Vec<usize>,
}

pub struct Repeat {
    pub top: f32,
    pub body_top: f32,
    pub bottom: f32,
    pub height: f32,
    pub items: Vec<Item>,
}

impl Repeat {
    fn translate(&mut self, x: f32, y: f32) {
        self.top += y;
        self.body_top += y;
        self.bottom += y;
        for item in &mut self.items {
            item.translate(x, y);
        }
    }
}

pub fn paginate(canvas: &Canvas, height: f32) -> Vec<Page> {
    let mut pages = Vec::new();
    let mut start = 0;
    let mut y = 0.0;
    while start < canvas.units.len() {
        let top = canvas.units[start].top;
        let repeats: Vec<usize> = canvas
            .repeats
            .iter()
            .enumerate()
            .filter(|(_, repeat)| {
                top >= repeat.body_top - 0.001
                    && top < repeat.bottom - 0.001
                    && repeat.height < height
            })
            .map(|(index, _)| index)
            .collect();
        let header = repeats
            .iter()
            .map(|&index| canvas.repeats[index].height)
            .sum::<f32>();
        let mut end = start + 1;
        while end < canvas.units.len() {
            let unit = &canvas.units[end];
            if unit.forced {
                break;
            }
            if unit.bottom - y > height - header {
                let mut allowed = end;
                while allowed > start && canvas.units[allowed].avoid {
                    allowed -= 1;
                }
                if allowed > start {
                    end = allowed;
                }
                break;
            }
            end += 1;
        }
        let next_y = canvas
            .units
            .get(end)
            .map_or(canvas.height.max(y + height), |unit| unit.start);
        pages.push(Page {
            units: start..end,
            start: y,
            end: next_y,
            header,
            repeats,
        });
        start = end;
        y = next_y;
    }
    if pages.is_empty() {
        pages.push(Page {
            units: 0..0,
            start: 0.0,
            end: height,
            header: 0.0,
            repeats: Vec::new(),
        });
    }
    pages
}

#[derive(Default)]
pub(super) struct State<'a> {
    pub units: Vec<Unit>,
    pub decos: Vec<Deco>,
    pub floats: Vec<FloatArea>,
    pub positioned: Vec<Positioned<'a>>,
    pub overlays: Vec<Overlay>,
    pub fixed: Vec<Item>,
    pub repeats: Vec<Repeat>,
    frames: Vec<Option<f32>>,
    pending_start: Option<f32>,
    forced: bool,
    avoid: bool,
    inside: Vec<usize>,
    pub pending: Vec<Item>,
    markers: Vec<(String, SharedStyle, f32)>,
    pub first_baseline: Option<f32>,
    pub last_baseline: Option<f32>,
}

#[derive(Default, Clone, Copy)]
pub(super) struct Margin {
    positive: f32,
    negative: f32,
}

impl Margin {
    fn add(&mut self, value: f32) {
        self.positive = self.positive.max(value);
        self.negative = self.negative.min(value);
    }
    pub fn value(self) -> f32 {
        self.positive + self.negative
    }
}

#[derive(Default)]
pub(super) struct Cursor {
    pub y: f32,
    pub margin: Margin,
}

pub(super) struct Engine<'a> {
    pub fonts: &'a mut Fonts,
    pub images: &'a Images,
    pub state: State<'a>,
    pub page: PageStyle,
    pub error: Option<GoatError>,
}

pub fn layout<'a>(
    block: &'a Block,
    fonts: &'a mut Fonts,
    images: &'a Images,
    page: PageStyle,
    background: Color,
) -> Result<Canvas, GoatError> {
    let mut engine = Engine {
        fonts,
        images,
        state: State::default(),
        page,
        error: None,
    };
    let mut cursor = Cursor::default();
    let width = (page.width - page.margin[1] - page.margin[3]).max(1.0);
    engine.block(block, 0.0, width, &mut cursor);
    if !engine.state.pending.is_empty() {
        engine.push_unit(cursor.y, cursor.y, Vec::new());
    }
    let content_height = (page.height - page.margin[0] - page.margin[2]).max(1.0);
    while !engine.state.positioned.is_empty() {
        engine.resolve_positioned(0, [0.0, 0.0, width, content_height], true);
    }
    if let Some(error) = engine.error {
        return Err(error);
    }
    let height = engine
        .state
        .floats
        .iter()
        .map(|area| area.y + area.height)
        .fold(cursor.y + cursor.margin.value(), f32::max);
    Ok(Canvas {
        units: engine.state.units,
        decos: engine.state.decos,
        overlays: engine.state.overlays,
        fixed: engine.state.fixed,
        repeats: engine.state.repeats,
        background,
        height,
    })
}

pub(super) struct SubLayout<'a> {
    pub units: Vec<Unit>,
    pub decos: Vec<Deco>,
    pub overlays: Vec<Overlay>,
    pub fixed: Vec<Item>,
    pub positioned: Vec<Positioned<'a>>,
    pub repeats: Vec<Repeat>,
    pub height: f32,
    pub first_baseline: f32,
    pub last_baseline: f32,
}

impl SubLayout<'_> {
    pub fn translate(&mut self, x: f32, y: f32) {
        for unit in &mut self.units {
            unit.top += y;
            unit.bottom += y;
            unit.start += y;
            for item in &mut unit.items {
                item.translate(x, y);
            }
        }
        for deco in &mut self.decos {
            deco.translate(x, y);
        }
        for overlay in &mut self.overlays {
            overlay.anchor += y;
            if let Some(align) = &mut overlay.align {
                align.bottom += y;
            }
            for item in &mut overlay.items {
                item.translate(x, y);
            }
        }
        for positioned in &mut self.positioned {
            positioned.translate(x, y);
        }
        for repeat in &mut self.repeats {
            repeat.translate(x, y);
        }
        self.first_baseline += y;
        self.last_baseline += y;
    }

    pub fn into_items(self) -> Vec<Item> {
        let mut items = Vec::new();
        for deco in self.decos {
            items.extend(deco.items(f32::NEG_INFINITY, f32::INFINITY));
        }
        for unit in self.units {
            items.extend(unit.items);
        }
        for overlay in self.overlays {
            items.extend(overlay.items);
        }
        items
    }
}

impl<'a> Engine<'a> {
    pub fn resolve(&mut self, cursor: &mut Cursor) {
        cursor.y += cursor.margin.value();
        cursor.margin = Margin::default();
        for top in self.state.frames.iter_mut().rev() {
            if top.is_some() {
                break;
            }
            *top = Some(cursor.y);
        }
    }

    fn note_start(&mut self, y: f32) {
        self.state.pending_start = Some(self.state.pending_start.map_or(y, |start| start.min(y)));
    }

    pub fn push_unit(&mut self, top: f32, bottom: f32, mut items: Vec<Item>) {
        let start = self.state.pending_start.take().unwrap_or(top).min(top);
        let forced = std::mem::take(&mut self.state.forced);
        let avoid = std::mem::take(&mut self.state.avoid)
            || self
                .state
                .inside
                .first()
                .is_some_and(|&first| self.state.units.len() > first);
        if !self.state.pending.is_empty() {
            let mut pending = std::mem::take(&mut self.state.pending);
            pending.append(&mut items);
            items = pending;
        }
        self.state.units.push(Unit {
            top,
            bottom,
            start,
            forced,
            avoid,
            items,
        });
    }

    pub fn block(&mut self, block: &'a Block, x: f32, available: f32, cursor: &mut Cursor) {
        if self.error.is_some() {
            return;
        }
        if matches!(block.style.position, Position::Absolute | Position::Fixed) {
            self.state.positioned.push(Positioned {
                block,
                x,
                y: cursor.y + cursor.margin.value(),
            });
            return;
        }
        self.clear_floats(block.style.clear, cursor);
        if block.style.float.is_some() {
            self.place_float(block, x, available, cursor);
            return;
        }
        self.flow_block(block, x, available, cursor);
    }

    fn flow_block(&mut self, block: &'a Block, x: f32, available: f32, cursor: &mut Cursor) {
        let s = &block.style;
        let geom = match &block.kind {
            Kind::Table(table) => Some(self.table_geometry(table, s, available)),
            _ => None,
        };
        let border = geom.as_ref().map_or(s.border_width, |g| g.border);
        let padding = geom.as_ref().map_or_else(
            || s.padding.map(|len| len.or_zero(available)),
            |g| g.padding,
        );
        let frame = border[1] + border[3] + padding[1] + padding[3];
        let margins = s.margin.map(|len| len.or_zero(available));
        let auto_width = (available - margins[1] - margins[3] - frame).max(0.0);
        let image_size = match block.kind {
            Kind::Image(id) => Some(self.image_size(id, s, available)),
            _ => None,
        };
        let mut width = geom.as_ref().map_or_else(
            || image_size.map_or_else(|| used_width(s, available, auto_width, frame), |(w, _)| w),
            |g| g.width,
        );
        width = width.max(0.0);
        let free = available - width - frame - margins[1] - margins[3];
        let left = margins[3]
            + match (s.margin[3], s.margin[1]) {
                (Len::Auto, Len::Auto) => free.max(0.0) / 2.0,
                (Len::Auto, _) => free.max(0.0),
                _ => 0.0,
            };
        let bx = x + left;
        let cx = bx + border[3] + padding[3];
        let first = self.state.units.len();
        let first_positioned = self.state.positioned.len();
        let first_overlay = self.state.overlays.len();
        let first_repeat = self.state.repeats.len();
        match s.break_before {
            Break::Page => self.state.forced = true,
            Break::Avoid => self.state.avoid = true,
            Break::Auto => {}
        }
        if s.break_before == Break::Page
            || (self.state.forced && self.state.pending_start.is_none())
        {
            cursor.margin = Margin::default();
            self.state.pending_start = Some(cursor.y);
        }
        if s.avoid_break_inside {
            self.state.inside.push(first);
        }
        cursor.margin.add(margins[0]);
        self.state.frames.push(None);
        let frame_index = self.state.frames.len() - 1;
        let deco_index = self.state.decos.len();
        // Collapsed table perimeters belong to the cells; the frame still takes up space.
        let painted_border = if geom.is_some() && s.border_collapse {
            [0.0; 4]
        } else {
            border
        };
        self.state.decos.push(Deco {
            x: bx,
            y: 0.0,
            w: width + frame,
            h: 0.0,
            background: s.background,
            radius: s.radius,
            border: painted_border,
            colors: s.border_color,
            styles: s.border_style,
        });
        let top_edge = border[0] + padding[0];
        if top_edge != 0.0
            || s.float.is_some()
            || matches!(s.position, Position::Absolute | Position::Fixed)
            || matches!(block.kind, Kind::Table(_) | Kind::Image(_))
            || matches!(s.display, Display::Flex | Display::Grid)
        {
            self.resolve(cursor);
            self.note_start(cursor.y);
            cursor.y += top_edge;
        }
        let marker_start = self.state.markers.len();
        if let Some(marker) = &block.marker {
            self.state.markers.push((marker.clone(), Rc::clone(s), cx));
        }
        match &block.kind {
            Kind::Flow(children) => self.flow_children(children, s, cx, width, cursor),
            Kind::Inline(inline) => self.inline(inline, s, cx, width, cursor),
            Kind::Table(table) => {
                if let Some(geom) = geom {
                    self.table(table, cx, geom, cursor);
                }
            }
            Kind::Image(id) => {
                let (_, height) = image_size.unwrap_or((0.0, 0.0));
                let items = vec![Item::Image {
                    x: cx,
                    y: cursor.y,
                    w: width,
                    h: height,
                    id: *id,
                }];
                self.push_unit(cursor.y, cursor.y + height, items);
                cursor.y += height;
            }
        }
        if self.state.markers.len() > marker_start {
            self.resolve(cursor);
            let (ascent, descent) = self.strut(s);
            let items = self.marker_items(cursor.y + ascent);
            self.push_unit(cursor.y, cursor.y + ascent + descent, items);
            cursor.y += ascent + descent;
        }
        let bottom_edge = border[2] + padding[2];
        let specified_height = s.height.resolve(0.0);
        let minimum = s.min_height.or_zero(0.0);
        if bottom_edge != 0.0 || specified_height.is_some() || minimum > 0.0 {
            self.resolve(cursor);
        }
        let top = self.state.frames[frame_index];
        if let Some(top) = top {
            let content_top = top + top_edge;
            let height_frame = top_edge + bottom_edge;
            let height = specified_height.map(|h| {
                if s.border_box {
                    (h - height_frame).max(0.0)
                } else {
                    h
                }
            });
            let min = if s.border_box {
                (minimum - height_frame).max(0.0)
            } else {
                minimum
            };
            let mut used = height.unwrap_or(cursor.y - content_top).max(min);
            if let Some(maximum) = s.max_height.resolve(0.0) {
                used = used.min(maximum.max(min));
            }
            cursor.y = content_top + used;
            cursor.y += bottom_edge;
            let bottom = cursor.y;
            self.state.decos[deco_index].y = top;
            self.state.decos[deco_index].h = (bottom - top).max(0.0);
            if self.state.units.len() == first && bottom > top {
                self.push_unit(top, bottom, Vec::new());
            }
            let mut marks = mark_items(&block.marks, bx, top, width + frame, bottom - top);
            if let Some(unit) = self.state.units.get_mut(first) {
                marks.append(&mut unit.items);
                unit.items = marks;
            } else {
                self.state.pending.extend(marks);
            }
        } else {
            self.state
                .pending
                .extend(mark_items(&block.marks, bx, cursor.y, width + frame, 0.0));
        }
        if s.position != Position::Static {
            let deco = &self.state.decos[deco_index];
            self.resolve_positioned(
                first_positioned,
                [
                    bx + border[3],
                    deco.y + border[0],
                    width + padding[1] + padding[3],
                    (deco.h - border[0] - border[2]).max(0.0),
                ],
                false,
            );
        }
        if s.position == Position::Relative {
            let (dx, dy) = relative_offset(s, available);
            for unit in &mut self.state.units[first..] {
                for item in &mut unit.items {
                    item.translate(dx, dy);
                }
            }
            for deco in &mut self.state.decos[deco_index..] {
                deco.translate(dx, dy);
            }
            for overlay in &mut self.state.overlays[first_overlay..] {
                for item in &mut overlay.items {
                    item.translate(dx, dy);
                }
            }
            for positioned in &mut self.state.positioned[first_positioned..] {
                positioned.translate(dx, dy);
            }
            for repeat in &mut self.state.repeats[first_repeat..] {
                repeat.translate(dx, dy);
            }
        }
        self.state.frames.pop();
        if s.avoid_break_inside {
            self.state.inside.pop();
        }
        cursor.margin.add(margins[2]);
        match s.break_after {
            Break::Page => self.state.forced = true,
            Break::Avoid => self.state.avoid = true,
            Break::Auto => {}
        }
    }

    fn flow_children(
        &mut self,
        children: &'a [Block],
        style: &Style,
        x: f32,
        width: f32,
        cursor: &mut Cursor,
    ) {
        if matches!(style.display, Display::Flex | Display::Grid) {
            if let Err(error) = self.formatting(children, style, x, width, cursor) {
                self.error = Some(GoatError::message(error.to_string()));
            }
        } else {
            for child in children {
                self.block(child, x, width, cursor);
            }
        }
    }

    pub fn sub_content(&mut self, block: &'a Block, width: f32) -> SubLayout<'a> {
        let saved = std::mem::take(&mut self.state);
        let mut cursor = Cursor::default();
        match &block.kind {
            Kind::Flow(children) => {
                self.flow_children(children, &block.style, 0.0, width, &mut cursor)
            }
            Kind::Inline(inline) => self.inline(inline, &block.style, 0.0, width, &mut cursor),
            Kind::Image(id) => {
                let (w, h) = self.image_size(*id, &block.style, width);
                self.push_unit(
                    0.0,
                    h,
                    vec![Item::Image {
                        x: 0.0,
                        y: 0.0,
                        w,
                        h,
                        id: *id,
                    }],
                );
                cursor.y = h;
            }
            Kind::Table(table) => {
                let geom = self.table_geometry(table, &block.style, width);
                self.table(table, 0.0, geom, &mut cursor);
            }
        }
        if block.style.position != Position::Static {
            let padding = block.style.padding.map(|value| value.or_zero(width));
            self.resolve_positioned(
                0,
                [
                    -padding[3],
                    -padding[0],
                    width + padding[1] + padding[3],
                    cursor.y + cursor.margin.value() + padding[0] + padding[2],
                ],
                false,
            );
        }
        self.finish_sub(saved, cursor.y + cursor.margin.value())
    }

    pub fn sub_flow_block(&mut self, block: &'a Block, width: f32) -> SubLayout<'a> {
        let saved = std::mem::take(&mut self.state);
        let mut cursor = Cursor::default();
        self.flow_block(block, 0.0, width, &mut cursor);
        self.finish_sub(saved, cursor.y + cursor.margin.value())
    }

    fn finish_sub(&mut self, saved: State<'a>, height: f32) -> SubLayout<'a> {
        if !self.state.pending.is_empty() {
            self.push_unit(height, height, Vec::new());
        }
        let state = std::mem::replace(&mut self.state, saved);
        let height = state
            .floats
            .iter()
            .map(|area| area.y + area.height)
            .fold(height, f32::max);
        SubLayout {
            units: state.units,
            decos: state.decos,
            overlays: state.overlays,
            fixed: state.fixed,
            positioned: state.positioned,
            repeats: state.repeats,
            height,
            first_baseline: state.first_baseline.unwrap_or(height),
            last_baseline: state.last_baseline.unwrap_or(height),
        }
    }

    pub fn absorb(&mut self, mut sub: SubLayout<'a>, units: &mut Vec<Unit>) {
        self.state.first_baseline.get_or_insert(sub.first_baseline);
        self.state.last_baseline = Some(sub.last_baseline);
        units.append(&mut sub.units);
        self.state.decos.append(&mut sub.decos);
        self.state.overlays.append(&mut sub.overlays);
        self.state.fixed.append(&mut sub.fixed);
        self.state.positioned.append(&mut sub.positioned);
        self.state.repeats.append(&mut sub.repeats);
    }

    pub fn merge_units(&mut self, mut units: Vec<Unit>) {
        units.sort_by(|left, right| left.top.total_cmp(&right.top));
        let mut merged: Vec<Unit> = Vec::with_capacity(units.len());
        for mut unit in units {
            if let Some(previous) = merged.last_mut()
                && unit.top < previous.bottom - 0.001
            {
                previous.bottom = previous.bottom.max(unit.bottom);
                previous.start = previous.start.min(unit.start);
                previous.forced |= unit.forced;
                previous.avoid |= unit.avoid;
                previous.items.append(&mut unit.items);
            } else {
                merged.push(unit);
            }
        }
        for unit in merged {
            self.state.forced |= unit.forced;
            self.state.avoid |= unit.avoid;
            self.note_start(unit.start);
            self.push_unit(unit.top, unit.bottom, unit.items);
        }
    }

    pub fn image_size(&self, id: ImageId, s: &Style, available: f32) -> (f32, f32) {
        let image = &self.images.list[id];
        let ratio = image.intrinsic.0 / image.intrinsic.1.max(1.0);
        let w = s.width.resolve(available);
        let h = s.height.resolve(0.0);
        let mut width = w.unwrap_or_else(|| h.map_or(image.intrinsic.0, |h| h * ratio));
        let mut height = h.unwrap_or(width / ratio);
        if let Some(maximum) = s.max_width.resolve(available)
            && width > maximum
        {
            width = maximum;
            if h.is_none() {
                height = width / ratio;
            }
        }
        let minimum = s.min_width.or_zero(available);
        if width < minimum {
            width = minimum;
            if h.is_none() {
                height = width / ratio;
            }
        }
        if let Some(maximum) = s.max_height.resolve(0.0)
            && height > maximum
        {
            height = maximum;
            if w.is_none() {
                width = height * ratio;
            }
        }
        (width.max(0.0), height.max(0.0))
    }

    fn strut(&mut self, style: &Style) -> (f32, f32) {
        let face = self.fonts.primary(style.family, style.weight, style.italic);
        self.extents(style, face)
    }

    fn extents(&self, style: &Style, face: Option<FaceId>) -> (f32, f32) {
        let (ascent, descent) = face.map_or((0.8, 0.2), |id| {
            (self.fonts.faces[id].ascent, self.fonts.faces[id].descent)
        });
        let a = ascent * style.font_size;
        let d = descent * style.font_size;
        let leading = (style.used_line_height(a + d) - a - d) / 2.0;
        (a + leading, d + leading)
    }

    fn marker_items(&mut self, baseline: f32) -> Vec<Item> {
        let mut items = Vec::new();
        for (text, style, right) in std::mem::take(&mut self.state.markers) {
            let mut shaped = Vec::new();
            let mut width = 0.0;
            for ch in text.chars() {
                if let Some(face) =
                    self.fonts
                        .face_for(style.family, style.weight, style.italic, ch)
                {
                    let f = &self.fonts.faces[face];
                    let gid = f.font.glyph_for_char(ch).unwrap_or(0);
                    let advance = f.advance_em(gid) * style.font_size;
                    shaped.push((
                        face,
                        Glyph {
                            gid,
                            unicode: Unicode::Scalar(ch),
                            advance,
                            x_offset: 0.0,
                            y_offset: 0.0,
                        },
                    ));
                    width += advance;
                }
            }
            let mut x = right - width;
            for (face, glyph) in shaped {
                let advance = glyph.advance;
                append_glyph(&mut items, [x, baseline], face, &style, glyph);
                x += advance;
            }
        }
        items
    }

    pub fn intrinsic(&mut self, block: &Block) -> (f32, f32) {
        let (mut min, mut max) = self.intrinsic_content(block);
        let s = &block.style;
        if let Len::Px(width) = s.width {
            min = width;
            max = width;
        }
        if let Len::Px(width) = s.max_width {
            min = min.min(width);
            max = max.min(width);
        }
        let frame = s.frame(1, 0.0) + s.frame(3, 0.0);
        (min + frame, max + frame)
    }

    pub fn intrinsic_content(&mut self, block: &Block) -> (f32, f32) {
        match &block.kind {
            Kind::Image(id) => {
                let (w, _) = self.image_size(*id, &block.style, 1.0e6);
                (w, w)
            }
            Kind::Table(table) => self.table_intrinsic(table, &block.style),
            Kind::Flow(children) => {
                let (mut min, mut max) = (0.0f32, 0.0f32);
                for child in children {
                    let (a, b) = self.intrinsic(child);
                    min = min.max(a);
                    max = max.max(b);
                }
                (min, max)
            }
            Kind::Inline(inline) => {
                let prepared = self.prepare(inline, 0.0, block.style.rtl);
                let mut sizes = Vec::with_capacity(prepared.atoms.len());
                for atom in &prepared.atoms {
                    sizes.push(match atom {
                        Atom::Image(id, s) => {
                            let (w, _) = self.image_size(*id, s, 1.0e6);
                            (w, w)
                        }
                        Atom::Block(block) => self.intrinsic(block),
                    });
                }
                let breaks = prepared.breaks();
                let (mut min, mut max, mut line) = (0.0f32, 0.0f32, 0.0f32);
                let mut previous = 0;
                for (end, mandatory) in breaks {
                    let visible = prepared.trim_end(previous, end);
                    let mut small = 0.0;
                    let mut large = 0.0;
                    for ch in &prepared.chars[previous..visible] {
                        let (a, b) = ch.atom.map_or((ch.advance, ch.advance), |id| sizes[id]);
                        small += a + ch.before + ch.after;
                        large += b + ch.before + ch.after;
                    }
                    min = min.max(small);
                    // Spaces between words remain in max-content width.
                    for ch in &prepared.chars[visible..end] {
                        large += ch.advance + ch.before + ch.after;
                    }
                    line += large;
                    if mandatory {
                        max = max.max(line);
                        line = 0.0;
                    }
                    previous = end;
                }
                (min, max.max(line))
            }
        }
    }

    fn prepare<'b>(&mut self, inline: &'b [Inline], width: f32, rtl: bool) -> Prepared<'b> {
        let mut p = Prepared {
            chars: Vec::new(),
            styles: Vec::new(),
            boxes: Vec::new(),
            atoms: Vec::new(),
            glyphs: Vec::new(),
        };
        let mut stack: Vec<usize> = Vec::new();
        for item in inline {
            match item {
                Inline::Open(s, marks) => {
                    let shift = stack.last().map_or(0.0, |&id| p.boxes[id].shift) + raise(s);
                    let inherited = stack.last().map_or((0.0, 0.0), |&id| p.boxes[id].offset);
                    let own = relative_offset(s, width);
                    let offset = (inherited.0 + own.0, inherited.1 + own.1);
                    let id = p.boxes.len();
                    p.boxes.push(Span {
                        start: p.chars.len(),
                        end: p.chars.len(),
                        style: s,
                        marks,
                        shift,
                        offset,
                    });
                    stack.push(id);
                }
                Inline::Close => {
                    if let Some(id) = stack.pop() {
                        p.boxes[id].end = p.chars.len();
                    }
                }
                Inline::Text(text, s) => {
                    let style = p.styles.len();
                    p.styles.push(s);
                    let shift = stack.last().map_or(0.0, |&id| p.boxes[id].shift);
                    let offset = stack.last().map_or((0.0, 0.0), |&id| p.boxes[id].offset);
                    let text = transform(text, s.transform);
                    let mut column = 0;
                    for ch in text.chars() {
                        if matches!(ch, '\u{00ad}' | '\r') {
                            continue;
                        }
                        if ch == '\t' && !s.white_space.collapses() {
                            let count = 8 - column % 8;
                            for _ in 0..count {
                                self.character(&mut p, ' ', style, shift, false, offset);
                            }
                            column += count;
                            continue;
                        }
                        let ch = if ch.is_ascii_whitespace()
                            && !(ch == '\n' && s.white_space.keeps_newlines())
                        {
                            ' '
                        } else {
                            ch
                        };
                        let collapsible = ch == ' ' && s.white_space.collapses();
                        if collapsible && p.chars.last().is_some_and(|c| c.collapsible) {
                            continue;
                        }
                        self.character(&mut p, ch, style, shift, collapsible, offset);
                        if ch == '\n' {
                            column = 0;
                        } else {
                            column += 1;
                        }
                    }
                }
                Inline::Break(s) => {
                    let style = p.styles.len();
                    p.styles.push(s);
                    self.character(&mut p, '\n', style, 0.0, false, (0.0, 0.0));
                }
                Inline::Image(id, s) => {
                    let atom = p.atoms.len();
                    p.atoms.push(Atom::Image(*id, s));
                    let style = p.styles.len();
                    p.styles.push(s);
                    let shift = stack.last().map_or(0.0, |&id| p.boxes[id].shift);
                    let offset = stack.last().map_or((0.0, 0.0), |&id| p.boxes[id].offset);
                    p.chars.push(Character {
                        ch: '\u{fffc}',
                        style,
                        face: None,
                        glyphs: 0..0,
                        level: Level::ltr(),
                        advance: 0.0,
                        before: 0.0,
                        after: 0.0,
                        shift,
                        offset,
                        collapsible: false,
                        atom: Some(atom),
                    });
                }
                Inline::Block(block) => {
                    let atom = p.atoms.len();
                    p.atoms.push(Atom::Block(block));
                    let style = p.styles.len();
                    p.styles.push(&block.style);
                    let shift =
                        stack.last().map_or(0.0, |&id| p.boxes[id].shift) + raise(&block.style);
                    let offset = stack.last().map_or((0.0, 0.0), |&id| p.boxes[id].offset);
                    p.chars.push(Character {
                        ch: '\u{fffc}',
                        style,
                        face: None,
                        glyphs: 0..0,
                        level: Level::ltr(),
                        advance: 0.0,
                        before: 0.0,
                        after: 0.0,
                        shift,
                        offset,
                        collapsible: false,
                        atom: Some(atom),
                    });
                }
            }
        }
        for span in &p.boxes {
            if span.end > span.start {
                p.chars[span.start].before += span.style.frame(3, width);
                p.chars[span.end - 1].after += span.style.frame(1, width);
            }
        }
        p.shape(self.fonts, rtl);
        p
    }

    fn character(
        &mut self,
        p: &mut Prepared<'_>,
        ch: char,
        style: usize,
        shift: f32,
        collapsible: bool,
        offset: (f32, f32),
    ) {
        let s = p.styles[style];
        let face = if matches!(ch, '\n' | '\u{200b}') {
            None
        } else {
            self.fonts.face_for(s.family, s.weight, s.italic, ch)
        };
        p.chars.push(Character {
            ch,
            style,
            face,
            glyphs: 0..0,
            level: Level::ltr(),
            advance: 0.0,
            before: 0.0,
            after: 0.0,
            shift,
            offset,
            collapsible,
            atom: None,
        });
    }

    fn atoms(&mut self, p: &Prepared<'a>, available: f32) -> Vec<AtomBox<'a>> {
        let mut out = Vec::with_capacity(p.atoms.len());
        for atom in &p.atoms {
            out.push(match atom {
                Atom::Image(id, style) => {
                    let (w, h) = self.image_size(*id, style, available);
                    let left = style.frame(3, available);
                    let right = style.frame(1, available);
                    let top = style.frame(0, available);
                    let bottom = style.frame(2, available);
                    AtomBox {
                        w: left + w + right,
                        h: top + h + bottom,
                        baseline: top + h + bottom,
                        items: vec![Item::Image {
                            x: left,
                            y: top,
                            w,
                            h,
                            id: *id,
                        }],
                        positioned: Vec::new(),
                        fixed: Vec::new(),
                    }
                }
                Atom::Block(block) => {
                    let (min, max) = self.intrinsic(block);
                    let width = max.min(available).max(min);
                    let mut sub = self.sub_flow_block(block, width);
                    let positioned = std::mem::take(&mut sub.positioned);
                    let fixed = std::mem::take(&mut sub.fixed);
                    AtomBox {
                        w: width,
                        h: sub.height,
                        baseline: sub.last_baseline,
                        items: sub.into_items(),
                        positioned,
                        fixed,
                    }
                }
            });
        }
        out
    }

    fn inline(
        &mut self,
        inline: &'a [Inline],
        style: &Style,
        x: f32,
        width: f32,
        cursor: &mut Cursor,
    ) {
        let mut p = self.prepare(inline, width, style.rtl);
        if p.chars.is_empty() {
            return;
        }
        let atoms = self.atoms(&p, width);
        for ch in &mut p.chars {
            if let Some(id) = ch.atom {
                ch.advance = atoms[id].w;
            }
        }
        let breaks = p.breaks();
        let first_unit = self.state.units.len();
        let (above, below) = self.strut(style);
        let mut start = 0;
        let mut line_index = 0;
        self.resolve(cursor);
        while start < p.chars.len() {
            while start < p.chars.len() && p.chars[start].collapsible {
                start += 1;
            }
            if start == p.chars.len() {
                break;
            }
            let indent = if line_index == 0 {
                style.text_indent.or_zero(width)
            } else {
                0.0
            };
            let (left, right, next_float) =
                self.float_space(x, width, cursor.y, (above + below).max(0.001));
            let available = (right - left).max(0.0);
            let line = p.line(start, (available - indent).max(0.0), &breaks);
            if p.width(start, p.trim_end(start, line.end)) > available
                && available < width
                && next_float.is_finite()
            {
                cursor.y = next_float;
                continue;
            }
            let context = LineContext {
                p: &p,
                atoms: &atoms,
                style,
                x: left,
                width: available,
                indent,
                last: line.end == p.chars.len(),
            };
            self.line(&line, context, cursor);
            start = line.end;
            line_index += 1;
        }
        let count = self.state.units.len() - first_unit;
        for index in 1..count {
            if index < style.orphans as usize || count - index < style.widows as usize {
                self.state.units[first_unit + index].avoid = true;
            }
        }
    }

    fn line(&mut self, line: &Line, context: LineContext<'_, 'a>, cursor: &mut Cursor) {
        let LineContext {
            p,
            atoms,
            style,
            x,
            width,
            indent,
            last,
        } = context;
        let start = line.start;
        let end = p.trim_end(start, line.end);
        if start == end && !p.chars[start..line.end].iter().any(|c| c.ch == '\n') {
            return;
        }
        let measured = p.width(start, end);
        let free = (width - indent - measured).max(0.0);
        let spaces = p.chars[start..end].iter().filter(|ch| ch.ch == ' ').count();
        let extra = if style.text_align == TextAlign::Justify && !last && !line.hard && spaces > 0 {
            free / spaces as f32
        } else {
            0.0
        };
        let offset = match style.text_align {
            TextAlign::Right => free,
            TextAlign::Start if style.rtl => free,
            TextAlign::End if !style.rtl => free,
            TextAlign::Center => free / 2.0,
            _ => 0.0,
        };
        let levels: Vec<Level> = p.chars[start..end].iter().map(|ch| ch.level).collect();
        let visual = BidiInfo::reorder_visual(&levels);
        let mut positions = vec![(0.0, 0.0); end - start];
        let mut px = x + indent + offset;
        for &index in &visual {
            let ch = &p.chars[start + index];
            let right =
                px + ch.before + ch.advance + ch.after + if ch.ch == ' ' { extra } else { 0.0 };
            positions[index] = (px, right);
            px = right;
        }
        let (mut above, mut below) = self.strut(style);
        for ch in &p.chars[start..end] {
            let (a, d) = ch.atom.map_or_else(
                || self.extents(p.styles[ch.style], ch.face),
                |id| (atoms[id].baseline, atoms[id].h - atoms[id].baseline),
            );
            above = above.max(a + ch.shift);
            below = below.max(d - ch.shift);
        }
        for span in &p.boxes {
            if span.start < end && span.end > start {
                let (a, d) = self.strut(span.style);
                above = above.max(a + span.shift);
                below = below.max(d - span.shift);
            }
        }
        self.resolve(cursor);
        let y = cursor.y;
        let baseline = y + above;
        let height = (above + below).max(0.0);
        self.state.first_baseline.get_or_insert(baseline);
        self.state.last_baseline = Some(baseline);
        let mut items = self.marker_items(baseline);
        for span in &p.boxes {
            if span.end <= start || span.start >= end {
                continue;
            }
            let s = span.style;
            let begin = span.start.max(start);
            let finish = span.end.min(end);
            let first = span.start >= start;
            let final_piece = span.end <= end;
            let left_margin = if first {
                s.margin[3].or_zero(width)
            } else {
                0.0
            };
            let right_margin = if final_piece {
                s.margin[1].or_zero(width)
            } else {
                0.0
            };
            let (left, right) = span_bounds(&positions[begin - start..finish - start]);
            let (left, right) = (
                left + left_margin + span.offset.0,
                right - right_margin + span.offset.0,
            );
            let face = self.fonts.primary(s.family, s.weight, s.italic);
            let (ascent, descent) = face.map_or((0.8, 0.2), |id| {
                (self.fonts.faces[id].ascent, self.fonts.faces[id].descent)
            });
            let content_top = baseline - span.shift + span.offset.1
                - (ascent - descent + 1.0) * s.font_size / 2.0;
            let top_padding = s.padding[0].or_zero(width) + s.border_width[0];
            let bottom_padding = s.padding[2].or_zero(width) + s.border_width[2];
            let mut border = s.border_width;
            if !first {
                border[3] = 0.0;
            }
            if !final_piece {
                border[1] = 0.0;
            }
            let deco = Deco {
                x: left,
                y: content_top - top_padding,
                w: right - left,
                h: s.font_size + top_padding + bottom_padding,
                background: s.background,
                radius: s.radius,
                border,
                colors: s.border_color,
                styles: s.border_style,
            };
            if s.visible {
                items.extend(deco.items(f32::NEG_INFINITY, f32::INFINITY));
            }
            let (a, d) = self.extents(s, face);
            let mut marks = span.marks.clone();
            if !first {
                marks.anchor = None;
                marks.bookmark = None;
            }
            items.extend(mark_items(
                &marks,
                left,
                baseline - span.shift + span.offset.1 - a,
                right - left,
                a + d,
            ));
        }
        for &index in &visual {
            let ch = &p.chars[start + index];
            let s = p.styles[ch.style];
            if !s.visible {
                continue;
            }
            let cx = positions[index].0 + ch.before + ch.offset.0;
            if let Some(id) = ch.atom {
                let atom = &atoms[id];
                for positioned in &atom.positioned {
                    let mut positioned = *positioned;
                    positioned.translate(cx, baseline - ch.shift + ch.offset.1 - atom.baseline);
                    self.state.positioned.push(positioned);
                }
                self.state.fixed.extend(atom.fixed.iter().cloned());
                for item in &atom.items {
                    let mut item = item.clone();
                    item.translate(cx, baseline - ch.shift + ch.offset.1 - atom.baseline);
                    items.push(item);
                }
                continue;
            }
            let Some(face) = ch.face else { continue };
            let mut gx = cx;
            for (index, glyph) in p.glyphs[ch.glyphs.clone()].iter().enumerate() {
                let mut glyph = glyph.clone();
                if index == 0 && ch.ch == ' ' {
                    glyph.advance += extra;
                }
                let advance = glyph.advance;
                append_glyph(
                    &mut items,
                    [gx, baseline - ch.shift + ch.offset.1],
                    face,
                    s,
                    glyph,
                );
                gx += advance;
            }
        }
        // Decorations propagate over descendants without becoming inherited CSS values.
        decoration_items(
            &mut items,
            style.decoration,
            style.decoration_color.unwrap_or(style.color),
            [x + indent + offset, baseline, measured, style.font_size],
        );
        for span in &p.boxes {
            if span.start >= end || span.end <= start {
                continue;
            }
            let (left, right) =
                span_bounds(&positions[span.start.max(start) - start..span.end.min(end) - start]);
            decoration_items(
                &mut items,
                span.style.decoration,
                span.style.decoration_color.unwrap_or(span.style.color),
                [
                    left + span.offset.0,
                    baseline - span.shift + span.offset.1,
                    right - left,
                    span.style.font_size,
                ],
            );
        }
        self.push_unit(y, y + height, items);
        cursor.y += height;
    }
}

pub(super) fn used_width(s: &Style, available: f32, auto: f32, frame: f32) -> f32 {
    let adjust = |width: f32| {
        if s.border_box {
            (width - frame).max(0.0)
        } else {
            width
        }
    };
    let min = adjust(s.min_width.or_zero(available));
    let mut width = s.width.resolve(available).map_or(auto, adjust);
    if let Some(max) = s.max_width.resolve(available) {
        width = width.min(adjust(max));
    }
    width.max(min)
}

fn transform(text: &str, mode: Transform) -> Cow<'_, str> {
    match mode {
        Transform::None => Cow::Borrowed(text),
        Transform::Upper => Cow::Owned(text.to_uppercase()),
        Transform::Lower => Cow::Owned(text.to_lowercase()),
        Transform::Capitalize => {
            let mut out = String::new();
            let mut word_start = true;
            for ch in text.chars() {
                if word_start {
                    out.extend(ch.to_uppercase());
                } else {
                    out.push(ch);
                }
                word_start = ch.is_whitespace();
            }
            Cow::Owned(out)
        }
    }
}

fn raise(style: &Style) -> f32 {
    match style.vertical_align {
        VAlign::Px(px) => px,
        VAlign::Super => style.font_size * 0.5,
        VAlign::Sub => -style.font_size * 0.5,
        _ => 0.0,
    }
}

fn relative_offset(style: &Style, width: f32) -> (f32, f32) {
    if style.position != Position::Relative {
        return (0.0, 0.0);
    }
    (
        style.inset[3]
            .resolve(width)
            .unwrap_or_else(|| -style.inset[1].or_zero(width)),
        style.inset[0]
            .resolve(0.0)
            .unwrap_or_else(|| -style.inset[2].or_zero(0.0)),
    )
}

struct Character {
    ch: char,
    style: usize,
    face: Option<FaceId>,
    glyphs: Range<usize>,
    level: Level,
    advance: f32,
    before: f32,
    after: f32,
    shift: f32,
    offset: (f32, f32),
    collapsible: bool,
    atom: Option<usize>,
}

struct Span<'a> {
    start: usize,
    end: usize,
    style: &'a Style,
    marks: &'a Marks,
    shift: f32,
    offset: (f32, f32),
}

enum Atom<'a> {
    Image(ImageId, &'a Style),
    Block(&'a Block),
}
struct AtomBox<'a> {
    w: f32,
    h: f32,
    baseline: f32,
    items: Vec<Item>,
    positioned: Vec<Positioned<'a>>,
    fixed: Vec<Item>,
}
struct Prepared<'a> {
    chars: Vec<Character>,
    styles: Vec<&'a Style>,
    boxes: Vec<Span<'a>>,
    atoms: Vec<Atom<'a>>,
    glyphs: Vec<Glyph>,
}
struct Line {
    start: usize,
    end: usize,
    hard: bool,
}
struct LineContext<'a, 'b> {
    p: &'a Prepared<'b>,
    atoms: &'a [AtomBox<'b>],
    style: &'a Style,
    x: f32,
    width: f32,
    indent: f32,
    last: bool,
}

impl Prepared<'_> {
    fn shape(&mut self, fonts: &Fonts, rtl: bool) {
        let mut text = String::new();
        let mut bytes = Vec::with_capacity(self.chars.len() + 1);
        for ch in &self.chars {
            bytes.push(text.len());
            text.push(ch.ch);
        }
        bytes.push(text.len());
        let bidi = BidiInfo::new(&text, Some(if rtl { Level::rtl() } else { Level::ltr() }));
        for (index, ch) in self.chars.iter_mut().enumerate() {
            ch.level = bidi.levels[bytes[index]];
        }
        let mut start = 0;
        while start < self.chars.len() {
            let first = &self.chars[start];
            let Some(face) = first.face else {
                start += 1;
                continue;
            };
            let mut script = first.ch.script();
            let mut end = start + 1;
            while end < self.chars.len() {
                let next = &self.chars[end];
                if next.face != Some(face) || next.style != first.style || next.level != first.level
                {
                    break;
                }
                let next_script = next.ch.script();
                if !matches!(next_script, Script::Common | Script::Inherited) {
                    if !matches!(script, Script::Common | Script::Inherited)
                        && script != next_script
                    {
                        break;
                    }
                    script = next_script;
                }
                end += 1;
            }
            let s = self.styles[first.style];
            let run = &text[bytes[start]..bytes[end]];
            let shaped = shaping::shape(
                &fonts.faces[face],
                run,
                first.level.is_rtl(),
                s.font_size,
                s.letter_spacing,
                s.word_spacing,
            );
            for shaped in shaped {
                let byte = bytes[start] + shaped.cluster;
                let Ok(index) = bytes.binary_search(&byte) else {
                    continue;
                };
                let ch = &mut self.chars[index];
                if ch.glyphs.is_empty() {
                    ch.glyphs.start = self.glyphs.len();
                }
                ch.advance += shaped.glyph.advance;
                self.glyphs.push(shaped.glyph);
                ch.glyphs.end = self.glyphs.len();
            }
            start = end;
        }
    }
    fn trim_end(&self, start: usize, mut end: usize) -> usize {
        while end > start && (self.chars[end - 1].collapsible || self.chars[end - 1].ch == '\n') {
            end -= 1;
        }
        end
    }
    fn width(&self, start: usize, end: usize) -> f32 {
        self.chars[start..end]
            .iter()
            .map(|ch| ch.before + ch.advance + ch.after)
            .sum()
    }
    fn breaks(&self) -> Vec<(usize, bool)> {
        let mut text = String::new();
        let mut ends = Vec::with_capacity(self.chars.len());
        for ch in &self.chars {
            text.push(ch.ch);
            ends.push(text.len());
        }
        linebreaks(&text)
            .filter_map(|(byte, opportunity)| {
                let end = ends.binary_search(&byte).ok()? + 1;
                let hard = opportunity == BreakOpportunity::Mandatory;
                (hard || self.styles[self.chars[end - 1].style].white_space.wraps())
                    .then_some((end, hard))
            })
            .collect()
    }
    fn line(&self, start: usize, available: f32, breaks: &[(usize, bool)]) -> Line {
        let break_index = breaks.partition_point(|&(end, _)| end <= start);
        let mut chosen = (self.chars.len(), true);
        let mut previous = None;
        for &(end, hard) in &breaks[break_index..] {
            let visible = self.trim_end(start, end);
            if self.width(start, visible) > available {
                if let Some(prior) = previous {
                    chosen = prior;
                } else if self.styles[self.chars[start].style].break_words {
                    let mut split = start + 1;
                    while split < visible && self.width(start, split + 1) <= available {
                        split += 1;
                    }
                    while split < visible && self.chars[split].glyphs.is_empty() {
                        split += 1;
                    }
                    chosen = (split, false);
                } else {
                    chosen = (end, hard);
                }
                break;
            }
            chosen = (end, hard);
            previous = Some((end, hard));
            if hard {
                break;
            }
        }
        Line {
            start,
            end: chosen.0,
            hard: chosen.1,
        }
    }
}

fn span_bounds(positions: &[(f32, f32)]) -> (f32, f32) {
    positions.iter().fold(
        (f32::INFINITY, f32::NEG_INFINITY),
        |(left, right), &(x0, x1)| (left.min(x0), right.max(x1)),
    )
}

fn append_glyph(items: &mut Vec<Item>, pos: [f32; 2], face: FaceId, style: &Style, glyph: Glyph) {
    if let Some(Item::Text {
        x,
        y,
        width,
        face: last_face,
        size,
        color,
        glyphs,
    }) = items.last_mut()
        && *last_face == face
        && *size == style.font_size
        && *color == style.color
        && (*y - pos[1]).abs() < 0.001
        && (*x + *width - pos[0]).abs() < 0.001
    {
        *width += glyph.advance;
        glyphs.push(glyph);
    } else {
        items.push(Item::Text {
            x: pos[0],
            y: pos[1],
            width: glyph.advance,
            face,
            size: style.font_size,
            color: style.color,
            glyphs: vec![glyph],
        });
    }
}

pub(super) fn mark_items(marks: &Marks, x: f32, y: f32, w: f32, h: f32) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(name) = &marks.anchor {
        items.push(Item::Anchor {
            x,
            y,
            name: name.clone(),
        });
    }
    if let Some((level, title)) = &marks.bookmark {
        items.push(Item::Bookmark {
            x,
            y,
            level: *level,
            title: title.clone(),
        });
    }
    if let Some(target) = &marks.link {
        items.push(Item::Link {
            x,
            y,
            w,
            h,
            target: target.clone(),
        });
    }
    items
}

fn decoration_items(items: &mut Vec<Item>, decoration: Decoration, color: Color, dims: [f32; 4]) {
    let [x, baseline, width, size] = dims;
    for (draw, offset) in [
        (decoration.underline, 0.09),
        (decoration.overline, -0.8),
        (decoration.line_through, -0.3),
    ] {
        if draw {
            items.push(Item::Rect {
                x,
                y: baseline + offset * size,
                w: width,
                h: (size / 18.0).max(0.5),
                color,
                radius: 0.0,
            });
        }
    }
}

pub(super) fn borders(
    items: &mut Vec<Item>,
    rect: [f32; 4],
    widths: [f32; 4],
    colors: [Color; 4],
    styles: [BorderStyle; 4],
) {
    let [x, y, w, h] = rect;
    for side in 0..4 {
        let size = widths[side];
        if size <= 0.0 || styles[side] == BorderStyle::None || !colors[side].visible() {
            continue;
        }
        let (bx, by, bw, bh) = match side {
            0 => (x, y, w, size),
            1 => (x + w - size, y, size, h),
            2 => (x, y + h - size, w, size),
            _ => (x, y, size, h),
        };
        let horizontal = side.is_multiple_of(2);
        let length = if horizontal { bw } else { bh };
        match styles[side] {
            BorderStyle::Double => {
                for offset in [0.0, size * 2.0 / 3.0] {
                    items.push(Item::Rect {
                        x: bx + if horizontal { 0.0 } else { offset },
                        y: by + if horizontal { offset } else { 0.0 },
                        w: if horizontal { bw } else { size / 3.0 },
                        h: if horizontal { size / 3.0 } else { bh },
                        color: colors[side],
                        radius: 0.0,
                    });
                }
            }
            BorderStyle::Dashed | BorderStyle::Dotted => {
                let dash = if styles[side] == BorderStyle::Dashed {
                    size * 3.0
                } else {
                    size
                };
                let count = ((length / (dash * 2.0)).ceil() as usize).min(10000);
                for index in 0..count {
                    let offset = index as f32 * dash * 2.0;
                    let segment = dash.min(length - offset).max(0.0);
                    items.push(Item::Rect {
                        x: bx + if horizontal { offset } else { 0.0 },
                        y: by + if horizontal { 0.0 } else { offset },
                        w: if horizontal { segment } else { size },
                        h: if horizontal { size } else { segment },
                        color: colors[side],
                        radius: 0.0,
                    });
                }
            }
            _ => items.push(Item::Rect {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                color: colors[side],
                radius: 0.0,
            }),
        }
    }
}
