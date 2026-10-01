//! `security verify`: pyhanko's `validate_pdf_signature` for every filled signature field,
//! reported as `cmd_sec_verify` does, and `validate_pdf_timestamp` for a field holding a
//! document time-stamp. Integrity comes from [`crate::pkcs7`] and [`crate::tsp`]; coverage
//! and the incremental-update review follow `pdf_embedded.evaluate_signature_coverage` and
//! `diff_analysis.StandardDiffPolicy` with pyhanko's default rules, under the DocMDP
//! permission and field lock the signature sets ([`crate::mdp`]). One deliberate
//! difference: a lock forbids changes to the fields it names only, where pyhanko also
//! refuses any later revision once a locked field has an appearance stream it left alone.
//! Beyond pyhanko's command, each row also judges the signer's certificate chain against
//! the trust anchors and its revocation status from the document security store, or live
//! with `--online` ([`crate::ltv`]), and reports the PAdES level up to B-LTA.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::SystemTime;

use const_oid::db::rfc5911;
use pdf_core::{Dict, Document, ObjRef, Object, PdfDate, Rect};
use serde_json::{Map, Value};
use x509_cert::Certificate;

use crate::ltv::{self, Context, Judgement, Store};
use crate::mdp::{self, Permission, Policy};
use crate::net::Http;
use crate::pkcs7::{Hash, Signature, Verdict};
use crate::tsp;
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
pub struct Field {
    pub name: String,
    pub id: ObjRef,
    pub dict: Dict,
    pub field_type: Option<Vec<u8>>,
}

impl Field {
    /// The field has a value; a signature field holds a signature.
    pub fn filled(&self) -> bool {
        self.dict.get(b"V").is_some_and(|v| !v.is_null())
    }
}

/// Every field of the document's form, children before their parents.
pub fn list_fields(doc: &Document) -> Result<Vec<Field>, String> {
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

/// What a filled signature field holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Signature,
    /// A document time-stamp (PAdES B-LTA): an RFC 3161 token over the revision it ends.
    DocumentTimeStamp,
}

impl Kind {
    /// `/Type /DocTimeStamp` or SubFilter `ETSI.RFC3161` mark a document time-stamp.
    fn of(sig: &Dict) -> Kind {
        if sig.has_type(b"DocTimeStamp") || sig.get_name(b"SubFilter") == Some(b"ETSI.RFC3161") {
            Kind::DocumentTimeStamp
        } else {
            Kind::Signature
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::Signature => "signature",
            Kind::DocumentTimeStamp => "document_timestamp",
        }
    }

    /// The `/Type` and `/SubFilter` the dictionary must have.
    fn check(self, sig: &Dict) -> Result<(), String> {
        let sub_filter = sig.get_name(b"SubFilter");
        let unknown = |kind: &str| {
            format!(
                "{} is not a recognized SubFilter type in {kind}.",
                sub_filter.map_or("None".to_owned(), |name| format!(
                    "/{}",
                    String::from_utf8_lossy(name)
                ))
            )
        };
        match self {
            Kind::Signature => {
                if sig.get_name(b"Type").is_some_and(|t| t != b"Sig") {
                    return Err("Signature object type must be /Sig".to_owned());
                }
                if !matches!(
                    sub_filter,
                    Some(b"adbe.pkcs7.detached" | b"ETSI.CAdES.detached")
                ) {
                    return Err(unknown("signatures"));
                }
            }
            Kind::DocumentTimeStamp => {
                if !sig.has_type(b"DocTimeStamp") {
                    return Err("Signature object type must be /DocTimeStamp".to_owned());
                }
                if sub_filter != Some(b"ETSI.RFC3161") {
                    return Err(unknown("document time-stamps"));
                }
            }
        }
        Ok(())
    }
}

/// One signature to report: the field and its signature dictionary.
struct Embedded {
    field: String,
    /// The field's dictionary, for its `/Lock`.
    field_dict: Dict,
    kind: Kind,
    sig_ref: ObjRef,
    sig: Dict,
    signed_revision: usize,
}

/// What `security verify` judges certificate chains with.
#[derive(Default)]
pub struct Checks {
    /// The trust anchors: the system's roots and those `--trust` adds.
    pub anchors: Vec<Certificate>,
    /// Where `--online` asks for the revocation answers the document lacks; `None` stays
    /// offline.
    pub online: Option<Http>,
}

/// What one verification judges chains with.
struct Judging<'a> {
    /// The document security store as the file ends.
    store: Option<&'a Store>,
    context: Context<'a>,
    /// `context` without the network, for the long-term levels.
    offline: Context<'a>,
}

/// `cmd_sec_verify`: the `signatures` list, in signing order.
pub fn verify_file(data: &[u8], checks: &Checks) -> Result<Vec<Map<String, Value>>, String> {
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
            field_dict: field.dict,
            kind: Kind::of(&sig),
            sig_ref,
            sig,
            signed_revision,
        });
    }
    embedded.sort_by_key(|e| e.signed_revision);
    // pyhanko's validation ignores `/Perms`; one it cannot read certifies nothing.
    let certification = mdp::certification(&doc)
        .ok()
        .flatten()
        .and_then(|c| c.signature);
    let store = Store::read(&doc);
    let now = SystemTime::now();
    let judging = Judging {
        store: store.as_ref(),
        context: Context {
            anchors: &checks.anchors,
            online: checks.online.as_ref(),
            now,
        },
        offline: Context {
            anchors: &checks.anchors,
            online: None,
            now,
        },
    };
    let mut out = Vec::with_capacity(embedded.len());
    // The signatures at B-LT, by row, and the revisions document time-stamps that check
    // out end: a later one over a store that still completes both chains makes B-LTA.
    let mut long_term = Vec::new();
    let mut archived = Vec::new();
    for entry in &embedded {
        let contents = entry
            .sig
            .get_string(b"Contents")
            .ok_or("Could not read /Contents entry in signature")?
            .as_bytes();
        let certified = certification == Some(entry.sig_ref);
        match entry.kind {
            Kind::Signature => {
                let (row, lt) =
                    signature_row(data, &doc, &chain, entry, contents, certified, &judging)?;
                long_term.extend(lt.map(|lt| (out.len(), lt)));
                out.push(row);
            }
            Kind::DocumentTimeStamp => {
                let (row, checks_out) =
                    time_stamp_row(data, &doc, &chain, entry, contents, certified, &judging);
                if checks_out {
                    archived.push(entry.signed_revision);
                }
                out.push(row);
            }
        }
    }
    for (index, lt) in long_term {
        let covered = archived
            .iter()
            .filter(|&&revision| revision > lt.revision)
            .any(|&revision| {
                store_at(data, &chain, revision)
                    .is_some_and(|store| lt.complete(&store, &judging.offline))
            });
        if covered && let Some(row) = out.get_mut(index) {
            row.insert("pades_level".into(), "B-LTA".into());
        }
    }
    Ok(out)
}

/// What a signature at B-LT needs a later document time-stamp's store to hold for B-LTA.
struct LongTerm {
    /// The revision the signature signs.
    revision: usize,
    signer: Certificate,
    signer_certs: Vec<Certificate>,
    authority: Certificate,
    authority_certs: Vec<Certificate>,
    /// The time its signature time-stamp attests.
    time: SystemTime,
}

impl LongTerm {
    /// `store` completes the chains of the signer and of its time-stamp authority.
    fn complete(&self, store: &Store, context: &Context<'_>) -> bool {
        ltv::judge(
            &self.signer,
            &self.signer_certs,
            Some(store),
            context,
            self.time,
        )
        .complete
            && ltv::judge(
                &self.authority,
                &self.authority_certs,
                Some(store),
                context,
                self.time,
            )
            .complete
    }
}

/// The document security store as revision `revision` left it.
fn store_at(data: &[u8], chain: &Chain, revision: usize) -> Option<Store> {
    let end = chain.revision_end(data, revision)?;
    let doc = Document::load(data.get(..end)?.to_vec()).ok()?;
    Store::read(&doc)
}

/// The row of a signature: what [`validate`] finds, its signature time-stamp, and the
/// judgement of its chain; with what B-LTA needs when it reaches B-LT.
fn signature_row(
    data: &[u8],
    doc: &Document,
    chain: &Chain,
    entry: &Embedded,
    contents: &[u8],
    certified: bool,
    judging: &Judging<'_>,
) -> Result<(Map<String, Value>, Option<LongTerm>), String> {
    let signature = Signature::parse(contents)?;
    let mut row = Map::new();
    row.insert("field".into(), entry.field.clone().into());
    row.insert("kind".into(), entry.kind.as_str().into());
    row.insert("signer".into(), signature.signer.clone().into());
    let checked = validate(data, doc, chain, entry, signature.digest, |digest| {
        signature.verify(digest)
    });
    insert_validation(&mut row, &checked, certified);
    let token = tsp::embedded_token(&signature).map(|token| {
        token.and_then(|token| token.check(signature.signature_value()).map(|()| token))
    });
    let stamp = token.as_ref().map(|token| {
        token
            .as_ref()
            .map(|token| token.time)
            .map_err(String::clone)
    });
    insert_time_stamp(&mut row, stamp.as_ref());
    let level = pades_level(&entry.sig, &signature, matches!(stamp, Some(Ok(_))));
    let stamped = match &token {
        Some(Ok(token)) => Some(token),
        _ => None,
    };
    let time = stamped.map_or(judging.context.now, |token| ltv::system_time(&token.time));
    let judgement = ltv::judge(
        signature.certificate(),
        signature.certificates(),
        judging.store,
        &judging.context,
        time,
    );
    insert_judgement(&mut row, &judgement);
    // B-LT: a B-T signature whose store completes its chain and its time-stamp
    // authority's.
    let long_term = stamped
        .filter(|_| level == "B-T" && judgement.complete)
        .map(|token| LongTerm {
            revision: entry.signed_revision,
            signer: signature.certificate().clone(),
            signer_certs: signature.certificates().to_vec(),
            authority: token.signature.certificate().clone(),
            authority_certs: token.signature.certificates().to_vec(),
            time,
        })
        .filter(|lt| {
            ltv::judge(
                &lt.authority,
                &lt.authority_certs,
                judging.store,
                &judging.offline,
                time,
            )
            .complete
        });
    row.insert(
        "pades_level".into(),
        if long_term.is_some() {
            "B-LT".into()
        } else {
            level
        },
    );
    Ok((row, long_term))
}

/// The row of a document time-stamp, as `validate_pdf_timestamp` checks one: the
/// authority is the signer, its token must cover the revision the `/ByteRange` ends, and
/// the revisions after it are reviewed as after a signature. `timestamp` is the time the
/// token attests to. Also whether it checks out: intact, valid, and over its revision.
fn time_stamp_row(
    data: &[u8],
    doc: &Document,
    chain: &Chain,
    entry: &Embedded,
    contents: &[u8],
    certified: bool,
    judging: &Judging<'_>,
) -> (Map<String, Value>, bool) {
    let mut row = Map::new();
    row.insert("field".into(), entry.field.clone().into());
    row.insert("kind".into(), entry.kind.as_str().into());
    let token = tsp::Token::parse(contents);
    row.insert(
        "signer".into(),
        token
            .as_ref()
            .map_or(Value::Null, |token| token.signature.signer.clone().into()),
    );
    let checked = token.as_ref().map_err(String::clone).and_then(|token| {
        validate(data, doc, chain, entry, token.imprint_hash()?, |digest| {
            token.verdict(digest)
        })
    });
    insert_validation(&mut row, &checked, certified);
    let stamp = match (&token, &checked) {
        (Ok(token), Ok(checked)) => token.check_revision(&checked.digest).map(|()| token.time),
        (Err(error), _) | (_, Err(error)) => Err(error.clone()),
    };
    insert_time_stamp(&mut row, Some(&stamp));
    if let Ok(token) = &token {
        let time = stamp.as_ref().map_or(judging.context.now, ltv::system_time);
        let judgement = ltv::judge(
            token.signature.certificate(),
            token.signature.certificates(),
            judging.store,
            &judging.context,
            time,
        );
        insert_judgement(&mut row, &judgement);
    }
    row.insert("pades_level".into(), Value::Null);
    let checks_out = stamp.is_ok()
        && checked
            .as_ref()
            .is_ok_and(|checked| checked.verdict.intact && checked.verdict.valid);
    (row, checks_out)
}

/// The keys [`validate`] decides, or its error as `validation_error`.
fn insert_validation(
    row: &mut Map<String, Value>,
    checked: &Result<Validation, String>,
    certified: bool,
) {
    let checked = match checked {
        Ok(checked) => checked,
        Err(error) => {
            row.insert("validation_error".into(), error.clone().into());
            return;
        }
    };
    row.insert("intact".into(), checked.verdict.intact.into());
    row.insert("valid".into(), checked.verdict.valid.into());
    row.insert("trusted".into(), checked.verdict.trusted.into());
    row.insert("coverage".into(), checked.coverage.as_str().into());
    row.insert(
        "modified".into(),
        checked
            .modification
            .as_ref()
            .map_or("OTHER", |level| level.name())
            .into(),
    );
    row.insert("certified".into(), certified.into());
    row.insert(
        "docmdp_level".into(),
        checked
            .policy
            .doc_mdp
            .map_or(Value::Null, |permission| permission.p().into()),
    );
    row.insert("changes_allowed".into(), checked.changes_allowed().into());
    if let Err(reason) = &checked.modification {
        row.insert("modification_error".into(), reason.clone().into());
    }
}

/// `timestamp`, `timestamp_valid`, and `timestamp_error` for the time-stamp check
/// `stamp`; all null for a row without a time-stamp.
fn insert_time_stamp(row: &mut Map<String, Value>, stamp: Option<&Result<PdfDate, String>>) {
    row.insert(
        "timestamp".into(),
        match stamp {
            Some(Ok(time)) => tsp::iso_utc(time).into(),
            _ => Value::Null,
        },
    );
    row.insert(
        "timestamp_valid".into(),
        stamp.map_or(Value::Null, |s| s.is_ok().into()),
    );
    if let Some(Err(error)) = stamp {
        row.insert("timestamp_error".into(), error.clone().into());
    }
}

/// `chain_trusted` and `revocation`, with `revocation_source` and the errors behind them.
fn insert_judgement(row: &mut Map<String, Value>, judgement: &Judgement) {
    row.insert(
        "chain_trusted".into(),
        judgement.trust_error.is_none().into(),
    );
    if let Some(error) = &judgement.trust_error {
        row.insert("trust_error".into(), error.clone().into());
    }
    row.insert(
        "revocation".into(),
        judgement
            .revocation
            .map_or(Value::Null, |revocation| revocation.as_str().into()),
    );
    row.insert(
        "revocation_source".into(),
        judgement.source.map_or(Value::Null, Value::from),
    );
    if let Some(error) = &judgement.revocation_error {
        row.insert("revocation_error".into(), error.clone().into());
    }
}

/// The PAdES baseline level the signature meets: B-B for a CAdES signature whose signed
/// attributes bind the signing certificate, B-T once a valid time-stamp covers it; null
/// for other signatures.
fn pades_level(sig: &Dict, signature: &Signature, stamped: bool) -> Value {
    let cades = sig.get_name(b"SubFilter") == Some(b"ETSI.CAdES.detached".as_slice());
    let bound = [
        rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2,
        rfc5911::ID_AA_SIGNING_CERTIFICATE,
    ]
    .into_iter()
    .any(|oid| signature.signed_attribute(oid).is_some());
    match (cades && bound, stamped) {
        (false, _) => Value::Null,
        (true, false) => "B-B".into(),
        (true, true) => "B-T".into(),
    }
}

/// What [`validate`] found for one signature.
struct Validation {
    verdict: Verdict,
    coverage: Coverage,
    policy: Policy,
    /// The modification level of the revisions after the signed one, or why they count as
    /// OTHER.
    modification: Result<Level, String>,
    /// The digest of the `/ByteRange`, under the hash the signature or token uses.
    digest: Vec<u8>,
}

impl Validation {
    /// pyhanko's `docmdp_ok`: nothing changed beyond what the signature's DocMDP permission
    /// allows, and nothing at level OTHER.
    fn changes_allowed(&self) -> bool {
        match (&self.modification, self.policy.doc_mdp) {
            (Err(_), _) => false,
            (Ok(_), None) => true,
            (Ok(level), Some(permission)) => {
                *level
                    <= match permission {
                        Permission::NoChanges => Level::LtaUpdates,
                        Permission::FillForms | Permission::Annotate => Level::FormFilling,
                    }
            }
        }
    }
}

/// Checks the signature dictionary of `entry`, digests its `/ByteRange` with `hash`, takes
/// the integrity verdict of that digest from `verdict`, and reviews the later revisions.
fn validate(
    data: &[u8],
    doc: &Document,
    chain: &Chain,
    entry: &Embedded,
    hash: Hash,
    verdict: impl FnOnce(&[u8]) -> Result<Verdict, String>,
) -> Result<Validation, String> {
    entry.kind.check(&entry.sig)?;
    if chain.sections.iter().any(|s| s.hybrid) {
        return Err(
            "Settings do not permit validation of signatures in hybrid-reference files.".to_owned(),
        );
    }
    let policy = Policy::read(doc, &entry.sig, &entry.field_dict)?;
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
    let digest = hash.digest_ranges(data, &ranges);
    let verdict = verdict(&digest)?;
    let coverage = coverage(
        data,
        chain,
        entry.signed_revision,
        &byte_range,
        contents_len,
    );
    let modification = match coverage {
        Coverage::EntireFile => Ok(Level::None),
        Coverage::Unclear | Coverage::ContiguousBlockFromStart => {
            Err("Nonstandard signature coverage level".to_owned())
        }
        Coverage::EntireRevision => review(data, chain, entry.signed_revision, &policy),
    };
    Ok(Validation {
        verdict,
        coverage,
        policy,
        modification,
        digest,
    })
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
/// signed one needs under the signature's `policy`; an error is a `SuspiciousModification`.
fn review(
    data: &[u8],
    chain: &Chain,
    signed_revision: usize,
    policy: &Policy,
) -> Result<Level, String> {
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
            policy,
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
    /// The DocMDP permission and field lock of the signature under review.
    policy: &'a Policy,
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
                    self.field_update(name, field.id.num, !visible, !visible)?;
                    explain(field.id.num, level);
                    new_widgets.insert(field.id.num);
                    for key in [b"AP".as_slice(), b"Lock", b"SV"] {
                        if let Some(value) = field.dict.get(key) {
                            for dep in self.dependencies(value) {
                                self.field_update(name, dep, false, true)?;
                                explain(dep, Level::FormFilling);
                            }
                        }
                    }
                    if let Some(kids) = field.dict.get(b"Kids") {
                        if let Some(kids_ref) = kids.as_reference() {
                            self.field_update(name, kids_ref.num, !visible, true)?;
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
                                self.field_update(name, kid_ref.num, !visible, true)?;
                                explain(kid_ref.num, level);
                                new_widgets.insert(kid_ref.num);
                                if let Some(ap) = kid_dict.get(b"AP") {
                                    for dep in self.dependencies(ap) {
                                        self.field_update(name, dep, false, true)?;
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
                            self.field_update(name, field.id.num, locked_ok, true)?;
                            explain(field.id.num, Level::LtaUpdates);
                        } else if !is_sig {
                            self.field_update(name, field.id.num, locked_ok, true)?;
                            explain(field.id.num, Level::FormFilling);
                        }
                        if (!is_sig || (!previously_signed && now_signed))
                            && let Some(ap) = field.dict.get(b"AP")
                        {
                            for dep in self.dependencies(ap) {
                                self.field_update(name, dep, false, true)?;
                                explain(dep, Level::FormFilling);
                            }
                        }
                        if !is_sig
                            && let Some(value) = field.dict.get(b"V")
                            && old.dict.get(b"V") != Some(value)
                        {
                            for dep in self.dependencies(value) {
                                self.field_update(name, dep, false, true)?;
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
                self.field_update(name, value_ref.num, timestamp, true)?;
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

    /// The checks `StandardDiffPolicy.apply` makes of a form update to field `name`: an
    /// update a lock forbids (`valid_when_locked` false) must not touch a field the
    /// signature locks, and some are allowed only after an approval signature.
    fn field_update(
        &self,
        name: &str,
        num: u32,
        valid_when_locked: bool,
        valid_when_certifying: bool,
    ) -> Result<(), String> {
        if !valid_when_locked
            && self
                .policy
                .lock
                .as_ref()
                .is_some_and(|lock| lock.locks(name))
        {
            return Err(format!(
                "Update of object {num} is not allowed because the form field {name} is locked."
            ));
        }
        if !valid_when_certifying && self.policy.doc_mdp.is_some() {
            return Err(format!(
                "Update of object {num} is only allowed after an approval signature, not a certification signature."
            ));
        }
        Ok(())
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
    widget_box(dict).is_some()
}

/// A widget's `/Rect`, normalized, when it has area.
pub fn widget_box(widget: &Dict) -> Option<Rect> {
    let Some([x0, y0, x1, y1]) = widget.get_array(b"Rect") else {
        return None;
    };
    let rect = Rect::new(x0.as_f64()?, y0.as_f64()?, x1.as_f64()?, y1.as_f64()?).normalized();
    (!rect.is_empty()).then_some(rect)
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
