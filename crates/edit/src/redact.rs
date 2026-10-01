//! Redaction rewrites character strings and image samples; a cover is not removal.
//! Provenance from pdf-interp keeps font decoding and glyph geometry in one owner.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pdf_core::{
    Dict, Document, Matrix, ObjRef, Object, Operation, Rect, Stream, parse_content, write_content,
};
use pdf_interp::{
    ContentSource, Device, FormCall, GlyphPos, ImageEvent, ImageMaskEvent, Provenance, RunOptions,
    TextRun, unrotated_transform,
};

use crate::{EditError, image_redact, path_redact};

#[derive(Clone, Copy)]
struct GlyphEdit {
    pos: GlyphPos,
    removed: bool,
    adjustment: f64,
}

struct Show {
    glyphs: Vec<GlyphEdit>,
    clips: bool,
}

struct Collector<'a> {
    rects: &'a [Rect],
    shows: HashMap<Provenance, Show>,
    images: HashMap<Provenance, Option<Stream>>,
    error: Option<EditError>,
}

impl Device for Collector<'_> {
    fn text(&mut self, run: &TextRun<'_>) {
        let glyphs = run
            .glyphs
            .iter()
            .map(|glyph| {
                let bbox = if run.wmode == 0 {
                    Rect::new(
                        0.0,
                        run.font.descender(),
                        glyph.advance,
                        run.font.ascender(),
                    )
                } else {
                    let font = run.font.bbox();
                    Rect::new(font.x0, 0.0, font.x1, glyph.advance)
                };
                let mut bbox = bbox.transform(&glyph.trm);
                let dx = bbox.width() / 10.0;
                let dy = bbox.height() / 10.0;
                bbox.x0 += dx;
                bbox.x1 -= dx;
                bbox.y0 += dy;
                bbox.y1 -= dy;
                let removed = self.rects.iter().any(|rect| {
                    let intersection = bbox.intersect(rect);
                    intersection.x0 <= intersection.x1 && intersection.y0 <= intersection.y1
                });
                let spacing = run.char_spacing
                    + if glyph.word_space {
                        run.word_spacing
                    } else {
                        0.0
                    };
                GlyphEdit {
                    pos: glyph.pos,
                    removed,
                    adjustment: -1000.0
                        * (glyph.width
                            + if run.size == 0.0 {
                                0.0
                            } else {
                                spacing / run.size
                            }),
                }
            })
            .collect();
        self.shows.insert(
            run.source.clone(),
            Show {
                glyphs,
                clips: run.clip,
            },
        );
    }

    fn fill_image(&mut self, event: &ImageEvent<'_>) {
        self.image(event.image, event.ctm, event.source);
    }

    fn fill_image_mask(&mut self, event: &ImageMaskEvent<'_>) {
        self.image(event.image, event.ctm, event.source);
    }
}

impl Collector<'_> {
    fn image(&mut self, image: &pdf_interp::PdfImage, ctm: Matrix, source: &Provenance) {
        if self.error.is_some()
            || !self
                .rects
                .iter()
                .any(|rect| image_redact::intersects(ctm, *rect))
        {
            return;
        }
        match image_redact::redact(image, ctm, self.rects) {
            Ok(replacement) => {
                self.images.insert(source.clone(), replacement);
            }
            Err(error) => self.error = Some(error),
        }
    }
}

pub(crate) fn apply(
    doc: &mut Document,
    index: usize,
    rects: &[Rect],
    fill: [f64; 3],
) -> Result<(), EditError> {
    let page = doc.page(index)?;
    let inverse = unrotated_transform(&page)
        .invert()
        .ok_or_else(|| EditError::Message("page has a singular transform".into()))?;
    let rects = rects
        .iter()
        .map(|r| r.transform(&inverse))
        .collect::<Vec<_>>();
    let mut collector = Collector {
        rects: &rects,
        shows: HashMap::new(),
        images: HashMap::new(),
        error: None,
    };
    let transform = pdf_interp::page_transform(&page)
        .invert()
        .ok_or_else(|| EditError::Message("page has a singular transform".into()))?;
    pdf_interp::run_page_contents(
        doc,
        &page,
        &mut collector,
        &RunOptions {
            transform,
            annotations: false,
            include_hidden_content: true,
            ..RunOptions::default()
        },
    )?;
    if let Some(error) = collector.error {
        return Err(error);
    }
    let contents = doc.resolve_key(&page.dict, b"Contents")?;
    let streams = match contents {
        Object::Array(parts) => parts
            .into_iter()
            .map(|part| doc.resolve(&part))
            .collect::<Result<Vec<_>, _>>()?,
        Object::Null => Vec::new(),
        value => vec![value],
    };
    let mut filter = Filter::new(&page.resources, Matrix::IDENTITY);
    let mut context = Context {
        doc,
        rects: &rects,
        shows: &collector.shows,
        images: collector.images,
        page_struct_parent: page.dict.get_i64(b"StructParents"),
        parents: None,
        edited_structure: HashSet::new(),
    };
    for (part, value) in streams.into_iter().enumerate() {
        let stream = value
            .as_stream()
            .ok_or_else(|| EditError::Message("page contents is not a stream".into()))?;
        let decoded = context.doc.decode_stream(stream)?;
        filter.process(
            &mut context,
            parse_content(&decoded.data)?,
            ContentSource::Page { part },
            &[],
        )?;
    }
    filter.finish();
    let resource = filter.resources;
    let content = context
        .doc
        .add(Stream::new(Dict::new(), write_content(&filter.out)));
    let mut overlay = Vec::with_capacity(rects.len() * 6);
    for rect in &rects {
        overlay.push(op("q", vec![]));
        overlay.push(op(
            "re",
            numbers(&[rect.x0, rect.y0, rect.width(), rect.height()]),
        ));
        overlay.push(op("RG", numbers(&fill)));
        overlay.push(op("rg", numbers(&fill)));
        overlay.push(op("B", vec![]));
        overlay.push(op("Q", vec![]));
    }
    let boxes = context
        .doc
        .add(Stream::new(Dict::new(), write_content(&overlay)));
    let mut dict = page.dict;
    dict.insert("Resources", resource);
    dict.insert(
        "Contents",
        vec![Object::Reference(content), Object::Reference(boxes)],
    );
    remove_annotations(context.doc, &mut dict, &rects)?;
    context.doc.set(page.id, dict);
    Ok(())
}

fn remove_annotations(doc: &Document, page: &mut Dict, rects: &[Rect]) -> Result<(), EditError> {
    let value = doc.resolve_key(page, b"Annots")?;
    let Some(annots) = value.as_array() else {
        return Ok(());
    };
    let mut remove = HashSet::new();
    for annot in annots {
        let value = doc.resolve(annot)?;
        if let Some(dict) = value.as_dict() {
            let removable = matches!(
                dict.get_name(b"Subtype"),
                Some(b"Link" | b"FreeText" | b"Redact")
            );
            let area = dict.get_array(b"Rect").and_then(Rect::from_array);
            if removable && area.is_some_and(|area| touches(area, rects)) {
                if let Some(reference) = annot.as_reference() {
                    remove.insert(reference);
                }
                if let Some(popup) = dict.get_ref(b"Popup") {
                    remove.insert(popup);
                }
            }
        }
    }
    page.insert(
        "Annots",
        annots
            .iter()
            .filter(|annot| !annot.as_reference().is_some_and(|r| remove.contains(&r)))
            .cloned()
            .collect::<Vec<_>>(),
    );
    Ok(())
}

struct Context<'a> {
    doc: &'a mut Document,
    rects: &'a [Rect],
    shows: &'a HashMap<Provenance, Show>,
    images: HashMap<Provenance, Option<Stream>>,
    page_struct_parent: Option<i64>,
    parents: Option<HashMap<i64, Object>>,
    edited_structure: HashSet<ObjRef>,
}

impl Context<'_> {
    fn clear_accessibility(
        &mut self,
        source: ContentSource,
        properties: &Dict,
    ) -> Result<(), EditError> {
        let Some(mcid) = properties
            .get_i64(b"MCID")
            .and_then(|n| usize::try_from(n).ok())
        else {
            return Ok(());
        };
        let parent = match source {
            ContentSource::Form(id) => self
                .doc
                .get(id)?
                .as_stream()
                .and_then(|s| s.dict.get_i64(b"StructParents")),
            _ => self.page_struct_parent,
        };
        let Some(parent) = parent else {
            return Ok(());
        };
        if self.parents.is_none() {
            let catalog = self.doc.catalog()?;
            let root = self.doc.resolve_key(&catalog, b"StructTreeRoot")?;
            let Some(root) = root.as_dict() else {
                return Ok(());
            };
            let Some(tree) = root.get(b"ParentTree") else {
                return Ok(());
            };
            self.parents = Some(self.doc.number_tree(tree)?.into_iter().collect());
        }
        let Some(value) = self.parents.as_ref().and_then(|tree| tree.get(&parent)) else {
            return Ok(());
        };
        let array = self.doc.resolve(value)?;
        let Some(element) = array.as_array().and_then(|array| array.get(mcid)) else {
            return Ok(());
        };
        let Some(id) = element.as_reference() else {
            return Err(EditError::Message(
                "cannot safely redact a direct structure element".into(),
            ));
        };
        if self.edited_structure.insert(id) {
            let object = self.doc.get(id)?;
            if let Some(dict) = object.as_dict() {
                let mut dict = dict.clone();
                for key in [b"ActualText".as_slice(), b"Alt", b"E", b"T"] {
                    dict.remove(key);
                }
                self.doc.set(id, dict);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct State {
    ctm: Matrix,
    width: f64,
    join: i64,
    miter: f64,
}

struct Filter<'a> {
    old_resources: &'a Dict,
    resources: Dict,
    state: State,
    stack: Vec<State>,
    path: Vec<Operation>,
    out: Vec<Operation>,
    clipping: bool,
    text_removed: bool,
    text_kept: bool,
    text_clip: bool,
    tags: Vec<(usize, bool)>,
}

impl<'a> Filter<'a> {
    fn new(resources: &'a Dict, transform: Matrix) -> Self {
        let mut kept = resources.clone();
        kept.remove(b"XObject");
        kept.remove(b"Properties");
        Self {
            old_resources: resources,
            resources: kept,
            state: State {
                ctm: transform,
                width: 1.0,
                join: 0,
                miter: 10.0,
            },
            stack: Vec::new(),
            path: Vec::new(),
            out: vec![op("q", vec![])],
            clipping: false,
            text_removed: false,
            text_kept: false,
            text_clip: false,
            tags: Vec::new(),
        }
    }

    fn process(
        &mut self,
        context: &mut Context<'_>,
        ops: Vec<Operation>,
        source: ContentSource,
        forms: &[FormCall],
    ) -> Result<(), EditError> {
        if forms.len() > 64 {
            return Err(EditError::Message("form nesting limit exceeded".into()));
        }
        let chain: Arc<[FormCall]> = Arc::from(forms);
        for (index, mut operation) in ops.into_iter().enumerate() {
            let provenance = Provenance {
                source,
                op: index,
                glyph: None,
                forms: Arc::clone(&chain),
            };
            match operation.operator.as_slice() {
                b"q" => {
                    if self.stack.len() >= 1024 {
                        return Err(EditError::Message(
                            "graphics state nesting limit exceeded".into(),
                        ));
                    }
                    self.stack.push(self.state);
                }
                b"Q" => {
                    if let Some(state) = self.stack.pop() {
                        self.state = state;
                    } else {
                        continue;
                    }
                }
                b"cm" => {
                    if let Some(matrix) = Matrix::from_array(&operation.operands) {
                        self.state.ctm = matrix.concat(&self.state.ctm);
                    }
                }
                b"w" => self.state.width = number(&operation, 0),
                b"j" => self.state.join = number(&operation, 0) as i64,
                b"M" => self.state.miter = number(&operation, 0),
                b"gs" => {
                    if let Some(name) = operation.operands.first().and_then(Object::as_name) {
                        let states = context.doc.resolve_key(self.old_resources, b"ExtGState")?;
                        if let Some(value) = states.as_dict().and_then(|d| d.get(name)) {
                            let value = context.doc.resolve(value)?;
                            if let Some(dict) = value.as_dict() {
                                if let Some(width) = dict.get_f64(b"LW") {
                                    self.state.width = width;
                                }
                                if let Some(join) = dict.get_i64(b"LJ") {
                                    self.state.join = join;
                                }
                                if let Some(miter) = dict.get_f64(b"ML") {
                                    self.state.miter = miter;
                                }
                            }
                        }
                    }
                }
                b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => {
                    self.path.push(operation);
                    continue;
                }
                b"W" | b"W*" => {
                    self.clipping = true;
                    self.path.push(operation);
                    continue;
                }
                b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                    let stroke = matches!(
                        operation.operator.as_slice(),
                        b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*"
                    ) && !self.clipping;
                    let style =
                        stroke.then_some((self.state.width, self.state.join, self.state.miter));
                    let drop = operation.operator != b"n";
                    let path = std::mem::take(&mut self.path);
                    let kept = path_redact::filter(
                        path,
                        self.state.ctm,
                        style,
                        if drop { context.rects } else { &[] },
                    );
                    self.clipping = false;
                    if kept.is_empty() {
                        continue;
                    }
                    self.out.extend(kept);
                }
                b"BT" => {
                    self.text_removed = false;
                    self.text_kept = false;
                    self.text_clip = false;
                }
                b"Tj" | b"TJ" | b"'" | b"\"" => {
                    let strings = operation
                        .operands
                        .last()
                        .map(|value| {
                            value.as_array().map_or_else(
                                || value.as_string().is_some_and(|s| !s.as_bytes().is_empty()),
                                |array| {
                                    array.iter().any(|v| {
                                        v.as_string().is_some_and(|s| !s.as_bytes().is_empty())
                                    })
                                },
                            )
                        })
                        .unwrap_or(false);
                    if !strings {
                        self.out.push(operation);
                        continue;
                    }
                    let show = context.shows.get(&provenance).ok_or_else(|| {
                        EditError::Message(
                            "cannot safely redact text without glyph provenance".into(),
                        )
                    })?;
                    self.text_removed |= show.glyphs.iter().any(|g| g.removed);
                    self.text_kept |= show.glyphs.iter().any(|g| !g.removed);
                    self.text_clip |= show.clips;
                    if show.glyphs.iter().any(|g| g.removed) {
                        for (_, removed) in &mut self.tags {
                            *removed = true;
                        }
                        if operation.operator == b"'" || operation.operator == b"\"" {
                            if operation.operator == b"\"" {
                                self.out.push(op("Tw", vec![num(number(&operation, 0))]));
                                self.out.push(op("Tc", vec![num(number(&operation, 1))]));
                            }
                            self.out.push(op("T*", vec![]));
                        }
                        let input = if operation.operator == b"TJ" {
                            operation
                                .operands
                                .first()
                                .and_then(Object::as_array)
                                .map(<[Object]>::to_vec)
                                .unwrap_or_default()
                        } else {
                            operation.operands.last().cloned().into_iter().collect()
                        };
                        let array = rewrite_show(input, show)?;
                        if !array.is_empty() {
                            self.out.push(op("TJ", vec![Object::Array(array)]));
                        }
                        continue;
                    }
                }
                b"ET" if self.text_clip && self.text_removed && !self.text_kept => {
                    self.out.push(operation);
                    self.out.push(op("re", numbers(&[0.0; 4])));
                    self.out.push(op("W", vec![]));
                    self.out.push(op("n", vec![]));
                    continue;
                }
                b"Do" => {
                    let Some(name) = operation.operands.first().and_then(Object::as_name) else {
                        continue;
                    };
                    let old = context.doc.resolve_key(self.old_resources, b"XObject")?;
                    let Some(value) = old.as_dict().and_then(|d| d.get(name)) else {
                        continue;
                    };
                    let object = context.doc.resolve(value)?;
                    let Some(stream) = object.as_stream() else {
                        continue;
                    };
                    let replacement = if stream.dict.get_name(b"Subtype") == Some(b"Form") {
                        let id = value.as_reference().unwrap_or(ObjRef {
                            num: 0,
                            generation: 0,
                        });
                        if forms.iter().any(|call| call.form == id) {
                            return Err(EditError::Message(
                                "cyclic form cannot be safely redacted".into(),
                            ));
                        }
                        let resources = context.doc.resolve_key(&stream.dict, b"Resources")?;
                        let resources = resources.as_dict().unwrap_or(self.old_resources);
                        let matrix = stream
                            .dict
                            .get_array(b"Matrix")
                            .and_then(Matrix::from_array)
                            .unwrap_or(Matrix::IDENTITY);
                        let mut nested = Filter::new(resources, matrix.concat(&self.state.ctm));
                        let mut calls = forms.to_vec();
                        calls.push(FormCall {
                            caller: source,
                            op: index,
                            form: id,
                        });
                        let decoded = context.doc.decode_stream(stream)?;
                        nested.process(
                            context,
                            parse_content(&decoded.data)?,
                            ContentSource::Form(id),
                            &calls,
                        )?;
                        nested.finish();
                        let mut dict = stream.dict.clone();
                        dict.remove(b"Filter");
                        dict.remove(b"DecodeParms");
                        dict.insert("Resources", nested.resources);
                        let reference = context
                            .doc
                            .add(Stream::new(dict, write_content(&nested.out)));
                        Object::Reference(reference)
                    } else if let Some(image) = context.images.remove(&provenance) {
                        let Some(image) = image else {
                            continue;
                        };
                        Object::Reference(add_image(context.doc, image))
                    } else {
                        if stream.dict.get_name(b"Subtype") == Some(b"Image")
                            && context
                                .rects
                                .iter()
                                .any(|rect| image_redact::intersects(self.state.ctm, *rect))
                        {
                            return Err(EditError::Message(
                                "cannot safely redact image without decoded samples".into(),
                            ));
                        }
                        value.clone()
                    };
                    let name = self.add_xobject(
                        replacement,
                        stream.dict.get_name(b"Subtype") == Some(b"Form"),
                    );
                    operation.operands = vec![Object::name(name)];
                }
                b"BI" => {
                    if let Some(image) = context.images.remove(&provenance) {
                        let Some(image) = image else {
                            continue;
                        };
                        let reference = add_image(context.doc, image);
                        let name = self.add_xobject(Object::Reference(reference), false);
                        operation = op("Do", vec![Object::name(name)]);
                    } else if context
                        .rects
                        .iter()
                        .any(|rect| image_redact::intersects(self.state.ctm, *rect))
                    {
                        return Err(EditError::Message(
                            "cannot safely redact inline image without decoded samples".into(),
                        ));
                    }
                }
                b"BDC" | b"DP" => {
                    if let Some(name) = operation.operands.get(1).and_then(Object::as_name) {
                        let old = context.doc.resolve_key(self.old_resources, b"Properties")?;
                        if let Some(value) = old.as_dict().and_then(|d| d.get(name)) {
                            let value = context.doc.resolve(value)?;
                            if value.as_dict().is_some_and(|d| {
                                d.contains_key(b"ActualText") || d.contains_key(b"Alt")
                            }) {
                                operation.operands[1] = value;
                            } else {
                                let mut kept = self
                                    .resources
                                    .remove(b"Properties")
                                    .and_then(|o| match o {
                                        Object::Dict(d) => Some(d),
                                        _ => None,
                                    })
                                    .unwrap_or_default();
                                if let Some(original) = old.as_dict().and_then(|d| d.get(name)) {
                                    kept.insert(name, original.clone());
                                }
                                self.resources.insert("Properties", kept);
                            }
                        }
                    }
                    if operation.operator == b"BDC" {
                        self.tags.push((self.out.len(), false));
                    }
                }
                b"BMC" => self.tags.push((self.out.len(), false)),
                b"EMC" => {
                    if let Some((start, true)) = self.tags.pop() {
                        // Replacement strings in marked content must not retain the
                        // removed text. Clone a named property before changing it.
                        if let Some(tag) = self.out.get_mut(start)
                            && let Some(properties) = tag.operands.get_mut(1)
                        {
                            let value = if let Some(name) = properties.as_name() {
                                let resources =
                                    context.doc.resolve_key(self.old_resources, b"Properties")?;
                                resources
                                    .as_dict()
                                    .and_then(|d| d.get(name))
                                    .cloned()
                                    .unwrap_or(Object::Null)
                            } else {
                                properties.clone()
                            };
                            let value = context.doc.resolve(&value)?;
                            if let Some(dict) = value.as_dict() {
                                context.clear_accessibility(source, dict)?;
                                let mut dict = dict.clone();
                                dict.remove(b"ActualText");
                                dict.remove(b"Alt");
                                *properties = Object::Dict(dict);
                            }
                        }
                    }
                }
                _ => {}
            }
            self.out.push(operation);
        }
        Ok(())
    }

    fn add_xobject(&mut self, value: Object, form: bool) -> String {
        let mut objects = self
            .resources
            .remove(b"XObject")
            .and_then(|o| match o {
                Object::Dict(d) => Some(d),
                _ => None,
            })
            .unwrap_or_default();
        let name = format!("{}{}", if form { "Fm" } else { "Im" }, objects.len() + 1);
        objects.insert(name.as_str(), value);
        self.resources.insert("XObject", objects);
        name
    }

    fn finish(&mut self) {
        for _ in self.stack.drain(..) {
            self.out.push(op("Q", vec![]));
        }
        self.out.push(op("Q", vec![]));
    }
}

fn rewrite_show(input: Vec<Object>, show: &Show) -> Result<Vec<Object>, EditError> {
    let mut output = Vec::new();
    for (element, item) in input.into_iter().enumerate() {
        let Some(string) = item.as_string() else {
            output.push(item);
            continue;
        };
        let bytes = string.as_bytes();
        let mut start = 0;
        for glyph in show
            .glyphs
            .iter()
            .filter(|g| g.pos.element as usize == element && g.removed)
        {
            let offset = glyph.pos.byte_offset as usize;
            let end = offset + glyph.pos.byte_len as usize;
            if offset < start || end > bytes.len() {
                return Err(EditError::Message(
                    "glyph provenance does not match the content string".into(),
                ));
            }
            if offset > start {
                output.push(Object::string(bytes[start..offset].to_vec()));
            }
            output.push(num(glyph.adjustment));
            start = end;
        }
        if start < bytes.len() {
            output.push(Object::string(bytes[start..].to_vec()));
        }
    }
    Ok(output)
}

fn add_image(doc: &mut Document, mut image: Stream) -> ObjRef {
    if let Some(Object::Stream(mask)) = image.dict.remove(b"SMask") {
        let reference = doc.add(mask);
        image.dict.insert("SMask", reference);
    }
    doc.add(image)
}

pub(crate) fn touches(area: Rect, rects: &[Rect]) -> bool {
    rects.iter().any(|rect| !area.intersect(rect).is_empty())
}

pub(crate) fn covered(area: Rect, rects: &[Rect]) -> bool {
    // MuPDF returns on the first touching rectangle, even when a later one
    // would completely contain the path.
    for rect in rects {
        if !area.intersect(rect).is_empty() {
            return rect.x0 <= area.x0
                && rect.y0 <= area.y0
                && rect.x1 >= area.x1
                && rect.y1 >= area.y1;
        }
    }
    false
}

pub(crate) fn num(value: f64) -> Object {
    Object::Real(value)
}
pub(crate) fn numbers(values: &[f64]) -> Vec<Object> {
    values.iter().copied().map(num).collect()
}
pub(crate) fn op(operator: &str, operands: Vec<Object>) -> Operation {
    Operation::new(operator, operands)
}
fn number(operation: &Operation, index: usize) -> f64 {
    operation
        .operands
        .get(index)
        .and_then(Object::as_f64)
        .unwrap_or(0.0)
}

pub(crate) fn append_content(
    doc: &mut Document,
    index: usize,
    bytes: Vec<u8>,
) -> Result<(), EditError> {
    let page = doc.page(index)?;
    let mut dict = page.dict;
    let contents = doc.resolve_key(&dict, b"Contents")?;
    let mut parts = if let Object::Array(parts) = contents {
        parts
    } else {
        dict.get(b"Contents").cloned().into_iter().collect()
    };
    parts.push(Object::Reference(doc.add(Stream::new(Dict::new(), bytes))));
    dict.insert("Contents", parts);
    doc.set(page.id, dict);
    Ok(())
}

/// Once every leaf has its effective resources, inherited dictionaries can be
/// removed. Otherwise they keep the unredacted form/image reachable during GC.
pub(crate) fn materialize_resources(doc: &mut Document) -> Result<(), EditError> {
    let pages = (0..doc.page_count()?)
        .map(|index| doc.page(index))
        .collect::<Result<Vec<_>, _>>()?;
    let mut parents = HashSet::new();
    for page in pages {
        let mut parent = page.dict.get_ref(b"Parent");
        let mut dict = page.dict;
        dict.insert("Resources", page.resources);
        doc.set(page.id, dict);
        for _ in 0..128 {
            let Some(id) = parent else {
                break;
            };
            if !parents.insert(id) {
                parent = None;
                break;
            }
            parent = doc.get(id)?.as_dict().and_then(|d| d.get_ref(b"Parent"));
        }
        if parent.is_some() {
            return Err(EditError::Message(
                "page parent nesting limit exceeded".into(),
            ));
        }
    }
    for id in parents {
        let object = doc.get(id)?;
        if let Some(dict) = object.as_dict() {
            let mut dict = dict.clone();
            dict.remove(b"Resources");
            doc.set(id, dict);
        }
    }
    Ok(())
}
