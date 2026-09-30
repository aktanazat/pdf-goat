//! Visibility, normal appearance selection, and annotation placement.

use crate::device::{AnnotationEvent, Device};
use crate::interp::Run;
use crate::{Intent, InterpError, page_transform};
use pdf_core::{Dict, Matrix, Name, ObjRef, Object, Page, Rect};

pub(crate) fn run_all(
    run: &Run<'_>,
    page: &Page,
    device: &mut dyn Device,
) -> Result<(), InterpError> {
    let annots = run.doc.resolve_key(&page.dict, b"Annots")?;
    let Some(annots) = annots.as_array() else {
        return Ok(());
    };
    let mut widgets = Vec::new();
    for object in annots {
        let Some(dict) = run.doc.resolve_dict(object)? else {
            continue;
        };
        if dict.get_name(b"Subtype") == Some(b"Widget") {
            widgets.push((object, dict));
        } else {
            run_dict(
                run,
                page,
                object.as_reference().unwrap_or(ObjRef::new(0, 0)),
                &dict,
                device,
            )?;
        }
    }
    for (object, dict) in widgets {
        run_dict(
            run,
            page,
            object.as_reference().unwrap_or(ObjRef::new(0, 0)),
            &dict,
            device,
        )?;
    }
    Ok(())
}

pub(crate) fn run_one(
    run: &Run<'_>,
    page: &Page,
    annot: ObjRef,
    device: &mut dyn Device,
) -> Result<bool, InterpError> {
    let Some(dict) = run.doc.resolve_dict(&Object::Reference(annot))? else {
        return Ok(false);
    };
    run_dict(run, page, annot, &dict, device)
}

fn inherited(run: &Run<'_>, dict: &Dict, key: &[u8]) -> Option<Object> {
    let mut dict = dict.clone();
    let mut visited = Vec::new();
    for _ in 0..64 {
        if let Some(o) = dict.get(key) {
            return run.doc.resolve(o).ok();
        }
        let parent = dict.get_ref(b"Parent")?;
        if visited.contains(&parent) {
            return None;
        }
        visited.push(parent);
        dict = run.doc.resolve_dict(&Object::Reference(parent)).ok()??;
    }
    None
}

fn run_dict(
    run: &Run<'_>,
    page: &Page,
    annot: ObjRef,
    dict: &Dict,
    device: &mut dyn Device,
) -> Result<bool, InterpError> {
    let subtype = dict.get_name(b"Subtype").unwrap_or(b"");
    if matches!(subtype, b"Link" | b"Popup") {
        return Ok(false);
    }
    if subtype == b"Widget"
        && (inherited(run, dict, b"FT").is_none() || inherited(run, dict, b"T").is_none())
    {
        return Ok(false);
    }
    let flags = dict.get_i64(b"F").unwrap_or(0);
    if flags & 3 != 0
        || (run.options.intent == Intent::Print && (flags & 4 == 0 || subtype == b"FileAttachment"))
        || (run.options.intent == Intent::View && flags & 32 != 0)
        || dict.get(b"OC").is_some_and(|o| run.hidden(o))
    {
        return Ok(false);
    }
    let ap = run.doc.resolve_key(dict, b"AP")?;
    let Some(ap) = ap.as_dict() else {
        return Ok(false);
    };
    let Some(normal) = ap.get(b"N") else {
        return Ok(false);
    };
    let normal_value = run.doc.resolve(normal)?;
    let appearance = if let Some(states) = normal_value.as_dict() {
        let state = dict.get_name(b"AS").unwrap_or(b"");
        let Some(o) = states.get(state) else {
            return Ok(false);
        };
        o
    } else {
        normal
    };
    let Some(stream) = run.doc.resolve_stream(appearance)? else {
        return Ok(false);
    };
    let appearance = appearance.as_reference().unwrap_or(ObjRef::new(0, 0));
    let Some(mut rect) = run
        .doc
        .resolve_key(dict, b"Rect")?
        .as_array()
        .and_then(Rect::from_array)
    else {
        return Ok(false);
    };
    let bbox = run
        .doc
        .resolve_key(&stream.dict, b"BBox")?
        .as_array()
        .and_then(Rect::from_array)
        .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
    let form_matrix = run
        .doc
        .resolve_key(&stream.dict, b"Matrix")?
        .as_array()
        .and_then(Matrix::from_array)
        .unwrap_or(Matrix::IDENTITY);
    if flags & 8 != 0 {
        rect.x1 = rect.x0 + bbox.width();
        rect.y0 = rect.y1 - bbox.height();
    }
    let rotate = if flags & 16 != 0 { page.rotation() } else { 0 };
    if rotate != 0 {
        let around = Matrix::translate(-rect.x0, -rect.y1)
            .concat(&Matrix::rotate(f64::from(rotate)))
            .concat(&Matrix::translate(rect.x0, rect.y1));
        rect = rect.transform(&around);
    }
    let bbox = bbox.transform(&form_matrix);
    let w = bbox.width();
    let h = bbox.height();
    let rotmat = match rotate {
        90 if w != 0.0 && h != 0.0 => {
            let mut m = Matrix::rotate(90.0).concat(&Matrix::scale(w / h, h / w));
            m.e = w;
            m
        }
        180 => {
            let mut m = Matrix::rotate(180.0);
            m.e = w;
            m.f = h;
            m
        }
        270 if w != 0.0 && h != 0.0 => {
            let mut m = Matrix::rotate(270.0).concat(&Matrix::scale(w / h, h / w));
            m.f = h;
            m
        }
        _ => Matrix::IDENTITY,
    };
    let sx = if w == 0.0 { 0.0 } else { rect.width() / w };
    let sy = if h == 0.0 { 0.0 } else { rect.height() / h };
    let placement = rotmat.concat(&Matrix::new(
        sx,
        0.0,
        0.0,
        sy,
        rect.x0 - bbox.x0 * sx,
        rect.y0 - bbox.y0 * sy,
    ));
    let page_matrix = page_transform(page).concat(&run.options.transform);
    device.begin_annotation(&AnnotationEvent {
        annot,
        subtype: &Name::new(subtype.to_vec()),
        rect: rect.transform(&page_matrix),
        flags,
        appearance,
        matrix: form_matrix.concat(&placement).concat(&page_matrix),
    });
    let result = run.appearance(page, annot, appearance, &stream, placement, device);
    device.end_annotation();
    result.map(|()| true)
}
