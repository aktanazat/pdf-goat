//! The document outline as PyMuPDF's `get_toc()` lists it, with each item's object so a
//! verb can edit it.

use std::collections::HashSet;

use goat_common::GoatError;
use pdf_core::{Dict, Document, ObjRef, Object};

use crate::dest::{self, Target};
use crate::open::pdf_error;

/// Deepest outline nesting followed.
const MAX_DEPTH: usize = 64;

/// One `get_toc()` row and the outline item it came from.
pub(crate) struct TocItem {
    /// The item object; `None` for an item written inline (PyMuPDF's xref 0).
    pub(crate) id: Option<ObjRef>,
    pub(crate) level: usize,
    pub(crate) title: String,
    /// One-based target page: `-1` for an external or missing target, 0 when MuPDF
    /// cannot find the page the target names.
    pub(crate) page: i64,
}

/// Every outline item, depth first. The caller has checked that the document is not
/// locked (PyMuPDF's `init_doc`).
pub(crate) fn toc(doc: &Document) -> Result<Vec<TocItem>, GoatError> {
    let catalog = doc.catalog().map_err(pdf_error)?;
    let Some(outlines) = catalog.get(b"Outlines") else {
        return Ok(Vec::new());
    };
    let Some(root) = doc.resolve_dict(outlines).map_err(pdf_error)? else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    if let Some(first) = root.get(b"First") {
        walk(doc, first.clone(), 1, &mut HashSet::new(), &mut items)?;
    }
    Ok(items)
}

/// One sibling chain from `first`, each item followed by its children. An item already
/// on the chain or on an ancestor's chain ends the chain, as MuPDF's object marks do.
fn walk(
    doc: &Document,
    first: Object,
    level: usize,
    marked: &mut HashSet<u32>,
    items: &mut Vec<TocItem>,
) -> Result<(), GoatError> {
    if level > MAX_DEPTH {
        return Ok(());
    }
    let mut chain = Vec::new();
    let mut node = Some(first);
    while let Some(object) = node.take() {
        let id = object.as_reference();
        if let Some(id) = id {
            if !marked.insert(id.num) {
                break;
            }
            chain.push(id.num);
        }
        let Some(item) = doc.resolve_dict(&object).map_err(pdf_error)? else {
            break;
        };
        let title = match doc.resolve_key(&item, b"Title").map_err(pdf_error)? {
            Object::String(text) => text.to_text(),
            _ => String::new(),
        };
        items.push(TocItem {
            id,
            level,
            title: if title.is_empty() {
                " ".to_owned()
            } else {
                title
            },
            page: item_page(doc, &item)?,
        });
        if let Some(child) = item.get(b"First") {
            walk(doc, child.clone(), level + 1, marked, items)?;
        }
        node = item.get(b"Next").cloned();
    }
    for num in chain {
        marked.remove(&num);
    }
    Ok(())
}

/// `/Dest` before `/A`; PyMuPDF adds one to the page MuPDF resolves.
fn item_page(doc: &Document, item: &Dict) -> Result<i64, GoatError> {
    let target = match (item.get(b"Dest"), item.get(b"A")) {
        (Some(dest), _) => dest::dest_target(doc, dest)?,
        (None, Some(action)) => dest::action_target(doc, action, None)?,
        (None, None) => Target::Nothing,
    };
    Ok(match target {
        Target::Page(page) => page + 1,
        Target::Nothing | Target::External => -1,
    })
}

/// `_remove_toc_item`: the item keeps its place and title, points nowhere, and turns gray.
pub(crate) fn disable_item(doc: &mut Document, id: ObjRef) -> Result<(), GoatError> {
    let Some(mut item) = doc.get(id).map_err(pdf_error)?.as_dict().cloned() else {
        return Ok(());
    };
    item.remove(b"Dest");
    item.remove(b"A");
    item.insert("C", Object::Array(vec![Object::Real(0.8); 3]));
    doc.set(id, Object::Dict(item));
    Ok(())
}
