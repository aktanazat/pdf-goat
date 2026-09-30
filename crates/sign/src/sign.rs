//! `security sign`: pyhanko's `sign_pdf` with a fresh self-signed certificate, as
//! `cmd_sec_sign` runs it: an invisible signature field appended in an incremental update,
//! a detached PKCS#7 signature over the two byte ranges around `/Contents`.

use pdf_core::{Dict, Document, Object, PdfDate, PdfString};

use crate::pkcs7::SelfSigned;

/// Room for the DER signature: pyhanko reserves 8192 bytes for a detached signature.
const CONTENTS_BYTES: usize = 8192;
/// Wide enough for any offset the placeholder must later hold.
const RANGE_PLACEHOLDER: i64 = 9_999_999_999;

/// The signed document as bytes.
pub fn sign_document(
    data: Vec<u8>,
    field_name: &str,
    reason: &str,
    common_name: &str,
) -> Result<Vec<u8>, String> {
    let mut doc = Document::load(data).map_err(|e| e.to_string())?;
    if doc.needs_password() {
        return Err("the document is encrypted".to_owned());
    }
    let signer = SelfSigned::generate(common_name)?;

    let catalog_ref = doc.catalog_ref().map_err(|e| e.to_string())?;
    let mut catalog = doc.catalog().map_err(|e| e.to_string())?;
    let form_value = catalog.get(b"AcroForm").cloned();
    let form_ref = form_value.as_ref().and_then(Object::as_reference);
    let mut form = form_value
        .as_ref()
        .map(|f| doc.resolve_dict(f))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .unwrap_or_default();
    let mut fields = form
        .get(b"Fields")
        .map(|f| doc.resolve_array(f))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .unwrap_or_default();
    for item in &fields {
        let Some(existing) = doc.resolve_dict(item).map_err(|e| e.to_string())? else {
            continue;
        };
        let name = existing
            .get(b"T")
            .and_then(Object::as_string)
            .map(PdfString::to_text);
        if name.as_deref() == Some(field_name) {
            if existing.get(b"V").is_some_and(|v| !v.is_null()) {
                return Err(format!(
                    "Signature field with name {field_name} appears to be filled already."
                ));
            }
            return Err(format!(
                "Signature field with name {field_name} already exists but is not a fresh field."
            ));
        }
    }

    let page = doc.page(0).map_err(|e| e.to_string())?;
    let mut sig = Dict::new();
    sig.insert("Type", Object::name("Sig"));
    sig.insert("Filter", Object::name("Adobe.PPKLite"));
    sig.insert("SubFilter", Object::name("adbe.pkcs7.detached"));
    sig.insert("Contents", PdfString::hex(vec![0; CONTENTS_BYTES]));
    sig.insert(
        "ByteRange",
        vec![
            Object::Integer(0),
            Object::Integer(RANGE_PLACEHOLDER),
            Object::Integer(RANGE_PLACEHOLDER),
            Object::Integer(RANGE_PLACEHOLDER),
        ],
    );
    sig.insert("M", Object::string(PdfDate::now().format().into_bytes()));
    sig.insert("Name", Object::text(common_name));
    sig.insert("Reason", Object::text(reason));
    let sig_ref = doc.add(sig);

    let mut widget = Dict::new();
    widget.insert("FT", Object::name("Sig"));
    widget.insert("T", Object::text(field_name));
    widget.insert("Type", Object::name("Annot"));
    widget.insert("Subtype", Object::name("Widget"));
    widget.insert("F", 132);
    widget.insert(
        "Rect",
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(0),
        ],
    );
    widget.insert("P", page.id);
    widget.insert("V", sig_ref);
    let widget_ref = doc.add(widget);

    fields.push(Object::Reference(widget_ref));
    if let Some(fields_ref) = form.get_ref(b"Fields") {
        doc.set(fields_ref, fields);
    } else {
        form.insert("Fields", fields);
    }
    form.insert("SigFlags", 3);
    match form_ref {
        Some(id) => doc.set(id, form),
        None => {
            catalog.insert("AcroForm", form);
            doc.set(catalog_ref, catalog);
        }
    }
    let mut page_dict = page.dict.clone();
    let mut annots = page_dict
        .get(b"Annots")
        .map(|a| doc.resolve_array(a))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .unwrap_or_default();
    annots.push(Object::Reference(widget_ref));
    if let Some(annots_ref) = page_dict.get_ref(b"Annots") {
        doc.set(annots_ref, annots);
    } else {
        page_dict.insert("Annots", annots);
    }
    doc.set(page.id, page_dict);

    let saved = doc.save_incremental(false).map_err(|e| e.to_string())?;
    let span = saved
        .signatures
        .iter()
        .find(|s| s.id == sig_ref)
        .ok_or("the writer did not report the signature span")?;
    let byte_range = span
        .byte_range
        .clone()
        .ok_or("the writer did not report the /ByteRange span")?;
    let mut data = saved.data;
    let contents = span.contents.clone();
    let tail = data
        .len()
        .checked_sub(contents.end)
        .ok_or("signature span past the end of the file")?;
    let range_text = format!("[0 {} {} {}]", contents.start, contents.end, tail);
    if range_text.len() > byte_range.len() {
        return Err("the /ByteRange placeholder is too narrow".to_owned());
    }
    let mut range_bytes = range_text.into_bytes();
    range_bytes.resize(byte_range.len(), b' ');
    data.get_mut(byte_range.clone())
        .ok_or("the /ByteRange span is out of bounds")?
        .copy_from_slice(&range_bytes);

    let digest = crate::pkcs7::Hash::Sha256
        .digest_ranges(&data, &[(0, contents.start), (contents.end, tail)]);
    let cms = signer.sign_detached(&digest)?;
    if cms.len() > CONTENTS_BYTES {
        return Err("the signature does not fit the reserved /Contents".to_owned());
    }
    let hex_slot = data
        .get_mut(contents.start + 1..contents.end - 1)
        .ok_or("the /Contents span is out of bounds")?;
    for (index, slot) in hex_slot.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let byte = cms.get(index).copied().unwrap_or(0);
        *slot = [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0x0f)]];
    }
    Ok(data)
}

const HEX: &[u8; 16] = b"0123456789abcdef";
