//! Flex/grid geometry uses the same text and table measurement as normal flow.

use std::collections::HashMap;

use taffy::prelude::{TaffyAuto, TaffyZero};
use taffy::{
    AvailableSpace, Dimension, LengthPercentage, LengthPercentageAuto, Rect, Size, TaffyTree,
};

use super::boxes::{Block, Kind};
use super::layout::{Cursor, Deco, Engine, SubLayout, mark_items};
use super::position::Positioned;
use super::style::{Display, Len, Position, Style};

fn dimension(value: Len) -> Dimension {
    match value {
        Len::Auto => Dimension::AUTO,
        Len::Px(n) => Dimension::length(n),
        Len::Pct(n) => Dimension::percent(n / 100.0),
    }
}
fn length_auto(value: Len) -> LengthPercentageAuto {
    match value {
        Len::Auto => LengthPercentageAuto::AUTO,
        Len::Px(n) => LengthPercentageAuto::length(n),
        Len::Pct(n) => LengthPercentageAuto::percent(n / 100.0),
    }
}
fn length(value: Len) -> LengthPercentage {
    match value {
        Len::Auto => LengthPercentage::ZERO,
        Len::Px(n) => LengthPercentage::length(n),
        Len::Pct(n) => LengthPercentage::percent(n / 100.0),
    }
}
fn rect<T: Copy>(values: [T; 4]) -> Rect<T> {
    Rect {
        top: values[0],
        right: values[1],
        bottom: values[2],
        left: values[3],
    }
}

fn item_style(style: &Style) -> taffy::Style {
    let mut result = style.formatting.as_deref().cloned().unwrap_or_default();
    result.display = taffy::Display::Block;
    result.box_sizing = if style.border_box {
        taffy::BoxSizing::BorderBox
    } else {
        taffy::BoxSizing::ContentBox
    };
    result.direction = if style.rtl {
        taffy::Direction::Rtl
    } else {
        taffy::Direction::Ltr
    };
    result.size = Size {
        width: dimension(style.width),
        height: dimension(style.height),
    };
    result.min_size = Size {
        width: length_auto(style.min_width),
        height: length_auto(style.min_height),
    };
    result.max_size = Size {
        width: length_auto(style.max_width),
        height: length_auto(style.max_height),
    };
    result.margin = rect(style.margin.map(length_auto));
    result.padding = rect(style.padding.map(length));
    result.border = rect(style.border_width.map(LengthPercentage::length));
    if style.position == Position::Relative {
        result.inset = rect(style.inset.map(length_auto));
    }
    result
}

struct Measure<'a> {
    block: &'a Block,
    intrinsic: (f32, f32),
    laid: HashMap<u32, SubLayout<'a>>,
}

impl<'a> Engine<'a> {
    pub fn formatting(
        &mut self,
        children: &'a [Block],
        style: &Style,
        x: f32,
        width: f32,
        cursor: &mut Cursor,
    ) -> Result<(), taffy::TaffyError> {
        self.resolve(cursor);
        let top = cursor.y;
        let mut tree = TaffyTree::new();
        tree.disable_rounding();
        let mut ordered: Vec<&Block> = children.iter().collect();
        ordered.sort_by_key(|child| child.style.order);
        let mut nodes = Vec::with_capacity(children.len());
        for child in ordered {
            if matches!(child.style.position, Position::Absolute | Position::Fixed) {
                self.state.positioned.push(Positioned {
                    block: child,
                    x,
                    y: top,
                });
                continue;
            }
            let intrinsic = self.intrinsic_content(child);
            let mut css = item_style(&child.style);
            css.item_is_replaced = matches!(child.kind, Kind::Image(_));
            let node = tree.new_leaf_with_context(
                css,
                Measure {
                    block: child,
                    intrinsic,
                    laid: HashMap::new(),
                },
            )?;
            nodes.push(node);
        }
        let mut root_style = style.formatting.as_deref().cloned().unwrap_or_default();
        root_style.display = if style.display == Display::Grid {
            taffy::Display::Grid
        } else {
            taffy::Display::Flex
        };
        root_style.box_sizing = taffy::BoxSizing::ContentBox;
        root_style.direction = if style.rtl {
            taffy::Direction::Rtl
        } else {
            taffy::Direction::Ltr
        };
        root_style.size.width = Dimension::length(width);
        let frame = style.padding[0].or_zero(width)
            + style.padding[2].or_zero(width)
            + style.border_width[0]
            + style.border_width[2];
        root_style.size.height = match style.height {
            Len::Px(height) if style.border_box => Dimension::length((height - frame).max(0.0)),
            other => dimension(other),
        };
        let root = tree.new_with_children(root_style, &nodes)?;
        tree.compute_layout_with_measure(
            root,
            Size {
                width: AvailableSpace::Definite(width),
                height: AvailableSpace::MaxContent,
            },
            |inputs, _, context, css| {
                let mut first = None;
                let mut last = None;
                let mut output = taffy::compute_leaf_layout(
                    inputs,
                    css,
                    |_, _| 0.0,
                    |known, available| {
                        let Some(context) = context else {
                            return Size::ZERO;
                        };
                        let width = match available.width {
                            AvailableSpace::Definite(n) if known.width.is_some() => n,
                            AvailableSpace::Definite(n) => {
                                context.intrinsic.1.min(n).max(context.intrinsic.0)
                            }
                            AvailableSpace::MinContent => context.intrinsic.0,
                            AvailableSpace::MaxContent => context.intrinsic.1,
                        }
                        .max(0.0);
                        let sub = context
                            .laid
                            .entry(width.to_bits())
                            .or_insert_with(|| self.sub_content(context.block, width));
                        let inset = context.block.style.padding[0]
                            .or_zero(inputs.parent_size.width.unwrap_or(0.0))
                            + context.block.style.border_width[0];
                        first = Some(sub.first_baseline + inset);
                        last = Some(sub.last_baseline + inset);
                        Size {
                            width,
                            height: sub.height,
                        }
                    },
                );
                output.baselines = taffy::Baselines { first, last };
                output
            },
        )?;
        let height = tree.layout(root)?.size.height;
        let mut units = Vec::new();
        for node in nodes {
            let layout = *tree.layout(node)?;
            let Some(context) = tree.get_node_context_mut(node) else {
                continue;
            };
            let child = context.block;
            let s = &child.style;
            let content_width = (layout.size.width
                - layout.padding.left
                - layout.padding.right
                - layout.border.left
                - layout.border.right)
                .max(0.0);
            let mut sub = context
                .laid
                .remove(&content_width.to_bits())
                .unwrap_or_else(|| self.sub_content(child, content_width));
            let left = x + layout.location.x;
            let child_top = top + layout.location.y;
            self.state.decos.push(Deco {
                x: left,
                y: child_top,
                w: layout.size.width,
                h: layout.size.height,
                background: s.background,
                radius: s.radius,
                border: s.border_width,
                colors: s.border_color,
                styles: s.border_style,
            });
            let mut marks = mark_items(
                &child.marks,
                left,
                child_top,
                layout.size.width,
                layout.size.height,
            );
            let content_x = left + layout.padding.left + layout.border.left;
            let content_y = child_top + layout.padding.top + layout.border.top;
            sub.translate(content_x, content_y);
            if let Some(first) = sub.units.first_mut() {
                marks.append(&mut first.items);
                first.items = marks;
            } else if !marks.is_empty() {
                self.state.pending.extend(marks);
            }
            self.absorb(sub, &mut units);
        }
        self.merge_units(units);
        cursor.y = top + height;
        Ok(())
    }
}
