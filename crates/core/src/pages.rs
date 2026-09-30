//! The page tree: a flattened page list with inherited attributes, page
//! boxes, and inserting, removing, moving, creating, and importing pages.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError};

use crate::document::Document;
use crate::error::{Error, Result};
use crate::geom::Rect;
use crate::object::{Dict, ObjRef, Object, Stream};
use crate::parser::MAX_DEPTH;

/// Deepest page tree accepted.
const MAX_TREE_DEPTH: usize = 64;
/// Page attributes a page inherits from its ancestors (ISO 32000-2 7.7.3.4).
pub(crate) const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
/// Keys whose values are resolved to direct objects in [`Page::dict`].
const DIRECT_KEYS: [&[u8]; 7] = [
    b"MediaBox",
    b"CropBox",
    b"BleedBox",
    b"TrimBox",
    b"ArtBox",
    b"Rotate",
    b"UserUnit",
];
/// US Letter, used when a page has no valid `/MediaBox`.
const DEFAULT_MEDIA_BOX: Rect = Rect::new(0.0, 0.0, 612.0, 792.0);

/// Values of the inheritable attributes, in [`INHERITABLE`] order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inherited([Option<Object>; 4]);

impl Inherited {
    /// These values overridden by the ones `dict` sets itself.
    fn under(&self, dict: &Dict) -> Inherited {
        let mut out = self.clone();
        for (slot, key) in out.0.iter_mut().zip(INHERITABLE) {
            if let Some(value) = dict.get(key).filter(|v| !v.is_null()) {
                *slot = Some(value.clone());
            }
        }
        out
    }

    /// Copy values into `dict` for the keys it does not set.
    fn fill(&self, dict: &mut Dict) {
        for (value, key) in self.0.iter().zip(INHERITABLE) {
            if let Some(value) = value
                && dict.get(key).is_none_or(Object::is_null)
            {
                dict.insert(key, value.clone());
            }
        }
    }
}

pub(crate) struct PageEntry {
    pub id: ObjRef,
    /// Page tree nodes from the root down to the page's parent.
    pub ancestors: Vec<ObjRef>,
    /// What the page inherits from its ancestors.
    pub inherited: Inherited,
}

/// The flattened page tree, rebuilt after any change to the document.
pub(crate) struct PageTree {
    pub root: ObjRef,
    /// What a page placed directly under the root inherits.
    pub root_inherited: Inherited,
    pub pages: Vec<PageEntry>,
    pub index: HashMap<u32, usize>,
}

impl PageTree {
    fn build(doc: &Document) -> Result<PageTree> {
        let catalog = doc.catalog()?;
        let root = catalog
            .get_ref(b"Pages")
            .ok_or_else(|| Error::invalid("the catalog has no /Pages reference"))?;
        let root_dict = doc.resolve_dict(&Object::Reference(root))?.ok_or_else(|| {
            Error::invalid(format!("the page tree root {root} is not a dictionary"))
        })?;
        let mut tree = PageTree {
            root,
            root_inherited: Inherited::default().under(&root_dict),
            pages: Vec::new(),
            index: HashMap::new(),
        };
        if root_dict.has_type(b"Page") {
            // A catalog whose /Pages is a single page.
            tree.index.insert(root.num, 0);
            tree.pages.push(PageEntry {
                id: root,
                ancestors: Vec::new(),
                inherited: Inherited::default(),
            });
            return Ok(tree);
        }
        let mut walker = Walker {
            doc,
            tree,
            visited: HashSet::from([root.num]),
            ancestors: Vec::new(),
        };
        walker.walk(root, &root_dict, &Inherited::default(), 0)?;
        Ok(walker.tree)
    }
}

/// Depth-first walk of the page tree in `/Kids` order.
struct Walker<'d> {
    doc: &'d Document,
    tree: PageTree,
    /// Nodes and pages already reached; a second reference is skipped.
    visited: HashSet<u32>,
    /// Nodes from the root to the one being walked.
    ancestors: Vec<ObjRef>,
}

impl Walker<'_> {
    fn walk(
        &mut self,
        node: ObjRef,
        dict: &Dict,
        inherited: &Inherited,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            return Err(Error::LimitExceeded(format!(
                "page tree deeper than {MAX_TREE_DEPTH} levels"
            )));
        }
        let here = inherited.under(dict);
        self.ancestors.push(node);
        let kids = match dict.get(b"Kids") {
            Some(kids) => self.doc.resolve_array(kids)?.unwrap_or_default(),
            None => Vec::new(),
        };
        for kid in &kids {
            let Object::Reference(kid_ref) = kid else {
                continue;
            };
            if !self.visited.insert(kid_ref.num) {
                continue;
            }
            let Some(kid_dict) = self.doc.resolve_dict(kid)? else {
                continue;
            };
            if is_page_tree_node(&kid_dict) {
                self.walk(*kid_ref, &kid_dict, &here, depth + 1)?;
            } else {
                let tree = &mut self.tree;
                tree.index.insert(kid_ref.num, tree.pages.len());
                tree.pages.push(PageEntry {
                    id: *kid_ref,
                    ancestors: self.ancestors.clone(),
                    inherited: here.clone(),
                });
            }
        }
        self.ancestors.pop();
        Ok(())
    }
}

fn is_page_tree_node(dict: &Dict) -> bool {
    dict.has_type(b"Pages") || (!dict.has_type(b"Page") && dict.contains_key(b"Kids"))
}

/// One page, with the attributes it inherits filled in.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// The page object.
    pub id: ObjRef,
    /// Zero-based position in the document.
    pub index: usize,
    /// The page dictionary with inherited `/Resources`, `/MediaBox`,
    /// `/CropBox`, and `/Rotate` filled in where the page does not set them,
    /// and the box, `/Rotate`, and `/UserUnit` values resolved to direct
    /// objects. The stored page object is unchanged.
    pub dict: Dict,
    /// `/Resources` resolved to a dictionary; empty when absent. Its values
    /// may still be references.
    pub resources: Dict,
}

impl Page {
    fn raw_box(&self, key: &[u8]) -> Option<Rect> {
        Rect::from_array(self.dict.get_array(key)?)
    }

    /// `/MediaBox`, or US Letter (0 0 612 792) when missing or empty.
    pub fn media_box(&self) -> Rect {
        self.raw_box(b"MediaBox")
            .filter(|r| !r.is_empty())
            .unwrap_or(DEFAULT_MEDIA_BOX)
    }

    /// `/CropBox` clipped to the media box; the media box when missing or
    /// when the clipped box is empty.
    pub fn crop_box(&self) -> Rect {
        let media = self.media_box();
        self.raw_box(b"CropBox")
            .map(|r| r.intersect(&media))
            .filter(|r| !r.is_empty())
            .unwrap_or(media)
    }

    fn box_or_crop(&self, key: &[u8]) -> Rect {
        let media = self.media_box();
        self.raw_box(key)
            .map(|r| r.intersect(&media))
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| self.crop_box())
    }

    /// `/BleedBox` clipped to the media box; the crop box by default.
    pub fn bleed_box(&self) -> Rect {
        self.box_or_crop(b"BleedBox")
    }

    /// `/TrimBox` clipped to the media box; the crop box by default.
    pub fn trim_box(&self) -> Rect {
        self.box_or_crop(b"TrimBox")
    }

    /// `/ArtBox` clipped to the media box; the crop box by default.
    pub fn art_box(&self) -> Rect {
        self.box_or_crop(b"ArtBox")
    }

    /// `/Rotate` normalized to 0, 90, 180, or 270 (other values round to
    /// the nearest quarter turn).
    pub fn rotation(&self) -> u16 {
        let raw = self
            .dict
            .get(b"Rotate")
            .and_then(Object::as_i64)
            .unwrap_or(0);
        let quarter = (raw.rem_euclid(360) + 45) / 90 % 4;
        match quarter {
            1 => 90,
            2 => 180,
            3 => 270,
            _ => 0,
        }
    }

    /// `/UserUnit`, 1.0 when absent or not positive.
    pub fn user_unit(&self) -> f64 {
        self.dict
            .get(b"UserUnit")
            .and_then(Object::as_f64)
            .filter(|u| *u > 0.0 && u.is_finite())
            .unwrap_or(1.0)
    }

    /// The content streams named by `/Contents`, in order.
    pub fn content_refs(&self) -> Vec<ObjRef> {
        match self.dict.get(b"Contents") {
            Some(Object::Reference(id)) => vec![*id],
            Some(Object::Array(items)) => items.iter().filter_map(Object::as_reference).collect(),
            _ => Vec::new(),
        }
    }
}

fn out_of_range(index: usize, count: usize) -> Error {
    Error::invalid(format!(
        "page index {index} is out of range for {count} pages"
    ))
}

impl Document {
    pub(crate) fn page_tree(&self) -> Result<Arc<PageTree>> {
        if let Some(tree) = self
            .page_tree
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Ok(Arc::clone(tree));
        }
        let tree = Arc::new(PageTree::build(self)?);
        *self
            .page_tree
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&tree));
        Ok(tree)
    }

    /// Number of pages reachable through the page tree.
    pub fn page_count(&self) -> Result<usize> {
        Ok(self.page_tree()?.pages.len())
    }

    /// The page objects in order.
    pub fn page_refs(&self) -> Result<Vec<ObjRef>> {
        Ok(self.page_tree()?.pages.iter().map(|p| p.id).collect())
    }

    /// The page at `index` (zero-based).
    pub fn page(&self, index: usize) -> Result<Page> {
        let tree = self.page_tree()?;
        let entry = tree
            .pages
            .get(index)
            .ok_or_else(|| out_of_range(index, tree.pages.len()))?;
        self.page_from_entry(entry, index)
    }

    /// Every page in order.
    pub fn pages(&self) -> Result<Vec<Page>> {
        let tree = self.page_tree()?;
        tree.pages
            .iter()
            .enumerate()
            .map(|(i, entry)| self.page_from_entry(entry, i))
            .collect()
    }

    /// The index of the page object `id` (generation ignored).
    pub fn page_index(&self, id: ObjRef) -> Result<Option<usize>> {
        Ok(self.page_tree()?.index.get(&id.num).copied())
    }

    fn page_from_entry(&self, entry: &PageEntry, index: usize) -> Result<Page> {
        let mut dict = self
            .resolve_dict(&Object::Reference(entry.id))?
            .ok_or_else(|| Error::invalid(format!("page {} is not a dictionary", entry.id)))?;
        entry.inherited.fill(&mut dict);
        for key in DIRECT_KEYS {
            let Some(value) = dict.get(key) else { continue };
            let value = match self.resolve(value)? {
                Object::Array(items) => Object::Array(
                    items
                        .iter()
                        .map(|item| self.resolve(item))
                        .collect::<Result<Vec<_>>>()?,
                ),
                other => other,
            };
            dict.insert(key, value);
        }
        let resources = match dict.get(b"Resources") {
            Some(value) => self.resolve_dict(value)?.unwrap_or_default(),
            None => Dict::new(),
        };
        Ok(Page {
            id: entry.id,
            index,
            dict,
            resources,
        })
    }

    /// The page's content streams decoded and joined with newlines.
    pub fn page_content(&self, page: &Page) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let streams = match page.dict.get(b"Contents") {
            Some(value) => match self.resolve(value)? {
                Object::Array(items) => items,
                other => vec![other],
            },
            None => Vec::new(),
        };
        for item in &streams {
            let Some(stream) = self.resolve_stream(item)? else {
                continue;
            };
            if !out.is_empty() {
                out.push(b'\n');
            }
            out.extend_from_slice(&self.decode_stream(&stream)?.data);
        }
        Ok(out)
    }

    /// Insert the existing page object `page` at `index` (0 to the page
    /// count). The page inherits whatever its new parent provides; its
    /// `/Parent` is set.
    pub fn insert_page(&mut self, index: usize, page: ObjRef) -> Result<()> {
        self.insert_page_refs(index, &[page]).map(|_| ())
    }

    /// Put `pages` at `index` under one parent, updating `/Kids`, every
    /// ancestor's `/Count`, and each page's `/Parent`. Returns what the pages
    /// inherit there.
    fn insert_page_refs(&mut self, index: usize, pages: &[ObjRef]) -> Result<Inherited> {
        let tree = self.page_tree()?;
        let count = tree.pages.len();
        if index > count {
            return Err(out_of_range(index, count));
        }
        if let Some(id) = pages.iter().find(|id| tree.index.contains_key(&id.num)) {
            return Err(Error::invalid(format!(
                "page {id} is already in the page tree"
            )));
        }
        let (ancestors, anchor, context) = if count == 0 {
            (vec![tree.root], None, tree.root_inherited.clone())
        } else {
            let (entry, after) = if index < count {
                (&tree.pages[index], false)
            } else {
                (&tree.pages[count - 1], true)
            };
            (
                entry.ancestors.clone(),
                Some((entry.id, after)),
                entry.inherited.clone(),
            )
        };
        let parent = *ancestors
            .last()
            .ok_or_else(|| Error::invalid("the page tree root is a page"))?;
        let added: Vec<Object> = pages.iter().map(|id| Object::Reference(*id)).collect();
        self.update_kids(parent, |kids| {
            let at = anchor
                .and_then(|(id, after)| {
                    kids.iter()
                        .position(|k| k.as_reference().is_some_and(|r| r.num == id.num))
                        .map(|i| i + usize::from(after))
                })
                .unwrap_or(kids.len());
            kids.splice(at..at, added);
        })?;
        self.adjust_counts(&ancestors, i64::try_from(pages.len()).unwrap_or(i64::MAX))?;
        for id in pages {
            let mut dict = self.page_dict(*id)?;
            dict.insert("Parent", parent);
            self.set(*id, dict);
        }
        Ok(context)
    }

    fn page_dict(&self, id: ObjRef) -> Result<Dict> {
        self.resolve_dict(&Object::Reference(id))?
            .ok_or_else(|| Error::invalid(format!("page {id} is not a dictionary")))
    }

    fn update_kids(&mut self, parent: ObjRef, change: impl FnOnce(&mut Vec<Object>)) -> Result<()> {
        let mut dict = self.page_dict(parent)?;
        let stored = dict.get(b"Kids").cloned();
        let mut kids = match &stored {
            Some(value) => self.resolve_array(value)?.unwrap_or_default(),
            None => Vec::new(),
        };
        change(&mut kids);
        match stored {
            Some(Object::Reference(id)) => self.set(id, kids),
            _ => {
                dict.insert("Kids", kids);
                self.set(parent, dict);
            }
        }
        Ok(())
    }

    fn adjust_counts(&mut self, nodes: &[ObjRef], delta: i64) -> Result<()> {
        for node in nodes {
            let mut dict = self.page_dict(*node)?;
            let count = self.resolve_key(&dict, b"Count")?.as_i64().unwrap_or(0);
            dict.insert("Count", count.saturating_add(delta).max(0));
            self.set(*node, dict);
        }
        Ok(())
    }

    /// Take the page at `index` out of the page tree and return it. The page
    /// object itself stays until a garbage-collecting save.
    pub fn remove_page(&mut self, index: usize) -> Result<ObjRef> {
        let tree = self.page_tree()?;
        let entry = tree
            .pages
            .get(index)
            .ok_or_else(|| out_of_range(index, tree.pages.len()))?;
        let id = entry.id;
        let ancestors = entry.ancestors.clone();
        let Some(&parent) = ancestors.last() else {
            return Err(Error::Unsupported(
                "removing the only page of a tree whose root is a page".into(),
            ));
        };
        self.update_kids(parent, |kids| {
            if let Some(at) = kids
                .iter()
                .position(|k| k.as_reference().is_some_and(|r| r.num == id.num))
            {
                kids.remove(at);
            }
        })?;
        self.adjust_counts(&ancestors, -1)?;
        Ok(id)
    }

    /// Move the page at `from` so it ends up at index `to`. It keeps its
    /// inherited attributes.
    pub fn move_page(&mut self, from: usize, to: usize) -> Result<()> {
        let tree = self.page_tree()?;
        let count = tree.pages.len();
        if to >= count {
            return Err(out_of_range(to, count));
        }
        let entry = tree
            .pages
            .get(from)
            .ok_or_else(|| out_of_range(from, count))?;
        let (id, own) = (entry.id, entry.inherited.clone());
        let mut dict = self.page_dict(id)?;
        own.fill(&mut dict);
        self.set(id, dict);
        self.remove_page(from)?;
        let context = self.insert_page_refs(to, &[id])?;
        self.pin_defaults(id, &context)
    }

    /// Create an empty page (`/MediaBox`, empty `/Resources`, an empty
    /// content stream) and insert it at `index`.
    pub fn insert_blank_page(&mut self, index: usize, media_box: Rect) -> Result<ObjRef> {
        let count = self.page_count()?;
        if index > count {
            return Err(out_of_range(index, count));
        }
        let contents = self.add(Stream::new(Dict::new(), Vec::new()));
        let mut page = Dict::new();
        page.insert("Type", Object::name("Page"));
        page.insert("MediaBox", media_box.normalized().to_object());
        page.insert("Resources", Dict::new());
        page.insert("Contents", contents);
        let id = self.add(page);
        let context = self.insert_page_refs(index, &[id])?;
        self.pin_defaults(id, &context)?;
        Ok(id)
    }

    /// Give a page explicit defaults for the attributes its new ancestors
    /// would otherwise lend it, so moving it does not change it.
    fn pin_defaults(&mut self, id: ObjRef, context: &Inherited) -> Result<()> {
        let mut dict = self.page_dict(id)?;
        let mut changed = false;
        for (value, key) in context.0.iter().zip(INHERITABLE) {
            if value.is_none() || dict.get(key).is_some_and(|v| !v.is_null()) {
                continue;
            }
            let default = match key {
                b"Resources" => Object::Dict(Dict::new()),
                b"CropBox" => dict
                    .get(b"MediaBox")
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_MEDIA_BOX.to_object()),
                b"Rotate" => Object::Integer(0),
                _ => DEFAULT_MEDIA_BOX.to_object(),
            };
            dict.insert(key, default);
            changed = true;
        }
        if changed {
            self.set(id, dict);
        }
        Ok(())
    }

    /// Copy pages `pages` (indices into `source`) and insert them at `index`
    /// in order. Each page is deep-copied with its content, resources, and
    /// annotations; an object several copied pages share is copied once per
    /// call. References to pages that are not copied become null, and the
    /// source's page tree and catalog are never copied. Returns the new
    /// page objects.
    pub fn import_pages(
        &mut self,
        source: &Document,
        pages: &[usize],
        index: usize,
    ) -> Result<Vec<ObjRef>> {
        if source.needs_password() {
            return Err(Error::NeedsPassword);
        }
        let count = self.page_count()?;
        if index > count {
            return Err(out_of_range(index, count));
        }
        let tree = source.page_tree()?;
        let mut copier = Copier {
            source,
            source_pages: tree.pages.iter().map(|p| p.id.num).collect(),
            page_map: HashMap::new(),
            memo: HashMap::new(),
            queue: Vec::new(),
        };
        let mut jobs = Vec::with_capacity(pages.len());
        for &i in pages {
            let entry = tree
                .pages
                .get(i)
                .ok_or_else(|| out_of_range(i, tree.pages.len()))?;
            let target = self.reserve();
            copier.page_map.entry(entry.id.num).or_insert(target);
            jobs.push((entry, target));
        }
        for (entry, target) in &jobs {
            let mut dict = source.page_dict(entry.id)?;
            for key in [b"Parent".as_slice(), b"StructParents", b"B"] {
                dict.remove(key);
            }
            entry.inherited.fill(&mut dict);
            let copied = copier.copy(self, Object::Dict(dict), 0)?;
            self.set(*target, copied);
        }
        while let Some((num, target)) = copier.queue.pop() {
            let object = source
                .fetch(num, 0)?
                .map(|o| o.as_ref().clone())
                .unwrap_or_default();
            let copied = copier.copy(self, object, 0)?;
            self.set(target, copied);
        }
        let targets: Vec<ObjRef> = jobs.iter().map(|(_, target)| *target).collect();
        let context = self.insert_page_refs(index, &targets)?;
        for target in &targets {
            self.pin_defaults(*target, &context)?;
        }
        Ok(targets)
    }
}

/// Deep copy of objects from another document with references renumbered.
struct Copier<'s> {
    source: &'s Document,
    source_pages: HashSet<u32>,
    /// Source page number to the first copy made of it.
    page_map: HashMap<u32, ObjRef>,
    /// Source object number to its copy.
    memo: HashMap<u32, ObjRef>,
    /// Source objects allocated a number but not yet copied.
    queue: Vec<(u32, ObjRef)>,
}

impl Copier<'_> {
    fn copy(&mut self, target: &mut Document, object: Object, depth: usize) -> Result<Object> {
        if depth > MAX_DEPTH {
            return Err(Error::LimitExceeded(format!(
                "objects nested deeper than {MAX_DEPTH} levels"
            )));
        }
        Ok(match object {
            Object::Reference(id) => self.map_ref(target, id)?,
            Object::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.copy(target, item, depth + 1)?);
                }
                Object::Array(out)
            }
            Object::Dict(dict) => Object::Dict(self.copy_dict(target, dict, depth)?),
            Object::Stream(mut stream) => {
                stream.dict = self.copy_dict(target, std::mem::take(&mut stream.dict), depth)?;
                Object::Stream(stream)
            }
            other => other,
        })
    }

    fn copy_dict(&mut self, target: &mut Document, dict: Dict, depth: usize) -> Result<Dict> {
        let mut out = Dict::with_capacity(dict.len());
        for (key, value) in dict {
            let value = self.copy(target, value, depth + 1)?;
            out.insert(key, value);
        }
        Ok(out)
    }

    fn map_ref(&mut self, target: &mut Document, id: ObjRef) -> Result<Object> {
        if let Some(copy) = self.page_map.get(&id.num) {
            return Ok(Object::Reference(*copy));
        }
        if self.source_pages.contains(&id.num) {
            return Ok(Object::Null);
        }
        if let Some(copy) = self.memo.get(&id.num) {
            return Ok(Object::Reference(*copy));
        }
        let Some(object) = self.source.fetch(id.num, 0)? else {
            return Ok(Object::Null);
        };
        if let Object::Dict(dict) = object.as_ref()
            && (dict.has_type(b"Pages") || dict.has_type(b"Catalog") || dict.has_type(b"Page"))
        {
            return Ok(Object::Null);
        }
        let copy = target.reserve();
        self.memo.insert(id.num, copy);
        self.queue.push((id.num, copy));
        Ok(Object::Reference(copy))
    }
}
