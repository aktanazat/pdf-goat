//! MuPDF's select() removes destinations that cannot survive its rebuilt page tree.
//! This differs from merely moving page objects: integer destinations and bare /Dest
//! names are dropped, and the names tree is rebuilt with destinations to retained pages.

use std::collections::HashSet;

use goat_common::GoatError;
use pdf_core::{Dict, Document, ObjRef, Object};

use crate::open::pdf_error;

pub(crate) fn cleanup(doc: &mut Document) -> Result<(), GoatError> {
    let pages: HashSet<ObjRef> = doc.page_refs().map_err(pdf_error)?.into_iter().collect();
    let mut entries = Vec::new();
    for (name, value) in doc.names(b"Dests").map_err(pdf_error)? {
        let resolved = doc.resolve(&value).map_err(pdf_error)?;
        let destination = match &resolved {
            Object::Dict(dict) => doc.resolve_key(dict, b"D").map_err(pdf_error)?,
            _ => resolved.clone(),
        };
        if valid_array(&destination, &pages) {
            entries.push((name, value));
        }
    }
    let names: HashSet<Vec<u8>> = entries.iter().map(|(name, _)| name.clone()).collect();
    doc.set_names(b"Dests", entries).map_err(pdf_error)?;
    for index in 0..doc.page_count().map_err(pdf_error)? {
        let mut page = doc.page(index).map_err(pdf_error)?;
        let Some(stored) = page.dict.get(b"Annots").cloned() else {
            continue;
        };
        let Some(annots) = doc.resolve_array(&stored).map_err(pdf_error)? else {
            continue;
        };
        let mut kept = Vec::with_capacity(annots.len());
        for annot in annots {
            let dict = doc.resolve_dict(&annot).map_err(pdf_error)?;
            let retain = match dict {
                Some(dict) if dict.get_name(b"Subtype") == Some(b"Link".as_slice()) => {
                    valid(doc, &dict, &pages, &names)?
                }
                _ => true,
            };
            if retain {
                kept.push(annot);
            }
        }
        match stored {
            Object::Reference(id) => doc.set(id, kept),
            _ => {
                page.dict.insert("Annots", kept);
                doc.set(page.id, page.dict);
            }
        }
    }
    let mut catalog = doc.catalog().map_err(pdf_error)?;
    if let Some(root) = catalog.get_ref(b"Outlines")
        && strip_outline(doc, root, &pages, &names, 0, &mut HashSet::new())? == 0
    {
        catalog.remove(b"Outlines");
        doc.set(doc.catalog_ref().map_err(pdf_error)?, catalog);
    }
    Ok(())
}

fn valid_array(value: &Object, pages: &HashSet<ObjRef>) -> bool {
    value
        .as_array()
        .and_then(|items| items.first())
        .and_then(Object::as_reference)
        .is_some_and(|id| pages.contains(&id))
}

fn valid(
    doc: &Document,
    dict: &Dict,
    pages: &HashSet<ObjRef>,
    names: &HashSet<Vec<u8>>,
) -> Result<bool, GoatError> {
    if let Some(action) = dict.get(b"A")
        && let Some(action) = doc.resolve_dict(action).map_err(pdf_error)?
        && action.get_name(b"S") == Some(b"GoTo".as_slice())
    {
        match doc.resolve_key(&action, b"D").map_err(pdf_error)? {
            value @ Object::Array(_) if !valid_array(&value, pages) => return Ok(false),
            Object::String(text) if !names.contains(text.as_bytes()) => return Ok(false),
            _ => {}
        }
    }
    Ok(match doc.resolve_key(dict, b"Dest").map_err(pdf_error)? {
        Object::Null => true,
        Object::String(text) => names.contains(text.as_bytes()),
        value => valid_array(&value, pages),
    })
}

fn strip_outline(
    doc: &mut Document,
    parent: ObjRef,
    pages: &HashSet<ObjRef>,
    names: &HashSet<Vec<u8>>,
    depth: usize,
    seen: &mut HashSet<ObjRef>,
) -> Result<usize, GoatError> {
    if depth > 64 || !seen.insert(parent) {
        return Ok(0);
    }
    let Some(mut root) = doc.get(parent).map_err(pdf_error)?.as_dict().cloned() else {
        return Ok(0);
    };
    let mut current = root.get_ref(b"First");
    let mut kept = Vec::new();
    let mut count = 0;
    let mut siblings = HashSet::new();
    while let Some(id) = current {
        if !siblings.insert(id) {
            break;
        }
        let Some(mut item) = doc.get(id).map_err(pdf_error)?.as_dict().cloned() else {
            break;
        };
        current = item.get_ref(b"Next");
        let children = if item.contains_key(b"First") {
            strip_outline(doc, id, pages, names, depth + 1, seen)?
        } else {
            0
        };
        if children > 0 {
            item = doc
                .get(id)
                .map_err(pdf_error)?
                .as_dict()
                .cloned()
                .unwrap_or(item);
        }
        let valid = valid(doc, &item, pages, names)?;
        if valid {
            count += 1;
        } else if children == 0 {
            continue;
        } else {
            item.remove(b"Dest");
            item.remove(b"A");
        }
        // A destinationless parent before the first valid sibling is skipped by select.
        if count > 0 {
            kept.push((id, item));
        }
    }
    for index in 0..kept.len() {
        let previous = index.checked_sub(1).map(|i| kept[i].0);
        let next = kept.get(index + 1).map(|(id, _)| *id);
        let (id, dict) = &mut kept[index];
        match previous {
            Some(id) => {
                dict.insert("Prev", id);
            }
            None => {
                dict.remove(b"Prev");
            }
        }
        match next {
            Some(id) => {
                dict.insert("Next", id);
            }
            None => {
                dict.remove(b"Next");
            }
        }
        doc.set(*id, dict.clone());
    }
    if let (Some((first, _)), Some((last, _))) = (kept.first(), kept.last()) {
        root.insert("First", *first);
        root.insert("Last", *last);
        let sign = if root.get(b"Count").and_then(Object::as_i64).unwrap_or(0) < 0 {
            -1
        } else {
            1
        };
        root.insert("Count", sign * crate::dest::to_i64(count));
    } else {
        root.remove(b"First");
        root.remove(b"Last");
        root.remove(b"Count");
    }
    doc.set(parent, root);
    Ok(count)
}
