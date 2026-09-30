//! Float exclusions and deferred absolute/fixed positioning in containing blocks.

use super::boxes::Block;
use super::layout::{Cursor, Engine, Item, Margin, SubLayout};
use super::style::{Len, Position};

#[derive(Clone)]
pub struct Overlay {
    pub anchor: f32,
    pub items: Vec<Item>,
    pub align: Option<FragmentAlign>,
}

#[derive(Clone, Copy)]
pub struct FragmentAlign {
    pub bottom: f32,
    pub occupied: f32,
    pub ratio: f32,
}

#[derive(Clone, Copy)]
pub struct Positioned<'a> {
    pub block: &'a Block,
    pub x: f32,
    pub y: f32,
}

impl Positioned<'_> {
    pub fn translate(&mut self, x: f32, y: f32) {
        if self.block.style.position != Position::Fixed {
            self.x += x;
            self.y += y;
        }
    }
}

pub struct FloatArea {
    pub right: bool,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl<'a> Engine<'a> {
    pub fn clear_floats(&mut self, clear: u8, cursor: &mut Cursor) {
        let bottom = self
            .state
            .floats
            .iter()
            .filter(|area| clear & if area.right { 2 } else { 1 } != 0)
            .map(|area| area.y + area.height)
            .fold(f32::NEG_INFINITY, f32::max);
        if bottom > cursor.y + cursor.margin.value() {
            self.resolve(cursor);
            cursor.y = bottom;
            cursor.margin = Margin::default();
        }
    }

    pub fn float_space(&self, x: f32, width: f32, y: f32, height: f32) -> (f32, f32, f32) {
        let (mut left, mut right, mut next) = (x, x + width, f32::INFINITY);
        for area in &self.state.floats {
            let bottom = area.y + area.height;
            if area.y >= y + height || bottom <= y + 0.001 {
                continue;
            }
            if area.right {
                right = right.min(area.x);
            } else {
                left = left.max(area.x + area.width);
            }
            next = next.min(bottom);
        }
        (left.max(x), right.min(x + width), next)
    }

    pub fn place_float(&mut self, block: &'a Block, x: f32, width: f32, cursor: &mut Cursor) {
        let mut top = cursor.y + cursor.margin.value();
        let (min, max) = self.intrinsic(block);
        let used = max.min(width).max(min).max(0.0);
        let sub = self.sub_flow_block(block, used);
        let right_float = block.style.float == Some(true);
        let mut left = x;
        for _ in 0..=self.state.floats.len() {
            let (a, b, next) = self.float_space(x, width, top, sub.height.max(0.001));
            left = if right_float { b - used } else { a };
            if b - a >= used || !next.is_finite() {
                break;
            }
            top = next;
        }
        self.state.floats.push(FloatArea {
            right: right_float,
            x: left,
            y: top,
            width: used,
            height: sub.height,
        });
        let items = self.overlay_items(sub, left, top);
        self.state.overlays.push(Overlay {
            anchor: top,
            items,
            align: None,
        });
    }

    pub fn resolve_positioned(&mut self, first: usize, rect: [f32; 4], root: bool) {
        let [x, y, width, height] = rect;
        let pending = self.state.positioned.split_off(first);
        for positioned in pending {
            let s = &positioned.block.style;
            if s.position == Position::Fixed && !root {
                self.state.positioned.push(positioned);
                continue;
            }
            let left = s.inset[3].resolve(width);
            let right = s.inset[1].resolve(width);
            let top = s.inset[0].resolve(height);
            let bottom = s.inset[2].resolve(height);
            let (min, max) = self.intrinsic(positioned.block);
            let used = if s.width == Len::Auto && left.is_some() && right.is_some() {
                (width - left.unwrap_or(0.0) - right.unwrap_or(0.0)).max(0.0)
            } else {
                max.min((width - left.unwrap_or(0.0) - right.unwrap_or(0.0)).max(0.0))
                    .max(min)
            };
            let mut sub = self.sub_flow_block(positioned.block, used);
            if s.height == Len::Auto
                && let (Some(top), Some(bottom)) = (top, bottom)
            {
                let stretched = (height - top - bottom).max(0.0);
                if let Some(deco) = sub.decos.first_mut() {
                    deco.h += stretched - sub.height;
                }
                sub.height = stretched;
            }
            let px = left.map_or_else(
                || right.map_or(positioned.x, |right| x + width - right - used),
                |left| x + left,
            );
            let py = top.map_or_else(
                || bottom.map_or(positioned.y, |bottom| y + height - bottom - sub.height),
                |top| y + top,
            );
            let items = self.overlay_items(sub, px, py);
            if s.position == Position::Fixed {
                self.state.fixed.extend(items);
            } else {
                self.state.overlays.push(Overlay {
                    anchor: y,
                    items,
                    align: None,
                });
            }
        }
    }

    pub fn overlay_items(&mut self, mut sub: SubLayout<'a>, x: f32, y: f32) -> Vec<Item> {
        sub.translate(x, y);
        self.state.positioned.append(&mut sub.positioned);
        self.state.fixed.append(&mut sub.fixed);
        sub.into_items()
    }
}
