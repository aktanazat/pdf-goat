use std::collections::HashSet;

use goat_common::GoatError;
use pdf_core::{Dict, Document, ObjRef, Object};

use crate::appearance::{child_dict, dict};
use crate::{error, field_full_name, page_widgets, update_widget_appearance};

const MAX_DEPTH: usize = 128;

/// Clear a field's live, reset, and rich-text values, including shared parents.
///
/// Every widget with the selected full field name is rebuilt. Old normal,
/// rollover, and down appearances are detached as well. The caller must save
/// with garbage collection, not incrementally, to remove the detached objects.
pub fn clear_field_value(doc: &mut Document, widget: ObjRef) -> Result<(), GoatError> {
    let selected = dict(doc, &Object::Reference(widget))?;
    let name = field_full_name(doc, &selected)?;
    let mut widgets = Vec::new();
    for page in 0..doc.page_count().map_err(error)? {
        for id in page_widgets(doc, page)? {
            let field = dict(doc, &Object::Reference(id))?;
            if field_full_name(doc, &field)? == name {
                widgets.push(id);
            }
        }
    }

    // Include matching field-tree nodes without a page widget: otherwise their
    // /DV or /RV could survive a full garbage-collecting save through /Fields.
    let acro = child_dict(doc, &doc.catalog().map_err(error)?, b"AcroForm")?;
    let fields = doc.resolve_key(&acro, b"Fields").map_err(error)?;
    let mut pending: Vec<_> = fields
        .as_array()
        .unwrap_or(&[])
        .iter()
        .map(|o| (o.clone(), 0_usize))
        .collect();
    let mut visited = HashSet::new();
    let mut affected: HashSet<ObjRef> = widgets.iter().copied().collect();
    affected.insert(widget);
    while let Some((object, depth)) = pending.pop() {
        if depth >= MAX_DEPTH {
            return Err(GoatError::message("form field nesting exceeds 128"));
        }
        if let Some(id) = object.as_reference()
            && !visited.insert(id)
        {
            continue;
        }
        let field = dict(doc, &object)?;
        if field_full_name(doc, &field)? == name
            && let Some(id) = object.as_reference()
        {
            affected.insert(id);
        }
        let kids = doc.resolve_key(&field, b"Kids").map_err(error)?;
        pending.extend(
            kids.as_array()
                .unwrap_or(&[])
                .iter()
                .map(|o| (o.clone(), depth + 1)),
        );
    }

    let mut ancestors: Vec<_> = affected.iter().map(|id| (*id, 0_usize)).collect();
    let mut visited = HashSet::new();
    while let Some((id, depth)) = ancestors.pop() {
        if depth >= MAX_DEPTH {
            return Err(GoatError::message("form field parent nesting exceeds 128"));
        }
        if !visited.insert(id) {
            continue;
        }
        let field = dict(doc, &Object::Reference(id))?;
        if let Some(parent) = field.get_ref(b"Parent") {
            affected.insert(parent);
            ancestors.push((parent, depth + 1));
        }
    }
    for id in affected {
        let mut field = dict(doc, &Object::Reference(id))?;
        clear_dictionary(&mut field, 0)?;
        doc.set(id, field);
    }
    for id in widgets {
        update_widget_appearance(doc, id)?;
    }
    Ok(())
}

fn clear_dictionary(field: &mut Dict, depth: usize) -> Result<(), GoatError> {
    if depth >= MAX_DEPTH {
        return Err(GoatError::message("form field parent nesting exceeds 128"));
    }
    for key in [b"V".as_slice(), b"DV", b"RV", b"I", b"AP"] {
        field.remove(key);
    }
    if field.contains_key(b"AS") {
        field.insert("AS", Object::name("Off"));
    }
    if let Some(Object::Dict(parent)) = field.get_mut(b"Parent") {
        clear_dictionary(parent, depth + 1)?;
    }
    Ok(())
}
