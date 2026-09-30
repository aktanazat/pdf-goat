//! Intrinsic-width tables, cell spans, collapsed borders, and repeated headers.

use super::boxes::{Cell, Table};
use super::layout::{Cursor, Deco, Engine, Repeat, SubLayout, Unit, mark_items, used_width};
use super::position::{FragmentAlign, Overlay};
use super::style::{BorderStyle, Color, Len, Style, VAlign};

#[derive(Clone, Copy)]
struct Edge {
    width: f32,
    style: BorderStyle,
    color: Color,
}

type Edges = [Edge; 4];

pub(super) struct TableGeometry {
    pub width: f32,
    pub border: [f32; 4],
    pub padding: [f32; 4],
    columns: Vec<f32>,
    edges: Vec<Vec<Edges>>,
    spacing: (f32, f32),
    collapse: bool,
}

fn own_edges(style: &Style) -> Edges {
    std::array::from_fn(|side| Edge {
        width: style.border_width[side],
        style: style.border_style[side],
        color: style.border_color[side],
    })
}

fn winner(a: Edge, b: Edge) -> Edge {
    let rank = |style| match style {
        BorderStyle::Double => 4,
        BorderStyle::Solid => 3,
        BorderStyle::Dashed => 2,
        BorderStyle::Dotted => 1,
        BorderStyle::None => 0,
    };
    if b.width > a.width || (b.width == a.width && rank(b.style) > rank(a.style)) {
        b
    } else {
        a
    }
}

fn at(table: &Table, row: usize, column: usize) -> Option<&Cell> {
    for (r, candidate) in table.rows.iter().enumerate().take(row + 1).rev() {
        if let Some(cell) = candidate.cells.iter().find(|cell| {
            r + cell.rowspan > row && cell.column <= column && cell.column + cell.colspan > column
        }) {
            return Some(cell);
        }
    }
    None
}

fn table_edges(table: &Table, style: &Style) -> Vec<Vec<Edges>> {
    let mut rows = Vec::with_capacity(table.rows.len());
    for (r, row) in table.rows.iter().enumerate() {
        let mut cells = Vec::with_capacity(row.cells.len());
        for cell in &row.cells {
            let mut edges = own_edges(&cell.block.style);
            if style.border_collapse {
                let adjacent = [
                    r.checked_sub(1).and_then(|r| at(table, r, cell.column)),
                    at(table, r, cell.column + cell.colspan),
                    at(table, r + cell.rowspan, cell.column),
                    cell.column.checked_sub(1).and_then(|c| at(table, r, c)),
                ];
                for side in 0..4 {
                    let other = adjacent[side].map_or_else(
                        || own_edges(style)[side],
                        |cell| own_edges(&cell.block.style)[(side + 2) % 4],
                    );
                    edges[side] = winner(edges[side], other);
                }
            }
            cells.push(edges);
        }
        rows.push(cells);
    }
    rows
}

impl<'a> Engine<'a> {
    fn column_intrinsic(
        &mut self,
        table: &Table,
        style: &Style,
        edges: &[Vec<Edges>],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut min = vec![0.0f32; table.columns];
        let mut max = min.clone();
        let factor = if style.border_collapse { 0.5 } else { 1.0 };
        let spacing = if style.border_collapse {
            0.0
        } else {
            style.border_spacing.0
        };
        for spanning in [false, true] {
            for (r, row) in table.rows.iter().enumerate() {
                for (c, cell) in row.cells.iter().enumerate() {
                    if cell.colspan == 0 || (cell.colspan > 1) != spanning {
                        continue;
                    }
                    let s = &cell.block.style;
                    let (mut a, mut b) = self.intrinsic_content(&cell.block);
                    if let Len::Px(width) = s.width {
                        a = a.max(width);
                        b = b.max(width);
                    }
                    let frame = s.padding[1].or_zero(0.0)
                        + s.padding[3].or_zero(0.0)
                        + factor * (edges[r][c][1].width + edges[r][c][3].width);
                    a += frame;
                    b += frame;
                    let range = cell.column..cell.column + cell.colspan;
                    let gaps = spacing * (cell.colspan - 1) as f32;
                    let add_min = ((a - gaps - min[range.clone()].iter().sum::<f32>())
                        / cell.colspan as f32)
                        .max(0.0);
                    let add_max = ((b - gaps - max[range.clone()].iter().sum::<f32>())
                        / cell.colspan as f32)
                        .max(0.0);
                    for column in range {
                        min[column] += add_min;
                        max[column] = (max[column] + add_max).max(min[column]);
                    }
                }
            }
        }
        (min, max)
    }

    pub fn table_intrinsic(&mut self, table: &Table, style: &Style) -> (f32, f32) {
        let edges = table_edges(table, style);
        let (min, max) = self.column_intrinsic(table, style, &edges);
        let gaps = if style.border_collapse {
            0.0
        } else {
            (table.columns + 1) as f32 * style.border_spacing.0
        };
        (
            min.iter().sum::<f32>() + gaps,
            max.iter().sum::<f32>() + gaps,
        )
    }

    pub fn table_geometry(
        &mut self,
        table: &Table,
        style: &Style,
        available: f32,
    ) -> TableGeometry {
        let edges = table_edges(table, style);
        let (min, max) = self.column_intrinsic(table, style, &edges);
        let collapse = style.border_collapse;
        let spacing = if collapse {
            (0.0, 0.0)
        } else {
            style.border_spacing
        };
        let padding = if collapse {
            [0.0; 4]
        } else {
            style.padding.map(|len| len.or_zero(available))
        };
        let mut border = style.border_width;
        if collapse {
            border = [0.0; 4];
            for (r, row) in table.rows.iter().enumerate() {
                for (c, cell) in row.cells.iter().enumerate() {
                    for (side, edge) in edges[r][c].iter().enumerate() {
                        let outer = match side {
                            0 => r == 0,
                            1 => cell.column + cell.colspan == table.columns,
                            2 => r + cell.rowspan == table.rows.len(),
                            _ => cell.column == 0,
                        };
                        if outer {
                            border[side] = border[side].max(edge.width / 2.0);
                        }
                    }
                }
            }
        }
        let frame = border[1] + border[3] + padding[1] + padding[3];
        let gaps = (table.columns + 1) as f32 * spacing.0;
        let min_sum = min.iter().sum::<f32>();
        let max_sum = max.iter().sum::<f32>();
        let margins = style.margin[1].or_zero(available) + style.margin[3].or_zero(available);
        let auto = (max_sum + gaps)
            .min(available - frame - margins)
            .max(min_sum + gaps);
        let width = used_width(style, available, auto, frame).max(min_sum + gaps);
        let assign = (width - gaps).max(min_sum);
        let columns = if assign >= max_sum {
            let extra = (assign - max_sum) / table.columns.max(1) as f32;
            max.iter().map(|value| value + extra).collect()
        } else {
            let ratio = if max_sum > min_sum {
                (assign - min_sum) / (max_sum - min_sum)
            } else {
                0.0
            };
            min.iter()
                .zip(&max)
                .map(|(min, max)| min + (max - min) * ratio)
                .collect()
        };
        TableGeometry {
            width,
            border,
            padding,
            columns,
            edges,
            spacing,
            collapse,
        }
    }

    pub fn table(&mut self, table: &'a Table, x: f32, geom: TableGeometry, cursor: &mut Cursor) {
        for caption in &table.captions {
            self.block(caption, x, geom.width, cursor);
        }
        self.resolve(cursor);
        let mut heights = vec![0.0f32; table.rows.len()];
        let mut baselines = heights.clone();
        let mut cells: Vec<Vec<(SubLayout<'a>, [f32; 4], f32)>> =
            Vec::with_capacity(table.rows.len());
        let factor = if geom.collapse { 0.5 } else { 1.0 };
        for (r, row) in table.rows.iter().enumerate() {
            let mut laid = Vec::with_capacity(row.cells.len());
            for (c, cell) in row.cells.iter().enumerate() {
                if cell.colspan == 0 {
                    continue;
                }
                let s = &cell.block.style;
                let width = geom.columns[cell.column..cell.column + cell.colspan]
                    .iter()
                    .sum::<f32>()
                    + (cell.colspan - 1) as f32 * geom.spacing.0;
                let frame = std::array::from_fn(|side| {
                    s.padding[side].or_zero(geom.width) + geom.edges[r][c][side].width * factor
                });
                let sub = self.sub_content(&cell.block, (width - frame[1] - frame[3]).max(0.0));
                let height = (sub.height + frame[0] + frame[2]).max(s.height.or_zero(0.0));
                if cell.rowspan == 1 {
                    heights[r] = heights[r].max(height);
                    baselines[r] = baselines[r].max(frame[0] + sub.first_baseline);
                }
                laid.push((sub, frame, height));
            }
            heights[r] = heights[r].max(row.style.height.or_zero(0.0));
            cells.push(laid);
        }
        for (r, row) in table.rows.iter().enumerate() {
            for (c, cell) in row.cells.iter().filter(|cell| cell.colspan > 0).enumerate() {
                if cell.rowspan <= 1 {
                    continue;
                }
                let end = r + cell.rowspan;
                let total = heights[r..end].iter().sum::<f32>()
                    + (cell.rowspan - 1) as f32 * geom.spacing.1;
                if total < cells[r][c].2 {
                    heights[end - 1] += cells[r][c].2 - total;
                }
            }
        }
        let mut row_y = cursor.y + geom.spacing.1;
        let table_top = row_y;
        let header_height =
            heights[..table.headers].iter().sum::<f32>() + table.headers as f32 * geom.spacing.1;
        let capacity =
            (self.page.height - self.page.margin[0] - self.page.margin[2] - header_height).max(1.0);
        let mut table_units = Vec::new();
        let mut header_items = Vec::new();
        let header_deco_start = self.state.decos.len();
        for (r, (row, laid)) in table.rows.iter().zip(cells).enumerate() {
            let mut row_units = Vec::new();
            if row.style.background.visible() {
                self.state.decos.push(Deco {
                    x,
                    y: row_y,
                    w: geom.width,
                    h: heights[r],
                    background: row.style.background,
                    radius: 0.0,
                    border: [0.0; 4],
                    colors: row.style.border_color,
                    styles: row.style.border_style,
                });
            }
            for (c, (cell, (mut sub, frame, _))) in row
                .cells
                .iter()
                .filter(|cell| cell.colspan > 0)
                .zip(laid)
                .enumerate()
            {
                let s = &cell.block.style;
                let cx = x
                    + geom.spacing.0
                    + geom.columns[..cell.column].iter().sum::<f32>()
                    + cell.column as f32 * geom.spacing.0;
                let width = geom.columns[cell.column..cell.column + cell.colspan]
                    .iter()
                    .sum::<f32>()
                    + (cell.colspan - 1) as f32 * geom.spacing.0;
                let height = heights[r..r + cell.rowspan].iter().sum::<f32>()
                    + (cell.rowspan - 1) as f32 * geom.spacing.1;
                let free = (height - frame[0] - frame[2] - sub.height).max(0.0);
                let shift = match s.vertical_align {
                    VAlign::Middle => free / 2.0,
                    VAlign::Bottom | VAlign::TextBottom => free,
                    VAlign::Baseline => (baselines[r] - frame[0] - sub.first_baseline).max(0.0),
                    _ => 0.0,
                };
                let edge = geom.edges[r][c];
                let widths = edge.map(|e| e.width);
                let colors = edge.map(|e| e.color);
                let styles = edge.map(|e| e.style);
                let background = s.background;
                let rect = if geom.collapse {
                    [
                        cx - widths[3] / 2.0,
                        row_y - widths[0] / 2.0,
                        width + (widths[1] + widths[3]) / 2.0,
                        height + (widths[0] + widths[2]) / 2.0,
                    ]
                } else {
                    [cx, row_y, width, height]
                };
                self.state.decos.push(Deco {
                    x: rect[0],
                    y: rect[1],
                    w: rect[2],
                    h: rect[3],
                    background,
                    radius: 0.0,
                    border: widths,
                    colors,
                    styles,
                });
                if height > capacity
                    && sub.units.len() == 1
                    && sub.height + frame[0] + frame[2] <= capacity
                    && matches!(
                        s.vertical_align,
                        VAlign::Middle | VAlign::Bottom | VAlign::TextBottom
                    )
                {
                    let occupied = sub.height + frame[0] + frame[2];
                    let mut items = self.overlay_items(sub, cx + frame[3], row_y + frame[0]);
                    items.extend(mark_items(&cell.block.marks, cx, row_y, width, height));
                    let ratio = if s.vertical_align == VAlign::Middle {
                        0.5
                    } else {
                        1.0
                    };
                    self.state.overlays.push(Overlay {
                        anchor: row_y,
                        items,
                        align: Some(FragmentAlign {
                            bottom: row_y + height,
                            occupied,
                            ratio,
                        }),
                    });
                    row_units.push(Unit {
                        top: row_y,
                        bottom: row_y + occupied,
                        start: row_y,
                        forced: false,
                        avoid: false,
                        items: Vec::new(),
                    });
                    continue;
                }
                let content_x = cx + frame[3];
                let content_y = row_y + frame[0] + shift;
                let mut marks = mark_items(&cell.block.marks, cx, row_y, width, height);
                sub.translate(content_x, content_y);
                if let Some(first) = sub.units.first_mut() {
                    marks.append(&mut first.items);
                    first.items = marks;
                } else if !marks.is_empty() {
                    self.state.pending.extend(marks);
                }
                self.absorb(sub, &mut row_units);
            }
            if heights[r] <= capacity || r < table.headers || row.style.avoid_break_inside {
                let mut items = Vec::new();
                for unit in row_units {
                    items.extend(unit.items);
                }
                table_units.push(Unit {
                    top: row_y,
                    bottom: row_y + heights[r],
                    start: row_y,
                    forced: false,
                    avoid: r > 0 && r <= table.headers,
                    items,
                });
            } else {
                row_units.sort_by(|a, b| a.top.total_cmp(&b.top));
                if let Some(first) = row_units.first_mut() {
                    first.start = row_y;
                    first.avoid |= r == table.headers;
                }
                if let Some(last) = row_units.last_mut() {
                    last.bottom = last.bottom.max(row_y + heights[r]);
                }
                table_units.extend(row_units);
            }
            if r + 1 == table.headers {
                for deco in &self.state.decos[header_deco_start..] {
                    header_items.extend(deco.items(f32::NEG_INFINITY, f32::INFINITY));
                }
                for unit in &table_units {
                    header_items.extend(unit.items.iter().cloned());
                }
            }
            row_y += heights[r] + geom.spacing.1;
        }
        if table.headers > 0 && table.headers < table.rows.len() {
            self.state.repeats.push(Repeat {
                top: table_top,
                body_top: table_top + header_height,
                bottom: row_y,
                height: header_height,
                items: header_items,
            });
        }
        self.merge_units(table_units);
        cursor.y = row_y;
    }
}
