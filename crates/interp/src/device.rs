//! What the interpreter reports while it runs content: the [`Device`]
//! trait and its events.
//!
//! Geometry in events is in device space: PDF user space through the CTM,
//! then [`crate::page_transform`] (MuPDF's page space: origin at the crop
//! box's top-left, y down, /Rotate applied), then
//! [`crate::RunOptions::transform`]. With the identity transform this is
//! the space PyMuPDF reports text and drawings in.

use std::fmt;
use std::sync::Arc;

use pdf_core::{Dict, Matrix, Name, ObjRef, Point, Rect};

use crate::font::PdfFont;
use crate::image::PdfImage;
use crate::path::{FillRule, Path, StrokeStyle};
use crate::pattern::{ShadingPaint, SoftMask, TilingPaint};
use crate::shading::Shading;

/// Receives the drawing of a page. Every method has an empty default, so
/// a device implements only what it uses. Events borrow their data for the
/// duration of the call.
pub trait Device {
    /// `f F f* B B* b b*` (the fill half).
    fn fill_path(&mut self, _path: &Path, _event: &FillEvent<'_>) {}
    /// `S s B B* b b*` (the stroke half).
    fn stroke_path(&mut self, _path: &Path, _event: &StrokeEvent<'_>) {}
    /// `W n`, `W* n` (after the painting operator, as the PDF model says),
    /// and text clipping at `ET` (render modes 4–7) with the glyph outlines
    /// already in device space. Undone by [`Device::pop_clip`].
    fn clip_path(&mut self, _path: &Path, _event: &ClipEvent<'_>) {}
    /// One show operation (`Tj TJ ' "`) in one font. Sent for every render
    /// mode, including 3 (invisible) and the clip modes.
    fn text(&mut self, _run: &TextRun<'_>) {}
    /// `sh`, painted over the current clip.
    fn fill_shading(&mut self, _event: &ShadingEvent<'_>) {}
    /// An image XObject or inline image that is not a stencil mask.
    fn fill_image(&mut self, _event: &ImageEvent<'_>) {}
    /// An `/ImageMask true` image: a stencil painted with the fill brush.
    fn fill_image_mask(&mut self, _event: &ImageMaskEvent<'_>) {}
    /// Undo the most recent clip that is still in force (on `Q`, at the end
    /// of a form, pattern cell or glyph procedure, and at the end of the run).
    fn pop_clip(&mut self) {}
    /// A transparency group opens (a form XObject with /Group).
    fn begin_group(&mut self, _event: &GroupEvent<'_>) {}
    /// Composite the innermost open group with the values of its
    /// [`GroupEvent`].
    fn end_group(&mut self) {}
    /// A form XObject starts (`Do`). Its content's events follow, then
    /// [`Device::end_form`].
    fn begin_form(&mut self, _event: &FormEvent<'_>) {}
    fn end_form(&mut self) {}
    /// `BMC`/`BDC`. Hidden optional content emits no drawing, but its
    /// brackets and text (with no paint brushes) retain their provenance.
    /// `RunOptions::include_hidden_content` makes all of it paintable.
    fn begin_marked_content(&mut self, _event: &MarkedContentEvent<'_>) {}
    /// `EMC` matching a reported begin.
    fn end_marked_content(&mut self) {}
    /// An annotation's normal appearance starts. Its content's events
    /// follow (as a form), then [`Device::end_annotation`].
    fn begin_annotation(&mut self, _event: &AnnotationEvent<'_>) {}
    fn end_annotation(&mut self) {}
    /// Whether Type 3 glyph procedures run through this device. When true,
    /// each visible Type 3 glyph is followed by `begin_type3_glyph`, the
    /// procedure's events (source [`ContentSource::CharProc`]) and
    /// `end_type3_glyph`. Default false: Type 3 glyphs appear only in
    /// [`TextRun`]s.
    fn wants_type3_procs(&self) -> bool {
        false
    }
    fn begin_type3_glyph(&mut self, _event: &Type3GlyphEvent<'_>) {}
    fn end_type3_glyph(&mut self) {}
}

/// The content stream an operation sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentSource {
    /// The page's content: `part` indexes the page's /Contents array
    /// (0 for a single stream), matching `Page::content_refs()`.
    Page { part: usize },
    /// A form XObject (also transparency groups and soft-mask groups).
    Form(ObjRef),
    /// A tiling pattern's cell stream.
    Pattern(ObjRef),
    /// A Type 3 glyph procedure (a /CharProcs value).
    CharProc(ObjRef),
    /// An annotation's normal appearance stream.
    Appearance { annot: ObjRef, stream: ObjRef },
}

/// One `Do` of a form XObject: the operation that called it and the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FormCall {
    pub caller: ContentSource,
    /// Index of the `Do` in the caller's operations.
    pub op: usize,
    pub form: ObjRef,
}

/// Where an event came from. `op` indexes `pdf_core::parse_content` of the
/// decoded bytes of `source`'s stream (for the page, of that /Contents
/// part alone). A stream that is a direct object reports `0 0 R`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Provenance {
    pub source: ContentSource,
    pub op: usize,
    /// For glyphs: which code of the show operation.
    pub glyph: Option<GlyphPos>,
    /// Form XObject calls from the page (or appearance) content down to
    /// `source`, outermost first; empty at page level.
    pub forms: Arc<[FormCall]>,
}

/// A glyph's place in its show operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GlyphPos {
    /// Code index within the whole operation (all strings of a `TJ` array).
    pub index: u32,
    /// Index of the string within the `TJ` array (0 for `Tj ' "`).
    pub element: u32,
    /// Offset of the code's first byte within that string.
    pub byte_offset: u32,
    /// Bytes in the code (1 for simple fonts, 1–4 for Type 0).
    pub byte_len: u8,
}

/// An RGB colour, components 0..=1, converted from the content's colour
/// space the way PyMuPDF's MuPDF converts it with colour management on.
pub type Rgb = [f32; 3];

/// What an area is painted with.
#[derive(Clone, Copy, Debug)]
pub enum Paint<'a> {
    Color(Rgb),
    /// A shading pattern (`/PatternType 2`).
    Shading(&'a ShadingPaint),
    /// A tiling pattern (`/PatternType 1`).
    Tiling(&'a TilingPaint<'a>),
}

/// PDF blend modes (`/BM`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    /// `Compatible` is `Normal`; an unknown name is `None`.
    pub fn from_pdf_name(name: &[u8]) -> Option<BlendMode> {
        Some(match name {
            b"Normal" | b"Compatible" => BlendMode::Normal,
            b"Multiply" => BlendMode::Multiply,
            b"Screen" => BlendMode::Screen,
            b"Overlay" => BlendMode::Overlay,
            b"Darken" => BlendMode::Darken,
            b"Lighten" => BlendMode::Lighten,
            b"ColorDodge" => BlendMode::ColorDodge,
            b"ColorBurn" => BlendMode::ColorBurn,
            b"HardLight" => BlendMode::HardLight,
            b"SoftLight" => BlendMode::SoftLight,
            b"Difference" => BlendMode::Difference,
            b"Exclusion" => BlendMode::Exclusion,
            b"Hue" => BlendMode::Hue,
            b"Saturation" => BlendMode::Saturation,
            b"Color" => BlendMode::Color,
            b"Luminosity" => BlendMode::Luminosity,
            _ => return None,
        })
    }
}

/// Paint plus the transparency state it is composited with.
#[derive(Clone, Copy, Debug)]
pub struct Brush<'a> {
    pub paint: Paint<'a>,
    /// `ca` for fills, `CA` for strokes.
    pub alpha: f32,
    pub blend: BlendMode,
    /// ExtGState `/AIS`: constant alpha contributes to shape, not opacity.
    pub alpha_is_shape: bool,
    /// The ExtGState soft mask in force, if any.
    pub soft_mask: Option<&'a SoftMask<'a>>,
}

#[derive(Clone, Copy, Debug)]
pub struct FillEvent<'a> {
    /// User space → device.
    pub ctm: Matrix,
    pub rule: FillRule,
    pub brush: Brush<'a>,
    /// The painting operator.
    pub source: &'a Provenance,
    /// Index of the path's first construction operator in the same stream
    /// (0 when the path began in an earlier /Contents part).
    pub path_start: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct StrokeEvent<'a> {
    pub ctm: Matrix,
    pub style: &'a StrokeStyle,
    pub brush: Brush<'a>,
    pub source: &'a Provenance,
    pub path_start: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ClipEvent<'a> {
    /// Maps the path to device space: the CTM for `W`, identity for text
    /// clips.
    pub ctm: Matrix,
    pub rule: FillRule,
    /// Where the clip was set: the painting operator after `W`/`W*`, or `ET`.
    pub source: &'a Provenance,
    pub path_start: usize,
    /// True for a text clip (render modes 4–7).
    pub text: bool,
}

/// The characters a glyph stands for: ToUnicode first, then the encoding's
/// glyph names through the Adobe Glyph List, then the font program's names
/// or CID collection, as MuPDF resolves them; U+FFFD when nothing maps.
/// At most 8 characters (a ligature destination).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphText {
    len: u8,
    chars: [char; 8],
}

impl GlyphText {
    pub const REPLACEMENT: GlyphText = GlyphText {
        len: 1,
        chars: ['\u{FFFD}'; 8],
    };

    /// Up to 8 characters from `chars`; empty input gives U+FFFD.
    pub fn new(chars: &[char]) -> GlyphText {
        if chars.is_empty() {
            return GlyphText::REPLACEMENT;
        }
        let mut out = GlyphText {
            len: 0,
            chars: ['\0'; 8],
        };
        for (slot, &c) in out.chars.iter_mut().zip(chars) {
            *slot = c;
            out.len += 1;
        }
        out
    }

    pub fn as_slice(&self) -> &[char] {
        &self.chars[..usize::from(self.len)]
    }

    pub fn first(&self) -> char {
        self.chars[0]
    }
}

impl fmt::Debug for GlyphText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: String = self.as_slice().iter().collect();
        write!(f, "{s:?}")
    }
}

impl fmt::Display for GlyphText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.as_slice() {
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

/// A quadrilateral in MuPDF's corner order.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quad {
    pub ul: Point,
    pub ur: Point,
    pub ll: Point,
    pub lr: Point,
}

impl Quad {
    /// The bounding rectangle of the four corners.
    pub fn rect(&self) -> Rect {
        let xs = [self.ul.x, self.ur.x, self.ll.x, self.lr.x];
        let ys = [self.ul.y, self.ur.y, self.ll.y, self.lr.y];
        Rect::new(
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            ys.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        )
    }
}

/// One shown code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    /// The character code from the string (1–4 bytes, big-endian).
    pub code: u32,
    /// The CID for Type 0 fonts, the code otherwise.
    pub cid: u32,
    /// The glyph in the font program; for Type 3 fonts, the code. Glyph 0 is
    /// `.notdef` (a missing glyph).
    pub gid: u32,
    pub unicode: GlyphText,
    /// Glyph space (1 unit = 1 em, the font matrix already applied; for
    /// Type 3 fonts, text space before /FontMatrix) → device:
    /// `[size·Th 0 0 size 0 rise] × Tm × CTM × page × transform`. The glyph
    /// origin is `(trm.e, trm.f)`. Text state, advances, and matrix arithmetic
    /// use float32 precision; published coordinates are widened to f64.
    pub trm: Matrix,
    /// MuPDF's `fz_advance_glyph`: the font program's advance in em (the
    /// PDF widths for substituted, non-embedded fonts).
    pub advance: f64,
    /// The PDF width that moved the pen (`w0`, or `w1` in vertical mode),
    /// in em (integer width times `0.001f32`; Type 3 widths through /FontMatrix).
    pub width: f64,
    /// MuPDF's stext quad using the PDF advance. Horizontal glyphs span
    /// descender to ascender. Vertical glyphs span one em to the right of
    /// their origin and the signed vertical advance, in device space.
    /// Endpoints use fused float32 multiply-add. The pen rounds its advance
    /// separately, so a glyph's right edge need not touch the next origin.
    pub quad: Quad,
    /// A single-byte code 32, so `Tw` applied after it.
    pub word_space: bool,
    /// MuPDF's guessed bidi level from the first character's bidi class:
    /// 0 left-to-right, 1 right-to-left. Neutral and weak characters keep
    /// the level of the character before them; `BT` resets it to 0.
    pub bidi: u8,
    pub pos: GlyphPos,
}

/// One show operation in one font.
#[derive(Clone, Copy, Debug)]
pub struct TextRun<'a> {
    pub font: &'a PdfFont,
    /// `Tf` size.
    pub size: f64,
    /// `Tr`, 0..=7.
    pub render_mode: u8,
    /// `Tc`, in unscaled text space units.
    pub char_spacing: f64,
    /// `Tw`.
    pub word_spacing: f64,
    /// `Tz` / 100.
    pub horizontal_scale: f64,
    /// `Ts`.
    pub rise: f64,
    /// 0 horizontal, 1 vertical (from the font's CMap).
    pub wmode: u8,
    /// User space → device at the operation.
    pub ctm: Matrix,
    /// `Tm` before the first glyph.
    pub text_matrix: Matrix,
    /// Modes 0, 2, 4, 6.
    pub fill: Option<Brush<'a>>,
    /// Modes 1, 2, 5, 6.
    pub stroke: Option<Brush<'a>>,
    pub stroke_style: &'a StrokeStyle,
    /// Modes 4..=7: the glyphs join the clip set at `ET`.
    pub clip: bool,
    pub glyphs: &'a [Glyph],
    /// The show operation; `glyph` is `None` here.
    pub source: &'a Provenance,
    /// Runs with equal values belong to one MuPDF text object (one
    /// `fz_text`): the counter advances wherever MuPDF's interpreter
    /// flushes pending text (`ET`, `Q`, `cm`, `W`/`W*`, colour and colour
    /// space operators, `gs`, line-style operators, marked content, `Do`,
    /// inline images, `sh`, the end of a stream, a render-mode change and
    /// a directly drawn Type 3 glyph).
    pub text_object: u64,
}

impl TextRun<'_> {
    /// The provenance of one glyph of this run.
    pub fn glyph_source(&self, glyph: &Glyph) -> Provenance {
        Provenance {
            glyph: Some(glyph.pos),
            ..self.source.clone()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ShadingEvent<'a> {
    pub shading: &'a Shading,
    /// Shading space → device.
    pub matrix: Matrix,
    pub alpha: f32,
    pub blend: BlendMode,
    /// ExtGState `/AIS`, false unless explicitly enabled.
    pub alpha_is_shape: bool,
    pub soft_mask: Option<&'a SoftMask<'a>>,
    pub source: &'a Provenance,
}

#[derive(Clone, Copy, Debug)]
pub struct ImageEvent<'a> {
    pub image: &'a PdfImage,
    /// Image space → device: row 0 lies at image-space y=0; y grows
    /// towards the last row at y=1. This is the PDF unit-square CTM
    /// preceded by `[1 0 0 -1 0 1]`.
    pub ctm: Matrix,
    /// The unit square's device-space bounds.
    pub bbox: Rect,
    pub alpha: f32,
    pub blend: BlendMode,
    /// ExtGState `/AIS`, false unless explicitly enabled.
    pub alpha_is_shape: bool,
    pub soft_mask: Option<&'a SoftMask<'a>>,
    /// The `Do` or inline `BI` operation.
    pub source: &'a Provenance,
}

#[derive(Clone, Copy, Debug)]
pub struct ImageMaskEvent<'a> {
    pub image: &'a PdfImage,
    /// Stencil space → device, with row 0 at y=0 and the last row at y=1,
    /// using the same convention as [`ImageEvent::ctm`].
    pub ctm: Matrix,
    pub bbox: Rect,
    /// The fill brush the stencil paints with.
    pub brush: Brush<'a>,
    pub source: &'a Provenance,
}

#[derive(Clone, Copy, Debug)]
pub struct GroupEvent<'a> {
    /// Device-space bounds of the group (/BBox through its matrix).
    pub bbox: Rect,
    pub isolated: bool,
    pub knockout: bool,
    pub blend: BlendMode,
    pub alpha: f32,
    /// ExtGState `/AIS` at group invocation.
    pub alpha_is_shape: bool,
    pub soft_mask: Option<&'a SoftMask<'a>>,
    pub source: &'a Provenance,
}

#[derive(Clone, Copy, Debug)]
pub struct FormEvent<'a> {
    pub form: ObjRef,
    /// Form space → device (/Matrix × CTM at the `Do`).
    pub matrix: Matrix,
    /// The CTM at the `Do`.
    pub ctm: Matrix,
    /// /BBox in form space.
    pub bbox: Rect,
    /// The `Do` operation (in the caller's stream).
    pub source: &'a Provenance,
    /// The form has /Group, so `begin_group` follows.
    pub group: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct MarkedContentEvent<'a> {
    pub tag: &'a Name,
    /// The property list, resolved from /Properties when named.
    pub properties: Option<&'a Dict>,
    /// False when optional content hides what follows.
    pub visible: bool,
    pub source: &'a Provenance,
}

#[derive(Clone, Copy, Debug)]
pub struct AnnotationEvent<'a> {
    pub annot: ObjRef,
    /// /Subtype, e.g. `Widget`, `Link`, `FreeText`.
    pub subtype: &'a Name,
    /// /Rect in device space.
    pub rect: Rect,
    /// /F.
    pub flags: i64,
    /// The appearance stream run.
    pub appearance: ObjRef,
    /// Appearance form space → device.
    pub matrix: Matrix,
}

#[derive(Clone, Copy, Debug)]
pub struct Type3GlyphEvent<'a> {
    pub font: &'a PdfFont,
    pub glyph: &'a Glyph,
    /// The procedure's glyph space → device (/FontMatrix × `glyph.trm`).
    pub matrix: Matrix,
    /// The show operation.
    pub source: &'a Provenance,
}
