//! Long-term validation, PAdES B-LT and B-LTA: the certificate chains behind a signature
//! and its time-stamp, with the OCSP responses and CRLs that show none of those
//! certificates was revoked. `security sign --ltv` fetches them from the addresses the
//! certificates name and keeps them in the document security store (`/DSS`);
//! `security verify` reads them back, judges each chain against its trust anchors, and
//! with `--online` asks the certificates' own responders for what the store lacks.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::SignedData;
use const_oid::ObjectIdentifier;
use const_oid::db::{rfc5280, rfc6960};
use der::{Decode, Encode};
use pdf_core::{Dict, Document, Object, PdfDate, Stream};
use sha1::{Digest, Sha1};
use x509_cert::Certificate;
use x509_cert::crl::CertificateList;
use x509_cert::ext::pkix::name::{DistributionPointName, GeneralName};
use x509_cert::ext::pkix::{
    AuthorityInfoAccessSyntax, BasicConstraints, CrlDistributionPoints, ExtendedKeyUsage, KeyUsage,
};
use x509_ocsp::builder::OcspRequestBuilder;
use x509_ocsp::ext::Nonce;
use x509_ocsp::{
    BasicOcspResponse, CertId, CertStatus, OcspResponse, OcspResponseStatus, Request, ResponderId,
};

use crate::net::Http;
use crate::path::{self, self_issued};
use crate::pkcs7::{Hash, human_friendly, verify_signed};
use crate::tsp;

/// How far an answer's issue time may run ahead of this clock.
const CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);
/// How long an answer that names no next update stays current, as pyhanko assumes.
const NO_NEXT_UPDATE: Duration = Duration::from_secs(30 * 60);
/// The most certificates a chain may hold.
const MAX_CHAIN: usize = 10;
/// `id-ce-noRevAvail` (RFC 9608 §2): the issuer publishes no revocation information for
/// the certificate.
const ID_CE_NO_REV_AVAIL: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.56");
/// The extensions path validation here processes: the constraints and key usages it
/// checks, the name constraints and certificate policies [`path::constrain`] applies, and
/// the revocation pointers and exemptions it follows. RFC 5280 §6.1.4 (o) and §6.1.5 (f)
/// make a path through any other critical extension invalid.
const PROCESSED: &[ObjectIdentifier] = &[
    rfc5280::ID_CE_BASIC_CONSTRAINTS,
    rfc5280::ID_CE_KEY_USAGE,
    rfc5280::ID_CE_EXT_KEY_USAGE,
    rfc5280::ID_CE_SUBJECT_KEY_IDENTIFIER,
    rfc5280::ID_CE_AUTHORITY_KEY_IDENTIFIER,
    rfc5280::ID_CE_SUBJECT_ALT_NAME,
    rfc5280::ID_CE_ISSUER_ALT_NAME,
    rfc5280::ID_CE_CERTIFICATE_POLICIES,
    rfc5280::ID_CE_POLICY_MAPPINGS,
    rfc5280::ID_CE_POLICY_CONSTRAINTS,
    rfc5280::ID_CE_INHIBIT_ANY_POLICY,
    rfc5280::ID_CE_NAME_CONSTRAINTS,
    rfc5280::ID_CE_CRL_DISTRIBUTION_POINTS,
    rfc5280::ID_PE_AUTHORITY_INFO_ACCESS,
    rfc6960::ID_PKIX_OCSP_NOCHECK,
    ID_CE_NO_REV_AVAIL,
];

/// The trust anchors and the network one judgement uses.
pub struct Context<'a> {
    /// The certificates a chain may end at.
    pub anchors: &'a [Certificate],
    /// Where missing certificates and revocation answers are fetched; `None` stays offline.
    pub online: Option<&'a Http>,
    pub now: SystemTime,
}

/// What `--ltv` adds to the document security store, each item DER-encoded and held
/// once.
#[derive(Default)]
pub struct Material {
    pub certs: Vec<Vec<u8>>,
    pub ocsps: Vec<Vec<u8>>,
    pub crls: Vec<Vec<u8>>,
    /// The certificates that name no OCSP responder or CRL, so nothing shows they were
    /// not revoked.
    pub unchecked: Vec<String>,
    /// The certificates whose revocation was already looked up.
    looked_up: Vec<Certificate>,
}

impl Material {
    /// How many certificates, OCSP responses, and CRLs the store is to hold.
    pub fn held(&self) -> usize {
        self.certs.len() + self.ocsps.len() + self.crls.len()
    }
}

/// Adds to `material` what shows that `leaves` (the signer's certificate, its time-stamp
/// authority's) chain to a root and were not revoked. `carried` are the certificates the
/// signature or time-stamp carries; a missing issuer comes from the address its
/// certificate names, else from the system's roots, and revocation from each
/// certificate's OCSP responder, else its CRL. A revoked certificate is an error, as is
/// a certificate that names a responder or CRL none of which answers; one that names
/// neither yet needs a check (RFC 9608 §4 exempts some) goes in `unchecked`.
pub fn gather(
    http: &Http,
    leaves: &[&Certificate],
    carried: &[Certificate],
    material: &mut Material,
) -> Result<(), String> {
    let now = SystemTime::now();
    let anchors = system_roots()?;
    let context = Context {
        anchors: &anchors,
        online: Some(http),
        now,
    };
    let mut pool = Vec::new();
    for cert in carried {
        hold(&mut pool, cert.clone());
    }
    let mut queue: Vec<Certificate> = leaves.iter().map(|&cert| cert.clone()).collect();
    while let Some(leaf) = queue.pop() {
        let walk = walk(&leaf, &mut pool, &context);
        if walk.end == End::Missing {
            let last = walk.path.last().unwrap_or(&leaf);
            return Err(format!(
                "--ltv could not find the certificate that issued {}",
                name(last)
            ));
        }
        for (cert, issuer) in links(&walk.path) {
            if material.looked_up.contains(cert) {
                continue;
            }
            material.looked_up.push(cert.clone());
            // RFC 9608 §4: no revocation check for a certificate that needs none.
            if exempt(cert) {
                continue;
            }
            let live = fetch_status(http, cert, issuer, now, now).map_err(|error| {
                format!(
                    "--ltv found no revocation status for {}: {error}",
                    name(cert)
                )
            })?;
            match live {
                None => material.unchecked.push(name(cert)),
                Some(Live {
                    status: Status::Revoked(at),
                    ..
                }) => return Err(format!("{} was revoked at {}", name(cert), iso(at))),
                Some(Live {
                    evidence: Evidence::Ocsp(der, responder),
                    ..
                }) => {
                    hold(&mut material.ocsps, der);
                    match responder.map(|responder| *responder) {
                        Some(responder) if exempt(&responder) => {
                            hold(&mut material.certs, encode(&responder)?);
                        }
                        Some(responder) if !material.looked_up.contains(&responder) => {
                            queue.push(responder);
                        }
                        _ => {}
                    }
                }
                Some(Live {
                    evidence: Evidence::Crl(der),
                    ..
                }) => hold(&mut material.crls, der),
            }
        }
        for cert in &walk.path {
            hold(&mut material.certs, encode(cert)?);
        }
    }
    Ok(())
}

/// `data` with `material` added to its document security store in a new revision; what
/// the store already holds stays, once.
pub fn add_dss(data: Vec<u8>, material: &Material) -> Result<Vec<u8>, String> {
    let mut doc = Document::load(data).map_err(|e| e.to_string())?;
    let catalog_ref = doc.catalog_ref().map_err(|e| e.to_string())?;
    let mut catalog = doc.catalog().map_err(|e| e.to_string())?;
    let existing = catalog.get(b"DSS").cloned();
    let mut dss = existing
        .as_ref()
        .map(|value| doc.resolve_dict(value))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .unwrap_or_default();
    for (key, items) in [
        ("Certs", &material.certs),
        ("OCSPs", &material.ocsps),
        ("CRLs", &material.crls),
    ] {
        let held = streams(&doc, &dss, key.as_bytes());
        let mut array = dss
            .get(key.as_bytes())
            .map(|value| doc.resolve_array(value))
            .transpose()
            .map_err(|e| e.to_string())?
            .flatten()
            .unwrap_or_default();
        for der in items.iter().filter(|der| !held.contains(der)) {
            let stream = Stream::new(Dict::new(), der.clone());
            array.push(Object::Reference(doc.add(Object::Stream(stream))));
        }
        if !array.is_empty() {
            dss.insert(key, array);
        }
    }
    match existing.as_ref().and_then(Object::as_reference) {
        Some(id) => doc.set(id, dss),
        None => {
            let id = doc.add(dss);
            catalog.insert("DSS", id);
            doc.set(catalog_ref, catalog);
        }
    }
    doc.save_incremental(true)
        .map(|saved| saved.data)
        .map_err(|e| e.to_string())
}

/// A document security store, decoded.
#[derive(Default)]
pub struct Store {
    certs: Vec<Certificate>,
    ocsps: Vec<BasicOcspResponse>,
    crls: Vec<CertificateList>,
}

impl Store {
    /// The document security store of `doc`, if it has one. Entries that do not decode
    /// are left out: they show nothing.
    pub fn read(doc: &Document) -> Option<Store> {
        let catalog = doc.catalog().ok()?;
        let dss = doc.resolve_dict(catalog.get(b"DSS")?).ok()??;
        Some(Store {
            certs: streams(doc, &dss, b"Certs")
                .iter()
                .filter_map(|der| Certificate::from_der(der).ok())
                .collect(),
            ocsps: streams(doc, &dss, b"OCSPs")
                .iter()
                .filter_map(|der| basic_response(der).ok())
                .collect(),
            crls: streams(doc, &dss, b"CRLs")
                .iter()
                .filter_map(|der| CertificateList::from_der(der).ok())
                .collect(),
        })
    }

    /// The store `material` alone makes.
    fn of(material: &Material) -> Store {
        Store {
            certs: material
                .certs
                .iter()
                .filter_map(|der| Certificate::from_der(der).ok())
                .collect(),
            ocsps: material
                .ocsps
                .iter()
                .filter_map(|der| basic_response(der).ok())
                .collect(),
            crls: material
                .crls
                .iter()
                .filter_map(|der| CertificateList::from_der(der).ok())
                .collect(),
        }
    }
}

/// The decoded streams the `/DSS` array `key` refers to.
fn streams(doc: &Document, dss: &Dict, key: &[u8]) -> Vec<Vec<u8>> {
    let Some(items) = dss
        .get(key)
        .and_then(|value| doc.resolve_array(value).ok().flatten())
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match doc.resolve(item).ok()? {
            Object::Stream(stream) => doc.decode_stream(&stream).ok().map(|decoded| decoded.data),
            _ => None,
        })
        .collect()
}

/// A revocation verdict over a chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Revocation {
    /// Every certificate below the root that needs a revocation check was shown not
    /// revoked.
    Good,
    /// Some certificate was revoked by the time judged.
    Revoked,
    /// Some certificate needs a check that no usable answer settles; naming no
    /// responder or CRL is no answer.
    Unknown,
}

impl Revocation {
    pub fn as_str(self) -> &'static str {
        match self {
            Revocation::Good => "good",
            Revocation::Revoked => "revoked",
            Revocation::Unknown => "unknown",
        }
    }
}

/// What became of one certificate chain.
pub struct Judgement {
    /// Why the chain does not reach a trust anchor with each link within the policy at
    /// the time judged; `None` when it does.
    pub trust_error: Option<String>,
    /// `None` when no certificate below the root needs a revocation check (RFC 9608 §4).
    pub revocation: Option<Revocation>,
    /// What made the verdict revoked or unknown.
    pub revocation_error: Option<String>,
    /// Where the answers behind the verdict came from: `online` when any was fetched
    /// live, else `dss`; `None` without a verdict or an answer.
    pub source: Option<&'static str>,
    /// The store completes the chain and holds a good answer for every certificate below
    /// the root that needs a revocation check; the root is the trust anchor, an input to
    /// RFC 5280's path validation rather than part of the path. This is the validation
    /// data PAdES B-LT asks for (ETSI EN 319 142-1 V1.2.1, 6.3 (t)).
    pub complete: bool,
}

/// Judges the chain of `leaf` for `time`. The chain is built from the anchors,
/// `carried` (the certificates the signature or token carries), and the store's;
/// revocation answers come from the store, then, when online, from each certificate's
/// own responders.
pub fn judge(
    leaf: &Certificate,
    carried: &[Certificate],
    store: Option<&Store>,
    context: &Context<'_>,
    time: SystemTime,
) -> Judgement {
    let mut pool = Vec::new();
    for cert in carried
        .iter()
        .chain(store.map_or(&[][..], |store| store.certs.as_slice()))
    {
        hold(&mut pool, cert.clone());
    }
    let walk = walk(leaf, &mut pool, context);
    let mut complete = store.is_some() && walk.end != End::Missing && !walk.fetched;
    let mut answered_online = false;
    let mut answered_store = false;
    let mut checked = false;
    let mut revoked = None;
    let mut unknown = None;
    for (cert, issuer) in links(&walk.path) {
        // RFC 9608 §4 skips RFC 5280's revocation step, §6.1.3 (a)(3), for these.
        if exempt(cert) {
            continue;
        }
        match answer(cert, issuer, &pool, store, context, time, true) {
            Answer::Good { online } => {
                checked = true;
                answered_online |= online;
                answered_store |= !online;
                complete &= !online;
            }
            Answer::Revoked { at, online } => {
                answered_online |= online;
                answered_store |= !online;
                complete = false;
                revoked.get_or_insert_with(|| format!("{} was revoked at {}", name(cert), iso(at)));
            }
            Answer::Unknown(why) => {
                complete = false;
                unknown.get_or_insert_with(|| format!("{}: {why}", name(cert)));
            }
        }
    }
    if walk.end == End::Missing
        && let Some(last) = walk.path.last().filter(|cert| !exempt(cert))
    {
        unknown.get_or_insert_with(|| {
            format!("{}: the certificate that issued it is missing", name(last))
        });
    }
    let (revocation, revocation_error) = match (revoked, unknown) {
        (Some(error), _) => (Some(Revocation::Revoked), Some(error)),
        (None, Some(error)) => (Some(Revocation::Unknown), Some(error)),
        (None, None) => (checked.then_some(Revocation::Good), None),
    };
    let source = match (revocation, answered_online, answered_store) {
        (None, _, _) | (Some(_), false, false) => None,
        (Some(_), true, _) => Some("online"),
        (Some(_), false, true) => Some("dss"),
    };
    Judgement {
        trust_error: trust(&walk, time).err(),
        revocation,
        revocation_error,
        source,
        complete,
    }
}

/// Whether `material` alone completes the chain of each of `leaves` for `time`, as
/// [`judge`] decides offline: what PAdES B-LTA asks of the store a document time-stamp
/// covers (ETSI EN 319 142-1 V1.2.1, 6.3 (t) and (x)).
pub fn completes(
    material: &Material,
    leaves: &[&Certificate],
    carried: &[Certificate],
    time: SystemTime,
) -> Result<bool, String> {
    let anchors = system_roots()?;
    let context = Context {
        anchors: &anchors,
        online: None,
        now: SystemTime::now(),
    };
    let store = Store::of(material);
    Ok(leaves
        .iter()
        .all(|leaf| judge(leaf, carried, Some(&store), &context, time).complete))
}

/// The trust anchors of the macOS system keychain (`SecTrustCopyAnchorCertificates`);
/// those this crate cannot decode are left out.
#[cfg(target_os = "macos")]
pub fn system_roots() -> Result<Vec<Certificate>, String> {
    let anchors = security_framework::trust::SecTrust::copy_anchor_certificates()
        .map_err(|e| format!("the system trust store: {e}"))?;
    Ok(anchors
        .iter()
        .filter_map(|cert| Certificate::from_der(&cert.to_der()).ok())
        .collect())
}

/// Other systems have no trust store this crate reads: only `--trust` anchors apply.
#[cfg(not(target_os = "macos"))]
pub fn system_roots() -> Result<Vec<Certificate>, String> {
    Ok(Vec::new())
}

/// The certificates in `data`: one DER certificate, PEM certificates, or a PKCS#7
/// certificate bundle, as trust anchor files and issuer addresses serve them.
pub fn certificates_in(data: &[u8]) -> Result<Vec<Certificate>, String> {
    if let Ok(cert) = Certificate::from_der(data) {
        return Ok(vec![cert]);
    }
    if let Ok(certs) = Certificate::load_pem_chain(data)
        && !certs.is_empty()
    {
        return Ok(certs);
    }
    let bundle = ContentInfo::from_der(data)
        .ok()
        .and_then(|content| content.content.decode_as::<SignedData>().ok())
        .ok_or("no certificate in DER, PEM, or PKCS#7 form")?;
    Ok(bundle
        .certificates
        .iter()
        .flat_map(|set| set.0.iter())
        .filter_map(|choice| match choice {
            CertificateChoices::Certificate(cert) => Some(cert.clone()),
            CertificateChoices::Other(_) => None,
        })
        .collect())
}

/// `date` as a system time; dates before 1970 count as 1970.
pub fn system_time(date: &PdfDate) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(date.to_unix()).unwrap_or(0))
}

/// Where a chain stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// At a trust anchor.
    Anchor,
    /// At a self-issued root that is not an anchor.
    Root,
    /// At a certificate whose issuer is nowhere at hand.
    Missing,
}

struct Walk {
    /// The leaf first, then each issuer.
    path: Vec<Certificate>,
    end: End,
    /// Some certificate on the path was fetched live.
    fetched: bool,
}

/// The chain from `leaf` up: each issuer an anchor, else from `pool`, else (online) from
/// the address the certificate names, which `pool` then keeps.
fn walk(leaf: &Certificate, pool: &mut Vec<Certificate>, context: &Context<'_>) -> Walk {
    let mut path = vec![leaf.clone()];
    let mut fetched = false;
    let end = loop {
        let Some(current) = path.last() else {
            break End::Missing;
        };
        if context
            .anchors
            .iter()
            .any(|anchor| same_key(anchor, current))
        {
            break End::Anchor;
        }
        if let Some(anchor) = issuer_in(context.anchors, current).cloned() {
            path.push(anchor);
            break End::Anchor;
        }
        if self_issued(current) {
            break End::Root;
        }
        if path.len() >= MAX_CHAIN {
            break End::Missing;
        }
        let issuer = match issuer_in(pool, current).cloned() {
            Some(issuer) => issuer,
            None => {
                let Some(http) = context.online else {
                    break End::Missing;
                };
                for url in access_urls(current, rfc5280::ID_AD_CA_ISSUERS) {
                    if let Ok(certs) = http.get(&url).and_then(|body| certificates_in(&body)) {
                        for cert in certs {
                            hold(pool, cert);
                        }
                    }
                }
                let Some(issuer) = issuer_in(pool, current).cloned() else {
                    break End::Missing;
                };
                fetched = true;
                issuer
            }
        };
        if path.contains(&issuer) {
            break End::Missing;
        }
        path.push(issuer);
    };
    Walk { path, end, fetched }
}

/// Why `walk` does not make its leaf trusted at `time`: the chain must end at an anchor,
/// each certificate below it valid then, signed within the policy, free of critical
/// extensions left unprocessed (RFC 5280 §6.1.4 (o), §6.1.5 (f)) and of a `noRevAvail`
/// RFC 9608 §3 forbids, each issuer below the anchor a certificate authority, and the
/// path below the anchor within the name constraints and policy requirements its
/// authorities set ([`path::constrain`]).
fn trust(walk: &Walk, time: SystemTime) -> Result<(), String> {
    let (last, below) = walk.path.split_last().ok_or("the chain is empty")?;
    match walk.end {
        End::Missing => {
            return Err(format!(
                "the certificate that issued {} is missing",
                name(last)
            ));
        }
        End::Root => {
            return Err(format!(
                "the chain ends at {}, which is not a trusted root",
                name(last)
            ));
        }
        End::Anchor => {}
    }
    let below_anchor = walk.path.len().saturating_sub(2);
    for (index, (cert, issuer)) in links(&walk.path).enumerate() {
        if !valid_at(cert, time) {
            return Err(format!("{} was not valid at {}", name(cert), iso(time)));
        }
        if let Some(id) = unprocessed(cert) {
            return Err(format!(
                "{} carries critical extension {id}, which is not processed here",
                name(cert)
            ));
        }
        if let Some(misuse) = no_rev_avail_misuse(cert) {
            return Err(format!(
                "{} is invalid: it says its issuer publishes no revocation information, yet {misuse} (RFC 9608 section 3)",
                name(cert)
            ));
        }
        if let Some(reason) = signed_by(cert, issuer)? {
            return Err(format!("{}: {reason}", name(cert)));
        }
        if index < below_anchor {
            authority(issuer, index)?;
        }
    }
    path::constrain(below)
}

/// Whether `cert` may issue the chain below it: a certificate authority allowed to sign
/// certificates, with `below` authorities under it within its path length.
fn authority(cert: &Certificate, below: usize) -> Result<(), String> {
    let constraints = cert
        .tbs_certificate
        .get::<BasicConstraints>()
        .ok()
        .flatten()
        .map(|(_, constraints)| constraints);
    match constraints {
        Some(constraints) if constraints.ca => {
            if constraints
                .path_len_constraint
                .is_some_and(|max| below > usize::from(max))
            {
                return Err(format!(
                    "{} allows fewer certificate authorities below it",
                    name(cert)
                ));
            }
        }
        _ => return Err(format!("{} is not a certificate authority", name(cert))),
    }
    if usage(cert).is_some_and(|usage| !usage.key_cert_sign()) {
        return Err(format!("{} may not sign certificates", name(cert)));
    }
    Ok(())
}

/// What revocation information says about one certificate at the time judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Good,
    /// Revoked at this time, no later than the time judged.
    Revoked(SystemTime),
}

/// One certificate's revocation answer.
enum Answer {
    Good {
        online: bool,
    },
    Revoked {
        at: SystemTime,
        online: bool,
    },
    /// Nothing usable shows it was not revoked; why.
    Unknown(String),
}

impl Answer {
    fn of(status: Status, online: bool) -> Answer {
        match status {
            Status::Good => Answer::Good { online },
            Status::Revoked(at) => Answer::Revoked { at, online },
        }
    }
}

/// The revocation answer for `cert`, issued by `issuer`, at `time`: the store's OCSP
/// responses, then its CRLs, then, when online, the certificate's own responders.
/// `others` may hold a delegated OCSP responder's certificate; an answer a delegated
/// responder signed counts once [`vouch`] accepts the responder, which it never does
/// with `delegates` false, for the answer about a responder itself.
fn answer(
    cert: &Certificate,
    issuer: &Certificate,
    others: &[Certificate],
    store: Option<&Store>,
    context: &Context<'_>,
    time: SystemTime,
    delegates: bool,
) -> Answer {
    let mut failures = Vec::new();
    if let Some(store) = store {
        for response in &store.ocsps {
            let about = response
                .tbs_response_data
                .responses
                .iter()
                .any(|single| names(&single.cert_id, cert, issuer));
            if !about {
                continue;
            }
            let vouched = ocsp_status(response, cert, issuer, others, time, context.now).and_then(
                |(status, responder)| {
                    let online = vouch(
                        responder.as_ref(),
                        issuer,
                        others,
                        Some(store),
                        context,
                        time,
                        delegates,
                    )?;
                    Ok((status, online))
                },
            );
            match vouched {
                Ok((status, online)) => return Answer::of(status, online),
                Err(error) => failures.push(format!("the stored OCSP response: {error}")),
            }
        }
        let lists = store
            .crls
            .iter()
            .filter(|crl| crl.tbs_cert_list.issuer == issuer.tbs_certificate.subject);
        for crl in lists {
            match crl_status(crl, cert, issuer, time, context.now) {
                Ok(status) => return Answer::of(status, false),
                Err(error) => failures.push(format!("the stored CRL: {error}")),
            }
        }
    }
    if let Some(http) = context.online {
        let fetched = fetch_status(http, cert, issuer, time, context.now).and_then(|live| {
            let Some(live) = live else {
                return Ok(None);
            };
            if let Evidence::Ocsp(_, responder) = &live.evidence {
                vouch(
                    responder.as_deref(),
                    issuer,
                    others,
                    store,
                    context,
                    time,
                    delegates,
                )?;
            }
            Ok(Some(live.status))
        });
        match fetched {
            Ok(Some(status)) => return Answer::of(status, true),
            Ok(None) => {}
            Err(error) => failures.push(error),
        }
    }
    if failures.is_empty() {
        let why = if declares(cert) {
            "the document holds no OCSP response or CRL for it; --online asks its responder"
        } else {
            "it names no OCSP responder or CRL, and nothing at hand shows it was not revoked"
        };
        failures.push(why.to_owned());
    }
    Answer::Unknown(failures.join("; "))
}

/// Whether the delegated OCSP `responder` that signed an answer for `issuer`, if one
/// did, may vouch for it; `Ok(true)` when what shows that was fetched live. One that
/// needs no check may (RFC 9608 §4); another only once an answer no delegate signed, or
/// its issuer's CRL, shows it was not revoked at `time` (RFC 6960 §4.2.2.2.1), which
/// `delegates` false refuses to look for.
fn vouch(
    responder: Option<&Certificate>,
    issuer: &Certificate,
    others: &[Certificate],
    store: Option<&Store>,
    context: &Context<'_>,
    time: SystemTime,
    delegates: bool,
) -> Result<bool, String> {
    let Some(responder) = responder.filter(|responder| !exempt(responder)) else {
        return Ok(false);
    };
    if !delegates {
        return Err(format!(
            "it comes from the delegated responder {}, which nothing here checks",
            name(responder)
        ));
    }
    match answer(responder, issuer, others, store, context, time, false) {
        Answer::Good { online } => Ok(online),
        Answer::Revoked { at, .. } => Err(format!(
            "its responder {} was revoked at {}",
            name(responder),
            iso(at)
        )),
        Answer::Unknown(why) => Err(format!(
            "nothing shows its responder {} was not revoked: {why}",
            name(responder)
        )),
    }
}

/// A live answer about one certificate.
struct Live {
    evidence: Evidence,
    status: Status,
}

enum Evidence {
    /// The OCSP response, and the delegated responder's certificate when the issuer did
    /// not sign it.
    Ocsp(Vec<u8>, Option<Box<Certificate>>),
    Crl(Vec<u8>),
}

/// Asks `cert`'s OCSP responders, then its CRL addresses, whether `cert`, issued by
/// `issuer`, was revoked by `time`. `None` when it names neither; an error lists why
/// none answered usably.
fn fetch_status(
    http: &Http,
    cert: &Certificate,
    issuer: &Certificate,
    time: SystemTime,
    now: SystemTime,
) -> Result<Option<Live>, String> {
    let responders = access_urls(cert, rfc5280::ID_AD_OCSP);
    let lists = crl_urls(cert);
    if responders.is_empty() && lists.is_empty() {
        return Ok(None);
    }
    let mut failures = Vec::new();
    for url in &responders {
        match ask_ocsp(http, url, cert, issuer, time, now) {
            Ok(live) => return Ok(Some(live)),
            Err(error) => failures.push(error),
        }
    }
    for url in &lists {
        let fetched = http.get(url).and_then(|der| {
            let crl =
                CertificateList::from_der(&der).map_err(|e| format!("{url}: not a CRL: {e}"))?;
            let status =
                crl_status(&crl, cert, issuer, time, now).map_err(|e| format!("{url}: {e}"))?;
            Ok(Live {
                evidence: Evidence::Crl(der),
                status,
            })
        });
        match fetched {
            Ok(live) => return Ok(Some(live)),
            Err(error) => failures.push(error),
        }
    }
    Err(failures.join("; "))
}

/// The answer of the OCSP responder at `url` about `cert`, issued by `issuer`, checked as
/// [`ocsp_status`] does and against the request's nonce when the responder echoes it.
fn ask_ocsp(
    http: &Http,
    url: &str,
    cert: &Certificate,
    issuer: &Certificate,
    time: SystemTime,
    now: SystemTime,
) -> Result<Live, String> {
    let nonce = rand::random::<[u8; 16]>();
    let query = OcspRequestBuilder::default()
        .with_request(Request::from_cert::<Sha1>(issuer, cert).map_err(|e| e.to_string())?)
        .with_extension(Nonce::new(nonce).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .build()
        .to_der()
        .map_err(|e| e.to_string())?;
    let der = http.post(url, "application/ocsp-request", &query)?;
    let response = basic_response(&der).map_err(|e| format!("{url}: {e}"))?;
    if response
        .nonce()
        .is_some_and(|echo| echo.0.as_bytes() != nonce.as_slice())
    {
        return Err(format!(
            "{url} answered a different OCSP request (nonce mismatch)"
        ));
    }
    let (status, responder) =
        ocsp_status(&response, cert, issuer, &[], time, now).map_err(|e| format!("{url}: {e}"))?;
    Ok(Live {
        evidence: Evidence::Ocsp(der, responder.map(Box::new)),
        status,
    })
}

/// The basic response an OCSP response carries.
fn basic_response(der: &[u8]) -> Result<BasicOcspResponse, String> {
    let response = OcspResponse::from_der(der).map_err(|e| format!("not an OCSP response: {e}"))?;
    if response.response_status != OcspResponseStatus::Successful {
        return Err(format!(
            "the responder refused the request ({:?})",
            response.response_status
        ));
    }
    let bytes = response
        .response_bytes
        .ok_or("the OCSP response is empty")?;
    if bytes.response_type != rfc6960::ID_PKIX_OCSP_BASIC {
        return Err(format!(
            "the OCSP response type {} is not the basic one",
            bytes.response_type
        ));
    }
    BasicOcspResponse::from_der(bytes.response.as_bytes())
        .map_err(|e| format!("the basic OCSP response: {e}"))
}

/// What `response` says of `cert`, issued by `issuer`, at `time`, once its signature,
/// its responder's authority, and its currency check out; with the delegated responder's
/// certificate when the issuer did not sign it. `others` may hold that certificate.
fn ocsp_status(
    response: &BasicOcspResponse,
    cert: &Certificate,
    issuer: &Certificate,
    others: &[Certificate],
    time: SystemTime,
    now: SystemTime,
) -> Result<(Status, Option<Certificate>), String> {
    let data = &response.tbs_response_data;
    let single = data
        .responses
        .iter()
        .find(|single| names(&single.cert_id, cert, issuer))
        .ok_or("the OCSP response is about another certificate")?;
    let responder = responder(response, issuer, others)?;
    let signer = responder.as_ref().unwrap_or(issuer);
    let tbs = data.to_der().map_err(|e| e.to_string())?;
    within_policy(
        "the OCSP response",
        verify_signed(
            &response.signature_algorithm,
            &signer.tbs_certificate.subject_public_key_info,
            &tbs,
            response.signature.raw_bytes(),
        ),
    )?;
    current(
        single.this_update.0.to_system_time(),
        single.next_update.map(|next| next.0.to_system_time()),
        time,
        now,
    )?;
    let status = match &single.cert_status {
        CertStatus::Good(_) => Status::Good,
        CertStatus::Revoked(info) => revoked_by(info.revocation_time.0.to_system_time(), time),
        CertStatus::Unknown(_) => {
            return Err("the OCSP responder does not know the certificate".to_owned());
        }
    };
    Ok((status, responder))
}

/// The certificate that signed `response` for `issuer`: `None` for the issuer itself,
/// else a responder the issuer authorised for OCSP signing that was valid when it
/// answered, from the response or `others`.
fn responder(
    response: &BasicOcspResponse,
    issuer: &Certificate,
    others: &[Certificate],
) -> Result<Option<Certificate>, String> {
    let id = &response.tbs_response_data.responder_id;
    if identifies(id, issuer) {
        return Ok(None);
    }
    let delegate = response
        .certs
        .iter()
        .flatten()
        .chain(others)
        .find(|cert| identifies(id, cert))
        .ok_or("the OCSP response is signed by a responder it does not include")?;
    // A signature that verifies yet falls short of the policy authorises nothing.
    let authorised = delegate.tbs_certificate.issuer == issuer.tbs_certificate.subject
        && matches!(signed_by(delegate, issuer), Ok(None));
    if !authorised {
        return Err("the OCSP responder was not authorised by the certificate's issuer".to_owned());
    }
    if !extended_usage(delegate).contains(&rfc5280::ID_KP_OCSP_SIGNING) {
        return Err("the OCSP responder's certificate does not allow OCSP signing".to_owned());
    }
    if let Some(id) = unprocessed(delegate) {
        return Err(format!(
            "the OCSP responder's certificate carries critical extension {id}, which is not processed here"
        ));
    }
    if !valid_at(
        delegate,
        response.tbs_response_data.produced_at.0.to_system_time(),
    ) {
        return Err("the OCSP responder's certificate was not valid when it answered".to_owned());
    }
    Ok(Some(delegate.clone()))
}

/// Whether `id` names the holder of `cert`.
fn identifies(id: &ResponderId, cert: &Certificate) -> bool {
    match id {
        ResponderId::ByName(name) => *name == cert.tbs_certificate.subject,
        ResponderId::ByKey(hash) => Sha1::digest(key_bits(cert)).as_slice() == hash.as_bytes(),
    }
}

/// Whether `id` names `cert` as issued by `issuer`: the same serial, and the hashes of
/// the issuer's name and key under `id`'s algorithm.
fn names(id: &CertId, cert: &Certificate, issuer: &Certificate) -> bool {
    let Some(hash) = Hash::from_oid(&id.hash_algorithm.oid) else {
        return false;
    };
    id.serial_number == cert.tbs_certificate.serial_number
        && issuer
            .tbs_certificate
            .subject
            .to_der()
            .is_ok_and(|name| hash.digest_bytes(&name) == id.issuer_name_hash.as_bytes())
        && hash.digest_bytes(key_bits(issuer)) == id.issuer_key_hash.as_bytes()
}

/// What the CRL `crl` says of `cert`, issued by `issuer`, at `time`, once its issuer,
/// signature, and currency check out.
fn crl_status(
    crl: &CertificateList,
    cert: &Certificate,
    issuer: &Certificate,
    time: SystemTime,
    now: SystemTime,
) -> Result<Status, String> {
    let list = &crl.tbs_cert_list;
    if list.issuer != issuer.tbs_certificate.subject {
        return Err("the CRL is from another issuer".to_owned());
    }
    if usage(issuer).is_some_and(|usage| !usage.crl_sign()) {
        return Err("the issuer may not sign CRLs".to_owned());
    }
    let tbs = list.to_der().map_err(|e| e.to_string())?;
    within_policy(
        "the CRL",
        verify_signed(
            &crl.signature_algorithm,
            &issuer.tbs_certificate.subject_public_key_info,
            &tbs,
            crl.signature.raw_bytes(),
        ),
    )?;
    current(
        list.this_update.to_system_time(),
        list.next_update.map(|next| next.to_system_time()),
        time,
        now,
    )?;
    Ok(list
        .revoked_certificates
        .iter()
        .flatten()
        .find(|entry| entry.serial_number == cert.tbs_certificate.serial_number)
        .map_or(Status::Good, |entry| {
            revoked_by(entry.revocation_date.to_system_time(), time)
        }))
}

/// A revocation at `at` as of `time`: one after `time` leaves the certificate good then.
fn revoked_by(at: SystemTime, time: SystemTime) -> Status {
    if at <= time {
        Status::Revoked(at)
    } else {
        Status::Good
    }
}

/// Whether revocation information issued at `this_update`, current until `next_update`,
/// speaks for `time`: it is not from the future, and it was issued after `time` (a later
/// good answer shows the certificate was not revoked then) or was still current at
/// `time`.
fn current(
    this_update: SystemTime,
    next_update: Option<SystemTime>,
    time: SystemTime,
    now: SystemTime,
) -> Result<(), String> {
    if this_update > now + CLOCK_SKEW {
        return Err(format!(
            "it was issued in the future, at {}",
            iso(this_update)
        ));
    }
    let until = next_update.unwrap_or(this_update + NO_NEXT_UPDATE);
    if this_update >= time || until >= time {
        Ok(())
    } else {
        Err(format!(
            "it was current only until {}, before {}",
            iso(until),
            iso(time)
        ))
    }
}

/// `check`, an X.509 signature verdict from [`verify_signed`], as a failure of `what`
/// when the signature does not verify or falls short of the policy.
fn within_policy(what: &str, check: Result<Option<&'static str>, String>) -> Result<(), String> {
    match check {
        Ok(None) => Ok(()),
        Ok(Some(reason)) => Err(format!("{what}: {reason}")),
        Err(error) => Err(format!("{what}: {error}")),
    }
}

/// Whether `issuer` signed `cert`; fails when the signature does not verify, and
/// otherwise returns why it falls short of the policy, if it does.
fn signed_by(cert: &Certificate, issuer: &Certificate) -> Result<Option<&'static str>, String> {
    let tbs = cert.tbs_certificate.to_der().map_err(|e| e.to_string())?;
    verify_signed(
        &cert.signature_algorithm,
        &issuer.tbs_certificate.subject_public_key_info,
        &tbs,
        cert.signature.raw_bytes(),
    )
}

/// The certificate in `certs` that issued `cert`: named as its issuer, with the key its
/// signature verifies under.
fn issuer_in<'a>(certs: &'a [Certificate], cert: &Certificate) -> Option<&'a Certificate> {
    certs.iter().find(|candidate| {
        candidate.tbs_certificate.subject == cert.tbs_certificate.issuer
            && signed_by(cert, candidate).is_ok()
    })
}

/// Each certificate of `path` with its issuer, the next one.
fn links(path: &[Certificate]) -> impl Iterator<Item = (&Certificate, &Certificate)> {
    path.iter().zip(path.iter().skip(1))
}

/// The same subject with the same key: an anchor matches a certificate re-issued for it.
fn same_key(anchor: &Certificate, cert: &Certificate) -> bool {
    anchor.tbs_certificate.subject == cert.tbs_certificate.subject
        && anchor.tbs_certificate.subject_public_key_info
            == cert.tbs_certificate.subject_public_key_info
}

fn valid_at(cert: &Certificate, time: SystemTime) -> bool {
    let validity = &cert.tbs_certificate.validity;
    validity.not_before.to_system_time() <= time && time <= validity.not_after.to_system_time()
}

/// The certificate names an OCSP responder or a CRL this crate can fetch.
fn declares(cert: &Certificate) -> bool {
    !access_urls(cert, rfc5280::ID_AD_OCSP).is_empty() || !crl_urls(cert).is_empty()
}

/// RFC 9608 §4: a certificate that carries `id-pkix-ocsp-nocheck` (RFC 6960 §4.2.2.2.1),
/// or a `noRevAvail` that §3 allows, needs no revocation check.
fn exempt(cert: &Certificate) -> bool {
    has_extension(cert, rfc6960::ID_PKIX_OCSP_NOCHECK)
        || (has_extension(cert, ID_CE_NO_REV_AVAIL) && no_rev_avail_misuse(cert).is_none())
}

/// What makes the `noRevAvail` of `cert`, when it carries one, invalid under RFC 9608
/// §3: a certificate authority may not carry it, nor may a certificate that names CRLs or
/// an OCSP responder.
fn no_rev_avail_misuse(cert: &Certificate) -> Option<&'static str> {
    if !has_extension(cert, ID_CE_NO_REV_AVAIL) {
        return None;
    }
    let authority = cert
        .tbs_certificate
        .get::<BasicConstraints>()
        .ok()
        .flatten()
        .is_some_and(|(_, constraints)| constraints.ca);
    let responder = cert
        .tbs_certificate
        .get::<AuthorityInfoAccessSyntax>()
        .ok()
        .flatten()
        .is_some_and(|(_, access)| {
            access
                .0
                .iter()
                .any(|description| description.access_method == rfc5280::ID_AD_OCSP)
        });
    if authority {
        Some("it is a certificate authority")
    } else if has_extension(cert, rfc5280::ID_CE_CRL_DISTRIBUTION_POINTS)
        || has_extension(cert, rfc5280::ID_CE_FRESHEST_CRL)
    {
        Some("it names CRLs")
    } else if responder {
        Some("it names an OCSP responder")
    } else {
        None
    }
}

/// The first critical extension of `cert` outside [`PROCESSED`].
fn unprocessed(cert: &Certificate) -> Option<ObjectIdentifier> {
    cert.tbs_certificate
        .extensions
        .iter()
        .flatten()
        .find(|extension| extension.critical && !PROCESSED.contains(&extension.extn_id))
        .map(|extension| extension.extn_id)
}

fn has_extension(cert: &Certificate, id: ObjectIdentifier) -> bool {
    cert.tbs_certificate
        .extensions
        .iter()
        .flatten()
        .any(|extension| extension.extn_id == id)
}

/// The HTTP addresses the certificate's authority information access gives for `method`.
fn access_urls(cert: &Certificate, method: ObjectIdentifier) -> Vec<String> {
    let Ok(Some((_, access))) = cert.tbs_certificate.get::<AuthorityInfoAccessSyntax>() else {
        return Vec::new();
    };
    access
        .0
        .iter()
        .filter(|description| description.access_method == method)
        .filter_map(|description| http_url(&description.access_location))
        .collect()
}

/// The HTTP addresses of the certificate's CRL distribution points.
fn crl_urls(cert: &Certificate) -> Vec<String> {
    let Ok(Some((_, points))) = cert.tbs_certificate.get::<CrlDistributionPoints>() else {
        return Vec::new();
    };
    points
        .0
        .iter()
        .filter_map(|point| match &point.distribution_point {
            Some(DistributionPointName::FullName(names)) => Some(names),
            _ => None,
        })
        .flatten()
        .filter_map(http_url)
        .collect()
}

fn http_url(name: &GeneralName) -> Option<String> {
    match name {
        GeneralName::UniformResourceIdentifier(uri) => {
            let uri = uri.as_str();
            (uri.starts_with("http://") || uri.starts_with("https://")).then(|| uri.to_owned())
        }
        _ => None,
    }
}

fn usage(cert: &Certificate) -> Option<KeyUsage> {
    cert.tbs_certificate
        .get::<KeyUsage>()
        .ok()
        .flatten()
        .map(|(_, usage)| usage)
}

fn extended_usage(cert: &Certificate) -> Vec<ObjectIdentifier> {
    cert.tbs_certificate
        .get::<ExtendedKeyUsage>()
        .ok()
        .flatten()
        .map(|(_, usage)| usage.0)
        .unwrap_or_default()
}

/// The subject public key bits, which OCSP key hashes digest.
fn key_bits(cert: &Certificate) -> &[u8] {
    cert.tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes()
}

fn name(cert: &Certificate) -> String {
    human_friendly(&cert.tbs_certificate.subject)
}

fn encode(cert: &Certificate) -> Result<Vec<u8>, String> {
    cert.to_der().map_err(|e| e.to_string())
}

/// `time` as ISO 8601 UTC.
fn iso(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
    });
    PdfDate::from_unix(seconds, 0)
        .map_or_else(|| format!("{seconds} s"), |date| tsp::iso_utc(&date))
}

fn hold<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}
