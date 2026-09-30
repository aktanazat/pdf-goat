//! Deterministic PDF fixtures for tests, built with pdf-core: positioned
//! standard-14 text, shapes, images, links, outlines, form widgets,
//! attachments, layers, encryption, and incremental updates, plus ports of
//! the Python suite's `tests/fixtures.py`.
//!
//! Positions use PyMuPDF page space: the origin is the top-left corner of the
//! page as created (`/MediaBox [0 0 width height]`), y grows downward, and
//! `/Rotate` is ignored. A text point is the baseline origin, as in
//! `page.insert_text((x, y), text)`; rects are `[x0, y0, x1, y1]`.
//!
//! Output is byte-for-byte deterministic (a fixed `/ID`, no dates) except
//! encrypted output, whose salts and IVs are random. Builders panic when they
//! are misused: they only ever run inside tests.

use std::path::{Path, PathBuf};

use pdf_core::filters::flate_encode;
use pdf_core::{
    Dict, Document, Encryption, NewEncryption, NewMethod, ObjRef, Object, PdfString, Rect,
    SaveOptions, Stream,
};

pub use pdf_codec::JpegColor;
pub use pdf_font::Standard14;

/// PyMuPDF's `new_page()` size.
pub const A4: (f64, f64) = (595.0, 842.0);
pub const LETTER: (f64, f64) = (612.0, 792.0);

const FILE_ID: &[u8; 16] = b"goat-fixtures-id";
const MAX_DEPTH: usize = 64;
/// `/Ff` bit 18: a choice field that is a combo box.
const COMBO_FLAG: i64 = 1 << 17;
/// Every permission (bits 1 and 2 are reserved as zero).
const ALL_PERMISSIONS: i32 = -4;

/// Font, size, and fill colour for [`PageBuilder::styled_text`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub font: Standard14,
    pub size: f64,
    pub color: [f64; 3],
}

impl TextStyle {
    pub fn new(font: Standard14, size: f64) -> TextStyle {
        TextStyle {
            font,
            size,
            color: [0.0; 3],
        }
    }

    pub fn with_color(mut self, color: [f64; 3]) -> TextStyle {
        self.color = color;
        self
    }
}

impl Default for TextStyle {
    /// Helvetica 11 black, PyMuPDF's `insert_text` defaults.
    fn default() -> TextStyle {
        TextStyle::new(Standard14::Helvetica, 11.0)
    }
}

/// How [`PageBuilder::rect`] and [`PageBuilder::line`] paint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paint {
    pub fill: Option<[f64; 3]>,
    pub stroke: Option<[f64; 3]>,
    pub width: f64,
}

impl Paint {
    pub fn fill(color: [f64; 3]) -> Paint {
        Paint {
            fill: Some(color),
            stroke: None,
            width: 1.0,
        }
    }

    pub fn stroke(color: [f64; 3], width: f64) -> Paint {
        Paint {
            fill: None,
            stroke: Some(color),
            width,
        }
    }

    pub fn fill_stroke(fill: [f64; 3], stroke: [f64; 3], width: f64) -> Paint {
        Paint {
            fill: Some(fill),
            stroke: Some(stroke),
            width,
        }
    }
}

/// An optional content group made by [`PdfBuilder::layer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer(usize);

#[derive(Clone, Debug, PartialEq)]
enum FieldKind {
    Text { value: String },
    Checkbox { checked: bool },
    Combo { options: Vec<String>, value: String },
}

#[derive(Clone, Debug)]
enum Annot {
    Uri {
        rect: [f64; 4],
        uri: String,
    },
    GoTo {
        rect: [f64; 4],
        page: usize,
    },
    Widget {
        name: String,
        rect: [f64; 4],
        kind: FieldKind,
    },
    FileAttachment {
        rect: [f64; 4],
        name: String,
        data: Vec<u8>,
    },
    Raw(Dict),
}

/// A whole document, turned into pdf-core objects by [`PdfBuilder::build`].
#[derive(Clone, Debug, Default)]
pub struct PdfBuilder {
    pages: Vec<PageBuilder>,
    outline: Vec<(u32, String, i64)>,
    info: Vec<(String, String)>,
    catalog: Vec<(String, Object)>,
    attachments: Vec<(String, Vec<u8>)>,
    layers: Vec<(String, bool)>,
    plain: bool,
}

impl PdfBuilder {
    pub fn new() -> PdfBuilder {
        PdfBuilder::default()
    }

    /// Append a page with `/MediaBox [0 0 width height]`.
    pub fn page(&mut self, width: f64, height: f64) -> &mut PageBuilder {
        let index = self.pages.len();
        self.pages.push(PageBuilder::new(width, height));
        &mut self.pages[index]
    }

    /// The page at `index` (0-based). Panics when out of range.
    pub fn page_at(&mut self, index: usize) -> &mut PageBuilder {
        &mut self.pages[index]
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// One PyMuPDF table-of-contents row: `level` from 1, `page` 1-based
    /// (below 1: no destination). Written like `set_toc`: a GoTo action to
    /// `[page /XYZ 72 (height - 36) 0]`, entries below level 1 collapsed.
    pub fn outline(&mut self, level: u32, title: &str, page: i64) -> &mut Self {
        self.outline.push((level, title.to_string(), page));
        self
    }

    /// A text entry of `/Info`, e.g. `("Title", "Report")`.
    pub fn info(&mut self, key: &str, value: &str) -> &mut Self {
        self.info.push((key.to_string(), value.to_string()));
        self
    }

    /// Any catalog entry, e.g. `("Lang", Object::string("en"))`. A stream in
    /// `value` is written as its own object.
    pub fn catalog_entry(&mut self, key: &str, value: Object) -> &mut Self {
        self.catalog.push((key.to_string(), value));
        self
    }

    /// A document-level attachment in `/Names /EmbeddedFiles`.
    pub fn attachment(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.attachments.push((name.to_string(), data.to_vec()));
        self
    }

    /// An optional content group listed in `/OCProperties`; hidden groups go
    /// in `/D /OFF`.
    pub fn layer(&mut self, name: &str, visible: bool) -> Layer {
        self.layers.push((name.to_string(), visible));
        Layer(self.layers.len() - 1)
    }

    /// Flate-compress unfiltered streams on save (the default). Off keeps
    /// content streams plain, so their hex text strings can be searched.
    pub fn compress(&mut self, on: bool) -> &mut Self {
        self.plain = !on;
        self
    }

    /// The document as pdf-core objects, for further changes before saving.
    pub fn build(&self) -> Document {
        let mut doc = Document::new();
        let file_id = Object::String(PdfString::hex(FILE_ID.to_vec()));
        doc.trailer_mut()
            .insert("ID", vec![file_id.clone(), file_id]);
        let catalog_ref = doc.catalog_ref().expect("a new document has a catalog");
        let mut catalog = doc.catalog().expect("a new document has a catalog");
        let pages_ref = catalog
            .get_ref(b"Pages")
            .expect("a new document has a page tree");

        let page_refs: Vec<ObjRef> = self.pages.iter().map(|_| doc.reserve()).collect();
        let layers: Vec<ObjRef> = self
            .layers
            .iter()
            .map(|(name, _)| {
                let mut ocg = Dict::new();
                ocg.insert("Type", Object::name("OCG"));
                ocg.insert("Name", Object::text(name));
                doc.add(ocg)
            })
            .collect();
        let mut ctx = Context {
            page_refs: page_refs.clone(),
            heights: self.pages.iter().map(|page| page.height).collect(),
            fonts: Vec::new(),
            layers: layers.clone(),
            fields: Vec::new(),
            shared_names: shared_field_names(&self.pages),
        };
        for (index, page) in self.pages.iter().enumerate() {
            let mut dict = page.build(&mut doc, &mut ctx, index);
            dict.insert("Parent", pages_ref);
            doc.set(page_refs[index], dict);
        }
        let mut tree = Dict::new();
        tree.insert("Type", Object::name("Pages"));
        tree.insert(
            "Kids",
            page_refs
                .iter()
                .map(|id| Object::Reference(*id))
                .collect::<Vec<_>>(),
        );
        tree.insert("Count", page_refs.len());
        doc.set(pages_ref, tree);

        if let Some(outline) = self.build_outline(&mut doc, &ctx) {
            catalog.insert("Outlines", outline);
        }
        if let Some(acroform) = ctx.finish_fields(&mut doc) {
            catalog.insert("AcroForm", acroform);
        }
        if !layers.is_empty() {
            catalog.insert("OCProperties", self.oc_properties(&layers));
        }
        for (key, value) in &self.catalog {
            let value = lift_streams(&mut doc, value.clone(), 0);
            catalog.insert(key.as_str(), value);
        }
        doc.set(catalog_ref, catalog);

        if !self.attachments.is_empty() {
            let entries = self
                .attachments
                .iter()
                .map(|(name, data)| {
                    (
                        PdfString::from_text(name).bytes,
                        Object::Reference(add_filespec(&mut doc, name, data)),
                    )
                })
                .collect();
            doc.set_names(b"EmbeddedFiles", entries)
                .expect("the catalog takes a name tree");
        }
        if !self.info.is_empty() {
            let info: Dict = self
                .info
                .iter()
                .map(|(key, value)| (key.as_str().into(), Object::text(value)))
                .collect();
            doc.set_info(info);
        }
        doc
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_bytes_with(&SaveOptions {
            compress_streams: !self.plain,
            ..SaveOptions::default()
        })
    }

    pub fn to_bytes_with(&self, options: &SaveOptions) -> Vec<u8> {
        self.build()
            .save_to_bytes(options)
            .expect("a built fixture saves")
    }

    /// AES-256 (V5 R6) with every permission granted.
    pub fn to_encrypted_bytes(&self, user: &str, owner: &str) -> Vec<u8> {
        let encryption = NewEncryption {
            method: NewMethod::Aes256,
            user_password: user.as_bytes().to_vec(),
            owner_password: owner.as_bytes().to_vec(),
            permissions: ALL_PERMISSIONS,
            encrypt_metadata: true,
        };
        self.to_bytes_with(&SaveOptions {
            compress_streams: !self.plain,
            encryption: Encryption::New(encryption),
            ..SaveOptions::default()
        })
    }

    /// Write [`PdfBuilder::to_bytes`] to `path` and return the path.
    pub fn save(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        std::fs::write(path, self.to_bytes()).expect("the fixture file is writable");
        path.to_path_buf()
    }

    fn build_outline(&self, doc: &mut Document, ctx: &Context) -> Option<ObjRef> {
        let first = self.outline.first()?;
        assert_eq!(first.0, 1, "the first outline row must be level 1");
        let root = doc.reserve();
        let ids: Vec<ObjRef> = self.outline.iter().map(|_| doc.reserve()).collect();
        // Node 0 is the root; row i is node i + 1.
        let mut nodes = vec![OutlineNode::default(); self.outline.len() + 1];
        let mut last_at_level: Vec<usize> = vec![0];
        for (row, (level, _, _)) in self.outline.iter().enumerate() {
            let level = *level as usize;
            assert!(
                (1..=last_at_level.len()).contains(&level),
                "outline row {row} jumps to level {level}"
            );
            last_at_level.truncate(level);
            let node = row + 1;
            let parent = last_at_level[level - 1];
            nodes[node].parent = Some(parent);
            nodes[parent].count += if level > 1 { -1 } else { 1 };
            match nodes[parent].last {
                Some(previous) => {
                    nodes[node].prev = Some(previous);
                    nodes[previous].next = Some(node);
                }
                None => nodes[parent].first = Some(node),
            }
            nodes[parent].last = Some(node);
            last_at_level.push(node);
        }
        let id_of = |node: usize| if node == 0 { root } else { ids[node - 1] };
        for (node, links) in nodes.iter().enumerate() {
            let mut dict = Dict::new();
            if links.count != 0 {
                dict.insert("Count", links.count);
            }
            if node > 0 {
                let (_, _, page) = &self.outline[node - 1];
                if let Some(index) = usize::try_from(*page - 1).ok().filter(|_| *page >= 1) {
                    let index = index.min(ctx.page_refs.len().saturating_sub(1));
                    if let Some(target) = ctx.page_refs.get(index) {
                        dict.insert("A", goto_action(*target, 72.0, ctx.heights[index] - 36.0));
                    }
                }
            }
            for (key, value) in [
                ("First", links.first),
                ("Last", links.last),
                ("Next", links.next),
            ] {
                if let Some(other) = value {
                    dict.insert(key, id_of(other));
                }
            }
            if let Some(parent) = links.parent {
                dict.insert("Parent", id_of(parent));
            }
            if let Some(prev) = links.prev {
                dict.insert("Prev", id_of(prev));
            }
            if node == 0 {
                dict.insert("Type", Object::name("Outlines"));
            } else {
                dict.insert("Title", Object::text(&self.outline[node - 1].1));
            }
            doc.set(id_of(node), dict);
        }
        Some(root)
    }

    fn oc_properties(&self, layers: &[ObjRef]) -> Dict {
        let refs = |visible: Option<bool>| -> Vec<Object> {
            layers
                .iter()
                .zip(&self.layers)
                .filter(|(_, (_, shown))| visible.is_none_or(|v| v == *shown))
                .map(|(id, _)| Object::Reference(*id))
                .collect()
        };
        let mut config = Dict::new();
        config.insert("Order", refs(None));
        config.insert("ON", refs(Some(true)));
        config.insert("OFF", refs(Some(false)));
        let mut properties = Dict::new();
        properties.insert("OCGs", refs(None));
        properties.insert("D", config);
        properties
    }
}

#[derive(Clone, Debug, Default)]
struct OutlineNode {
    parent: Option<usize>,
    first: Option<usize>,
    last: Option<usize>,
    prev: Option<usize>,
    next: Option<usize>,
    count: i64,
}

/// One page: drawing operations, resources, annotations, and raw entries.
#[derive(Clone, Debug)]
pub struct PageBuilder {
    width: f64,
    height: f64,
    rotate: Option<i64>,
    boxes: Vec<(String, [f64; 4])>,
    content: Vec<u8>,
    fonts: Vec<Standard14>,
    images: Vec<Stream>,
    resources: Vec<(String, String, Object)>,
    layers: Vec<usize>,
    annots: Vec<Annot>,
    entries: Vec<(String, Object)>,
}

impl PageBuilder {
    fn new(width: f64, height: f64) -> PageBuilder {
        PageBuilder {
            width,
            height,
            rotate: None,
            boxes: Vec::new(),
            content: Vec::new(),
            fonts: Vec::new(),
            images: Vec::new(),
            resources: Vec::new(),
            layers: Vec::new(),
            annots: Vec::new(),
            entries: Vec::new(),
        }
    }

    /// `/Rotate degrees`, written as given. Content stays in unrotated space.
    pub fn rotate(&mut self, degrees: i64) -> &mut Self {
        self.rotate = Some(degrees);
        self
    }

    /// A page box in raw PDF coordinates, e.g. `("CropBox", [0.0, 0.0, 300.0, 400.0])`.
    pub fn set_box(&mut self, key: &str, rect: [f64; 4]) -> &mut Self {
        self.boxes.push((key.to_string(), rect));
        self
    }

    /// One line of Helvetica 11 black text with its baseline origin at (x, y).
    pub fn text(&mut self, x: f64, y: f64, text: &str) -> &mut Self {
        self.styled_text(x, y, text, TextStyle::default())
    }

    /// One line of text with its baseline origin at (x, y). Characters the
    /// font's encoding lacks are written as code 0xB7.
    pub fn styled_text(&mut self, x: f64, y: f64, text: &str, style: TextStyle) -> &mut Self {
        let resource = self.use_font(style.font);
        let [r, g, b] = style.color;
        let op = format!(
            "q\nBT\n{} {} {} rg\n/{resource} {} Tf\n1 0 0 1 {} {} Tm\n<{}> Tj\nET\nQ\n",
            num(r),
            num(g),
            num(b),
            num(style.size),
            num(x),
            num(self.height - y),
            hex(&style.font.encode_text(text)),
        );
        self.content.extend_from_slice(op.as_bytes());
        self
    }

    pub fn rect(&mut self, rect: [f64; 4], paint: Paint) -> &mut Self {
        let [x0, y0, x1, y1] = rect;
        let op = format!(
            "q\n{}{} {} {} {} re\n{}\nQ\n",
            paint_setup(&paint),
            num(x0),
            num(self.height - y1),
            num(x1 - x0),
            num(y1 - y0),
            paint_operator(&paint),
        );
        self.content.extend_from_slice(op.as_bytes());
        self
    }

    /// A stroked segment in the paint's stroke colour (black when unset).
    pub fn line(&mut self, from: (f64, f64), to: (f64, f64), paint: Paint) -> &mut Self {
        let stroke = Paint {
            fill: None,
            stroke: Some(paint.stroke.unwrap_or([0.0; 3])),
            width: paint.width,
        };
        let op = format!(
            "q\n{}{} {} m\n{} {} l\nS\nQ\n",
            paint_setup(&stroke),
            num(from.0),
            num(self.height - from.1),
            num(to.0),
            num(self.height - to.1),
        );
        self.content.extend_from_slice(op.as_bytes());
        self
    }

    /// An 8-bit DeviceRGB Flate image stretched over `rect`.
    pub fn image_rgb(&mut self, rect: [f64; 4], width: u32, height: u32, rgb: &[u8]) -> &mut Self {
        self.flate_image(rect, width, height, "DeviceRGB", 3, rgb)
    }

    /// An 8-bit DeviceGray Flate image stretched over `rect`.
    pub fn image_gray(
        &mut self,
        rect: [f64; 4],
        width: u32,
        height: u32,
        gray: &[u8],
    ) -> &mut Self {
        self.flate_image(rect, width, height, "DeviceGray", 1, gray)
    }

    /// JPEG bytes stored as-is in a `/DCTDecode` image stretched over
    /// `rect`. Adobe CMYK gets `/Decode [1 0 1 0 1 0 1 0]`.
    pub fn image_jpeg(&mut self, rect: [f64; 4], jpeg: &[u8]) -> &mut Self {
        let info = pdf_codec::jpeg_info(jpeg).expect("image_jpeg needs JPEG data");
        let space = match info.components {
            1 => "DeviceGray",
            4 => "DeviceCMYK",
            _ => "DeviceRGB",
        };
        let mut dict = image_dict(info.width, info.height, Object::name(space));
        dict.insert("Filter", Object::name("DCTDecode"));
        if info.adobe_inverted_cmyk() {
            dict.insert(
                "Decode",
                [1, 0, 1, 0, 1, 0, 1, 0]
                    .into_iter()
                    .map(Object::Integer)
                    .collect::<Vec<_>>(),
            );
        }
        self.image_xobject(rect, Stream::new(dict, jpeg.to_vec()))
    }

    /// Any image XObject stretched over `rect`, named `/Im<n>` in page order.
    /// A stream nested in its dictionary (e.g. `/SMask`) becomes its own object.
    pub fn image_xobject(&mut self, rect: [f64; 4], image: Stream) -> &mut Self {
        let [x0, y0, x1, y1] = rect;
        let name = format!("Im{}", self.images.len());
        self.images.push(image);
        let op = format!(
            "q\n{} 0 0 {} {} {} cm\n/{name} Do\nQ\n",
            num(x1 - x0),
            num(y1 - y0),
            num(x0),
            num(self.height - y1),
        );
        self.content.extend_from_slice(op.as_bytes());
        self
    }

    /// A Link annotation with a URI action, as PyMuPDF's `insert_link` writes it.
    pub fn link_uri(&mut self, rect: [f64; 4], uri: &str) -> &mut Self {
        self.annots.push(Annot::Uri {
            rect,
            uri: uri.to_string(),
        });
        self
    }

    /// A Link annotation to page `page` (0-based), as PyMuPDF writes it:
    /// `/A << /S /GoTo /D [page /XYZ 0 height 0] >>`.
    pub fn link_goto(&mut self, rect: [f64; 4], page: usize) -> &mut Self {
        self.annots.push(Annot::GoTo { rect, page });
        self
    }

    /// A text field widget showing `value`. Widgets that share a name (on
    /// any page) become the `/Kids` of one field.
    pub fn text_field(&mut self, name: &str, rect: [f64; 4], value: &str) -> &mut Self {
        self.widget(
            name,
            rect,
            FieldKind::Text {
                value: value.to_string(),
            },
        )
    }

    /// A checkbox widget with `/AP /N << /Yes .. /Off .. >>` and `/AS`.
    pub fn checkbox(&mut self, name: &str, rect: [f64; 4], checked: bool) -> &mut Self {
        self.widget(name, rect, FieldKind::Checkbox { checked })
    }

    /// A combo box widget with `/Opt` and the value shown.
    pub fn combo(
        &mut self,
        name: &str,
        rect: [f64; 4],
        options: &[&str],
        value: &str,
    ) -> &mut Self {
        let options = options.iter().map(|option| (*option).to_string()).collect();
        self.widget(
            name,
            rect,
            FieldKind::Combo {
                options,
                value: value.to_string(),
            },
        )
    }

    /// A `/FileAttachment` annotation embedding `data` as `name`.
    pub fn file_attachment_annotation(
        &mut self,
        rect: [f64; 4],
        name: &str,
        data: &[u8],
    ) -> &mut Self {
        self.annots.push(Annot::FileAttachment {
            rect,
            name: name.to_string(),
            data: data.to_vec(),
        });
        self
    }

    /// Any annotation dictionary, added to `/Annots` as given (raw PDF
    /// coordinates). A stream inside it becomes its own object.
    pub fn annotation(&mut self, dict: Dict) -> &mut Self {
        self.annots.push(Annot::Raw(dict));
        self
    }

    /// Start marked content belonging to `layer`: `/OC /oc<n> BDC`.
    pub fn begin_layer(&mut self, layer: Layer) -> &mut Self {
        if !self.layers.contains(&layer.0) {
            self.layers.push(layer.0);
        }
        self.content
            .extend_from_slice(format!("/OC /oc{} BDC\n", layer.0).as_bytes());
        self
    }

    pub fn end_layer(&mut self) -> &mut Self {
        self.content.extend_from_slice(b"EMC\n");
        self
    }

    /// Bytes appended to the content stream as given.
    pub fn raw_content(&mut self, bytes: &[u8]) -> &mut Self {
        self.content.extend_from_slice(bytes);
        if !bytes.ends_with(b"\n") {
            self.content.push(b'\n');
        }
        self
    }

    /// A resource entry, e.g. `("XObject", "Im9", Object::Stream(..))`. A
    /// stream becomes its own object.
    pub fn resource(&mut self, category: &str, name: &str, value: Object) -> &mut Self {
        self.resources
            .push((category.to_string(), name.to_string(), value));
        self
    }

    /// Any page dictionary entry, written last so it overrides the builder's.
    pub fn entry(&mut self, key: &str, value: Object) -> &mut Self {
        self.entries.push((key.to_string(), value));
        self
    }

    fn use_font(&mut self, font: Standard14) -> &'static str {
        if !self.fonts.contains(&font) {
            self.fonts.push(font);
        }
        font_resource_name(font)
    }

    fn widget(&mut self, name: &str, rect: [f64; 4], kind: FieldKind) -> &mut Self {
        self.annots.push(Annot::Widget {
            name: name.to_string(),
            rect,
            kind,
        });
        self
    }

    fn flate_image(
        &mut self,
        rect: [f64; 4],
        width: u32,
        height: u32,
        space: &str,
        channels: usize,
        samples: &[u8],
    ) -> &mut Self {
        let expected = width as usize * height as usize * channels;
        assert_eq!(
            samples.len(),
            expected,
            "a {width}x{height} {space} image needs {expected} bytes"
        );
        let mut dict = image_dict(width, height, Object::name(space));
        dict.insert("Filter", Object::name("FlateDecode"));
        self.image_xobject(rect, Stream::new(dict, flate_encode(samples)))
    }

    fn pdf_rect(&self, rect: [f64; 4]) -> Object {
        let [x0, y0, x1, y1] = rect;
        Rect::new(x0, self.height - y1, x1, self.height - y0).to_object()
    }

    fn build(&self, doc: &mut Document, ctx: &mut Context, index: usize) -> Dict {
        let mut dict = Dict::new();
        dict.insert("Type", Object::name("Page"));
        dict.insert(
            "MediaBox",
            Rect::new(0.0, 0.0, self.width, self.height).to_object(),
        );
        for (key, [x0, y0, x1, y1]) in &self.boxes {
            dict.insert(key.as_str(), Rect::new(*x0, *y0, *x1, *y1).to_object());
        }
        if let Some(rotate) = self.rotate {
            dict.insert("Rotate", rotate);
        }
        let resources = self.build_resources(doc, ctx);
        dict.insert("Resources", resources);
        let contents = doc.add(Stream::new(Dict::new(), self.content.clone()));
        dict.insert("Contents", contents);
        let annots = self.build_annots(doc, ctx, index);
        if !annots.is_empty() {
            dict.insert(
                "Annots",
                annots
                    .into_iter()
                    .map(Object::Reference)
                    .collect::<Vec<_>>(),
            );
        }
        for (key, value) in &self.entries {
            let value = lift_streams(doc, value.clone(), 0);
            dict.insert(key.as_str(), value);
        }
        dict
    }

    fn build_resources(&self, doc: &mut Document, ctx: &mut Context) -> Dict {
        let mut resources = Dict::new();
        if !self.fonts.is_empty() {
            let mut fonts = Dict::new();
            for font in &self.fonts {
                fonts.insert(font_resource_name(*font), ctx.font(doc, *font));
            }
            resources.insert("Font", fonts);
        }
        if !self.images.is_empty() {
            let mut xobjects = Dict::new();
            for (n, image) in self.images.iter().enumerate() {
                let image = lift_streams(doc, Object::Stream(image.clone()), 0);
                xobjects.insert(format!("Im{n}"), image);
            }
            resources.insert("XObject", xobjects);
        }
        if !self.layers.is_empty() {
            let mut properties = Dict::new();
            for layer in &self.layers {
                properties.insert(format!("oc{layer}"), ctx.layers[*layer]);
            }
            resources.insert("Properties", properties);
        }
        for (category, name, value) in &self.resources {
            let value = lift_streams(doc, value.clone(), 0);
            match resources
                .get_mut(category.as_bytes())
                .and_then(Object::as_dict_mut)
            {
                Some(entries) => {
                    entries.insert(name.as_str(), value);
                }
                None => {
                    let mut entries = Dict::new();
                    entries.insert(name.as_str(), value);
                    resources.insert(category.as_str(), entries);
                }
            }
        }
        resources
    }

    fn build_annots(&self, doc: &mut Document, ctx: &mut Context, index: usize) -> Vec<ObjRef> {
        let page_ref = ctx.page_refs[index];
        let mut out = Vec::with_capacity(self.annots.len());
        for annot in &self.annots {
            let id = match annot {
                Annot::Uri { rect, uri } => {
                    let mut action = Dict::new();
                    action.insert("S", Object::name("URI"));
                    action.insert("URI", Object::string(uri.as_bytes()));
                    doc.add(self.link_dict(*rect, action))
                }
                Annot::GoTo { rect, page } => {
                    let target = *ctx
                        .page_refs
                        .get(*page)
                        .unwrap_or_else(|| panic!("link_goto: no page {page}"));
                    let action = goto_action(target, 0.0, ctx.heights[*page]);
                    doc.add(self.link_dict(*rect, action))
                }
                Annot::Widget { name, rect, kind } => {
                    self.build_widget(doc, ctx, page_ref, name, *rect, kind)
                }
                Annot::FileAttachment { rect, name, data } => {
                    let spec = add_filespec(doc, name, data);
                    let mut dict = Dict::new();
                    dict.insert("Type", Object::name("Annot"));
                    dict.insert("Subtype", Object::name("FileAttachment"));
                    dict.insert("Rect", self.pdf_rect(*rect));
                    dict.insert("FS", spec);
                    dict.insert("Contents", Object::text(name));
                    dict.insert("Name", Object::name("PushPin"));
                    doc.add(dict)
                }
                Annot::Raw(dict) => match lift_streams(doc, Object::Dict(dict.clone()), 0) {
                    Object::Dict(dict) => doc.add(dict),
                    other => doc.add(other),
                },
            };
            out.push(id);
        }
        out
    }

    fn link_dict(&self, rect: [f64; 4], action: Dict) -> Dict {
        let mut border = Dict::new();
        border.insert("W", 0);
        let mut dict = Dict::new();
        dict.insert("Type", Object::name("Annot"));
        dict.insert("Subtype", Object::name("Link"));
        dict.insert("Rect", self.pdf_rect(rect));
        dict.insert("BS", border);
        dict.insert("A", action);
        dict
    }

    fn build_widget(
        &self,
        doc: &mut Document,
        ctx: &mut Context,
        page_ref: ObjRef,
        name: &str,
        rect: [f64; 4],
        kind: &FieldKind,
    ) -> ObjRef {
        let widget = doc.reserve();
        let (width, height) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());
        let mut dict = Dict::new();
        dict.insert("Type", Object::name("Annot"));
        dict.insert("Subtype", Object::name("Widget"));
        dict.insert("Rect", self.pdf_rect(rect));
        dict.insert("F", 4);
        dict.insert("P", page_ref);
        let mut appearance = Dict::new();
        match kind {
            FieldKind::Text { value } | FieldKind::Combo { value, .. } => {
                dict.insert("MK", Dict::new());
                let helv = ctx.font(doc, Standard14::Helvetica);
                let content = if value.is_empty() {
                    "/Tx BMC\nEMC\n".to_string()
                } else {
                    format!(
                        "/Tx BMC\nq\nBT\n/Helv {} Tf\n0 g\n2 {} Td\n<{}> Tj\nET\nQ\nEMC\n",
                        num(FIELD_FONT_SIZE),
                        num((height - FIELD_FONT_SIZE) / 2.0 + 0.22 * FIELD_FONT_SIZE),
                        hex(&Standard14::Helvetica.encode_text(value)),
                    )
                };
                appearance.insert(
                    "N",
                    form_xobject(doc, width, height, Some(("Helv", helv)), content),
                );
            }
            FieldKind::Checkbox { checked } => {
                let mut mk = Dict::new();
                mk.insert("CA", Object::string("4"));
                dict.insert("MK", mk);
                dict.insert("AS", Object::name(if *checked { "Yes" } else { "Off" }));
                let zadb = ctx.font(doc, Standard14::ZapfDingbats);
                let size = 0.8 * width.min(height);
                let glyph = f64::from(Standard14::ZapfDingbats.code_width(b'4')) * size / 1000.0;
                let on = format!(
                    "q\nBT\n/ZaDb {} Tf\n0 g\n{} {} Td\n<34> Tj\nET\nQ\n",
                    num(size),
                    num((width - glyph) / 2.0),
                    num((height - 0.7 * size) / 2.0),
                );
                let mut states = Dict::new();
                states.insert(
                    "Yes",
                    form_xobject(doc, width, height, Some(("ZaDb", zadb)), on),
                );
                states.insert("Off", form_xobject(doc, width, height, None, String::new()));
                appearance.insert("N", states);
            }
        }
        dict.insert("AP", appearance);
        if ctx.shared_names.iter().any(|shared| shared == name) {
            let parent = match ctx
                .fields
                .iter_mut()
                .find(|field| field.shared && field.name == name)
            {
                Some(field) => {
                    field.kids.push(widget);
                    field.id
                }
                None => {
                    let id = doc.reserve();
                    ctx.fields.push(Field {
                        name: name.to_string(),
                        id,
                        kind: kind.clone(),
                        kids: vec![widget],
                        shared: true,
                    });
                    id
                }
            };
            dict.insert("Parent", parent);
        } else {
            field_entries(&mut dict, name, kind);
            ctx.fields.push(Field {
                name: name.to_string(),
                id: widget,
                kind: kind.clone(),
                kids: Vec::new(),
                shared: false,
            });
        }
        doc.set(widget, dict);
        widget
    }
}

const FIELD_FONT_SIZE: f64 = 11.0;

struct Field {
    name: String,
    /// The field dictionary: the widget itself unless the name is shared.
    id: ObjRef,
    kind: FieldKind,
    kids: Vec<ObjRef>,
    shared: bool,
}

struct Context {
    page_refs: Vec<ObjRef>,
    heights: Vec<f64>,
    fonts: Vec<(Standard14, ObjRef)>,
    layers: Vec<ObjRef>,
    /// Terminal fields in the order their first widget appears.
    fields: Vec<Field>,
    /// Field names more than one widget uses.
    shared_names: Vec<String>,
}

impl Context {
    fn font(&mut self, doc: &mut Document, font: Standard14) -> ObjRef {
        if let Some((_, id)) = self.fonts.iter().find(|(known, _)| *known == font) {
            return *id;
        }
        let id = doc.add(font_dict(font));
        self.fonts.push((font, id));
        id
    }

    /// Write the shared parent fields and return `/AcroForm`, when any
    /// widget exists.
    fn finish_fields(&mut self, doc: &mut Document) -> Option<Dict> {
        if self.fields.is_empty() {
            return None;
        }
        for field in self.fields.iter().filter(|field| field.shared) {
            let mut dict = Dict::new();
            field_entries(&mut dict, &field.name, &field.kind);
            dict.insert(
                "Kids",
                field
                    .kids
                    .iter()
                    .map(|id| Object::Reference(*id))
                    .collect::<Vec<_>>(),
            );
            doc.set(field.id, dict);
        }
        let mut fonts = Dict::new();
        fonts.insert("Helv", self.font(doc, Standard14::Helvetica));
        fonts.insert("ZaDb", self.font(doc, Standard14::ZapfDingbats));
        let mut resources = Dict::new();
        resources.insert("Font", fonts);
        let mut acroform = Dict::new();
        acroform.insert(
            "Fields",
            self.fields
                .iter()
                .map(|field| Object::Reference(field.id))
                .collect::<Vec<_>>(),
        );
        acroform.insert("DR", resources);
        acroform.insert("DA", Object::string("/Helv 0 Tf 0 g"));
        Some(acroform)
    }
}

fn shared_field_names(pages: &[PageBuilder]) -> Vec<String> {
    let mut seen: Vec<(&str, usize)> = Vec::new();
    for annot in pages.iter().flat_map(|page| &page.annots) {
        if let Annot::Widget { name, .. } = annot {
            match seen.iter_mut().find(|(known, _)| known == name) {
                Some((_, count)) => *count += 1,
                None => seen.push((name, 1)),
            }
        }
    }
    seen.into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name.to_string())
        .collect()
}

/// `/FT`, `/T`, `/Ff`, `/Opt`, `/V`, and `/DA` of a terminal field.
fn field_entries(dict: &mut Dict, name: &str, kind: &FieldKind) {
    let text_da = format!("/Helv {} Tf 0 g", num(FIELD_FONT_SIZE));
    match kind {
        FieldKind::Text { value } => {
            dict.insert("FT", Object::name("Tx"));
            dict.insert("T", Object::text(name));
            dict.insert("V", Object::text(value));
            dict.insert("DA", Object::string(text_da));
        }
        FieldKind::Checkbox { checked } => {
            dict.insert("FT", Object::name("Btn"));
            dict.insert("T", Object::text(name));
            dict.insert("V", Object::name(if *checked { "Yes" } else { "Off" }));
            dict.insert("DA", Object::string("/ZaDb 0 Tf 0 g"));
        }
        FieldKind::Combo { options, value } => {
            dict.insert("FT", Object::name("Ch"));
            dict.insert("T", Object::text(name));
            dict.insert("Ff", COMBO_FLAG);
            dict.insert(
                "Opt",
                options
                    .iter()
                    .map(|option| Object::text(option))
                    .collect::<Vec<_>>(),
            );
            dict.insert("V", Object::text(value));
            dict.insert("DA", Object::string(text_da));
        }
    }
}

fn form_xobject(
    doc: &mut Document,
    width: f64,
    height: f64,
    font: Option<(&str, ObjRef)>,
    content: String,
) -> ObjRef {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Form"));
    dict.insert("BBox", Rect::new(0.0, 0.0, width, height).to_object());
    let mut resources = Dict::new();
    if let Some((name, id)) = font {
        let mut fonts = Dict::new();
        fonts.insert(name, id);
        resources.insert("Font", fonts);
    }
    dict.insert("Resources", resources);
    doc.add(Stream::new(dict, content.into_bytes()))
}

/// `<< /S /GoTo /D [page /XYZ left top 0] >>`.
fn goto_action(page: ObjRef, left: f64, top: f64) -> Dict {
    let mut action = Dict::new();
    action.insert("S", Object::name("GoTo"));
    action.insert(
        "D",
        vec![
            Object::Reference(page),
            Object::name("XYZ"),
            real(left),
            real(top),
            Object::Integer(0),
        ],
    );
    action
}

fn add_filespec(doc: &mut Document, name: &str, data: &[u8]) -> ObjRef {
    let mut params = Dict::new();
    params.insert("Size", data.len());
    let mut file = Dict::new();
    file.insert("Type", Object::name("EmbeddedFile"));
    file.insert("Params", params);
    let file = doc.add(Stream::new(file, data.to_vec()));
    let mut embedded = Dict::new();
    embedded.insert("F", file);
    let mut spec = Dict::new();
    spec.insert("Type", Object::name("Filespec"));
    spec.insert("F", Object::text(name));
    spec.insert("UF", Object::text(name));
    spec.insert("EF", embedded);
    doc.add(spec)
}

fn image_dict(width: u32, height: u32, color_space: Object) -> Dict {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Image"));
    dict.insert("Width", width);
    dict.insert("Height", height);
    dict.insert("ColorSpace", color_space);
    dict.insert("BitsPerComponent", 8);
    dict
}

/// A non-embedded standard-14 font with its AFM widths for codes 32-255.
fn font_dict(font: Standard14) -> Dict {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Font"));
    dict.insert("Subtype", Object::name("Type1"));
    dict.insert("BaseFont", Object::name(font.name()));
    if !matches!(font, Standard14::Symbol | Standard14::ZapfDingbats) {
        dict.insert("Encoding", Object::name("WinAnsiEncoding"));
    }
    dict.insert("FirstChar", 32);
    dict.insert("LastChar", 255);
    let widths: Vec<Object> = (32..=255u8)
        .map(|code| Object::Integer(i64::from(font.code_width(code))))
        .collect();
    dict.insert("Widths", widths);
    dict
}

/// PyMuPDF's resource name for each base-14 font.
fn font_resource_name(font: Standard14) -> &'static str {
    match font {
        Standard14::Courier => "cour",
        Standard14::CourierBold => "cobo",
        Standard14::CourierBoldOblique => "cobi",
        Standard14::CourierOblique => "coit",
        Standard14::Helvetica => "helv",
        Standard14::HelveticaBold => "hebo",
        Standard14::HelveticaBoldOblique => "hebi",
        Standard14::HelveticaOblique => "heit",
        Standard14::Symbol => "symb",
        Standard14::TimesBold => "tibo",
        Standard14::TimesBoldItalic => "tibi",
        Standard14::TimesItalic => "tiit",
        Standard14::TimesRoman => "tiro",
        Standard14::ZapfDingbats => "zadb",
    }
}

/// Replace every stream inside `object` (and `object` itself, when it is
/// one) with a reference to a new object: PDF streams must be indirect.
fn lift_streams(doc: &mut Document, object: Object, depth: usize) -> Object {
    assert!(
        depth < MAX_DEPTH,
        "fixture objects nest deeper than {MAX_DEPTH} levels"
    );
    match object {
        Object::Stream(mut stream) => {
            stream.dict = lift_dict(doc, std::mem::take(&mut stream.dict), depth);
            Object::Reference(doc.add(stream))
        }
        Object::Dict(dict) => Object::Dict(lift_dict(doc, dict, depth)),
        Object::Array(items) => Object::Array(
            items
                .into_iter()
                .map(|item| lift_streams(doc, item, depth + 1))
                .collect(),
        ),
        other => other,
    }
}

fn lift_dict(doc: &mut Document, dict: Dict, depth: usize) -> Dict {
    dict.into_iter()
        .map(|(key, value)| (key, lift_streams(doc, value, depth + 1)))
        .collect()
}

fn paint_setup(paint: &Paint) -> String {
    let mut setup = String::new();
    if let Some([r, g, b]) = paint.fill {
        setup.push_str(&format!("{} {} {} rg\n", num(r), num(g), num(b)));
    }
    if let Some([r, g, b]) = paint.stroke {
        setup.push_str(&format!(
            "{} {} {} RG\n{} w\n",
            num(r),
            num(g),
            num(b),
            num(paint.width)
        ));
    }
    setup
}

fn paint_operator(paint: &Paint) -> &'static str {
    match (paint.fill.is_some(), paint.stroke.is_some()) {
        (true, true) => "B",
        (true, false) => "f",
        (false, true) => "S",
        (false, false) => "n",
    }
}

/// A number for content streams: integers bare, otherwise at most four
/// decimals.
fn num(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded == rounded.trunc() && rounded.abs() < 1e15 {
        return format!("{}", rounded as i64);
    }
    let text = format!("{rounded:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A whole number as an integer object, anything else as a real.
fn real(value: f64) -> Object {
    if value == value.trunc() && value.abs() < 1e15 {
        Object::Integer(value as i64)
    } else {
        Object::Real(value)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn save(doc: &Document) -> Vec<u8> {
    doc.save_to_bytes(&SaveOptions {
        compress_streams: true,
        ..SaveOptions::default()
    })
    .expect("a built fixture saves")
}

/// Load `base`, apply `edit`, and return the file with the changes appended
/// as an incremental update (the original bytes come first, unchanged).
pub fn incremental_update(base: &[u8], edit: impl FnOnce(&mut Document)) -> Vec<u8> {
    let mut doc = Document::load(base.to_vec()).expect("the base of an incremental update loads");
    edit(&mut doc);
    doc.save_incremental(true)
        .expect("the incremental update saves")
        .data
}

/// One A4 page per text, each drawn at (72, 72) in Helvetica 11: the Python
/// tests' `write_cache_document`.
pub fn text_pages(texts: &[&str]) -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    for text in texts {
        builder.page(A4.0, A4.1).text(72.0, 72.0, text);
    }
    builder.to_bytes()
}

/// `write_transcript`: a positioned two-column transcript on one Letter page.
pub fn transcript(issue_date: &str, terms: &[(&str, &[&str])]) -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let page = builder.page(LETTER.0, LETTER.1);
    let style = TextStyle::new(Standard14::Helvetica, 10.0);
    let issued = format!("Issued: {issue_date}");
    let left = [
        "UNIVERSITY OF TEST",
        "OFFICIAL ACADEMIC TRANSCRIPT",
        "Student: REDACTED",
        issued.as_str(),
        "Degree: Master of Science",
        "Degree Awarded: 2026-06-15",
    ];
    let mut row = 0.0;
    for text in left {
        page.styled_text(40.0, 60.0 + row * 20.0, text, style);
        row += 1.0;
    }
    row = 0.0;
    for (term, courses) in terms {
        page.styled_text(350.0, 60.0 + row * 20.0, term, style);
        row += 1.0;
        page.styled_text(
            350.0,
            60.0 + row * 20.0,
            "Course ID Course Title Grade Units Points",
            style,
        );
        row += 1.0;
        for course in *courses {
            page.styled_text(350.0, 60.0 + row * 20.0, course, style);
            row += 1.0;
        }
        page.styled_text(350.0, 60.0 + row * 20.0, "Term GPA: 3.50", style);
        row += 1.0;
    }
    page.styled_text(
        350.0,
        60.0 + row * 20.0,
        "Transfer Credit: EXAMPLE COLLEGE",
        style,
    );
    page.styled_text(
        350.0,
        80.0 + row * 20.0,
        "HIST 100 World History B 3.00 9.00",
        style,
    );
    builder.to_bytes()
}

/// `write_single_column`.
pub fn single_column() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let style = TextStyle::new(Standard14::Helvetica, 10.0);
    builder
        .page(LETTER.0, LETTER.1)
        .styled_text(40.0, 60.0, "SINGLE COLUMN SAFE FIXTURE", style)
        .styled_text(40.0, 80.0, "No private content", style);
    builder.to_bytes()
}

/// `jpeg_bytes`: one flat JPEG as PIL's `Image.new(mode, size)` makes it,
/// black for Gray and Rgb, no ink for Cmyk (stored Adobe-inverted), quality 75.
pub fn jpeg_bytes(color: JpegColor, width: u32, height: u32) -> Vec<u8> {
    let channels = match color {
        JpegColor::Gray => 1,
        JpegColor::Rgb => 3,
        JpegColor::Cmyk => 4,
    };
    let samples = vec![0; width as usize * height as usize * channels];
    pdf_codec::encode_jpeg(&samples, width, height, color, 75, None)
        .expect("jpeg_bytes takes an encodable size")
}

/// `write_image_pdf`: page 1 draws `rgb_jpeg` over (10, 10, 110, 85); page
/// 2 draws the 16x12 `cmyk_jpeg` (`/DecodeParms << /ColorTransform 1 >>`)
/// behind a zero soft mask with `/Matte [0 0 0 0]`.
pub fn image_pdf(rgb_jpeg: &[u8], cmyk_jpeg: &[u8]) -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    builder
        .page(200.0, 200.0)
        .image_jpeg([10.0, 10.0, 110.0, 85.0], rgb_jpeg);
    let mut mask = image_dict(16, 12, Object::name("DeviceGray"));
    mask.insert("Matte", vec![Object::Integer(0); 4]);
    let mut image = image_dict(16, 12, Object::name("DeviceCMYK"));
    image.insert("Filter", Object::name("DCTDecode"));
    let mut parms = Dict::new();
    parms.insert("ColorTransform", 1);
    image.insert("DecodeParms", parms);
    image.insert("SMask", Stream::new(mask, vec![0; 16 * 12]));
    builder
        .page(200.0, 200.0)
        .resource(
            "XObject",
            "Im0",
            Object::Stream(Stream::new(image, cmyk_jpeg.to_vec())),
        )
        .raw_content(b"q 100 0 0 75 10 100 cm /Im0 Do Q");
    builder.to_bytes()
}

/// `write_empty_page_pdf`: a blank page, a page with text, and a page with an
/// empty content stream that keeps the second page's `/Resources`.
pub fn empty_page_pdf() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    builder.page(200.0, 200.0);
    builder
        .page(200.0, 200.0)
        .text(20.0, 40.0, "second page has text");
    let mut doc = builder.build();
    let text_page = doc.page(1).expect("the fixture has a second page");
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("Page"));
    dict.insert(
        "MediaBox",
        text_page.dict.get(b"MediaBox").cloned().unwrap_or_default(),
    );
    dict.insert(
        "Resources",
        text_page
            .dict
            .get(b"Resources")
            .cloned()
            .unwrap_or_default(),
    );
    let contents = doc.add(Stream::new(Dict::new(), Vec::new()));
    dict.insert("Contents", contents);
    let page = doc.add(dict);
    doc.insert_page(2, page).expect("a page can be appended");
    save(&doc)
}

/// `write_declined_image_pdf`: one 100x100 page drawing `/Im0`, Flate-labelled
/// garbage, and `/Im1`, a 4x4 `/Separation /Spot` raster.
pub fn declined_image_pdf() -> Vec<u8> {
    let mut broken = image_dict(4, 4, Object::name("DeviceGray"));
    broken.insert("Filter", Object::name("FlateDecode"));
    let mut tint = Dict::new();
    tint.insert("FunctionType", 2);
    tint.insert("Domain", vec![Object::Integer(0), Object::Integer(1)]);
    tint.insert("C0", vec![Object::Integer(1)]);
    tint.insert("C1", vec![Object::Integer(0)]);
    tint.insert("N", 1);
    let separation = vec![
        Object::name("Separation"),
        Object::name("Spot"),
        Object::name("DeviceGray"),
        Object::Dict(tint),
    ];
    let spot = image_dict(4, 4, Object::Array(separation));
    let mut builder = PdfBuilder::new();
    builder
        .page(100.0, 100.0)
        .resource(
            "XObject",
            "Im0",
            Object::Stream(Stream::new(broken, b"not deflate data".to_vec())),
        )
        .resource(
            "XObject",
            "Im1",
            Object::Stream(Stream::new(spot, (0..16).collect())),
        )
        .raw_content(b"q 40 0 0 40 10 10 cm /Im0 Do Q q 40 0 0 40 50 50 cm /Im1 Do Q");
    builder.to_bytes()
}
