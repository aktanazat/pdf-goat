//! `security verify`: pyhanko's `validate_pdf_signature` for every filled signature field,
//! reported as `cmd_sec_verify` does. Integrity comes from [`crate::pkcs7`]; coverage and
//! the incremental-update review follow `pdf_embedded.evaluate_signature_coverage` and
//! `diff_analysis.StandardDiffPolicy` with pyhanko's default rules.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use pdf_core::{Dict, Document, ObjRef, Object};
use serde_json::{Map, Value};

use crate::pkcs7::Signature;
use crate::xref::{Chain, startxref_at_eof};

const MAX_FIELD_DEPTH: usize = 32;
const MAX_WALK: usize = 200_000;
/// `FORMFIELD_ALWAYS_MODIFIABLE`.
const ALWAYS_MODIFIABLE: &[&[u8]] = &[b"Ff"];
/// `VALUE_UPDATE_KEYS`.
const VALUE_UPDATE_KEYS: &[&[u8]] = &[b"Ff", b"AP", b"AS", b"V", b"I"];
const CATALOG_MODIFIABLE: &[&[u8]] = &[
    b"AcroForm",
    b"DSS",
    b"Extensions",
    b"Metadata",
    b"MarkInfo",
    b"Version",
];
const ACROFORM_MODIFIABLE: &[&[u8]] = &[b"Fields", b"DR", b"DA", b"Q", b"NeedAppearances"];

/// `SignatureCoverageLevel`, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Coverage {
    Unclear,
    ContiguousBlockFromStart,
    EntireRevision,
    EntireFile,
}

impl Coverage {
    fn as_str(self) -> &'static str {
        match self {
            Coverage::Unclear => "SignatureCoverageLevel.UNCLEAR",
            Coverage::ContiguousBlockFromStart => {
                "SignatureCoverageLevel.CONTIGUOUS_BLOCK_FROM_START"
            }
            Coverage::EntireRevision => "SignatureCoverageLevel.ENTIRE_REVISION",
            Coverage::EntireFile => "SignatureCoverageLevel.ENTIRE_FILE",
        }
    }
}

/// `ModificationLevel`, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    None,
    LtaUpdates,
    FormFilling,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::None => "NONE",
            Level::LtaUpdates => "LTA_UPDATES",
            Level::FormFilling => "FORM_FILLING",
        }
    }
}

/// A form field with its fully qualified name and inherited type.
struct Field {
    name: String,
    id: ObjRef,
    dict: Dict,
    field_type: Option<Vec<u8>>,
}

fn list_fields(doc: &Document) -> Result<Vec<Field>, String> {
    let catalog = doc.catalog().map_err(|e| e.to_string())?;
    let Some(form) = doc
        .resolve_dict(catalog.get(b"AcroForm").unwrap_or(&Object::Null))
        .map_err(|e| e.to_string())?
    else {
        return Ok(Vec::new());
    };
    let Some(fields) = doc
        .resolve_array(form.get(b"Fields").unwrap_or(&Object::Null))
        .map_err(|e| e.to_string())?
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    walk_fields(doc, &fields, "", None, &mut HashSet::new(), 0, &mut out)?;
    Ok(out)
}

fn walk_fields(
    doc: &Document,
    list: &[Object],
    parent: &str,
    inherited: Option<&[u8]>,
    seen: &mut HashSet<u32>,
    depth: usize,
    out: &mut Vec<Field>,
) -> Result<(), String> {
    if depth > MAX_FIELD_DEPTH {
        return Err("Form tree too deep".to_owned());
    }
    for item in list {
        let Some(id) = item.as_reference() else {
            continue;
        };
        if !seen.insert(id.num) {
            return Err("Circular reference in form tree".to_owned());
        }
        let Some(dict) = doc.resolve_dict(item).map_err(|e| e.to_string())? else {
            continue;
        };
        let Some(partial) = dict
            .get(b"T")
            .map(|t| doc.resolve(t))
            .transpose()
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        let partial = partial.as_string().map(|s| s.to_text()).unwrap_or_default();
        let name = if parent.is_empty() {
            partial
        } else {
            format!("{parent}.{partial}")
        };
        let field_type = dict
            .get_name(b"FT")
            .map(<[u8]>::to_vec)
            .or_else(|| inherited.map(<[u8]>::to_vec));
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|k| doc.resolve_array(k))
            .transpose()
            .map_err(|e| e.to_string())?
            .flatten()
        {
            walk_fields(
                doc,
                &kids,
                &name,
                field_type.as_deref(),
                seen,
                depth + 1,
                out,
            )?;
        }
        out.push(Field {
            name,
            id,
            dict,
            field_type,
        });
    }
    Ok(())
}

/// One signature to report: the field and its signature dictionary.
struct Embedded {
    field: String,
    sig: Dict,
    signed_revision: usize,
}

/// `cmd_sec_verify`: the `signatures` list, in signing order.
pub fn verify_file(data: &[u8]) -> Result<Vec<Map<String, Value>>, String> {
    let doc = Document::load(data.to_vec()).map_err(|e| e.to_string())?;
    let chain = Chain::read(data)?;
    let mut embedded = Vec::new();
    for field in list_fields(&doc)? {
        if field.field_type.as_deref() != Some(b"Sig") {
            continue;
        }
        let Some(sig_ref) = field.dict.get_ref(b"V") else {
            continue;
        };
        let Some(sig) = doc
            .resolve_dict(&Object::Reference(sig_ref))
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        if sig.is_empty() {
            continue;
        }
        let signed_revision = chain
            .last_definer(sig_ref.num)
            .ok_or_else(|| format!("Could not determine history of {sig_ref} in xref sections"))?;
        embedded.push(Embedded {
            field: field.name,
            sig,
            signed_revision,
        });
    }
    embedded.sort_by_key(|e| e.signed_revision);
    let mut out = Vec::with_capacity(embedded.len());
    for entry in &embedded {
        let contents = entry
            .sig
            .get_string(b"Contents")
            .ok_or("Could not read /Contents entry in signature")?;
        let signature = Signature::parse(contents.as_bytes())?;
        let mut row = Map::new();
        row.insert("field".into(), entry.field.clone().into());
        row.insert("signer".into(), signature.signer.clone().into());
        match validate(data, &doc, &chain, entry, &signature) {
            Ok((verdict, coverage, modified)) => {
                row.insert("intact".into(), verdict.intact.into());
                row.insert("valid".into(), verdict.valid.into());
                row.insert("trusted".into(), verdict.trusted.into());
                row.insert("coverage".into(), coverage.as_str().into());
                row.insert("modified".into(), modified.into());
            }
            Err(error) => {
                row.insert("validation_error".into(), error.into());
            }
        }
        out.push(row);
    }
    Ok(out)
}

fn validate(
    data: &[u8],
    doc: &Document,
    chain: &Chain,
    entry: &Embedded,
    signature: &Signature,
) -> Result<(crate::pkcs7::Verdict, Coverage, &'static str), String> {
    if entry.sig.get_name(b"Type").is_some_and(|t| t != b"Sig") {
        return Err("Signature object type must be /Sig".to_owned());
    }
    match entry.sig.get_name(b"SubFilter") {
        Some(b"adbe.pkcs7.detached" | b"ETSI.CAdES.detached") => {}
        Some(other) => {
            return Err(format!(
                "/{} is not a recognized SubFilter type in signatures.",
                String::from_utf8_lossy(other)
            ));
        }
        None => return Err("None is not a recognized SubFilter type in signatures.".to_owned()),
    }
    if chain.sections.iter().any(|s| s.hybrid) {
        return Err(
            "Settings do not permit validation of signatures in hybrid-reference files.".to_owned(),
        );
    }
    let byte_range: Vec<i64> = doc
        .resolve_array(
            entry
                .sig
                .get(b"ByteRange")
                .ok_or("Could not read /ByteRange entry in signature")?,
        )
        .map_err(|e| e.to_string())?
        .ok_or("Could not read /ByteRange entry in signature")?
        .iter()
        .map(Object::as_i64)
        .collect::<Option<Vec<i64>>>()
        .ok_or("/ByteRange contains non-integers")?;
    let contents_len = entry
        .sig
        .get_string(b"Contents")
        .map_or(0, |s| s.as_bytes().len());
    let ranges: Vec<(usize, usize)> = byte_range
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            Ok((
                usize::try_from(pair[0]).map_err(|_| "negative /ByteRange offset")?,
                usize::try_from(pair[1]).map_err(|_| "negative /ByteRange length")?,
            ))
        })
        .collect::<Result<Vec<_>, &str>>()?;
    let digest = signature.digest.digest_ranges(data, &ranges);
    let verdict = signature.verify(&digest)?;
    let coverage = coverage(
        data,
        chain,
        entry.signed_revision,
        &byte_range,
        contents_len,
    );
    let modified = match coverage {
        Coverage::EntireFile => "NONE",
        Coverage::Unclear | Coverage::ContiguousBlockFromStart => "OTHER",
        Coverage::EntireRevision => match review(data, chain, entry.signed_revision) {
            Ok(level) => level.name(),
            Err(_) => "OTHER",
        },
    };
    Ok((verdict, coverage, modified))
}

fn coverage(
    data: &[u8],
    chain: &Chain,
    signed_revision: usize,
    byte_range: &[i64],
    contents_len: usize,
) -> Coverage {
    if byte_range.len() != 4 || byte_range[0] != 0 {
        return Coverage::Unclear;
    }
    let (Ok(len1), Ok(start2), Ok(len2)) = (
        usize::try_from(byte_range[1]),
        usize::try_from(byte_range[2]),
        usize::try_from(byte_range[3]),
    ) else {
        return Coverage::Unclear;
    };
    let embedded = contents_len * 2 + 2;
    let signed_zone_len = len1 + len2 + embedded;
    if data.len() == signed_zone_len {
        return Coverage::EntireFile;
    }
    if start2 != len1 + embedded {
        return Coverage::Unclear;
    }
    // pyhanko reads the markers backwards from `signed_zone_len`; past the end of the
    // file that read fails.
    let (Some(section), Some(prefix)) = (
        chain.sections.get(signed_revision),
        data.get(..signed_zone_len),
    ) else {
        return Coverage::ContiguousBlockFromStart;
    };
    match startxref_at_eof(prefix) {
        Ok(startxref) if startxref == section.declared_offset => {}
        _ => return Coverage::ContiguousBlockFromStart,
    }
    if chain.sections[..=signed_revision]
        .iter()
        .any(|s| s.end > signed_zone_len)
    {
        return Coverage::ContiguousBlockFromStart;
    }
    Coverage::EntireRevision
}

/// `StandardDiffPolicy.review_file`: the highest modification level any revision after the
/// signed one needs; an error is a `SuspiciousModification`.
fn review(data: &[u8], chain: &Chain, signed_revision: usize) -> Result<Level, String> {
    let mut level = Level::None;
    let mut old = load_revision(data, chain, signed_revision)?;
    for index in signed_revision + 1..chain.sections.len() {
        let new = load_revision(data, chain, index)?;
        let fresh: BTreeSet<u32> = chain.sections[signed_revision + 1..=index]
            .iter()
            .flat_map(|s| s.defined.iter().map(|id| id.num))
            .collect();
        let revision = RevisionDiff {
            old: &old,
            new: &new,
            index,
            chain,
            fresh,
        };
        level = level.max(revision.apply()?);
        old = new;
    }
    Ok(level)
}

fn load_revision(data: &[u8], chain: &Chain, index: usize) -> Result<Document, String> {
    let end = chain
        .revision_end(data, index)
        .ok_or("revision has no %%EOF")?;
    Document::load(data[..end].to_vec()).map_err(|e| e.to_string())
}

struct RevisionDiff<'a> {
    old: &'a Document,
    new: &'a Document,
    index: usize,
    chain: &'a Chain,
    /// Objects introduced after the signed revision, up to and including this one.
    fresh: BTreeSet<u32>,
}

impl RevisionDiff<'_> {
    fn apply(&self) -> Result<Level, String> {
        let section = &self.chain.sections[self.index];
        if !section.freed.is_empty() {
            return Err("Objects were freed".to_owned());
        }
        let new_xrefs: BTreeSet<u32> = section.defined.iter().map(|id| id.num).collect();
        let mut explained: BTreeMap<u32, Level> = BTreeMap::new();
        let mut explain = |num: u32, level: Level| {
            let slot = explained.entry(num).or_insert(level);
            *slot = (*slot).min(level);
        };
        if let Some(id) = section.stream_ref {
            explain(id.num, Level::LtaUpdates);
        }
        // Identical overrides carry no change.
        for &num in &new_xrefs {
            let id = ObjRef::new(num, 0);
            if self.old.has_object(id) && self.old.get(id).ok() == self.new.get(id).ok() {
                explain(num, Level::None);
            }
        }
        // Object streams and the trailer's /Info.
        for &num in &new_xrefs {
            if let Ok(Object::Stream(stream)) = self.new.get(ObjRef::new(num, 0))
                && stream.dict.has_type(b"ObjStm")
            {
                explain(num, Level::LtaUpdates);
            }
        }
        if let Some(info) = self.new.trailer().get_ref(b"Info") {
            let old_info = self.old.trailer().get_ref(b"Info");
            if self.ref_allowed(old_info, info) {
                explain(info.num, Level::LtaUpdates);
            }
        }
        // The catalog.
        let old_root = self.old.catalog_ref().map_err(|e| e.to_string())?;
        let new_root = self.new.catalog_ref().map_err(|e| e.to_string())?;
        if old_root != new_root {
            return Err("Root reference changed".to_owned());
        }
        let old_catalog = self.old.catalog().map_err(|e| e.to_string())?;
        let new_catalog = self.new.catalog().map_err(|e| e.to_string())?;
        compare_dicts(&old_catalog, &new_catalog, CATALOG_MODIFIABLE)?;
        explain(new_root.num, Level::LtaUpdates);
        for key in [b"Metadata".as_slice(), b"Extensions", b"MarkInfo", b"DSS"] {
            if let Some(new_ref) = new_catalog.get_ref(key)
                && self.ref_allowed(old_catalog.get_ref(key), new_ref)
            {
                explain(new_ref.num, Level::LtaUpdates);
                for dep in self.dependencies(&Object::Reference(new_ref)) {
                    explain(dep, Level::LtaUpdates);
                }
            }
        }
        // The form.
        let old_form = old_catalog.get(b"AcroForm").cloned();
        let new_form = new_catalog.get(b"AcroForm").cloned();
        let old_form_dict = old_form
            .as_ref()
            .map(|f| self.old.resolve_dict(f))
            .transpose()
            .map_err(|e| e.to_string())?
            .flatten();
        let new_form_dict = new_form
            .as_ref()
            .map(|f| self.new.resolve_dict(f))
            .transpose()
            .map_err(|e| e.to_string())?
            .flatten();
        if let (Some(new_form), Some(new_form_dict)) = (&new_form, &new_form_dict) {
            if let Some(new_ref) = new_form.as_reference() {
                let old_ref = old_form.as_ref().and_then(Object::as_reference);
                if !self.ref_allowed(old_ref, new_ref) {
                    return Err("/AcroForm reference clobbers an existing object".to_owned());
                }
                explain(new_ref.num, Level::LtaUpdates);
            }
            let empty = Dict::new();
            let old_form_dict = old_form_dict.as_ref().unwrap_or(&empty);
            compare_dicts(old_form_dict, new_form_dict, ACROFORM_MODIFIABLE)?;
            if let Some(fields_ref) = new_form_dict.get_ref(b"Fields")
                && self.ref_allowed(old_form_dict.get_ref(b"Fields"), fields_ref)
            {
                explain(fields_ref.num, Level::LtaUpdates);
            }
            if let Some(dr) = new_form_dict.get(b"DR")
                && old_form_dict.get(b"DR") != Some(dr)
            {
                if let Some(dr_ref) = dr.as_reference() {
                    explain(dr_ref.num, Level::FormFilling);
                }
                for dep in self.dependencies(dr) {
                    explain(dep, Level::FormFilling);
                }
            }
        } else if new_form.is_some() {
            return Err("/AcroForm is not a dictionary".to_owned());
        }
        // Fields.
        let old_fields = list_fields(self.old)?;
        let new_fields = list_fields(self.new)?;
        let old_by_name: BTreeMap<&str, &Field> =
            old_fields.iter().map(|f| (f.name.as_str(), f)).collect();
        let mut new_by_name: BTreeMap<&str, &Field> = BTreeMap::new();
        for field in &new_fields {
            if new_by_name.insert(field.name.as_str(), field).is_some() {
                return Err(format!("Duplicate field name {}", field.name));
            }
        }
        for name in old_by_name.keys() {
            if !new_by_name.contains_key(name) {
                return Err(format!("Field {name} was deleted"));
            }
        }
        let mut new_widgets: BTreeSet<u32> = BTreeSet::new();
        for (name, field) in &new_by_name {
            let old_field = old_by_name.get(name);
            if old_field.is_some_and(|old| old.field_type != field.field_type) {
                return Err(format!("Type of field {name} changed"));
            }
            let is_sig = field.field_type.as_deref() == Some(b"Sig");
            let now_signed = field.dict.get(b"V").is_some_and(|v| !v.is_null());
            let changed = self.fresh.contains(&field.id.num) && new_xrefs.contains(&field.id.num);
            match old_field {
                None => {
                    if !is_sig {
                        return Err(format!("Field {name} is not a signature field"));
                    }
                    if !self.fresh.contains(&field.id.num) {
                        return Err(format!("New field {name} reuses an existing object"));
                    }
                    let visible = self.field_visible(&field.dict);
                    let level = if visible {
                        Level::FormFilling
                    } else {
                        Level::LtaUpdates
                    };
                    explain(field.id.num, level);
                    new_widgets.insert(field.id.num);
                    for key in [b"AP".as_slice(), b"Lock", b"SV"] {
                        if let Some(value) = field.dict.get(key) {
                            for dep in self.dependencies(value) {
                                explain(dep, Level::FormFilling);
                            }
                        }
                    }
                    if let Some(kids) = field.dict.get(b"Kids") {
                        if let Some(kids_ref) = kids.as_reference() {
                            explain(kids_ref.num, level);
                        }
                        for kid in self
                            .new
                            .resolve_array(kids)
                            .map_err(|e| e.to_string())?
                            .unwrap_or_default()
                        {
                            if let Some(kid_ref) = kid.as_reference() {
                                let kid_dict = self
                                    .new
                                    .resolve_dict(&kid)
                                    .map_err(|e| e.to_string())?
                                    .unwrap_or_default();
                                if kid_dict.contains_key(b"T") {
                                    continue;
                                }
                                explain(kid_ref.num, level);
                                new_widgets.insert(kid_ref.num);
                                if let Some(ap) = kid_dict.get(b"AP") {
                                    for dep in self.dependencies(ap) {
                                        explain(dep, Level::FormFilling);
                                    }
                                }
                            }
                        }
                    }
                }
                Some(old) => {
                    if changed {
                        let previously_signed = old.dict.get(b"V").is_some_and(|v| !v.is_null());
                        compare_dicts(&old.dict, &field.dict, VALUE_UPDATE_KEYS)?;
                        let locked_ok =
                            compare_dicts(&old.dict, &field.dict, ALWAYS_MODIFIABLE).is_ok();
                        if is_sig && (!previously_signed && now_signed || locked_ok) {
                            explain(field.id.num, Level::LtaUpdates);
                        } else if !is_sig {
                            explain(field.id.num, Level::FormFilling);
                        }
                        if (!is_sig || (!previously_signed && now_signed))
                            && let Some(ap) = field.dict.get(b"AP")
                        {
                            for dep in self.dependencies(ap) {
                                explain(dep, Level::FormFilling);
                            }
                        }
                        if !is_sig
                            && let Some(value) = field.dict.get(b"V")
                            && old.dict.get(b"V") != Some(value)
                        {
                            for dep in self.dependencies(value) {
                                explain(dep, Level::FormFilling);
                            }
                        }
                    }
                }
            }
            if is_sig
                && now_signed
                && old_field.is_none_or(|old| !old.dict.get(b"V").is_some_and(|v| !v.is_null()))
            {
                let value_ref = field.dict.get_ref(b"V").ok_or_else(|| {
                    format!("Value of signature field {name} should be an indirect reference")
                })?;
                let sig = self
                    .new
                    .resolve_dict(&Object::Reference(value_ref))
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        format!("Value of signature field {name} is not a dictionary")
                    })?;
                let timestamp = sig.get_name(b"Type") == Some(b"DocTimeStamp")
                    && !self.field_visible(&field.dict);
                explain(
                    value_ref.num,
                    if timestamp {
                        Level::LtaUpdates
                    } else {
                        Level::FormFilling
                    },
                );
            }
        }
        // Page annotations: only new signature widgets may be added.
        if !new_widgets.is_empty() {
            let old_pages = self.old.page_refs().map_err(|e| e.to_string())?;
            let new_pages = self.new.page_refs().map_err(|e| e.to_string())?;
            if old_pages != new_pages {
                return Err("Unexpected change to page tree structure.".to_owned());
            }
            for page_ref in new_pages {
                let old_page = self
                    .old
                    .get(page_ref)
                    .ok()
                    .and_then(|p| p.as_dict().cloned())
                    .unwrap_or_default();
                let new_page = self
                    .new
                    .get(page_ref)
                    .ok()
                    .and_then(|p| p.as_dict().cloned())
                    .unwrap_or_default();
                let old_annots = self.annot_refs(self.old, &old_page)?;
                let new_annots = self.annot_refs(self.new, &new_page)?;
                if old_annots == new_annots {
                    continue;
                }
                if old_annots.difference(&new_annots).next().is_some() {
                    return Err("Annotations were deleted.".to_owned());
                }
                if new_annots
                    .difference(&old_annots)
                    .any(|added| !new_widgets.contains(added))
                {
                    return Err("The newly added annotations were not recognised.".to_owned());
                }
                compare_dicts(&old_page, &new_page, &[b"Annots"])?;
                explain(page_ref.num, Level::LtaUpdates);
                if let Some(annots_ref) = new_page.get_ref(b"Annots")
                    && self.ref_allowed(old_page.get_ref(b"Annots"), annots_ref)
                {
                    explain(annots_ref.num, Level::LtaUpdates);
                }
            }
        }
        // Orphans: new objects nothing reaches.
        let reachable = self.reachable();
        for &num in &new_xrefs {
            if !reachable.contains(&num) {
                explain(num, Level::LtaUpdates);
            }
        }
        let mut level = Level::None;
        for &num in &new_xrefs {
            match explained.get(&num) {
                Some(explained_at) => level = level.max(*explained_at),
                None => return Err(format!("Unexplained change to object {num}")),
            }
        }
        Ok(level)
    }

    /// `safe_whitelist`: the same reference as before, or a number the old revision could
    /// not have assigned.
    fn ref_allowed(&self, old: Option<ObjRef>, new: ObjRef) -> bool {
        old == Some(new) || new.num >= self.old.xref_size()
    }

    /// `collect_dependencies(obj, since_revision)`: fresh objects reachable from `value`.
    fn dependencies(&self, value: &Object) -> Vec<u32> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![value.clone()];
        let mut budget = MAX_WALK;
        while let Some(object) = stack.pop() {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                break;
            }
            match object {
                Object::Reference(id) => {
                    if self.fresh.contains(&id.num)
                        && seen.insert(id.num)
                        && let Ok(target) = self.new.get(id)
                    {
                        stack.push(target);
                    }
                }
                Object::Array(items) => stack.extend(items),
                Object::Dict(dict) => stack.extend(dict.into_iter().map(|(_, v)| v)),
                Object::Stream(stream) => stack.extend(stream.dict.into_iter().map(|(_, v)| v)),
                _ => {}
            }
        }
        seen.into_iter().collect()
    }

    /// Every object number reachable from the new trailer.
    fn reachable(&self) -> BTreeSet<u32> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<Object> = self.new.trailer().iter().map(|(_, v)| v.clone()).collect();
        let mut budget = MAX_WALK;
        while let Some(object) = stack.pop() {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                break;
            }
            match object {
                Object::Reference(id) => {
                    if seen.insert(id.num)
                        && let Ok(target) = self.new.get(id)
                    {
                        stack.push(target);
                    }
                }
                Object::Array(items) => stack.extend(items),
                Object::Dict(dict) => stack.extend(dict.into_iter().map(|(_, v)| v)),
                Object::Stream(stream) => stack.extend(stream.dict.into_iter().map(|(_, v)| v)),
                _ => {}
            }
        }
        seen
    }

    fn annot_refs(&self, doc: &Document, page: &Dict) -> Result<BTreeSet<u32>, String> {
        let Some(annots) = page.get(b"Annots") else {
            return Ok(BTreeSet::new());
        };
        let items = doc
            .resolve_array(annots)
            .map_err(|e| e.to_string())?
            .ok_or("Not an array object")?;
        items
            .iter()
            .map(|item| {
                item.as_reference()
                    .map(|id| id.num)
                    .ok_or_else(|| "Array contains direct objects".to_owned())
            })
            .collect()
    }

    /// `is_field_visible`: the field's own widget or any kid has a rectangle with area.
    fn field_visible(&self, field: &Dict) -> bool {
        if annot_visible(field) {
            return true;
        }
        let kids = field
            .get(b"Kids")
            .and_then(|k| self.new.resolve_array(k).ok().flatten())
            .unwrap_or_default();
        kids.iter().any(|kid| {
            self.new
                .resolve_dict(kid)
                .ok()
                .flatten()
                .is_some_and(|dict| annot_visible(&dict))
        })
    }
}

fn annot_visible(dict: &Dict) -> bool {
    match dict.get_array(b"Rect") {
        Some([x0, y0, x1, y1]) => {
            let values = [x0, y0, x1, y1].map(|v| v.as_f64().unwrap_or(0.0));
            (values[2] - values[0]).abs() > 0.0 && (values[3] - values[1]).abs() > 0.0
        }
        _ => false,
    }
}

/// `compare_dicts`: every key outside `ignored` must be present in both with the same raw
/// value.
fn compare_dicts(old: &Dict, new: &Dict, ignored: &[&[u8]]) -> Result<(), String> {
    let keys: BTreeSet<&[u8]> = old
        .keys()
        .chain(new.keys())
        .map(|k| k.as_bytes())
        .filter(|k| !ignored.contains(k))
        .collect();
    for key in keys {
        if old.get(key) != new.get(key) {
            return Err(format!(
                "Dictionary entry /{} changed",
                String::from_utf8_lossy(key)
            ));
        }
    }
    Ok(())
}
