//! `security sign`: a signature appended in an incremental update and a detached CMS
//! signature over the two byte ranges around `/Contents`. By default this is pyhanko's
//! `sign_pdf` with a fresh self-signed certificate, as `cmd_sec_sign` runs it; with a
//! PKCS#12 identity it is a PAdES signature (`ETSI.CAdES.detached`). The signature fills
//! the existing empty signature field of the requested name, or else a new field, which is
//! invisible unless the request gives it a box to show. A certification signature also
//! records its DocMDP permission, as pyhanko's `SigMDPSetup` does. With `--ltv`, later
//! revisions keep what long-term validation needs in the document security store and,
//! with `--tsa`, time-stamp the whole document over it (PAdES B-LTA).

use goat_common::parse::selected_page;
use pdf_core::{Dict, Document, ObjRef, Object, Page, PdfDate, PdfString, Rect};
use pdf_interp::unrotated_transform;

use crate::appearance::{self, Face};
use crate::cades::CadesSigner;
use crate::ltv::{self, Material};
use crate::mdp::{self, Permission};
use crate::net::Http;
use crate::pkcs7::{Hash, SelfSigned, Signature};
use crate::tsp;
use crate::verify::{Field, list_fields, widget_box};

/// Room for the DER signature: pyhanko reserves 8192 bytes for a detached signature.
const CONTENTS_BYTES: usize = 8192;
/// Wide enough for any offset the placeholder must later hold.
const RANGE_PLACEHOLDER: i64 = 9_999_999_999;
/// Headroom over the measured size when a signature outgrew its first reservation.
const RETRY_SLACK: usize = 1024;
/// Room for a signature time-stamp token: the TSA's certificates and signature.
const TIME_STAMP_BYTES: usize = 16 * 1024;

/// Who signs, and in which format.
pub enum Signer {
    /// A fresh self-signed demonstration certificate, `adbe.pkcs7.detached`.
    Demo(SelfSigned),
    /// A PKCS#12 identity, `ETSI.CAdES.detached` (PAdES).
    Pades(CadesSigner),
}

impl Signer {
    fn sub_filter(&self) -> &'static str {
        match self {
            Signer::Demo(_) => "adbe.pkcs7.detached",
            Signer::Pades(_) => "ETSI.CAdES.detached",
        }
    }

    fn digest(&self) -> Hash {
        match self {
            Signer::Demo(_) => Hash::Sha256,
            Signer::Pades(signer) => signer.digest(),
        }
    }

    /// The `/Contents` bytes to reserve before the signature exists.
    fn reserve(&self) -> Result<usize, String> {
        match self {
            Signer::Demo(_) => Ok(CONTENTS_BYTES),
            Signer::Pades(signer) => signer.size_hint(),
        }
    }

    fn sign(&self, message_digest: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Signer::Demo(signer) => signer.sign_detached(message_digest),
            Signer::Pades(signer) => signer.sign(message_digest),
        }
    }
}

/// One `security sign` run.
pub struct Request<'a> {
    pub signer: &'a Signer,
    /// The field to sign: the existing empty signature field of this name, else a new one.
    pub field: &'a str,
    pub reason: &'a str,
    /// The signature dictionary's `/Name`; `None` leaves the name to the certificate.
    pub name: Option<&'a str>,
    /// The claimed signing time, written as `/M`.
    pub date: PdfDate,
    /// The 1-based page that holds a new field (default 1).
    pub page: Option<i64>,
    /// A new field's box in `search`'s frame; `None` makes the field invisible.
    pub rect: Option<Rect>,
    /// What a visible signature shows.
    pub face: Face<'a>,
    /// The request chose `face` rather than taking the default.
    pub face_chosen: bool,
    /// The time-stamp authority that stamps the signature; `None` leaves it unstamped.
    pub tsa: Option<TimeStamping<'a>>,
    /// Certify the document, allowing this much change after signing.
    pub certify: Option<Permission>,
    /// Where `--ltv` fetches the validation material; `None` leaves it out.
    pub ltv: Option<&'a Http>,
}

/// Where `--tsa` asks for the signature time-stamp.
pub struct TimeStamping<'a> {
    pub http: &'a Http,
    pub url: &'a str,
}

/// The signed document and what the signature turned out to be.
pub struct Signed {
    pub data: Vec<u8>,
    /// The time the signature time-stamp token attests.
    pub time_stamp: Option<PdfDate>,
    pub visible: bool,
    /// Signing made the field rather than filling an existing one.
    pub field_created: bool,
    /// The DocMDP permission the signature sets, from `--certify` or its field's lock.
    pub permission: Option<Permission>,
    /// What `--ltv` keeps in the document security store.
    pub ltv: Option<Material>,
    /// The time the document time-stamp attests, when `--ltv --tsa` added one.
    pub document_time_stamp: Option<PdfDate>,
}

/// The signed document.
pub fn sign_document(data: Vec<u8>, request: &Request<'_>) -> Result<Signed, String> {
    let mut doc = Document::load(data).map_err(|e| e.to_string())?;
    if doc.needs_password() {
        return Err("the document is encrypted".to_owned());
    }
    let field_name = request.field;
    let fields = list_fields(&doc)?;
    let signatures_exist = fields
        .iter()
        .any(|field| field.field_type.as_deref() == Some(b"Sig") && field.filled());
    if request.certify.is_some() && signatures_exist {
        return Err(
            "Certification signatures must be the first signature in a given document.".to_owned(),
        );
    }
    if mdp::certification(&doc)?.is_some_and(|c| c.permission == Some(Permission::NoChanges)) {
        return Err("Author signature forbids all changes".to_owned());
    }
    let existing = match fields.iter().find(|field| field.name == field_name) {
        Some(field) if field.filled() => {
            return Err(format!(
                "Signature field with name {field_name} appears to be filled already."
            ));
        }
        Some(field) if field.field_type.as_deref() != Some(b"Sig") => {
            return Err(format!(
                "Signature field with name {field_name} already exists but is not a fresh field."
            ));
        }
        Some(_) if request.rect.is_some() || request.page.is_some() => {
            return Err(format!(
                "the signature field {field_name} already exists; --rect and --page only place a new field"
            ));
        }
        None if request.rect.is_none() && request.face_chosen => {
            return Err(
                "--appearance-text and --appearance-image need a box: give --rect, or --field naming a visible signature field"
                    .to_owned(),
            );
        }
        found => found,
    };

    let catalog_ref = doc.catalog_ref().map_err(|e| e.to_string())?;
    let setup = mdp::Setup::new(&doc, request.certify, existing.map(|field| &field.dict))?;
    let date = request.date.format();
    let mut reserve =
        request.signer.reserve()? + request.tsa.as_ref().map_or(0, |_| TIME_STAMP_BYTES);
    let mut sig = signature_dict(request, &date, reserve);
    let references = setup.references(catalog_ref);
    if !references.is_empty() {
        sig.insert("Reference", references);
    }
    let sig_ref = doc.add(sig.clone());

    let mut catalog = doc.catalog().map_err(|e| e.to_string())?;
    let mut catalog_changed = false;
    let (form_ref, mut form) = acro_form(&doc, &catalog)?;
    let visible = match existing {
        Some(field) => fill_field(&mut doc, field, sig_ref, request)?,
        None => {
            let widget_ref = new_field(&mut doc, sig_ref, request)?;
            add_to_fields(&mut doc, &mut form, widget_ref)?;
            request.rect.is_some()
        }
    };
    form.insert("SigFlags", 3);
    match form_ref {
        Some(id) => doc.set(id, form),
        None => {
            catalog.insert("AcroForm", form);
            catalog_changed = true;
        }
    }
    if setup.certify {
        let perms_value = catalog.get(b"Perms").cloned();
        let mut perms = perms_value
            .as_ref()
            .map(|p| doc.resolve_dict(p))
            .transpose()
            .map_err(|e| e.to_string())?
            .flatten()
            .unwrap_or_default();
        perms.insert("DocMDP", sig_ref);
        match perms_value.as_ref().and_then(Object::as_reference) {
            Some(id) => doc.set(id, perms),
            None => {
                catalog.insert("Perms", perms);
                catalog_changed = true;
            }
        }
    }
    if catalog_changed {
        doc.set(catalog_ref, catalog);
    }

    // A signature larger than its reservation is signed once more with room for it.
    for _ in 0..2 {
        match sign_revision(&doc, sig_ref, request)? {
            (Attempt::Sealed { data, contents }, time_stamp) => {
                let (data, ltv, document_time_stamp) = match request.ltv {
                    Some(http) => {
                        let (data, material, stamped) = long_term(data, &contents, http, request)?;
                        (data, Some(material), stamped)
                    }
                    None => (data, None, None),
                };
                return Ok(Signed {
                    data,
                    time_stamp,
                    visible,
                    field_created: existing.is_none(),
                    permission: setup.permission,
                    ltv,
                    document_time_stamp,
                });
            }
            (Attempt::TooLarge(needed), _) => {
                reserve = needed + RETRY_SLACK;
                sig.insert("Contents", PdfString::hex(vec![0; reserve]));
                doc.set(sig_ref, sig.clone());
            }
        }
    }
    Err("the signature does not fit the reserved /Contents".to_owned())
}

/// The catalog's `/AcroForm`, resolved, and its object when it is indirect.
fn acro_form(doc: &Document, catalog: &Dict) -> Result<(Option<ObjRef>, Dict), String> {
    let value = catalog.get(b"AcroForm");
    let form = value
        .map(|f| doc.resolve_dict(f))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .unwrap_or_default();
    Ok((value.and_then(Object::as_reference), form))
}

/// Adds the field `widget_ref` to the form's top-level `/Fields`.
fn add_to_fields(doc: &mut Document, form: &mut Dict, widget_ref: ObjRef) -> Result<(), String> {
    let mut top = form
        .get(b"Fields")
        .map(|f| doc.resolve_array(f))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .unwrap_or_default();
    top.push(Object::Reference(widget_ref));
    if let Some(fields_ref) = form.get_ref(b"Fields") {
        doc.set(fields_ref, top);
    } else {
        form.insert("Fields", top);
    }
    Ok(())
}

/// `data` made verifiable long-term (`--ltv`): the certificate chains of the signature
/// whose CMS is `cms` and of its time-stamp, with their revocation answers, in the
/// document security store; then, with `--tsa`, a document time-stamp over that, and the
/// material for the time-stamp's own chain when the store lacked it.
fn long_term(
    data: Vec<u8>,
    cms: &[u8],
    http: &Http,
    request: &Request<'_>,
) -> Result<(Vec<u8>, Material, Option<PdfDate>), String> {
    let signature = Signature::parse(cms)?;
    let token = tsp::embedded_token(&signature).transpose()?;
    let mut leaves = vec![signature.certificate()];
    let mut carried = signature.certificates().to_vec();
    if let Some(token) = &token {
        leaves.push(token.signature.certificate());
        carried.extend_from_slice(token.signature.certificates());
    }
    let mut material = Material::default();
    ltv::gather(http, &leaves, &carried, &mut material)?;
    let data = ltv::add_dss(data, &material)?;
    let Some(tsa) = &request.tsa else {
        return Ok((data, material, None));
    };
    let (data, stamp) = time_stamp_document(data, tsa, request.signer.digest())?;
    let held = material.held();
    // The pool for finding issuers keeps the certificates seen so far: a token often
    // carries only the authority's own certificate.
    carried.extend_from_slice(stamp.signature.certificates());
    ltv::gather(
        http,
        &[stamp.signature.certificate()],
        &carried,
        &mut material,
    )?;
    let data = if material.held() > held {
        ltv::add_dss(data, &material)?
    } else {
        data
    };
    Ok((data, material, Some(stamp.time)))
}

/// `data` with a document time-stamp in a new revision: an invisible signature field on
/// the first page whose `/DocTimeStamp` value holds the token `tsa` issues for the `hash`
/// digest of the revision's byte ranges.
fn time_stamp_document(
    data: Vec<u8>,
    tsa: &TimeStamping<'_>,
    hash: Hash,
) -> Result<(Vec<u8>, tsp::Token), String> {
    let mut doc = Document::load(data).map_err(|e| e.to_string())?;
    let fields = list_fields(&doc)?;
    let mut number = 1;
    while fields
        .iter()
        .any(|field| field.name == format!("DocTimeStamp{number}"))
    {
        number += 1;
    }
    let mut reserve = TIME_STAMP_BYTES;
    let mut stamp = Dict::new();
    stamp.insert("Type", Object::name("DocTimeStamp"));
    stamp.insert("Filter", Object::name("Adobe.PPKLite"));
    stamp.insert("SubFilter", Object::name("ETSI.RFC3161"));
    stamp.insert("Contents", PdfString::hex(vec![0; reserve]));
    stamp.insert("ByteRange", range_placeholder());
    let stamp_ref = doc.add(stamp.clone());
    let page = doc.page(0).map_err(|e| e.to_string())?;
    let mut widget = field_widget(&format!("DocTimeStamp{number}"));
    hide(&mut widget);
    let widget_ref = place(&mut doc, &page, widget, stamp_ref)?;
    let catalog_ref = doc.catalog_ref().map_err(|e| e.to_string())?;
    let mut catalog = doc.catalog().map_err(|e| e.to_string())?;
    let (form_ref, mut form) = acro_form(&doc, &catalog)?;
    add_to_fields(&mut doc, &mut form, widget_ref)?;
    match form_ref {
        Some(id) => doc.set(id, form),
        None => {
            catalog.insert("AcroForm", form);
            doc.set(catalog_ref, catalog);
        }
    }
    for _ in 0..2 {
        let attempt = seal_revision(&doc, stamp_ref, hash, |digest| {
            tsp::document_stamp(tsa.http, tsa.url, hash, digest)
        })?;
        match attempt {
            Attempt::Sealed { data, contents } => return Ok((data, tsp::Token::parse(&contents)?)),
            Attempt::TooLarge(needed) => {
                reserve = needed + RETRY_SLACK;
                stamp.insert("Contents", PdfString::hex(vec![0; reserve]));
                doc.set(stamp_ref, stamp.clone());
            }
        }
    }
    Err("the document time-stamp does not fit the reserved /Contents".to_owned())
}

/// A new signature field holding `sig_ref` on the requested page, visible when the request
/// gives a box, and added to the page's annotations.
fn new_field(doc: &mut Document, sig_ref: ObjRef, request: &Request<'_>) -> Result<ObjRef, String> {
    let page_count = doc.page_count().map_err(|e| e.to_string())?;
    let index = selected_page(request.page.unwrap_or(1), page_count).map_err(|e| e.to_string())?;
    let page = doc.page(index).map_err(|e| e.to_string())?;
    let mut widget = field_widget(request.field);
    match request.rect {
        Some(rect) => {
            let to_user = unrotated_transform(&page)
                .invert()
                .ok_or("page has a singular transform")?;
            let rect = rect.transform(&to_user);
            let face = appearance::form(doc, rect.width(), rect.height(), &request.face)?;
            let mut states = Dict::new();
            states.insert("N", face);
            widget.insert("F", 4);
            widget.insert(
                "Rect",
                vec![
                    Object::Real(rect.x0),
                    Object::Real(rect.y0),
                    Object::Real(rect.x1),
                    Object::Real(rect.y1),
                ],
            );
            widget.insert("AP", states);
        }
        None => hide(&mut widget),
    }
    place(doc, &page, widget, sig_ref)
}

/// The widget of a new signature field named `name`.
fn field_widget(name: &str) -> Dict {
    let mut widget = Dict::new();
    widget.insert("FT", Object::name("Sig"));
    widget.insert("T", Object::text(name));
    widget.insert("Type", Object::name("Annot"));
    widget.insert("Subtype", Object::name("Widget"));
    widget
}

/// Makes `widget` invisible: hidden, printable, and with an empty box.
fn hide(widget: &mut Dict) {
    widget.insert("F", 132);
    widget.insert("Rect", vec![Object::Integer(0); 4]);
}

/// Adds `widget`, holding the signature value `sig_ref`, to `page` and its annotations.
fn place(
    doc: &mut Document,
    page: &Page,
    mut widget: Dict,
    sig_ref: ObjRef,
) -> Result<ObjRef, String> {
    widget.insert("P", page.id);
    widget.insert("V", sig_ref);
    let widget_ref = doc.add(widget);

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
    Ok(widget_ref)
}

/// Puts `sig_ref` in the existing empty signature field `field` and, when its widget has a
/// box, the requested face in the box (pyhanko's `get_sig_field_annot`); returns whether
/// the signature is visible.
fn fill_field(
    doc: &mut Document,
    field: &Field,
    sig_ref: ObjRef,
    request: &Request<'_>,
) -> Result<bool, String> {
    let mut field_dict = field.dict.clone();
    field_dict.insert("V", sig_ref);
    let mut kid = match field.dict.get(b"Kids") {
        None => None,
        Some(kids) => {
            let kids = doc
                .resolve_array(kids)
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            let only = match kids.as_slice() {
                [only] => only.as_reference(),
                _ => None,
            };
            let widget = only
                .map(|id| doc.resolve_dict(&Object::Reference(id)))
                .transpose()
                .map_err(|e| e.to_string())?
                .flatten()
                .filter(|widget| !widget.contains_key(b"T"));
            match only.zip(widget) {
                Some(kid) => Some(kid),
                None => {
                    return Err("Failed to access signature field's annotation. Signature field must have exactly one child annotation, or it must be combined with its annotation.".to_owned());
                }
            }
        }
    };
    let area = widget_box(kid.as_ref().map_or(&field_dict, |(_, widget)| widget));
    match area {
        Some(area) => {
            let face = appearance::form(doc, area.width(), area.height(), &request.face)?;
            let mut states = Dict::new();
            states.insert("N", face);
            match &mut kid {
                Some((_, widget)) => widget.insert("AP", states),
                None => field_dict.insert("AP", states),
            };
        }
        None if request.face_chosen => {
            return Err(format!(
                "the signature field {} has no box to show --appearance-text or --appearance-image",
                field.name
            ));
        }
        None => {}
    }
    if let Some((id, widget)) = kid {
        doc.set(id, widget);
    }
    doc.set(field.id, field_dict);
    Ok(area.is_some())
}

/// The outcome of sealing one written update.
enum Attempt {
    /// The sealed file and the bytes its `/Contents` holds.
    Sealed { data: Vec<u8>, contents: Vec<u8> },
    /// The seal needs this many bytes, more than `/Contents` reserves.
    TooLarge(usize),
}

/// The signature dictionary with a zeroed `/Contents` of `reserve` bytes and a
/// `/ByteRange` placeholder.
fn signature_dict(request: &Request<'_>, date: &str, reserve: usize) -> Dict {
    let mut sig = Dict::new();
    sig.insert("Type", Object::name("Sig"));
    sig.insert("Filter", Object::name("Adobe.PPKLite"));
    sig.insert("SubFilter", Object::name(request.signer.sub_filter()));
    sig.insert("Contents", PdfString::hex(vec![0; reserve]));
    sig.insert("ByteRange", range_placeholder());
    sig.insert("M", Object::string(date.as_bytes().to_vec()));
    if let Some(name) = request.name {
        sig.insert("Name", Object::text(name));
    }
    sig.insert("Reason", Object::text(request.reason));
    sig
}

/// A `/ByteRange` wide enough for any offset it must later hold.
fn range_placeholder() -> Vec<Object> {
    vec![
        Object::Integer(0),
        Object::Integer(RANGE_PLACEHOLDER),
        Object::Integer(RANGE_PLACEHOLDER),
        Object::Integer(RANGE_PLACEHOLDER),
    ]
}

/// Writes the update, signs it, and time-stamps the signature; with the time the token
/// attests.
fn sign_revision(
    doc: &Document,
    sig_ref: ObjRef,
    request: &Request<'_>,
) -> Result<(Attempt, Option<PdfDate>), String> {
    let signer = request.signer;
    let mut time_stamp = None;
    let attempt = seal_revision(doc, sig_ref, signer.digest(), |digest| match &request.tsa {
        Some(tsa) => {
            let (cms, time) =
                tsp::stamp(tsa.http, tsa.url, signer.digest(), &signer.sign(digest)?)?;
            time_stamp = Some(time);
            Ok(cms)
        }
        None => signer.sign(digest),
    })?;
    Ok((attempt, time_stamp))
}

/// Writes the update, fixes the `/ByteRange` of `sig_ref`, and fills its `/Contents` with
/// what `seal` makes of the `hash` digest of the byte ranges.
fn seal_revision(
    doc: &Document,
    sig_ref: ObjRef,
    hash: Hash,
    seal: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Attempt, String> {
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
    data.get_mut(byte_range)
        .ok_or("the /ByteRange span is out of bounds")?
        .copy_from_slice(&range_bytes);

    let digest = hash.digest_ranges(&data, &[(0, contents.start), (contents.end, tail)]);
    let cms = seal(&digest)?;
    let hex_slot = data
        .get_mut(contents.start + 1..contents.end - 1)
        .ok_or("the /Contents span is out of bounds")?;
    let (slots, _) = hex_slot.as_chunks_mut::<2>();
    if cms.len() > slots.len() {
        return Ok(Attempt::TooLarge(cms.len()));
    }
    for (index, slot) in slots.iter_mut().enumerate() {
        let byte = cms.get(index).copied().unwrap_or(0);
        *slot = [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0x0f)]];
    }
    Ok(Attempt::Sealed {
        data,
        contents: cms,
    })
}

const HEX: &[u8; 16] = b"0123456789abcdef";
