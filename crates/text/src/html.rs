use crate::{Block, Char, FontInfo, Line, TextError, TextPage};
use base64::Engine;
use std::fmt::Write;

impl TextPage {
    /// MuPDF's positioned HTML fragment for one page.
    pub fn html(&self, page_number: usize) -> String {
        let mut out = format!(
            "<div id=\"page{page_number}\" style=\"width:{:.1}pt;height:{:.1}pt\">\n",
            self.rect.x1 - self.rect.x0,
            self.rect.y1 - self.rect.y0
        );
        for block in &self.blocks {
            match block {
                Block::Text(block) => {
                    for line in &block.lines {
                        let Some(first) = line.chars.first() else {
                            continue;
                        };
                        let _ = write!(
                            out,
                            "<p style=\"top:{:.1}pt;left:{:.1}pt;line-height:{:.1}pt\">",
                            first.origin.y - 0.8 * first.size,
                            line.bbox.x0,
                            first.size
                        );
                        let mut style: Option<(&Char, &FontInfo, bool)> = None;
                        for ch in &line.chars {
                            let Some(font) = self.fonts.get(ch.font) else {
                                continue;
                            };
                            let sup = superscript(line, ch);
                            let changed = style.is_none_or(|(prior, _, old_sup)| {
                                prior.font != ch.font
                                    || prior.size != ch.size
                                    || prior.color != ch.color
                                    || prior.alpha != ch.alpha
                                    || old_sup != sup
                            });
                            if changed {
                                if let Some((_, font, sup)) = style {
                                    close(&mut out, font, sup);
                                }
                                open(&mut out, font, ch, sup);
                                style = Some((ch, font, sup));
                            }
                            escape_char(&mut out, ch.c);
                        }
                        if let Some((_, font, sup)) = style {
                            close(&mut out, font, sup);
                        }
                        out.push_str("</p>\n");
                    }
                }
                Block::Image(image) => {
                    let m = image.transform;
                    let w = image.width as f32;
                    let h = image.height as f32;
                    if w == 0.0 || h == 0.0 {
                        continue;
                    }
                    let scale = 4.0_f32 / 3.0;
                    let a = m.a as f32 / w * scale;
                    let b = m.b as f32 / w * scale;
                    let c = m.c as f32 / h * scale;
                    let d = m.d as f32 / h * scale;
                    let e = (m.e as f32 + (m.a as f32 + m.c as f32) / 2.0) * scale - w / 2.0;
                    let f = (m.f as f32 + (m.b as f32 + m.d as f32) / 2.0) * scale - h / 2.0;
                    let _ = writeln!(
                        out,
                        "<img style=\"position:absolute;transform:matrix({a},{b},{c},{d},{e},{f})\" src=\"{}\">",
                        image.data_uri
                    );
                }
            }
        }
        out.push_str("</div>\n");
        out
    }
}

fn superscript(line: &Line, ch: &Char) -> bool {
    line.wmode == 0
        && line.dir.x == 1.0
        && line.dir.y == 0.0
        && line
            .chars
            .first()
            .is_some_and(|first| ch.origin.y < first.origin.y - ch.size * 0.1)
}
fn family(font: &FontInfo) -> String {
    let mut name = font
        .full_name
        .split_once('+')
        .map_or(font.full_name.as_str(), |(_, name)| name)
        .to_owned();
    if name.starts_with("Times") {
        name = "Times New Roman".to_owned();
    } else if name.starts_with("Arial") || name.starts_with("Helvetica") {
        name = if name.contains("Narrow") || name.contains("Condensed") {
            "Arial Narrow"
        } else {
            "Arial"
        }
        .to_owned();
    } else if name.starts_with("Courier") {
        name = "Courier".to_owned();
    } else if let Some(index) = name.rfind('-') {
        name.truncate(index);
    }
    name.push_str(if font.mono {
        ",monospace"
    } else if font.serif {
        ",serif"
    } else {
        ",sans-serif"
    });
    name
}
fn open(out: &mut String, font: &FontInfo, ch: &Char, sup: bool) {
    if sup {
        out.push_str("<sup>");
    }
    if font.mono {
        out.push_str("<tt>");
    }
    if font.bold {
        out.push_str("<b>");
    }
    if font.italic {
        out.push_str("<i>");
    }
    let _ = write!(
        out,
        "<span style=\"font-family:{};font-size:{:.1}pt",
        family(font),
        ch.size
    );
    let _ = write!(out, ";color:#{:06x}", ch.color);
    out.push_str("\">");
}
fn close(out: &mut String, font: &FontInfo, sup: bool) {
    out.push_str("</span>");
    if font.italic {
        out.push_str("</i>");
    }
    if font.bold {
        out.push_str("</b>");
    }
    if font.mono {
        out.push_str("</tt>");
    }
    if sup {
        out.push_str("</sup>");
    }
}
fn escape_char(out: &mut String, c: char) {
    match c {
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '&' => out.push_str("&amp;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&apos;"),
        c if (' '..='\u{7f}').contains(&c) => out.push(c),
        c => {
            let _ = write!(out, "&#x{:x};", u32::from(c));
        }
    }
}

pub(crate) fn image_uri(image: &pdf_interp::PdfImage) -> Result<String, TextError> {
    if image.is_mask() {
        let pixels = image.decode_stencil()?;
        let bytes = pdf_codec::encode_png(
            &pixels.data,
            pixels.width,
            pixels.height,
            pdf_codec::PngColor::Gray,
            Some((96.0, 96.0)),
        )
        .map_err(|e| TextError::Interp(e.to_string()))?;
        return Ok(data_uri("png", &bytes));
    }
    if image.filter() == Some(b"DCTDecode") && image.stream().raw().starts_with(&[0xff, 0xd8]) {
        return Ok(data_uri("jpeg", image.stream().raw()));
    }
    let pixels = image.decode_rgb()?;
    let (samples, color) = if let Some(alpha) = pixels.alpha {
        let mut rgba = Vec::with_capacity(pixels.rgb.len() / 3 * 4);
        for (i, rgb) in pixels.rgb.as_chunks::<3>().0.iter().enumerate() {
            let x = i % pixels.width as usize;
            let y = i / pixels.width as usize;
            let ax = x * alpha.width as usize / pixels.width as usize;
            let ay = y * alpha.height as usize / pixels.height as usize;
            rgba.extend_from_slice(rgb);
            rgba.push(
                alpha
                    .data
                    .get(ay * alpha.width as usize + ax)
                    .copied()
                    .unwrap_or(255),
            );
        }
        (rgba, pdf_codec::PngColor::Rgba)
    } else {
        (pixels.rgb, pdf_codec::PngColor::Rgb)
    };
    let bytes = pdf_codec::encode_png(
        &samples,
        pixels.width,
        pixels.height,
        color,
        Some((96.0, 96.0)),
    )
    .map_err(|e| TextError::Interp(e.to_string()))?;
    Ok(data_uri("png", &bytes))
}

pub(crate) fn shading_uri(
    event: &pdf_interp::ShadingEvent<'_>,
    x0: f32,
    y0: f32,
    width: u32,
    height: u32,
) -> Result<String, TextError> {
    let count = u64::from(width) * u64::from(height);
    if count > 64 * 1024 * 1024 {
        return Err(TextError::Interp(
            "shading text image exceeds 64 megapixels".into(),
        ));
    }
    let inverse = event
        .matrix
        .invert()
        .ok_or_else(|| TextError::Interp("singular shading transform".into()))?;
    let mut pixels = Vec::with_capacity(count as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let p = crate::Point::new(
                f64::from(x0) + f64::from(x) + 0.5,
                f64::from(y0) + f64::from(y) + 0.5,
            )
            .transform(&inverse);
            if let Some(rgb) = event
                .shading
                .sample(p)
                .or_else(|| event.shading.background())
            {
                pixels.extend(rgb.map(|v| (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8));
                pixels.push(255);
            } else {
                pixels.extend([0, 0, 0, 0]);
            }
        }
    }
    let bytes = pdf_codec::encode_png(
        &pixels,
        width,
        height,
        pdf_codec::PngColor::Rgba,
        Some((96.0, 96.0)),
    )
    .map_err(|e| TextError::Interp(e.to_string()))?;
    Ok(data_uri("png", &bytes))
}

fn data_uri(format: &str, bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = format!("data:image/{format};base64,\n");
    for (i, chunk) in encoded.as_bytes().chunks(64).enumerate() {
        if i != 0 {
            out.push('\n');
        }
        for &byte in chunk {
            out.push(char::from(byte));
        }
    }
    out
}
