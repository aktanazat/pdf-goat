//! Stateful interpretation of page and nested content streams.

use crate::colorspace::{ColorSpace, ColorSpaceCache};
use crate::device::*;
use crate::font::{self, PdfFont};
use crate::function::Function;
use crate::image::PdfImage;
use crate::ocg::OptionalContent;
use crate::path::{FillRule, LineCap, LineJoin, Path, StrokeStyle};
use crate::pattern::{Replay, ShadingPaint, SoftMask, TilingPaint};
use crate::shading::Shading;
use crate::{Intent, InterpError, RunOptions, page_transform};
use pdf_core::{
    Dict, Document, Matrix, ObjRef, Object, Operation, Page, Point, Rect, Stream, parse_content,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const MAX_NESTING: usize = 64;
static FONT_ID: AtomicU64 = AtomicU64::new(1);
static PAINT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub(crate) struct Cache {
    fonts: RefCell<HashMap<ObjRef, Arc<PdfFont>>>,
    direct_fonts: RefCell<Vec<(Dict, Arc<PdfFont>)>>,
    fallback: RefCell<Option<Arc<PdfFont>>>,
    images: RefCell<HashMap<ObjRef, Arc<PdfImage>>>,
    contents: RefCell<HashMap<ObjRef, Arc<[Operation]>>>,
    shadings: RefCell<HashMap<ObjRef, Arc<Shading>>>,
    colors: ColorSpaceCache,
}

pub(crate) struct Run<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) cache: &'a Cache,
    pub(crate) options: &'a RunOptions,
    pub(crate) oc: OptionalContent,
    operations: Cell<u64>,
}

impl<'a> Run<'a> {
    pub(crate) fn new(doc: &'a Document, cache: &'a Cache, options: &'a RunOptions) -> Run<'a> {
        Run {
            doc,
            cache,
            options,
            oc: OptionalContent::load(doc),
            operations: Cell::new(0),
        }
    }
    pub(crate) fn hidden(&self, object: &Object) -> bool {
        !self.options.include_hidden_content
            && self.oc.is_hidden(
                self.doc,
                if self.options.intent == Intent::Print {
                    "Print"
                } else {
                    "View"
                },
                object,
            )
    }
    pub(crate) fn page_contents(
        &self,
        page: &Page,
        device: &mut dyn Device,
    ) -> Result<(), InterpError> {
        if self.doc.needs_password() {
            return Err(pdf_core::Error::NeedsPassword.into());
        }
        let mut engine = Engine::new(
            self,
            page_transform(page).concat(&self.options.transform),
            page.resources.clone(),
        );
        let contents = page.dict.get(b"Contents").cloned().unwrap_or(Object::Null);
        let streams = match self.doc.resolve(&contents)? {
            Object::Array(a) => a,
            Object::Null => Vec::new(),
            _ => vec![contents],
        };
        let mut result = Ok(());
        for (part, o) in streams.iter().enumerate() {
            result = match self.doc.resolve_stream(o) {
                Ok(Some(stream)) => {
                    engine.source.source = ContentSource::Page { part };
                    engine.stream(&stream, o.as_reference(), device)
                }
                Ok(None) => Ok(()),
                Err(error) => Err(error.into()),
            };
            if result.is_err() {
                break;
            }
        }
        engine.finish(device);
        result
    }
    pub(crate) fn appearance(
        &self,
        page: &Page,
        annot: ObjRef,
        appearance: ObjRef,
        stream: &Stream,
        matrix: Matrix,
        device: &mut dyn Device,
    ) -> Result<(), InterpError> {
        let mut engine = Engine::new(
            self,
            matrix
                .concat(&page_transform(page))
                .concat(&self.options.transform),
            page.resources.clone(),
        );
        engine.source.source = ContentSource::Appearance {
            annot,
            stream: appearance,
        };
        let result = engine.form(
            appearance,
            stream,
            Some(ContentSource::Appearance {
                annot,
                stream: appearance,
            }),
            device,
        );
        engine.finish(device);
        result
    }

    fn shading(&self, object: &Object, resources: &Dict) -> Option<Arc<Shading>> {
        if let Some(id) = object.as_reference()
            && let Some(s) = self.cache.shadings.borrow().get(&id)
        {
            return Some(s.clone());
        }
        let shading = Arc::new(Shading::load(
            self.doc,
            object,
            resources,
            &self.cache.colors,
        )?);
        if let Some(id) = object.as_reference() {
            self.cache.shadings.borrow_mut().insert(id, shading.clone());
        }
        Some(shading)
    }

    fn font(&self, object: &Object, resources: &Dict) -> Arc<PdfFont> {
        if let Some(id) = object.as_reference()
            && let Some(f) = self.cache.fonts.borrow().get(&id)
        {
            return f.clone();
        }
        let dict = self.doc.resolve_dict(object).ok().flatten();
        if object.as_reference().is_none()
            && let Some(d) = &dict
            && let Some((_, font)) = self
                .cache
                .direct_fonts
                .borrow()
                .iter()
                .find(|(key, _)| key == d)
        {
            return font.clone();
        }
        let id = FONT_ID.fetch_add(1, Ordering::Relaxed);
        let font = match font::load(self.doc, object, resources, id) {
            Ok(f) => Arc::new(f),
            Err(_) => {
                let mut fallback = self.cache.fallback.borrow_mut();
                fallback
                    .get_or_insert_with(|| Arc::new(font::fallback(id)))
                    .clone()
            }
        };
        if let Some(id) = object.as_reference() {
            self.cache.fonts.borrow_mut().insert(id, font.clone());
        } else if let Some(dict) = dict {
            self.cache
                .direct_fonts
                .borrow_mut()
                .push((dict, font.clone()));
        }
        font
    }
}

#[derive(Clone)]
struct Material {
    space: Arc<ColorSpace>,
    values: Vec<f32>,
    alpha: f32,
    pattern: Option<PatternDef>,
}
impl Material {
    fn new() -> Material {
        Material {
            space: ColorSpace::gray(),
            values: vec![0.0],
            alpha: 1.0,
            pattern: None,
        }
    }
}

#[derive(Clone)]
struct TextState {
    font: Option<Arc<PdfFont>>,
    size: f32,
    char_space: f32,
    word_space: f32,
    hscale: f32,
    leading: f32,
    mode: u8,
    rise: f32,
}
impl Default for TextState {
    fn default() -> Self {
        Self {
            font: None,
            size: -1.0,
            char_space: 0.0,
            word_space: 0.0,
            hscale: 1.0,
            leading: 0.0,
            mode: 0,
            rise: 0.0,
        }
    }
}

// MuPDF keeps text state and every intermediate text calculation in float32.
// Rounding only the final origin loses small gaps that control word spacing.
#[derive(Clone, Copy)]
struct TextMatrix {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}
impl From<Matrix> for TextMatrix {
    fn from(m: Matrix) -> Self {
        Self {
            a: m.a as f32,
            b: m.b as f32,
            c: m.c as f32,
            d: m.d as f32,
            e: m.e as f32,
            f: m.f as f32,
        }
    }
}
impl From<TextMatrix> for Matrix {
    fn from(m: TextMatrix) -> Self {
        Self::new(
            m.a.into(),
            m.b.into(),
            m.c.into(),
            m.d.into(),
            m.e.into(),
            m.f.into(),
        )
    }
}
impl TextMatrix {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
    fn concat(self, m: Self) -> Self {
        Self {
            a: self.a.mul_add(m.a, self.b * m.c),
            b: self.a.mul_add(m.b, self.b * m.d),
            c: self.c.mul_add(m.a, self.d * m.c),
            d: self.c.mul_add(m.b, self.d * m.d),
            e: self.e.mul_add(m.a, self.f * m.c) + m.e,
            f: self.e.mul_add(m.b, self.f * m.d) + m.f,
        }
    }
    fn translated(mut self, x: f32, y: f32) -> Self {
        self.e += x.mul_add(self.a, y * self.c);
        self.f += x.mul_add(self.b, y * self.d);
        self
    }
    fn quad(self, width: f32, wmode: u8, ascent: f32, descent: f32) -> Quad {
        // Native stext fuses this endpoint calculation, unlike the separately
        // rounded PDF pen advance. That distinction can create a word gap.
        let (p, q, a, d) = if wmode == 0 {
            (
                (self.e, self.f),
                (width.mul_add(self.a, self.e), width.mul_add(self.b, self.f)),
                (ascent * self.c, ascent * self.d),
                (descent * self.c, descent * self.d),
            )
        } else {
            (
                (width.mul_add(self.c, self.e), width.mul_add(self.d, self.f)),
                (self.e, self.f),
                (self.a, self.b),
                (0.0, 0.0),
            )
        };
        let add =
            |p: (f32, f32), v: (f32, f32)| Point::new(f64::from(p.0 + v.0), f64::from(p.1 + v.1));
        Quad {
            ul: add(p, a),
            ur: add(q, a),
            ll: add(p, d),
            lr: add(q, d),
        }
    }
}
#[derive(Clone)]
struct State {
    ctm: Matrix,
    fill: Material,
    stroke: Material,
    style: StrokeStyle,
    text: TextState,
    blend: BlendMode,
    clips: usize,
    alpha_is_shape: bool,
    soft_mask: Option<Arc<MaskDef>>,
}
impl State {
    fn new(ctm: Matrix) -> Self {
        Self {
            ctm,
            fill: Material::new(),
            stroke: Material::new(),
            style: StrokeStyle::default(),
            text: TextState::default(),
            blend: BlendMode::Normal,
            clips: 0,
            alpha_is_shape: false,
            soft_mask: None,
        }
    }
}

#[derive(Clone)]
enum PatternDef {
    Shading(Arc<ShadingPaint>),
    Tile(Arc<TileDef>),
}
struct TileDef {
    content: ReplayContent,
    matrix: Matrix,
    bbox: Rect,
    x_step: f64,
    y_step: f64,
    colored: bool,
    color: Option<Rgb>,
    key: u64,
}
struct MaskDef {
    content: ReplayContent,
    luminosity: bool,
    backdrop: Rgb,
    transfer: Option<Arc<[u8; 256]>>,
    bbox: Rect,
    key: u64,
}
struct ReplayContent {
    state: State,
    resources: Vec<Dict>,
    stream: Stream,
    id: ObjRef,
    source: Provenance,
    active: Vec<ObjRef>,
    depth: usize,
    bbox: Rect,
    form: bool,
    uncolored: bool,
}
struct ContentReplay<'a, 'd> {
    run: &'a Run<'d>,
    content: &'a ReplayContent,
}
impl Replay for ContentReplay<'_, '_> {
    fn replay(&self, device: &mut dyn Device) -> Result<(), InterpError> {
        let c = self.content;
        if c.depth >= MAX_NESTING {
            return Err(InterpError::Limit("paint recursion exceeds 64".into()));
        }
        let mut engine = Engine::new(self.run, c.state.ctm, Dict::new());
        engine.state = c.state.clone();
        engine.state.clips = 0;
        engine.resources = c.resources.clone();
        engine.source = c.source.clone();
        engine.active = c.active.clone();
        engine.depth = c.depth + 1;
        engine.type3_mask = c.uncolored;
        let result = if c.form {
            engine.force_isolated = true;
            engine.form(c.id, &c.stream, None, device)
        } else {
            let mut path = Path::new();
            path.rect(c.bbox.x0, c.bbox.y0, c.bbox.width(), c.bbox.height());
            device.clip_path(
                &path,
                &ClipEvent {
                    ctm: engine.state.ctm,
                    rule: FillRule::NonZero,
                    source: &engine.source,
                    path_start: 0,
                    text: false,
                },
            );
            engine.state.clips += 1;
            engine.stream(&c.stream, (c.id.num != 0).then_some(c.id), device)
        };
        engine.finish(device);
        result
    }
}

struct Engine<'r, 'd> {
    run: &'r Run<'d>,
    state: State,
    stack: Vec<State>,
    resources: Vec<Dict>,
    path: Path,
    path_start: usize,
    pending_clip: Option<FillRule>,
    text_clip: Path,
    tm: TextMatrix,
    tlm: TextMatrix,
    bidi: u8,
    text_object: u64,
    text_pending: bool,
    last_text_mode: u8,
    source: Provenance,
    active: Vec<ObjRef>,
    depth: usize,
    marks: Vec<bool>,
    hidden: usize,
    type3_mask: bool,
    type3_fonts: Vec<u64>,
    compatibility: usize,
    base_ctm: Matrix,
    force_isolated: bool,
}
impl<'r, 'd> Engine<'r, 'd> {
    fn new(run: &'r Run<'d>, ctm: Matrix, resources: Dict) -> Self {
        Self {
            run,
            state: State::new(ctm),
            stack: Vec::new(),
            resources: vec![resources],
            path: Path::new(),
            path_start: 0,
            pending_clip: None,
            text_clip: Path::new(),
            tm: TextMatrix::IDENTITY,
            tlm: TextMatrix::IDENTITY,
            bidi: 0,
            text_object: 0,
            text_pending: false,
            last_text_mode: 0,
            source: Provenance {
                source: ContentSource::Page { part: 0 },
                op: 0,
                glyph: None,
                forms: Arc::from([]),
            },
            active: Vec::new(),
            depth: 0,
            marks: Vec::new(),
            hidden: 0,
            type3_mask: false,
            type3_fonts: Vec::new(),
            compatibility: 0,
            base_ctm: ctm,
            force_isolated: false,
        }
    }

    fn flush(&mut self) {
        if self.text_pending {
            self.text_object += 1;
            self.text_pending = false;
        }
    }
    fn finish(&mut self, device: &mut dyn Device) {
        self.flush();
        while self.state.clips != 0 {
            device.pop_clip();
            self.state.clips -= 1;
        }
        while self.marks.pop().is_some() {
            device.end_marked_content();
        }
    }
    fn restore(&mut self, state: State, device: &mut dyn Device) {
        self.flush();
        while self.state.clips > state.clips {
            device.pop_clip();
            self.state.clips -= 1;
        }
        self.state = state;
    }
    fn resource(&self, kind: &[u8], key: &[u8]) -> Option<Object> {
        self.resources.iter().rev().find_map(|r| {
            r.get(kind)
                .and_then(|o| self.run.doc.resolve_dict(o).ok().flatten())
                .and_then(|d| d.get(key).cloned())
        })
    }
    fn resources(&self) -> &Dict {
        &self.resources[self.resources.len() - 1]
    }

    fn with_mask<R>(&self, f: impl FnOnce(Option<&SoftMask<'_>>) -> R) -> R {
        if let Some(mask) = &self.state.soft_mask {
            let replay = ContentReplay {
                run: self.run,
                content: &mask.content,
            };
            let mask = SoftMask {
                luminosity: mask.luminosity,
                backdrop: mask.backdrop,
                transfer: mask.transfer.clone(),
                bbox: mask.bbox,
                key: mask.key,
                group: &replay,
            };
            f(Some(&mask))
        } else {
            f(None)
        }
    }
    fn with_brush<R>(&self, material: &Material, f: impl FnOnce(Brush<'_>) -> R) -> R {
        self.with_mask(|soft_mask| {
            let brush = |paint| Brush {
                paint,
                alpha: material.alpha,
                blend: self.state.blend,
                alpha_is_shape: self.state.alpha_is_shape,
                soft_mask,
            };
            match material.pattern.as_ref() {
                Some(PatternDef::Shading(shading)) => f(brush(Paint::Shading(shading))),
                Some(PatternDef::Tile(tile)) => {
                    let TileDef {
                        content,
                        matrix,
                        bbox,
                        x_step,
                        y_step,
                        colored,
                        color,
                        key,
                    } = tile.as_ref();
                    let replay = ContentReplay {
                        run: self.run,
                        content,
                    };
                    let tile = TilingPaint {
                        pattern: content.id,
                        matrix: *matrix,
                        bbox: *bbox,
                        x_step: *x_step,
                        y_step: *y_step,
                        colored: *colored,
                        color: *color,
                        key: *key,
                        cell: &replay,
                    };
                    f(brush(Paint::Tiling(&tile)))
                }
                None => f(brush(Paint::Color(material.space.to_rgb(&material.values)))),
            }
        })
    }

    fn pattern(&self, object: &Object, material: &Material) -> Option<PatternDef> {
        let value = self.run.doc.resolve(object).ok()?;
        let d = value
            .as_dict()
            .or_else(|| value.as_stream().map(|s| &s.dict))?;
        let matrix = d
            .get_array(b"Matrix")
            .and_then(Matrix::from_array)
            .unwrap_or(Matrix::IDENTITY)
            .concat(&self.base_ctm);
        if d.get_i64(b"PatternType") == Some(2) {
            return Some(PatternDef::Shading(Arc::new(ShadingPaint {
                shading: self.run.shading(d.get(b"Shading")?, self.resources())?,
                matrix,
            })));
        }
        let stream = value.as_stream()?.clone();
        let bbox = d.get_array(b"BBox").and_then(Rect::from_array)?;
        let x_step = d.get_f64(b"XStep")?;
        let y_step = d.get_f64(b"YStep")?;
        if !x_step.is_finite() || !y_step.is_finite() || x_step == 0.0 || y_step == 0.0 {
            return None;
        }
        let colored = d.get_i64(b"PaintType") != Some(2);
        let color = (!colored).then(|| {
            material
                .space
                .pattern_base()
                .map_or([0.0; 3], |s| s.to_rgb(&material.values))
        });
        let mut state = self.state.clone();
        state.ctm = matrix;
        state.soft_mask = None;
        state.fill.pattern = None;
        state.stroke.pattern = None;
        state.fill.alpha = 1.0;
        state.stroke.alpha = 1.0;
        state.blend = BlendMode::Normal;
        if let Some(color) = color {
            state.fill.space = ColorSpace::rgb();
            state.fill.values = color.to_vec();
            state.stroke = state.fill.clone();
        }
        let mut resources = self.resources.clone();
        if let Some(r) = d
            .get(b"Resources")
            .and_then(|o| self.run.doc.resolve_dict(o).ok().flatten())
        {
            resources.push(r);
        }
        let id = object.as_reference().unwrap_or(ObjRef::new(0, 0));
        let mut source = self.source.clone();
        source.source = ContentSource::Pattern(id);
        let content = ReplayContent {
            state,
            resources,
            stream,
            id,
            source,
            active: self.active.clone(),
            depth: self.depth,
            bbox,
            form: false,
            uncolored: !colored,
        };
        Some(PatternDef::Tile(Arc::new(TileDef {
            content,
            matrix,
            bbox,
            x_step,
            y_step,
            colored,
            color,
            key: PAINT_ID.fetch_add(1, Ordering::Relaxed),
        })))
    }

    fn stream(
        &mut self,
        stream: &Stream,
        id: Option<ObjRef>,
        device: &mut dyn Device,
    ) -> Result<(), InterpError> {
        let cached = id.and_then(|id| self.run.cache.contents.borrow().get(&id).cloned());
        let ops = if let Some(ops) = cached {
            ops
        } else {
            let bytes = self.run.doc.decode_stream(stream)?.data;
            let ops: Arc<[Operation]> = parse_content(&bytes)?.into();
            if let Some(id) = id {
                self.run.cache.contents.borrow_mut().insert(id, ops.clone());
            }
            ops
        };
        let mut errors = 0;
        for (index, op) in ops.iter().enumerate() {
            let count = self.run.operations.get() + 1;
            if count > self.run.options.max_operations {
                return Err(InterpError::Limit(
                    "content operation limit exceeded".into(),
                ));
            }
            self.run.operations.set(count);
            self.source.op = index;
            if !self.operator(op, device)? {
                errors += 1;
                if errors >= 100 {
                    break;
                }
            }
        }
        self.flush();
        self.path_start = 0;
        Ok(())
    }

    fn operator(&mut self, op: &Operation, device: &mut dyn Device) -> Result<bool, InterpError> {
        let a = &op.operands;
        let n = |i: usize| a.get(i).and_then(Object::as_f64).unwrap_or(0.0);
        let point = |i| Point::new(n(i), n(i + 1));
        let code = op.operator.as_slice();
        if matches!(
            code,
            b"w" | b"j"
                | b"J"
                | b"M"
                | b"d"
                | b"ri"
                | b"gs"
                | b"Q"
                | b"cm"
                | b"W"
                | b"W*"
                | b"ET"
                | b"CS"
                | b"cs"
                | b"SC"
                | b"sc"
                | b"SCN"
                | b"scn"
                | b"G"
                | b"g"
                | b"RG"
                | b"rg"
                | b"K"
                | b"k"
                | b"sh"
                | b"Do"
                | b"BI"
                | b"BMC"
                | b"BDC"
                | b"EMC"
        ) {
            self.flush();
        }
        if self.path.is_empty() && matches!(code, b"m" | b"l" | b"c" | b"v" | b"y" | b"re") {
            self.path_start = self.source.op;
        }
        match code {
            b"q" => {
                if self.stack.len() >= 2047 {
                    return Err(InterpError::Limit(
                        "graphics state nesting exceeds 2047".into(),
                    ));
                }
                self.stack.push(self.state.clone());
            }
            b"Q" => {
                if let Some(saved) = self.stack.pop() {
                    self.restore(saved, device);
                }
            }
            b"cm" => {
                if let Some(m) = Matrix::from_array(a) {
                    self.state.ctm = TextMatrix::from(m).concat(self.state.ctm.into()).into();
                }
            }
            b"w" => self.state.style.width = n(0).abs(),
            b"J" => {
                self.state.style.cap = match a.first().map_or(0, font::integer).clamp(0, 2) {
                    1 => LineCap::Round,
                    2 => LineCap::Square,
                    _ => LineCap::Butt,
                }
            }
            b"j" => {
                self.state.style.join = match a.first().map_or(0, font::integer).clamp(0, 2) {
                    1 => LineJoin::Round,
                    2 => LineJoin::Bevel,
                    _ => LineJoin::Miter,
                }
            }
            b"M" => self.state.style.miter_limit = n(0),
            b"d" => {
                self.state.style.dash = a
                    .first()
                    .and_then(Object::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|o| o.as_f64().unwrap_or(0.0).max(0.0))
                            .collect()
                    })
                    .unwrap_or_default();
                self.state.style.dash_phase = n(1);
            }
            b"m" => self.path.move_to(point(0)),
            b"l" => self.path.line_to(point(0)),
            b"c" => self.path.curve_to(point(0), point(2), point(4)),
            b"v" => self.path.curve_to(
                self.path.current_point().unwrap_or(point(0)),
                point(0),
                point(2),
            ),
            b"y" => self.path.curve_to(point(0), point(2), point(2)),
            b"h" => self.path.close(),
            b"re" => self.path.rect(n(0), n(1), n(2), n(3)),
            b"W" => self.pending_clip = Some(FillRule::NonZero),
            b"W*" => self.pending_clip = Some(FillRule::EvenOdd),
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                if matches!(code, b"s" | b"b" | b"b*") {
                    self.path.close();
                }
                let fill = matches!(code, b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*");
                let stroke = matches!(code, b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
                self.paint_path(
                    fill,
                    stroke,
                    if code.ends_with(b"*") {
                        FillRule::EvenOdd
                    } else {
                        FillRule::NonZero
                    },
                    device,
                );
            }
            b"BT" => {
                self.tm = TextMatrix::IDENTITY;
                self.tlm = TextMatrix::IDENTITY;
                self.bidi = 0;
            }
            b"ET" => {
                if !self.text_clip.is_empty() {
                    device.clip_path(
                        &self.text_clip,
                        &ClipEvent {
                            ctm: Matrix::IDENTITY,
                            rule: FillRule::NonZero,
                            source: &self.source,
                            path_start: self.source.op,
                            text: true,
                        },
                    );
                    self.state.clips += 1;
                    self.text_clip = Path::new();
                }
            }
            b"Tc" => self.state.text.char_space = n(0) as f32,
            b"Tw" => self.state.text.word_space = n(0) as f32,
            b"Tz" => self.state.text.hscale = n(0) as f32 / 100.0,
            b"TL" => self.state.text.leading = n(0) as f32,
            b"Tr" => self.state.text.mode = a.first().map_or(0, font::integer).clamp(0, 7) as u8,
            b"Ts" => self.state.text.rise = n(0) as f32,
            b"Tf" => {
                if let Some(name) = a.first().and_then(Object::as_name) {
                    let o = self.resource(b"Font", name).unwrap_or(Object::Null);
                    let f = self.run.font(&o, self.resources());
                    self.state.text.font = Some(if self.type3_fonts.contains(&f.id()) {
                        Arc::new(font::fallback(FONT_ID.fetch_add(1, Ordering::Relaxed)))
                    } else {
                        f
                    });
                    self.state.text.size = n(1) as f32;
                }
            }
            b"Td" | b"TD" => {
                if code == b"TD" {
                    self.state.text.leading = -n(1) as f32;
                }
                self.move_text(n(0) as f32, n(1) as f32);
            }
            b"Tm" => {
                if let Some(m) = Matrix::from_array(a) {
                    self.tm = m.into();
                    self.tlm = self.tm;
                }
            }
            b"T*" => self.move_text(0.0, -self.state.text.leading),
            b"Tj" | b"TJ" => {
                if let Some(o) = a.first() {
                    self.show(o, device)?;
                }
            }
            b"'" => {
                self.move_text(0.0, -self.state.text.leading);
                if let Some(o) = a.first() {
                    self.show(o, device)?;
                }
            }
            b"\"" => {
                self.state.text.word_space = n(0) as f32;
                self.state.text.char_space = n(1) as f32;
                self.move_text(0.0, -self.state.text.leading);
                if let Some(o) = a.get(2) {
                    self.show(o, device)?;
                }
            }
            b"g" | b"G" | b"rg" | b"RG" | b"k" | b"K" => {
                if !self.type3_mask {
                    let space = match code {
                        b"g" | b"G" => ColorSpace::gray(),
                        b"rg" | b"RG" => ColorSpace::rgb(),
                        _ => ColorSpace::cmyk(),
                    };
                    self.set_color(code[0].is_ascii_uppercase(), space, a);
                }
            }
            b"cs" | b"CS" => {
                if !self.type3_mask
                    && let Some(o) = a.first()
                {
                    let resolved = o.as_name().and_then(|n| self.resource(b"ColorSpace", n));
                    if let Some(space) = ColorSpace::load(
                        self.run.doc,
                        resolved.as_ref().unwrap_or(o),
                        Some(self.resources()),
                        &self.run.cache.colors,
                    ) {
                        let values = space.initial_color();
                        let mat = if code == b"CS" {
                            &mut self.state.stroke
                        } else {
                            &mut self.state.fill
                        };
                        mat.space = space;
                        mat.values = values;
                        mat.pattern = None;
                    }
                }
            }
            b"sc" | b"SC" | b"scn" | b"SCN" => {
                if !self.type3_mask {
                    let stroke = code[0].is_ascii_uppercase();
                    let mat = if stroke {
                        &mut self.state.stroke
                    } else {
                        &mut self.state.fill
                    };
                    mat.values = a
                        .iter()
                        .filter_map(Object::as_f64)
                        .map(|v| v as f32)
                        .collect();
                    let pattern = if mat.space.is_pattern() {
                        a.last()
                            .and_then(Object::as_name)
                            .and_then(|n| self.resource(b"Pattern", n))
                            .and_then(|o| {
                                self.pattern(
                                    &o,
                                    if stroke {
                                        &self.state.stroke
                                    } else {
                                        &self.state.fill
                                    },
                                )
                            })
                    } else {
                        None
                    };
                    if stroke {
                        self.state.stroke.pattern = pattern;
                    } else {
                        self.state.fill.pattern = pattern;
                    }
                }
            }
            b"gs" => {
                if let Some(name) = a.first().and_then(Object::as_name)
                    && let Some(o) = self.resource(b"ExtGState", name)
                    && let Some(d) = self.run.doc.resolve_dict(&o)?
                {
                    self.extgstate(&d);
                }
            }
            b"Do" => {
                if let Some(name) = a.first().and_then(Object::as_name)
                    && let Some(o) = self.resource(b"XObject", name)
                    && let Some(s) = self.run.doc.resolve_stream(&o)?
                {
                    let hidden = usize::from(s.dict.get(b"OC").is_some_and(|o| self.run.hidden(o)));
                    self.hidden += hidden;
                    let result = match s
                        .dict
                        .get_name(b"Subtype2")
                        .or_else(|| s.dict.get_name(b"Subtype"))
                    {
                        Some(b"Form") => self.form(
                            o.as_reference().unwrap_or(ObjRef::new(0, 0)),
                            &s,
                            None,
                            device,
                        ),
                        Some(b"Image") => {
                            self.image(o.as_reference(), s, device);
                            Ok(())
                        }
                        _ => Ok(()),
                    };
                    self.hidden -= hidden;
                    result?;
                }
            }
            b"BI" => {
                if let Some(Object::Stream(s)) = a.first() {
                    self.image(None, s.clone(), device);
                }
            }
            b"BMC" | b"BDC" => {
                if let Some(Object::Name(tag)) = a.first() {
                    let property = if code == b"BDC" {
                        a.get(1).map(|o| {
                            if let Some(n) = o.as_name() {
                                self.resource(b"Properties", n).unwrap_or(Object::Null)
                            } else {
                                o.clone()
                            }
                        })
                    } else {
                        None
                    };
                    let hidden = self.hidden != 0
                        || (tag.as_bytes() == b"OC"
                            && property.as_ref().is_some_and(|o| self.run.hidden(o)));
                    self.marks.push(hidden);
                    if hidden {
                        self.hidden += 1;
                    }
                    let dict = property
                        .as_ref()
                        .and_then(|o| self.run.doc.resolve_dict(o).ok().flatten());
                    device.begin_marked_content(&MarkedContentEvent {
                        tag,
                        properties: dict.as_ref(),
                        visible: !hidden,
                        source: &self.source,
                    });
                }
            }
            b"EMC" => {
                if let Some(hidden) = self.marks.pop() {
                    if hidden {
                        self.hidden = self.hidden.saturating_sub(1);
                    }
                    device.end_marked_content();
                }
            }
            b"d1" => {
                if !self.type3_fonts.is_empty() {
                    self.type3_mask = true;
                }
            }
            b"d0" => {
                if !self.type3_fonts.is_empty() {
                    self.type3_mask = false;
                }
            }
            b"BX" => self.compatibility += 1,
            b"EX" => self.compatibility = self.compatibility.saturating_sub(1),
            b"sh" => {
                if self.hidden == 0
                    && let Some(o) = a
                        .first()
                        .and_then(Object::as_name)
                        .and_then(|n| self.resource(b"Shading", n))
                    && let Some(shading) = self.run.shading(&o, self.resources())
                {
                    self.with_mask(|soft_mask| {
                        device.fill_shading(&ShadingEvent {
                            shading: &shading,
                            matrix: self.state.ctm,
                            alpha: self.state.fill.alpha,
                            blend: self.state.blend,
                            alpha_is_shape: self.state.alpha_is_shape,
                            soft_mask,
                            source: &self.source,
                        })
                    });
                }
            }
            b"ri" | b"i" | b"MP" | b"DP" => {}
            _ => return Ok(self.compatibility != 0),
        }
        Ok(true)
    }

    fn set_color(&mut self, stroke: bool, space: Arc<ColorSpace>, a: &[Object]) {
        let values = a
            .iter()
            .take(space.components())
            .map(|o| o.as_f64().unwrap_or(0.0) as f32)
            .collect();
        let mat = if stroke {
            &mut self.state.stroke
        } else {
            &mut self.state.fill
        };
        mat.space = space;
        mat.values = values;
        mat.pattern = None;
    }
    fn paint_path(&mut self, fill: bool, stroke: bool, rule: FillRule, device: &mut dyn Device) {
        if self.hidden == 0 {
            if fill {
                self.with_brush(&self.state.fill, |brush| {
                    device.fill_path(
                        &self.path,
                        &FillEvent {
                            ctm: self.state.ctm,
                            rule,
                            brush,
                            source: &self.source,
                            path_start: self.path_start,
                        },
                    )
                });
            }
            if stroke {
                self.with_brush(&self.state.stroke, |brush| {
                    device.stroke_path(
                        &self.path,
                        &StrokeEvent {
                            ctm: self.state.ctm,
                            style: &self.state.style,
                            brush,
                            source: &self.source,
                            path_start: self.path_start,
                        },
                    )
                });
            }
        }
        if let Some(rule) = self.pending_clip.take() {
            device.clip_path(
                &self.path,
                &ClipEvent {
                    ctm: self.state.ctm,
                    rule,
                    source: &self.source,
                    path_start: self.path_start,
                    text: false,
                },
            );
            self.state.clips += 1;
        }
        self.path = Path::new();
    }
    fn move_text(&mut self, x: f32, y: f32) {
        self.tlm = self.tlm.translated(x, y);
        self.tm = self.tlm;
    }
    fn text_space(&mut self, value: f32, wmode: u8) {
        self.tm = if wmode == 0 {
            self.tm.translated(value * self.state.text.hscale, 0.0)
        } else {
            self.tm.translated(0.0, value)
        };
    }

    fn show(&mut self, object: &Object, device: &mut dyn Device) -> Result<(), InterpError> {
        let Some(font) = self.state.text.font.clone() else {
            return Ok(());
        };
        let ts = self.state.text.clone();
        let wmode = font.wmode();
        let matrix = self.tm.into();
        let ctm = TextMatrix::from(self.state.ctm);
        let mode = if font.type3_matrix().is_some() {
            let m = if ts.mode >= 4 { ts.mode - 4 } else { ts.mode };
            if m == 3 { 3 } else { 0 }
        } else {
            ts.mode
        };
        if self.text_pending && mode != self.last_text_mode {
            self.flush();
        }
        self.last_text_mode = mode;
        let elements = match object {
            Object::Array(a) => a.as_slice(),
            o => std::slice::from_ref(o),
        };
        let mut glyphs = Vec::new();
        let mut index = 0u32;
        for (element, o) in elements.iter().enumerate() {
            if let Some(value) = o.as_f64() {
                self.text_space(-(value as f32) * ts.size * 0.001, wmode);
                continue;
            }
            let Some(string) = o.as_string() else {
                continue;
            };
            let mut offset = 0;
            while offset < string.bytes.len() {
                let Some(code) = font.decode(&string.bytes[offset..]) else {
                    break;
                };
                let word_space = code.code == 32 && code.len == 1;
                if let Some(cid) = font.cid(code) {
                    let gid = font.gid(cid);
                    let mut tsm = TextMatrix {
                        a: ts.size * ts.hscale,
                        b: 0.0,
                        c: 0.0,
                        d: ts.size,
                        e: 0.0,
                        f: ts.rise,
                    };
                    let width = if wmode == 0 {
                        font.horizontal(cid)
                    } else {
                        let (x, y, w) = font.vertical(cid);
                        tsm.e = (x as f32 * ts.size.abs()).mul_add(-0.001, tsm.e);
                        tsm.f = (y as f32 * ts.size).mul_add(-0.001, tsm.f);
                        w as f32 * 0.001
                    };
                    let trm = tsm.concat(self.tm).concat(ctm);
                    let unicode = font.unicode(code.code, cid);
                    self.bidi = bidi_level(unicode.first(), self.bidi);
                    let glyph = Glyph {
                        code: code.code,
                        cid,
                        gid,
                        unicode,
                        trm: trm.into(),
                        advance: font.advance_glyph(gid, wmode),
                        width: width.into(),
                        quad: trm.quad(
                            width,
                            wmode,
                            font.ascender() as f32,
                            font.descender() as f32,
                        ),
                        word_space,
                        bidi: self.bidi,
                        pos: GlyphPos {
                            index,
                            element: element as u32,
                            byte_offset: offset as u32,
                            byte_len: code.len,
                        },
                    };
                    if mode >= 4
                        && let Some(path) = font.glyph_path(gid)
                    {
                        self.text_clip.append_transformed(&path, &glyph.trm);
                    }
                    glyphs.push(glyph);
                    if wmode == 0 {
                        self.tm = self
                            .tm
                            .translated(width.mul_add(ts.size, ts.char_space) * ts.hscale, 0.0);
                    } else {
                        self.tm = self
                            .tm
                            .translated(0.0, width.mul_add(ts.size, ts.char_space));
                    }
                }
                if word_space {
                    self.text_space(ts.word_space, wmode);
                }
                offset += usize::from(code.len);
                index = index.saturating_add(1);
            }
        }
        let visible = self.hidden == 0;
        self.with_brush(&self.state.fill, |fill| {
            self.with_brush(&self.state.stroke, |stroke| {
                device.text(&TextRun {
                    font: &font,
                    size: ts.size.into(),
                    render_mode: mode,
                    char_spacing: ts.char_space.into(),
                    word_spacing: ts.word_space.into(),
                    horizontal_scale: ts.hscale.into(),
                    rise: ts.rise.into(),
                    wmode,
                    ctm: self.state.ctm,
                    text_matrix: matrix,
                    fill: (visible && matches!(mode, 0 | 2 | 4 | 6)).then_some(fill),
                    stroke: (visible && matches!(mode, 1 | 2 | 5 | 6)).then_some(stroke),
                    stroke_style: &self.state.style,
                    clip: mode >= 4,
                    glyphs: &glyphs,
                    source: &self.source,
                    text_object: self.text_object,
                });
            })
        });
        self.text_pending |= !glyphs.is_empty();
        if visible
            && mode != 3
            && device.wants_type3_procs()
            && let Some(t3) = &font.type3
        {
            for glyph in &glyphs {
                if let Some(proc) = t3.procs.get(glyph.gid as usize).and_then(Option::as_ref) {
                    self.type3_glyph(&font, glyph, proc, device)?;
                }
            }
        }
        Ok(())
    }

    fn type3_glyph(
        &mut self,
        font: &PdfFont,
        glyph: &Glyph,
        proc: &font::CharProc,
        device: &mut dyn Device,
    ) -> Result<(), InterpError> {
        if self.depth >= MAX_NESTING {
            return Err(InterpError::Limit("glyph recursion exceeds 64".into()));
        }
        let Some(t3) = &font.type3 else {
            return Ok(());
        };
        let matrix = font
            .type3_matrix()
            .unwrap_or(Matrix::IDENTITY)
            .concat(&glyph.trm);
        device.begin_type3_glyph(&Type3GlyphEvent {
            font,
            glyph,
            matrix,
            source: &self.source,
        });
        let saved = self.state.clone();
        let source = self.source.clone();
        let path = std::mem::take(&mut self.path);
        let clip = self.pending_clip.take();
        let stack = std::mem::take(&mut self.stack);
        let tm = self.tm;
        let tlm = self.tlm;
        let mask = self.type3_mask;
        let text_clip = std::mem::take(&mut self.text_clip);
        let path_start = self.path_start;
        let marks = self.marks.len();
        let hidden = self.hidden;
        let base_ctm = self.base_ctm;
        self.base_ctm = matrix;
        self.state.ctm = matrix;
        self.state.text.font = None;
        self.source.source = ContentSource::CharProc(proc.object);
        self.source.glyph = Some(glyph.pos);
        self.resources.push(t3.resources.clone());
        self.type3_fonts.push(font.id());
        self.depth += 1;
        let result = self.stream(
            &proc.stream,
            (proc.object.num != 0).then_some(proc.object),
            device,
        );
        self.depth -= 1;
        self.type3_fonts.pop();
        self.resources.pop();
        while self.marks.len() > marks {
            self.marks.pop();
            device.end_marked_content();
        }
        self.hidden = hidden;
        self.text_clip = text_clip;
        self.path_start = path_start;
        self.base_ctm = base_ctm;
        self.restore(saved, device);
        self.source = source;
        self.path = path;
        self.pending_clip = clip;
        self.stack = stack;
        self.tm = tm;
        self.tlm = tlm;
        self.type3_mask = mask;
        device.end_type3_glyph();
        result
    }

    fn image(&self, id: Option<ObjRef>, stream: Stream, device: &mut dyn Device) {
        if self.hidden != 0 {
            return;
        }
        let cached = id.and_then(|id| self.run.cache.images.borrow().get(&id).cloned());
        let image = if let Some(image) = cached {
            image
        } else {
            let Ok(image) = PdfImage::load(
                self.run.doc,
                id,
                stream,
                self.resources(),
                &self.run.cache.colors,
            ) else {
                return;
            };
            let image = Arc::new(image);
            if let Some(id) = id {
                self.run.cache.images.borrow_mut().insert(id, image.clone());
            }
            image
        };
        let ctm = Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, 1.0).concat(&self.state.ctm);
        let bbox = Rect::new(0.0, 0.0, 1.0, 1.0).transform(&ctm);
        if image.is_mask() {
            self.with_brush(&self.state.fill, |brush| {
                device.fill_image_mask(&ImageMaskEvent {
                    image: &image,
                    ctm,
                    bbox,
                    brush,
                    source: &self.source,
                })
            });
        } else {
            self.with_mask(|soft_mask| {
                device.fill_image(&ImageEvent {
                    image: &image,
                    ctm,
                    bbox,
                    alpha: self.state.fill.alpha,
                    blend: self.state.blend,
                    alpha_is_shape: self.state.alpha_is_shape,
                    soft_mask,
                    source: &self.source,
                })
            });
        }
    }

    fn form(
        &mut self,
        id: ObjRef,
        stream: &Stream,
        source_override: Option<ContentSource>,
        device: &mut dyn Device,
    ) -> Result<(), InterpError> {
        if self.active.contains(&id) && id.num != 0 {
            return Ok(());
        }
        if self.depth >= MAX_NESTING {
            return Err(InterpError::Limit("form recursion exceeds 64".into()));
        }
        let d = &stream.dict;
        let form_matrix = self
            .run
            .doc
            .resolve_key(d, b"Matrix")?
            .as_array()
            .and_then(Matrix::from_array)
            .unwrap_or(Matrix::IDENTITY);
        let bbox = self
            .run
            .doc
            .resolve_key(d, b"BBox")?
            .as_array()
            .and_then(Rect::from_array)
            .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
        let matrix = form_matrix.concat(&self.state.ctm);
        let group = self.run.doc.resolve_key(d, b"Group")?;
        let is_group = group.as_dict().is_some();
        let resources = d
            .get(b"Resources")
            .map(|o| self.run.doc.resolve_dict(o))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| self.resources().clone());
        device.begin_form(&FormEvent {
            form: id,
            matrix,
            ctm: self.state.ctm,
            bbox,
            source: &self.source,
            group: is_group,
        });
        if let Some(g) = group.as_dict() {
            self.with_mask(|soft_mask| {
                device.begin_group(&GroupEvent {
                    bbox: bbox.transform(&matrix),
                    isolated: self.force_isolated || g.get_bool(b"I").unwrap_or(false),
                    knockout: g.get_bool(b"K").unwrap_or(false),
                    blend: self.state.blend,
                    alpha: self.state.fill.alpha,
                    alpha_is_shape: self.state.alpha_is_shape,
                    soft_mask,
                    source: &self.source,
                })
            });
        }
        let saved = self.state.clone();
        let source = self.source.clone();
        let path = std::mem::take(&mut self.path);
        let clip = self.pending_clip.take();
        let stack = std::mem::take(&mut self.stack);
        let text_clip = std::mem::take(&mut self.text_clip);
        let path_start = self.path_start;
        let tm = self.tm;
        let tlm = self.tlm;
        let marks = self.marks.len();
        let hidden = self.hidden;
        let base_ctm = self.base_ctm;
        self.base_ctm = self.state.ctm;
        self.state.ctm = matrix;
        if is_group {
            self.state.fill.alpha = 1.0;
            self.state.stroke.alpha = 1.0;
            self.state.blend = BlendMode::Normal;
            self.state.soft_mask = None;
        }
        let mut clip_path = Path::new();
        clip_path.rect(bbox.x0, bbox.y0, bbox.width(), bbox.height());
        device.clip_path(
            &clip_path,
            &ClipEvent {
                ctm: matrix,
                rule: FillRule::NonZero,
                source: &self.source,
                path_start: self.source.op,
                text: false,
            },
        );
        self.state.clips += 1;
        self.resources.push(resources);
        let mut forms = self.source.forms.to_vec();
        forms.push(FormCall {
            caller: self.source.source,
            op: self.source.op,
            form: id,
        });
        self.source = Provenance {
            source: source_override.unwrap_or(ContentSource::Form(id)),
            op: 0,
            glyph: None,
            forms: forms.into(),
        };
        self.active.push(id);
        self.depth += 1;
        let result = self.stream(stream, (id.num != 0).then_some(id), device);
        self.depth -= 1;
        self.active.pop();
        self.resources.pop();
        while self.marks.len() > marks {
            self.marks.pop();
            device.end_marked_content();
        }
        self.hidden = hidden;
        self.restore(saved, device);
        self.source = source;
        self.path = path;
        self.pending_clip = clip;
        self.stack = stack;
        self.tm = tm;
        self.tlm = tlm;
        self.base_ctm = base_ctm;
        self.text_clip = text_clip;
        self.path_start = path_start;
        if is_group {
            device.end_group();
        }
        device.end_form();
        result
    }

    fn extgstate(&mut self, d: &Dict) {
        for (key, value) in d.iter() {
            let value = self.run.doc.resolve(value).unwrap_or(Object::Null);
            let num = value.as_f64().unwrap_or(0.0);
            match key.as_bytes() {
                b"LW" => self.state.style.width = num.abs(),
                b"LC" => {
                    self.state.style.cap = match font::integer(&value).clamp(0, 2) {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    }
                }
                b"LJ" => {
                    self.state.style.join = match font::integer(&value).clamp(0, 2) {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    }
                }
                b"ML" => self.state.style.miter_limit = num,
                b"D" => {
                    if let Some(a) = value.as_array() {
                        self.state.style.dash = a
                            .first()
                            .and_then(Object::as_array)
                            .map(|a| {
                                a.iter()
                                    .map(|v| v.as_f64().unwrap_or(0.0).max(0.0))
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.state.style.dash_phase =
                            a.get(1).and_then(Object::as_f64).unwrap_or(0.0);
                    }
                }
                b"CA" => self.state.stroke.alpha = num.clamp(0.0, 1.0) as f32,
                b"ca" => self.state.fill.alpha = num.clamp(0.0, 1.0) as f32,
                b"AIS" => self.state.alpha_is_shape = value.as_bool().unwrap_or(false),
                b"SMask" => self.state.soft_mask = self.load_mask(&value),
                b"BM" => {
                    let value = value.as_array().and_then(|a| a.first()).unwrap_or(&value);
                    self.state.blend = value
                        .as_name()
                        .and_then(BlendMode::from_pdf_name)
                        .unwrap_or_default();
                }
                b"Font" => {
                    if let Some(a) = value.as_array()
                        && let Some(o) = a.first()
                    {
                        self.state.text.font = Some(self.run.font(o, self.resources()));
                        self.state.text.size =
                            a.get(1).and_then(Object::as_f64).unwrap_or(0.0) as f32;
                    }
                }
                _ => {}
            }
        }
    }

    fn load_mask(&self, value: &Object) -> Option<Arc<MaskDef>> {
        let d = value.as_dict()?;
        let object = d.get(b"G")?;
        let stream = self.run.doc.resolve_stream(object).ok()??;
        let matrix = stream
            .dict
            .get_array(b"Matrix")
            .and_then(Matrix::from_array)
            .unwrap_or(Matrix::IDENTITY)
            .concat(&self.state.ctm);
        let bbox = stream
            .dict
            .get_array(b"BBox")
            .and_then(Rect::from_array)?
            .transform(&matrix);
        let group = self.run.doc.resolve_key(&stream.dict, b"Group").ok()?;
        let space = group
            .as_dict()
            .and_then(|g| g.get(b"CS"))
            .and_then(|o| {
                ColorSpace::load(
                    self.run.doc,
                    o,
                    Some(self.resources()),
                    &self.run.cache.colors,
                )
            })
            .unwrap_or_else(ColorSpace::gray);
        let bc = d
            .get_array(b"BC")
            .map(|a| {
                a.iter()
                    .map(|o| o.as_f64().unwrap_or(0.0) as f32)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| space.initial_color());
        let transfer = d
            .get(b"TR")
            .and_then(|o| Function::load(self.run.doc, o))
            .map(|function| {
                Arc::new(std::array::from_fn(|i| {
                    let mut v = [0.0];
                    function.eval(&[i as f64 / 255.0], &mut v);
                    (v[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8
                }))
            });
        let mut state = self.state.clone();
        state.soft_mask = None;
        state.fill.alpha = 1.0;
        state.stroke.alpha = 1.0;
        state.blend = BlendMode::Normal;
        let id = object.as_reference().unwrap_or(ObjRef::new(0, 0));
        let content = ReplayContent {
            state,
            resources: self.resources.clone(),
            stream,
            id,
            source: self.source.clone(),
            active: self.active.clone(),
            depth: self.depth,
            bbox,
            form: true,
            uncolored: false,
        };
        Some(Arc::new(MaskDef {
            content,
            luminosity: d.get_name(b"S") == Some(b"Luminosity"),
            backdrop: space.to_rgb(&bc),
            transfer,
            bbox,
            key: PAINT_ID.fetch_add(1, Ordering::Relaxed),
        }))
    }
}

fn bidi_level(c: char, current: u8) -> u8 {
    use unicode_bidi::BidiClass::*;
    match unicode_bidi::bidi_class(c) {
        L | EN | ES | ET => 0,
        R | AL | AN => 1,
        CS | NSM | BN | B | S | WS | ON => current,
        _ => 0,
    }
}
