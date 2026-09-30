use pdf_core::{Dict, Document, Object, Point, Rect, write_content};

use crate::EditError;
use crate::pyfmt::{format_g, format_g_seq, jm_tuple};
use crate::redact::{append_content, num, op, unrotated_transform};

pub(crate) struct Replacement {
    pub rect: Rect,
    pub text: String,
    pub size: f64,
    pub color: u32,
    pub origin: Point,
}

pub(crate) fn insert(
    doc: &mut Document,
    index: usize,
    edit: &Replacement,
) -> Result<(), EditError> {
    if edit.text.is_empty() {
        return Ok(());
    }
    let page = doc.page(index)?;
    let inverse = unrotated_transform(&page)
        .invert()
        .ok_or_else(|| EditError::Message("page has a singular transform".into()))?;
    let origin = edit.origin.transform(&inverse);
    let mut resources = page.resources;
    let value = doc.resolve_key(&resources, b"Font")?;
    let mut fonts = value.as_dict().cloned().unwrap_or_default();
    if !fonts.contains_key(b"helv") {
        let mut font = Dict::new();
        font.insert("Type", Object::name("Font"));
        font.insert("Subtype", Object::name("Type1"));
        font.insert("BaseFont", Object::name("Helvetica"));
        font.insert("Encoding", Object::name("WinAnsiEncoding"));
        fonts.insert("helv", doc.add(font));
    }
    resources.insert("Font", fonts);
    let mut dict = page.dict;
    dict.insert("Resources", resources);
    doc.set(page.id, dict);
    let color = [
        f64::from((edit.color >> 16) & 255) / 255.0,
        f64::from((edit.color >> 8) & 255) / 255.0,
        f64::from(edit.color & 255) / 255.0,
    ];
    let mut bytes = format!(
        "\nq\nBT\n1 0 0 1 {} Tm\n/helv {} Tf\n{} RG\n{} rg\n",
        format_g_seq(&jm_tuple(&[origin.x, origin.y])),
        format_g(edit.size),
        format_g_seq(&color),
        format_g_seq(&color)
    )
    .into_bytes();
    // PyMuPDF's simple-font path writes Latin-1 bytes, replacing codes >255
    // with a middle dot. It does not embed or reflow the original font.
    let normalized = edit.text.replace("\r\n", "\n");
    let mut lines = normalized.split_terminator([
        '\n', '\r', '\u{000b}', '\u{000c}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}',
        '\u{2028}', '\u{2029}',
    ]);
    let leading = edit.size * (1.075_f32 - (-0.299_f32)) as f64;
    let mut space = origin.y;
    if let Some(line) = lines.next() {
        bytes.extend(write_content(&[show(line)]));
        for line in lines {
            if space < leading {
                break;
            }
            bytes.extend(write_content(&[
                op("Td", vec![num(0.0), num(-leading)]),
                show(line),
            ]));
            space -= leading;
        }
    }
    bytes.extend_from_slice(b"ET\nQ\n");
    append_content(doc, index, bytes)
}

fn show(text: &str) -> pdf_core::Operation {
    let bytes = text
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(0xb7))
        .collect::<Vec<_>>();
    op("TJ", vec![Object::Array(vec![Object::string(bytes)])])
}
