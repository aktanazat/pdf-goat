//! Office Open XML packages written by hand: a deflated zip container and
//! the minimal parts openpyxl, python-docx, python-pptx (and Office) read.

use std::fmt::Write as _;

/// A zip archive with deflated members, no data descriptors, no zip64.
#[derive(Default)]
pub struct Zip {
    data: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
}

impl Zip {
    pub fn add(&mut self, name: &str, content: &[u8]) {
        let crc = crc32fast::hash(content);
        let deflated = miniz_oxide::deflate::compress_to_vec(content, 6);
        let (method, body): (u16, &[u8]) = if deflated.len() < content.len() {
            (8, &deflated)
        } else {
            (0, content)
        };
        let offset = self.data.len() as u32;
        let name_bytes = name.as_bytes();
        let header = |out: &mut Vec<u8>, central: bool| {
            if central {
                out.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
                out.extend_from_slice(&20u16.to_le_bytes()); // version made by
            } else {
                out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            }
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // mod time
            out.extend_from_slice(&0x21u16.to_le_bytes()); // mod date: 1980-01-01
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra length
            if central {
                out.extend_from_slice(&0u16.to_le_bytes()); // comment length
                out.extend_from_slice(&0u16.to_le_bytes()); // disk number
                out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
                out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
                out.extend_from_slice(&offset.to_le_bytes());
            }
            out.extend_from_slice(name_bytes);
        };
        header(&mut self.data, false);
        self.data.extend_from_slice(body);
        header(&mut self.central, true);
        self.entries = self.entries.saturating_add(1);
    }

    pub fn finish(mut self) -> Vec<u8> {
        let central_offset = self.data.len() as u32;
        let central_size = self.central.len() as u32;
        self.data.extend_from_slice(&self.central);
        self.data.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        self.data.extend_from_slice(&0u16.to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes());
        self.data.extend_from_slice(&self.entries.to_le_bytes());
        self.data.extend_from_slice(&self.entries.to_le_bytes());
        self.data.extend_from_slice(&central_size.to_le_bytes());
        self.data.extend_from_slice(&central_offset.to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes());
        self.data
    }
}

/// XML text content: entities escaped, characters XML 1.0 forbids dropped.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || matches!(c, '\u{fffe}' | '\u{ffff}') => {}
            c => out.push(c),
        }
    }
    out
}

const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";

/// The column letters of a 1-based column index.
fn column_name(mut index: usize) -> String {
    let mut name = Vec::new();
    while index > 0 {
        let rem = (index - 1) % 26;
        name.push(b'A' + rem as u8);
        index = (index - 1) / 26;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_default()
}

/// An openpyxl-style workbook: one sheet per `(title, rows)`; a row of
/// `None` is an appended empty row.
pub fn xlsx(sheets: &[(String, Vec<Option<Vec<String>>>)]) -> Vec<u8> {
    let mut zip = Zip::default();
    let mut types = String::from(XML_HEAD);
    types
        .push_str("<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">");
    types.push_str("<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>");
    types.push_str("<Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    types.push_str("<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>");
    types.push_str("<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>");
    for i in 1..=sheets.len() {
        let _ = write!(
            types,
            "<Override PartName=\"/xl/worksheets/sheet{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        );
    }
    types.push_str("</Types>");
    zip.add("[Content_Types].xml", types.as_bytes());
    zip.add(
        "_rels/.rels",
        format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>").as_bytes(),
    );
    let mut workbook = format!(
        "{XML_HEAD}<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>"
    );
    let mut rels = format!(
        "{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">"
    );
    for (i, (title, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        let _ = write!(
            workbook,
            "<sheet name=\"{}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>",
            xml_escape(title)
        );
        let _ = write!(
            rels,
            "<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"
        );
        let mut sheet = format!(
            "{XML_HEAD}<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>"
        );
        for (r, row) in rows.iter().enumerate() {
            let Some(cells) = row else { continue };
            let _ = write!(sheet, "<row r=\"{}\">", r + 1);
            for (c, value) in cells.iter().enumerate() {
                let cell = format!("{}{}", column_name(c + 1), r + 1);
                if value.is_empty() {
                    let _ = write!(sheet, "<c r=\"{cell}\" t=\"inlineStr\"/>");
                } else if let Some(formula) = value.strip_prefix('=').filter(|f| !f.is_empty()) {
                    let _ = write!(
                        sheet,
                        "<c r=\"{cell}\"><f>{}</f><v/></c>",
                        xml_escape(formula)
                    );
                } else if matches!(
                    value.as_str(),
                    "#NULL!" | "#DIV/0!" | "#VALUE!" | "#REF!" | "#NAME?" | "#NUM!" | "#N/A"
                ) {
                    let _ = write!(
                        sheet,
                        "<c r=\"{cell}\" t=\"e\"><v>{}</v></c>",
                        xml_escape(value)
                    );
                } else {
                    let _ = write!(
                        sheet,
                        "<c r=\"{cell}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
                        xml_escape(value)
                    );
                }
            }
            sheet.push_str("</row>");
        }
        sheet.push_str("</sheetData></worksheet>");
        zip.add(&format!("xl/worksheets/sheet{n}.xml"), sheet.as_bytes());
    }
    let styles_id = sheets.len() + 1;
    let _ = write!(
        rels,
        "<Relationship Id=\"rId{styles_id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/></Relationships>"
    );
    workbook.push_str("</sheets></workbook>");
    zip.add("xl/workbook.xml", workbook.as_bytes());
    zip.add("xl/_rels/workbook.xml.rels", rels.as_bytes());
    zip.add(
        "xl/styles.xml",
        format!("{XML_HEAD}<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts><fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>").as_bytes(),
    );
    zip.finish()
}

/// Editable positioned paragraphs and merged tables over faithful page graphics.
pub fn docx(pages: &[crate::word::Page]) -> Vec<u8> {
    use crate::word::Element;

    let mut zip = Zip::default();
    zip.add("[Content_Types].xml", format!("{XML_HEAD}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"png\" ContentType=\"image/png\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/><Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/></Types>").as_bytes());
    zip.add("_rels/.rels", format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>").as_bytes());
    zip.add("word/styles.xml", format!("{XML_HEAD}<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Arial\" w:hAnsi=\"Arial\"/><w:sz w:val=\"22\"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:before=\"0\" w:after=\"0\"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/></w:style></w:styles>").as_bytes());
    let mut rels = format!(
        "{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>"
    );
    let mut body = format!(
        "{XML_HEAD}<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><w:body>"
    );
    for (i, page) in pages.iter().enumerate() {
        if let Some(png) = &page.background {
            let picture = i + 1;
            let id = picture + 1;
            let _ = write!(
                rels,
                "<Relationship Id=\"rId{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/page{picture}.png\"/>"
            );
            zip.add(&format!("word/media/page{picture}.png"), png);
            word_background(&mut body, picture, id, page.width, page.height);
        }
        for element in &page.elements {
            match element {
                Element::Paragraph(paragraph) => {
                    word_paragraph(&mut body, paragraph, true, 0.0, 0.0)
                }
                Element::Table(table) => word_table(&mut body, table),
            }
        }
        let sect = format!(
            "<w:sectPr><w:type w:val=\"nextPage\"/><w:pgSz w:w=\"{}\" w:h=\"{}\"{}/><w:pgMar w:top=\"0\" w:right=\"0\" w:bottom=\"0\" w:left=\"0\" w:header=\"0\" w:footer=\"0\" w:gutter=\"0\"/></w:sectPr>",
            twips(page.width),
            twips(page.height),
            if page.width > page.height {
                " w:orient=\"landscape\""
            } else {
                ""
            }
        );
        if i + 1 < pages.len() {
            let _ = write!(
                body,
                "<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"1\" w:lineRule=\"exact\"/>{sect}</w:pPr></w:p>"
            );
        } else {
            body.push_str(&sect);
        }
    }
    if pages.is_empty() {
        body.push_str("<w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/></w:sectPr>");
    }
    rels.push_str("</Relationships>");
    body.push_str("</w:body></w:document>");
    zip.add("word/document.xml", body.as_bytes());
    zip.add("word/_rels/document.xml.rels", rels.as_bytes());
    zip.finish()
}

fn twips(points: f64) -> u64 {
    (points * 20.0).round().max(0.0) as u64
}

fn word_paragraph(
    body: &mut String,
    paragraph: &crate::word::Paragraph,
    floating: bool,
    before: f64,
    left: f64,
) {
    let Some(first) = paragraph.lines.first() else {
        return;
    };
    let line_height = paragraph.line_height();
    body.push_str("<w:p><w:pPr>");
    if floating {
        let _ = write!(
            body,
            "<w:framePr w:w=\"{}\" w:h=\"{}\" w:hRule=\"atLeast\" w:x=\"{}\" w:y=\"{}\" w:hAnchor=\"page\" w:vAnchor=\"page\" w:wrap=\"none\"/>",
            twips(paragraph.rect.width() + first.size * 0.75),
            twips(line_height * paragraph.lines.len() as f64),
            (paragraph.rect.x0 * 20.0).round() as i64,
            ((first.baseline - 0.8 * line_height) * 20.0).round() as i64
        );
    }
    body.push_str("<w:tabs>");
    for line in &paragraph.lines {
        let indent = line.rect.x0 - paragraph.rect.x0;
        if indent > 0.05 {
            let _ = write!(body, "<w:tab w:val=\"left\" w:pos=\"{}\"/>", twips(indent));
        }
    }
    let _ = write!(
        body,
        "</w:tabs><w:spacing w:before=\"{}\" w:after=\"0\" w:line=\"{}\" w:lineRule=\"exact\"/><w:ind w:left=\"{}\"/></w:pPr>",
        twips(before),
        twips(line_height),
        twips(left)
    );
    for (index, line) in paragraph.lines.iter().enumerate() {
        if index > 0 {
            body.push_str("<w:r><w:br/></w:r>");
        }
        if line.rect.x0 - paragraph.rect.x0 > 0.05 {
            body.push_str("<w:r><w:tab/></w:r>");
        }
        let baseline_shift = first.baseline + index as f64 * line_height - line.baseline;
        for run in &line.runs {
            let font = xml_escape(&run.font);
            let _ = write!(
                body,
                "<w:r><w:rPr><w:rFonts w:ascii=\"{font}\" w:hAnsi=\"{font}\" w:eastAsia=\"{font}\"/>{}{}<w:color w:val=\"{:06X}\"/><w:position w:val=\"{}\"/><w:sz w:val=\"{}\"/></w:rPr>",
                if run.bold { "<w:b/>" } else { "" },
                if run.italic { "<w:i/>" } else { "" },
                run.color,
                ((run.rise + baseline_shift) * 2.0).round() as i64,
                (run.size * 2.0).round() as u64
            );
            word_text(body, &run.text);
            body.push_str("</w:r>");
        }
    }
    body.push_str("</w:p>");
}

fn word_table(body: &mut String, table: &crate::word::Table) {
    let grid = &table.geometry;
    let _ = write!(
        body,
        "<w:tbl><w:tblPr><w:tblpPr w:horzAnchor=\"page\" w:vertAnchor=\"page\" w:tblpX=\"{}\" w:tblpY=\"{}\" w:leftFromText=\"0\" w:rightFromText=\"0\" w:topFromText=\"0\" w:bottomFromText=\"0\"/><w:tblOverlap w:val=\"overlap\"/><w:tblW w:w=\"{}\" w:type=\"dxa\"/><w:tblLayout w:type=\"fixed\"/><w:tblBorders>",
        (grid.rect.x0 * 20.0).round() as i64,
        (grid.rect.y0 * 20.0).round() as i64,
        twips(grid.rect.width())
    );
    // The graphics layer carries the original line colours, widths and gaps.
    // Native cells retain their editable merge topology without doubling borders.
    for edge in ["top", "left", "bottom", "right", "insideH", "insideV"] {
        let _ = write!(body, "<w:{edge} w:val=\"nil\"/>");
    }
    body.push_str("</w:tblBorders><w:tblCellMar><w:top w:w=\"0\" w:type=\"dxa\"/><w:left w:w=\"0\" w:type=\"dxa\"/><w:bottom w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"0\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr><w:tblGrid>");
    for width in &grid.widths {
        let _ = write!(body, "<w:gridCol w:w=\"{}\"/>", twips(*width));
    }
    body.push_str("</w:tblGrid>");
    for (row, height) in grid.heights.iter().enumerate() {
        let _ = write!(
            body,
            "<w:tr><w:trPr><w:trHeight w:val=\"{}\" w:hRule=\"exact\"/></w:trPr>",
            twips(*height)
        );
        let mut column = 0;
        while column < grid.widths.len() {
            let cell = grid.cells.iter().enumerate().find(|(_, cell)| {
                cell.column == column && row >= cell.row && row < cell.row + cell.row_span
            });
            let span = cell.map_or(1, |(_, cell)| cell.column_span);
            let width: f64 = grid.widths[column..column + span].iter().sum();
            let _ = write!(
                body,
                "<w:tc><w:tcPr><w:tcW w:w=\"{}\" w:type=\"dxa\"/>",
                twips(width)
            );
            if span > 1 {
                let _ = write!(body, "<w:gridSpan w:val=\"{span}\"/>");
            }
            if let Some((_, cell)) = cell
                && cell.row_span > 1
            {
                if row == cell.row {
                    body.push_str("<w:vMerge w:val=\"restart\"/>");
                } else {
                    body.push_str("<w:vMerge/>");
                }
            }
            body.push_str("<w:noWrap/></w:tcPr>");
            let mut wrote = false;
            if let Some((index, cell)) = cell
                && row == cell.row
            {
                let mut bottom = cell.rect.y0;
                for paragraph in &table.cells[index] {
                    let Some(first) = paragraph.lines.first() else {
                        continue;
                    };
                    let line_height = paragraph.line_height();
                    let before = (first.baseline - bottom - 0.8 * line_height).max(0.0);
                    word_paragraph(
                        body,
                        paragraph,
                        false,
                        before,
                        paragraph.rect.x0 - cell.rect.x0,
                    );
                    bottom += before + line_height * paragraph.lines.len() as f64;
                    wrote = true;
                }
            }
            if !wrote {
                body.push_str("<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"1\" w:lineRule=\"exact\"/></w:pPr></w:p>");
            }
            body.push_str("</w:tc>");
            column += span;
        }
        body.push_str("</w:tr>");
    }
    body.push_str("</w:tbl>");
}

fn word_background(body: &mut String, picture: usize, id: usize, width: f64, height: f64) {
    let width = (width * 12700.0).round().max(1.0) as u64;
    let height = (height * 12700.0).round().max(1.0) as u64;
    let _ = write!(
        body,
        "<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"1\" w:lineRule=\"exact\"/></w:pPr><w:r><w:drawing><wp:anchor distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\" simplePos=\"0\" relativeHeight=\"0\" behindDoc=\"1\" locked=\"0\" layoutInCell=\"1\" allowOverlap=\"1\"><wp:simplePos x=\"0\" y=\"0\"/><wp:positionH relativeFrom=\"page\"><wp:posOffset>0</wp:posOffset></wp:positionH><wp:positionV relativeFrom=\"page\"><wp:posOffset>0</wp:posOffset></wp:positionV><wp:extent cx=\"{width}\" cy=\"{height}\"/><wp:wrapNone/><wp:docPr id=\"{picture}\" name=\"Page graphics {picture}\"/><wp:cNvGraphicFramePr/><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic><pic:nvPicPr><pic:cNvPr id=\"{picture}\" name=\"page{picture}.png\"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed=\"rId{id}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{width}\" cy=\"{height}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:anchor></w:drawing></w:r></w:p>"
    );
}

fn word_text(body: &mut String, text: &str) {
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            body.push_str("<w:br/>");
        }
        let _ = write!(
            body,
            "<w:t xml:space=\"preserve\">{}</w:t>",
            xml_escape(line)
        );
    }
}

/// One slide: its size in EMU and the PNG that fills it.
pub struct Slide {
    pub width_emu: u64,
    pub height_emu: u64,
    pub png: Vec<u8>,
}

/// A presentation with one picture slide per page on a blank layout.
pub fn pptx(slides: &[Slide]) -> Vec<u8> {
    const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
    const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    let (width, height) = slides
        .last()
        .map_or((9_144_000, 6_858_000), |s| (s.width_emu, s.height_emu));
    let mut zip = Zip::default();
    let mut types = format!(
        "{XML_HEAD}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"png\" ContentType=\"image/png\"/><Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/><Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/><Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/><Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>"
    );
    for i in 1..=slides.len() {
        let _ = write!(
            types,
            "<Override PartName=\"/ppt/slides/slide{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>"
        );
    }
    types.push_str("</Types>");
    zip.add("[Content_Types].xml", types.as_bytes());
    zip.add(
        "_rels/.rels",
        format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/officeDocument\" Target=\"ppt/presentation.xml\"/></Relationships>").as_bytes(),
    );
    let mut presentation = format!(
        "{XML_HEAD}<p:presentation xmlns:a=\"{A}\" xmlns:r=\"{REL}\" xmlns:p=\"{P}\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>"
    );
    let mut rels = format!(
        "{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/><Relationship Id=\"rId2\" Type=\"{REL}/theme\" Target=\"theme/theme1.xml\"/>"
    );
    for i in 1..=slides.len() {
        let _ = write!(
            presentation,
            "<p:sldId id=\"{}\" r:id=\"rId{}\"/>",
            255 + i,
            i + 2
        );
        let _ = write!(
            rels,
            "<Relationship Id=\"rId{}\" Type=\"{REL}/slide\" Target=\"slides/slide{i}.xml\"/>",
            i + 2
        );
    }
    let _ = write!(
        presentation,
        "</p:sldIdLst><p:sldSz cx=\"{width}\" cy=\"{height}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>"
    );
    rels.push_str("</Relationships>");
    zip.add("ppt/presentation.xml", presentation.as_bytes());
    zip.add("ppt/_rels/presentation.xml.rels", rels.as_bytes());
    let master = format!(
        "{XML_HEAD}<p:sldMaster xmlns:a=\"{A}\" xmlns:r=\"{REL}\" xmlns:p=\"{P}\"><p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/><p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr sz=\"4400\"/></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:defRPr sz=\"3200\"/></a:lvl1pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr><a:defRPr sz=\"1800\"/></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>"
    );
    zip.add("ppt/slideMasters/slideMaster1.xml", master.as_bytes());
    zip.add(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/><Relationship Id=\"rId2\" Type=\"{REL}/theme\" Target=\"../theme/theme1.xml\"/></Relationships>").as_bytes(),
    );
    zip.add(
        "ppt/slideLayouts/slideLayout1.xml",
        format!("{XML_HEAD}<p:sldLayout xmlns:a=\"{A}\" xmlns:r=\"{REL}\" xmlns:p=\"{P}\" type=\"blank\" preserve=\"1\"><p:cSld name=\"Blank\"><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>").as_bytes(),
    );
    zip.add(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/></Relationships>").as_bytes(),
    );
    let colors = [
        "dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    let values = [
        "000000", "FFFFFF", "1F497D", "EEECE1", "4F81BD", "C0504D", "9BBB59", "8064A2", "4BACC6",
        "F79646", "0000FF", "800080",
    ];
    let mut theme = format!(
        "{XML_HEAD}<a:theme xmlns:a=\"{A}\" name=\"Office Theme\"><a:themeElements><a:clrScheme name=\"Office\">"
    );
    for (name, value) in colors.iter().zip(values) {
        let _ = write!(theme, "<a:{name}><a:srgbClr val=\"{value}\"/></a:{name}>");
    }
    theme.push_str("</a:clrScheme><a:fontScheme name=\"Office\"><a:majorFont><a:latin typeface=\"Calibri\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont><a:minorFont><a:latin typeface=\"Calibri\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme><a:fmtScheme name=\"Office\"><a:fillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w=\"9525\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln><a:ln w=\"25400\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln><a:ln w=\"38100\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>");
    zip.add("ppt/theme/theme1.xml", theme.as_bytes());
    for (i, slide) in slides.iter().enumerate() {
        let n = i + 1;
        let xml = format!(
            "{XML_HEAD}<p:sld xmlns:a=\"{A}\" xmlns:r=\"{REL}\" xmlns:p=\"{P}\"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr><p:pic><p:nvPicPr><p:cNvPr id=\"2\" name=\"Picture {n}\"/><p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed=\"rId2\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{}\" cy=\"{}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>",
            slide.width_emu, slide.height_emu
        );
        zip.add(&format!("ppt/slides/slide{n}.xml"), xml.as_bytes());
        zip.add(
            &format!("ppt/slides/_rels/slide{n}.xml.rels"),
            format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/><Relationship Id=\"rId2\" Type=\"{REL}/image\" Target=\"../media/image{n}.png\"/></Relationships>").as_bytes(),
        );
        zip.add(&format!("ppt/media/image{n}.png"), &slide.png);
    }
    zip.finish()
}
