//! Emits page content, TrueType subsets, image XObjects, annotations, and outline trees.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use goat_common::GoatError;
use pdf_codec::PdfColorSpace;
use pdf_core::{Dict, Document, ObjRef, Object, PdfDate, PdfString, SaveOptions, Stream};
use pdf_font::{GlyphMapping, embed_truetype};

use super::boxes::Link;
use super::fonts::Fonts;
use super::image::{Images, Pixels};
use super::layout::{Canvas, Item, paginate};
use super::shaping::Unicode;
use super::style::{Color, PageStyle};
use super::svg;
use crate::pdf_error;

struct EmbeddedFont {
    reference: ObjRef,
    codes: HashMap<(u16, Unicode), (u16, f32)>,
}

fn array(values: impl IntoIterator<Item = f32>) -> Object {
    Object::Array(values.into_iter().map(Object::from).collect())
}

fn embed_font(
    doc: &mut Document,
    fonts: &Fonts,
    face: usize,
    chars: &BTreeMap<(u16, Unicode), ()>,
) -> Result<EmbeddedFont, GoatError> {
    let face = &fonts.faces[face];
    let unicode: Vec<String> = chars.keys().map(|(_, source)| source.text()).collect();
    let mut mappings = Vec::with_capacity(chars.len());
    let mut codes = HashMap::with_capacity(chars.len());
    for (index, ((gid, source), text)) in chars.keys().zip(&unicode).enumerate() {
        let code = u16::try_from(index)
            .map_err(|_| GoatError::message("too many distinct characters in one font"))?;
        mappings.push(GlyphMapping {
            code,
            glyph_id: *gid,
            unicode: text,
        });
        codes.insert(
            (*gid, source.clone()),
            (code, face.advance_em(*gid) * 1000.0),
        );
    }
    let reference = embed_truetype(doc, &face.font, &mappings)
        .map_err(|error| GoatError::message(error.to_string()))?;
    Ok(EmbeddedFont { reference, codes })
}

fn embed_images(doc: &mut Document, images: Images) -> Vec<ObjRef> {
    let mut references = Vec::with_capacity(images.list.len());
    for image in images.list {
        let mut dict = Dict::new();
        dict.insert("Type", Object::name("XObject"));
        dict.insert("Subtype", Object::name("Image"));
        dict.insert("Width", image.width);
        dict.insert("Height", image.height);
        dict.insert("BitsPerComponent", 8);
        let data = match image.pixels {
            Pixels::Jpeg {
                data,
                color_space,
                inverted,
            } => {
                dict.insert("Filter", Object::name("DCTDecode"));
                dict.insert(
                    "ColorSpace",
                    Object::name(match color_space {
                        PdfColorSpace::DeviceGray => "DeviceGray",
                        PdfColorSpace::DeviceRGB => "DeviceRGB",
                        PdfColorSpace::DeviceCMYK => "DeviceCMYK",
                    }),
                );
                if inverted {
                    dict.insert("Decode", array([1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0]));
                }
                data
            }
            Pixels::Raw {
                samples,
                gray,
                alpha,
            } => {
                dict.insert(
                    "ColorSpace",
                    Object::name(if gray { "DeviceGray" } else { "DeviceRGB" }),
                );
                if let Some(alpha) = alpha {
                    let mut mask = Dict::new();
                    mask.insert("Type", Object::name("XObject"));
                    mask.insert("Subtype", Object::name("Image"));
                    mask.insert("Width", image.width);
                    mask.insert("Height", image.height);
                    mask.insert("BitsPerComponent", 8);
                    mask.insert("ColorSpace", Object::name("DeviceGray"));
                    dict.insert("SMask", doc.add(Stream::new(mask, alpha)));
                }
                samples
            }
        };
        references.push(doc.add(Stream::new(dict, data)));
    }
    references
}

#[derive(Clone, Copy)]
struct Destination {
    page: usize,
    x: f32,
    y: f32,
}

fn dest_object(dest: Destination, pages: &[ObjRef]) -> Object {
    Object::Array(vec![
        Object::Reference(pages[dest.page]),
        Object::name("XYZ"),
        Object::from(dest.x),
        Object::from(dest.y),
        Object::Integer(0),
    ])
}

struct Bookmark {
    level: u8,
    title: String,
    dest: Destination,
}

pub fn write(
    mut canvas: Canvas,
    fonts: &Fonts,
    mut images: Images,
    page: PageStyle,
    title: &str,
) -> Result<Vec<u8>, GoatError> {
    let content_height = (page.height - page.margin[0] - page.margin[2]).max(1.0);
    let slices = paginate(&canvas, content_height);
    let mut pages_items = Vec::with_capacity(slices.len());
    for slice in &slices {
        let mut items = Vec::new();
        for deco in &canvas.decos {
            items.extend(deco.items(slice.start, slice.end));
        }
        let mut top = slice.start - slice.header;
        for &index in &slice.repeats {
            let repeat = &canvas.repeats[index];
            for item in &repeat.items {
                let mut item = item.clone();
                item.translate(0.0, top - repeat.top);
                items.push(item);
            }
            top += repeat.height;
        }
        for unit in &mut canvas.units[slice.units.clone()] {
            items.append(&mut unit.items);
        }
        for overlay in &canvas.overlays {
            if overlay.anchor >= slice.start && overlay.anchor < slice.end {
                let dy = overlay.align.map_or(0.0, |align| {
                    (align.bottom.min(slice.end) - overlay.anchor - align.occupied).max(0.0)
                        * align.ratio
                });
                for item in &overlay.items {
                    let mut item = item.clone();
                    item.translate(0.0, dy);
                    items.push(item);
                }
            }
        }
        for item in &canvas.fixed {
            let mut item = item.clone();
            item.translate(0.0, slice.start - slice.header);
            items.push(item);
        }
        pages_items.push(items);
    }
    let mut doc = Document::new();
    let page_refs: Vec<ObjRef> = slices.iter().map(|_| doc.reserve()).collect();
    let mut used: BTreeMap<usize, BTreeMap<(u16, Unicode), ()>> = BTreeMap::new();
    let mut alphas: BTreeMap<u32, usize> = BTreeMap::new();
    let mut anchors = HashMap::new();
    let mut bookmarks = Vec::new();
    for (index, items) in pages_items.iter().enumerate() {
        let destination = |x: f32, y: f32| Destination {
            page: index,
            x: (page.margin[3] + x) * 0.75,
            y: (page.height - page.margin[0] - y + slices[index].start - slices[index].header)
                * 0.75,
        };
        for item in items {
            match item {
                Item::Text {
                    face,
                    glyphs,
                    color,
                    ..
                } => {
                    let chars = used.entry(*face).or_default();
                    for glyph in glyphs {
                        chars.entry((glyph.gid, glyph.unicode.clone())).or_default();
                    }
                    if color.a < 1.0 {
                        let next = alphas.len();
                        alphas.entry(color.a.to_bits()).or_insert(next);
                    }
                }
                Item::Image { id, .. } => {
                    for glyph in &images.list[*id].text {
                        used.entry(glyph.face)
                            .or_default()
                            .entry((glyph.gid, glyph.unicode.clone()))
                            .or_default();
                    }
                }
                Item::Rect { color, .. } => {
                    if color.a < 1.0 {
                        let next = alphas.len();
                        alphas.entry(color.a.to_bits()).or_insert(next);
                    }
                }
                Item::Anchor { x, y, name } => {
                    anchors
                        .entry(name.clone())
                        .or_insert_with(|| destination(*x, *y));
                }
                Item::Bookmark { x, y, level, title } => bookmarks.push(Bookmark {
                    level: *level,
                    title: title.clone(),
                    dest: destination(*x, *y),
                }),
                _ => {}
            }
        }
    }
    let mut embedded = HashMap::new();
    let mut resources = Dict::new();
    let mut font_resources = Dict::new();
    for (id, chars) in used {
        let font = embed_font(&mut doc, fonts, id, &chars)?;
        font_resources.insert(format!("F{id}"), font.reference);
        embedded.insert(id, font);
    }
    resources.insert("Font", font_resources);
    let image_text: Vec<_> = images
        .list
        .iter_mut()
        .map(|image| std::mem::take(&mut image.text))
        .collect();
    let image_refs = embed_images(&mut doc, images);
    let mut xobjects = Dict::new();
    for (id, reference) in image_refs.into_iter().enumerate() {
        xobjects.insert(format!("Im{id}"), reference);
    }
    resources.insert("XObject", xobjects);
    let mut states = Dict::new();
    for (&bits, &id) in &alphas {
        let mut state = Dict::new();
        state.insert("Type", Object::name("ExtGState"));
        state.insert("ca", f32::from_bits(bits));
        states.insert(format!("A{id}"), doc.add(state));
    }
    resources.insert("ExtGState", states);
    let resources = doc.add(resources);
    for (index, items) in pages_items.iter().enumerate() {
        let mut stream = String::new();
        if canvas.background.visible() {
            let c = canvas.background;
            let _ = writeln!(
                stream,
                "q {} {} {} rg 0 0 {} {} re f Q",
                c.r,
                c.g,
                c.b,
                page.width * 0.75,
                page.height * 0.75
            );
        }
        let _ = writeln!(
            stream,
            "q 0.75 0 0 -0.75 {} {} cm",
            page.margin[3] * 0.75,
            (page.height - page.margin[0] + slices[index].start - slices[index].header) * 0.75
        );
        let mut annotations = Vec::new();
        for item in items {
            match item {
                Item::Link { x, y, w, h, target } => {
                    let top = (page.height - page.margin[0] - y + slices[index].start
                        - slices[index].header)
                        * 0.75;
                    let left = (page.margin[3] + x) * 0.75;
                    let mut annotation = Dict::new();
                    annotation.insert("Type", Object::name("Annot"));
                    annotation.insert("Subtype", Object::name("Link"));
                    annotation.insert("Rect", array([left, top - h * 0.75, left + w * 0.75, top]));
                    annotation.insert("Border", array([0.0, 0.0, 0.0]));
                    annotation.insert("P", page_refs[index]);
                    match target {
                        Link::Anchor(name) => {
                            let Some(dest) = anchors.get(name) else {
                                continue;
                            };
                            annotation.insert("Dest", dest_object(*dest, &page_refs));
                        }
                        Link::Uri(uri) => {
                            let mut action = Dict::new();
                            action.insert("S", Object::name("URI"));
                            action.insert("URI", Object::string(uri.as_bytes()));
                            annotation.insert("A", action);
                        }
                    }
                    annotations.push(Object::Reference(doc.add(annotation)));
                }
                Item::Anchor { .. } | Item::Bookmark { .. } => {}
                _ => paint(&mut stream, item, &embedded, &alphas, &image_text),
            }
        }
        stream.push_str("Q\n");
        let content = doc.add(Stream::new(Dict::new(), stream.into_bytes()));
        let mut dict = Dict::new();
        dict.insert("Type", Object::name("Page"));
        dict.insert(
            "MediaBox",
            array([0.0, 0.0, page.width * 0.75, page.height * 0.75]),
        );
        dict.insert("Contents", content);
        dict.insert("Resources", resources);
        if !annotations.is_empty() {
            dict.insert("Annots", Object::Array(annotations));
        }
        doc.set(page_refs[index], dict);
        doc.insert_page(index, page_refs[index])
            .map_err(pdf_error)?;
    }
    outlines(&mut doc, &bookmarks, &page_refs)?;
    let mut info = Dict::new();
    if !title.is_empty() {
        info.insert("Title", Object::text(title));
    }
    info.insert("Producer", Object::text("pdf-goat"));
    info.insert("CreationDate", PdfString::literal(PdfDate::now().format()));
    doc.set_info(info);
    doc.save_to_bytes(&SaveOptions {
        compress_streams: true,
        new_id: true,
        version: Some((1, 7)),
        ..SaveOptions::default()
    })
    .map_err(pdf_error)
}

fn set_color(out: &mut String, color: Color, alphas: &BTreeMap<u32, usize>) {
    let _ = writeln!(out, "{} {} {} rg", color.r, color.g, color.b);
    if let Some(id) = alphas.get(&color.a.to_bits()) {
        let _ = writeln!(out, "/A{id} gs");
    }
}

fn paint(
    out: &mut String,
    item: &Item,
    fonts: &HashMap<usize, EmbeddedFont>,
    alphas: &BTreeMap<u32, usize>,
    image_text: &[Vec<svg::Glyph>],
) {
    match item {
        Item::Rect {
            x,
            y,
            w,
            h,
            color,
            radius,
        } if *w > 0.0 && *h > 0.0 && color.visible() => {
            out.push_str("q\n");
            set_color(out, *color, alphas);
            let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
            if r == 0.0 {
                let _ = writeln!(out, "{x} {y} {w} {h} re f");
            } else {
                let k = r * 0.552_284_8;
                let (right, bottom) = (x + w, y + h);
                let _ = writeln!(
                    out,
                    "{} {} m {} {} l {} {} {} {} {} {} c {} {} l {} {} {} {} {} {} c {} {} l {} {} {} {} {} {} c {} {} l {} {} {} {} {} {} c f",
                    x + r,
                    y,
                    right - r,
                    y,
                    right - r + k,
                    y,
                    right,
                    y + r - k,
                    right,
                    y + r,
                    right,
                    bottom - r,
                    right,
                    bottom - r + k,
                    right - r + k,
                    bottom,
                    right - r,
                    bottom,
                    x + r,
                    bottom,
                    x + r - k,
                    bottom,
                    x,
                    bottom - r + k,
                    x,
                    bottom - r,
                    x,
                    y + r,
                    x,
                    y + r - k,
                    x + r - k,
                    y,
                    x + r,
                    y
                );
            }
            out.push_str("Q\n");
        }
        Item::Image { x, y, w, h, id } => {
            let _ = writeln!(out, "q {w} 0 0 {} {x} {} cm /Im{id} Do Q", -h, y + h);
            if !image_text[*id].is_empty() {
                let _ = write!(out, "q {w} 0 0 {h} {x} {y} cm BT 3 Tr ");
                let mut current = None;
                for glyph in &image_text[*id] {
                    let font = &fonts[&glyph.face];
                    let (code, _) = font.codes[&(glyph.gid, glyph.unicode.clone())];
                    if current != Some(glyph.face) {
                        let _ = write!(out, "/F{} 1 Tf ", glyph.face);
                        current = Some(glyph.face);
                    }
                    if glyph.unicode == Unicode::Empty {
                        out.push_str("/Span << /ActualText <FEFF> >> BDC ");
                    }
                    let t = glyph.transform;
                    let _ = write!(
                        out,
                        "{} {} {} {} {} {} Tm <{code:04X}> Tj ",
                        t.sx, t.ky, t.kx, t.sy, t.tx, t.ty
                    );
                    if glyph.unicode == Unicode::Empty {
                        out.push_str("EMC ");
                    }
                }
                out.push_str("ET Q\n");
            }
        }
        Item::Text {
            x,
            y,
            face,
            size,
            color,
            glyphs,
            ..
        } if *size > 0.0 => {
            let Some(font) = fonts.get(face) else { return };
            out.push_str("q\n");
            set_color(out, *color, alphas);
            let _ = write!(out, "BT /F{face} {size} Tf ");
            if glyphs
                .iter()
                .all(|glyph| glyph.y_offset == 0.0 && glyph.unicode != Unicode::Empty)
            {
                let start = x + glyphs.first().map_or(0.0, |glyph| glyph.x_offset);
                let _ = write!(out, "1 0 0 -1 {start} {y} Tm [");
                for (index, glyph) in glyphs.iter().enumerate() {
                    let (code, nominal) = font.codes[&(glyph.gid, glyph.unicode.clone())];
                    let _ = write!(out, "<{code:04X}>");
                    let next_offset = glyphs.get(index + 1).map_or(0.0, |next| next.x_offset);
                    let adjust =
                        nominal - (glyph.advance + next_offset - glyph.x_offset) / size * 1000.0;
                    if adjust.abs() > 0.001 {
                        let _ = write!(out, " {adjust:.4} ");
                    }
                }
                out.push_str("] TJ ");
            } else {
                let mut advance = 0.0;
                for glyph in glyphs {
                    let (code, _) = font.codes[&(glyph.gid, glyph.unicode.clone())];
                    if glyph.unicode == Unicode::Empty {
                        out.push_str("/Span << /ActualText <FEFF> >> BDC ");
                    }
                    let _ = write!(
                        out,
                        "1 0 0 -1 {} {} Tm <{code:04X}> Tj ",
                        x + advance + glyph.x_offset,
                        y - glyph.y_offset
                    );
                    if glyph.unicode == Unicode::Empty {
                        out.push_str("EMC ");
                    }
                    advance += glyph.advance;
                }
            }
            out.push_str("ET Q\n");
        }
        _ => {}
    }
}

fn outlines(doc: &mut Document, bookmarks: &[Bookmark], pages: &[ObjRef]) -> Result<(), GoatError> {
    if bookmarks.is_empty() {
        return Ok(());
    }
    let root = doc.reserve();
    let references: Vec<ObjRef> = bookmarks.iter().map(|_| doc.reserve()).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); bookmarks.len() + 1];
    let mut parents = Vec::with_capacity(bookmarks.len());
    let mut stack: Vec<usize> = Vec::new();
    for (index, bookmark) in bookmarks.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|&parent| bookmarks[parent].level >= bookmark.level)
        {
            stack.pop();
        }
        let parent = stack.last().map_or(bookmarks.len(), |&parent| parent);
        parents.push(parent);
        children[parent].push(index);
        stack.push(index);
    }
    let mut counts = vec![0usize; bookmarks.len() + 1];
    for index in (0..bookmarks.len()).rev() {
        counts[parents[index]] += counts[index] + 1;
    }
    for (parent, siblings) in children.iter().enumerate() {
        for (position, &index) in siblings.iter().enumerate() {
            let bookmark = &bookmarks[index];
            let mut dict = Dict::new();
            dict.insert("Title", Object::text(&bookmark.title));
            dict.insert(
                "Parent",
                if parent == bookmarks.len() {
                    root
                } else {
                    references[parent]
                },
            );
            dict.insert("Dest", dest_object(bookmark.dest, pages));
            if position > 0 {
                dict.insert("Prev", references[siblings[position - 1]]);
            }
            if let Some(&next) = siblings.get(position + 1) {
                dict.insert("Next", references[next]);
            }
            if let (Some(&first), Some(&last)) = (children[index].first(), children[index].last()) {
                dict.insert("First", references[first]);
                dict.insert("Last", references[last]);
                dict.insert("Count", counts[index]);
            }
            doc.set(references[index], dict);
        }
    }
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Outlines"));
    dict.insert("Count", bookmarks.len());
    if let (Some(&first), Some(&last)) = (
        children[bookmarks.len()].first(),
        children[bookmarks.len()].last(),
    ) {
        dict.insert("First", references[first]);
        dict.insert("Last", references[last]);
    }
    doc.set(root, dict);
    let reference = doc.catalog_ref().map_err(pdf_error)?;
    let mut catalog = doc.catalog().map_err(pdf_error)?;
    catalog.insert("Outlines", root);
    doc.set(reference, catalog);
    Ok(())
}
