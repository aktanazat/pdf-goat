//! The top-left coordinate system exposed by PyMuPDF.

use goat_common::GoatError;
use goat_common::parse::parse_rect;
use pdf_core::{Document, Matrix, ObjRef, Object, Page, Rect};

use crate::open::pdf_error;

pub(crate) fn rect_arg(text: &str) -> Result<Rect, GoatError> {
    let values = parse_rect(text)?;
    let [x0, y0, x1, y1] = values.as_slice() else {
        return Err(GoatError::exception(
            "TypeError",
            "Value after * must be an iterable, not float",
        ));
    };
    Ok(Rect::new(*x0, *y0, *x1, *y1))
}

pub(crate) fn values(rect: Rect) -> [f64; 4] {
    [rect.x0, rect.y0, rect.x1, rect.y1]
}

/// PDF user space to the rotated page's top-left origin.
pub(crate) fn ctm(page: &Page) -> Matrix {
    let unit = page.user_unit();
    let base = Matrix::scale(unit, -unit).concat(&Matrix::rotate(f64::from(page.rotation())));
    let bounds = page.crop_box().transform(&base);
    base.concat(&Matrix::translate(-bounds.x0, -bounds.y0))
}

pub(crate) fn page_rect(page: &Page) -> Rect {
    page.crop_box().transform(&ctm(page))
}

/// PyMuPDF's transformation_matrix deliberately excludes a nonzero rotation.
pub(crate) fn text_matrix(page: &Page) -> Matrix {
    if page.rotation() == 0 {
        ctm(page)
    } else {
        Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, page.crop_box().height())
    }
}

pub(crate) fn new_page(doc: &mut Document, width: f64, height: f64) -> Result<ObjRef, GoatError> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(GoatError::value_error(
            "page width and height must be positive",
        ));
    }
    let count = doc.page_count().map_err(pdf_error)?;
    let id = doc
        .insert_blank_page(count, Rect::new(0.0, 0.0, width, height))
        .map_err(pdf_error)?;
    let mut dict = doc
        .get(id)
        .map_err(pdf_error)?
        .as_dict()
        .cloned()
        .ok_or_else(|| GoatError::message("invalid page"))?;
    dict.remove(b"Contents");
    dict.insert("Rotate", Object::Integer(0));
    let resources = doc.add(pdf_core::Dict::new());
    dict.insert("Resources", Object::Reference(resources));
    doc.set(id, dict);
    Ok(id)
}
