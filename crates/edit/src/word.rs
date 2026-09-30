use goat_common::GoatError;
use pdf_core::{Document, Object, Rect};
use pdf_text::{Block, FontInfo, TextFlags};

use crate::{EditError, plumber};

pub(crate) struct Page {
    pub width: f64,
    pub height: f64,
    pub background: Option<Vec<u8>>,
    pub elements: Vec<Element>,
}

pub(crate) enum Element {
    Paragraph(Paragraph),
    Table(Table),
}

impl Element {
    pub fn rect(&self) -> Rect {
        match self {
            Self::Paragraph(paragraph) => paragraph.rect,
            Self::Table(table) => table.geometry.rect,
        }
    }
}

pub(crate) struct Table {
    pub geometry: plumber::Table,
    pub cells: Vec<Vec<Paragraph>>,
}

pub(crate) struct Paragraph {
    pub rect: Rect,
    pub lines: Vec<Line>,
}

impl Paragraph {
    pub fn line_height(&self) -> f64 {
        match (self.lines.first(), self.lines.last()) {
            (Some(first), Some(last)) if self.lines.len() > 1 => {
                (last.baseline - first.baseline) / (self.lines.len() - 1) as f64
            }
            (Some(first), _) => first.size * 1.2,
            _ => 1.0,
        }
    }

    fn join_cost(&self, line: &Line) -> Option<f64> {
        let last = self.lines.last()?;
        let size = last.size.max(line.size);
        let gap = line.baseline - last.baseline;
        let overlap = last.rect.x1.min(line.rect.x1) - last.rect.x0.max(line.rect.x0);
        let indent = (last.rect.x0 - line.rect.x0).abs();
        if gap < size * 0.8
            || gap > size * 1.8
            || (last.size - line.size).abs() > size * 0.15
            || indent > size * 2.5
            || overlap < last.rect.width().min(line.rect.width()) * 0.25
        {
            return None;
        }
        if self.lines.len() > 1 && (gap - self.line_height()).abs() > size * 0.2 {
            return None;
        }
        Some(gap + indent * 0.01)
    }
}

pub(crate) struct Line {
    pub rect: Rect,
    pub baseline: f64,
    pub size: f64,
    pub runs: Vec<Run>,
}

pub(crate) struct Run {
    pub text: String,
    pub font: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    pub color: u32,
    pub rise: f64,
}

fn office_font(font: &FontInfo) -> &str {
    // Standard-14 PDF names are not necessarily installed Office font names.
    // These metrically compatible families also resolve in LibreOffice.
    if font.name.starts_with("Helvetica") {
        "Arial"
    } else if font.name.starts_with("Times-") {
        "Times New Roman"
    } else if font.name.starts_with("Courier") {
        "Courier New"
    } else {
        &font.name
    }
}

fn paragraphs(mut lines: Vec<Line>) -> Vec<Paragraph> {
    lines.sort_by(|a, b| {
        a.baseline
            .total_cmp(&b.baseline)
            .then(a.rect.x0.total_cmp(&b.rect.x0))
    });
    let mut out: Vec<Paragraph> = Vec::new();
    for line in lines {
        let best = out
            .iter()
            .enumerate()
            .filter_map(|(index, paragraph)| paragraph.join_cost(&line).map(|cost| (index, cost)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index);
        if let Some(index) = best {
            out[index].rect = out[index].rect.union(&line.rect);
            out[index].lines.push(line);
        } else {
            out.push(Paragraph {
                rect: line.rect,
                lines: vec![line],
            });
        }
    }
    out
}

type CellOwner = Option<(usize, usize)>;

fn cell_at(tables: &[plumber::Table], rect: Rect) -> CellOwner {
    let x = (rect.x0 + rect.x1) / 2.0;
    let y = (rect.y0 + rect.y1) / 2.0;
    tables.iter().enumerate().find_map(|(table, grid)| {
        if x < grid.rect.x0 || x >= grid.rect.x1 || y < grid.rect.y0 || y >= grid.rect.y1 {
            return None;
        }
        grid.cells
            .iter()
            .position(|cell| {
                x >= cell.rect.x0 && x < cell.rect.x1 && y >= cell.rect.y0 && y < cell.rect.y1
            })
            .map(|cell| (table, cell))
    })
}

fn finish_line(
    pending: &mut Option<(CellOwner, Line)>,
    body: &mut Vec<Line>,
    cells: &mut [Vec<Vec<Line>>],
) {
    let Some((owner, line)) = pending.take() else {
        return;
    };
    if line.runs.iter().all(|run| run.text.trim().is_empty()) {
        return;
    }
    if let Some((table, cell)) = owner {
        cells[table][cell].push(line);
    } else {
        body.push(line);
    }
}

pub(crate) fn page(doc: &Document, index: usize) -> Result<Page, GoatError> {
    let mut page = doc.page(index).map_err(EditError::from)?;
    page.dict.insert("Rotate", 0);
    let text = pdf_text::extract_page(doc, index, TextFlags::DICT)?;

    // Word, unlike pdfplumber's public table exports, uses unrotated crop-space
    // points. Keep that conversion here rather than changing the CSV contract.
    let mut table_page = page.clone();
    let crop = page.crop_box();
    table_page.dict.insert(
        "MediaBox",
        vec![crop.x0, crop.y0, crop.x1, crop.y1]
            .into_iter()
            .map(Object::Real)
            .collect::<Vec<_>>(),
    );
    let mut grids = plumber::plumber_page(doc, &table_page)?.table_layouts();
    let unit = page.user_unit();
    if unit != 1.0 {
        let scale = pdf_core::Matrix::scale(unit, unit);
        for table in &mut grids {
            table.rect = table.rect.transform(&scale);
            for width in &mut table.widths {
                *width *= unit;
            }
            for height in &mut table.heights {
                *height *= unit;
            }
            for cell in &mut table.cells {
                cell.rect = cell.rect.transform(&scale);
            }
        }
    }
    let mut cell_lines: Vec<Vec<Vec<Line>>> = grids
        .iter()
        .map(|table| table.cells.iter().map(|_| Vec::new()).collect())
        .collect();
    let mut body_lines = Vec::new();
    for block in &text.blocks {
        let Block::Text(block) = block else {
            continue;
        };
        for source in &block.lines {
            let mut pending: Option<(CellOwner, Line)> = None;
            for ch in &source.chars {
                let rect = ch.bbox(source);
                let owner = cell_at(&grids, rect);
                let split = pending.as_ref().is_some_and(|(previous, line)| {
                    *previous != owner
                        || rect.x0 - line.rect.x1 > ch.size * 2.0
                        || (ch.origin.y - line.baseline).abs() > ch.size * 0.6
                });
                if split {
                    finish_line(&mut pending, &mut body_lines, &mut cell_lines);
                }
                if pending.is_none() && ch.c.is_whitespace() {
                    continue;
                }
                let font = text
                    .fonts
                    .get(ch.font)
                    .ok_or_else(|| GoatError::message("text character has no font"))?;
                let (_, line) = pending.get_or_insert_with(|| {
                    (
                        owner,
                        Line {
                            rect,
                            baseline: ch.origin.y,
                            size: ch.size,
                            runs: Vec::new(),
                        },
                    )
                });
                line.rect = line.rect.union(&rect);
                line.size = line.size.max(ch.size);
                let rise = line.baseline - ch.origin.y;
                let name = office_font(font);
                let same_style = line.runs.last().is_some_and(|run| {
                    run.font == name
                        && run.size == ch.size
                        && run.color == ch.color
                        && run.bold == font.bold
                        && run.italic == font.italic
                        && run.rise == rise
                });
                if !same_style {
                    line.runs.push(Run {
                        text: String::new(),
                        font: name.to_owned(),
                        size: ch.size,
                        bold: font.bold,
                        italic: font.italic,
                        color: ch.color,
                        rise,
                    });
                }
                if let Some(run) = line.runs.last_mut() {
                    run.text.push(ch.c);
                }
            }
            finish_line(&mut pending, &mut body_lines, &mut cell_lines);
        }
    }
    let mut elements: Vec<Element> = paragraphs(body_lines)
        .into_iter()
        .map(Element::Paragraph)
        .collect();
    elements.extend(grids.into_iter().zip(cell_lines).map(|(geometry, cells)| {
        Element::Table(Table {
            geometry,
            cells: cells.into_iter().map(paragraphs).collect(),
        })
    }));
    elements.sort_by(|a, b| {
        a.rect()
            .y0
            .total_cmp(&b.rect().y0)
            .then(a.rect().x0.total_cmp(&b.rect().x0))
    });

    // One faithful, transparent graphics layer retains clipping, paths, images,
    // shading and compositing without baking the editable body text into pixels.
    let pixels = pdf_render::render_page_graphics(
        doc,
        &page,
        &pdf_render::RenderOptions {
            dpi: 144.0,
            alpha: true,
            annotations: false,
        },
    )
    .map_err(|error| GoatError::message(error.to_string()))?;
    let background = if pixels
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != 0)
    {
        Some(
            pdf_codec::encode_png(
                &pixels.to_rgba8(),
                pixels.width(),
                pixels.height(),
                pdf_codec::PngColor::Rgba,
                Some((144.0, 144.0)),
            )
            .map_err(|error| GoatError::message(error.to_string()))?,
        )
    } else {
        None
    };
    Ok(Page {
        width: text.rect.width(),
        height: text.rect.height(),
        background,
        elements,
    })
}
