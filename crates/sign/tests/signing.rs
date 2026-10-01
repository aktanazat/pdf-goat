//! `security sign` beyond the demonstration signature, through the CLI surface: PAdES
//! signatures with PKCS#12 identities from an in-test certificate authority verify, carry
//! the PAdES baseline attributes, and `security verify` catches a tampered file or a
//! signature naming another certificate; visible signatures draw inside their box; a
//! loopback time-stamp authority stamps signatures to PAdES B-T; `--ltv` keeps the
//! chains and the answers of the authority's loopback OCSP responder and CRL in the
//! document security store and, with a time-stamp, stamps the document to B-LTA;
//! certification and field locks decide which later changes `security verify` allows;
//! prepared signature fields are signed in place.

mod pki;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedAttributes, SignedData, SignerIdentifier,
    SignerInfo, SignerInfos,
};
use const_oid::ObjectIdentifier;
use const_oid::db::{rfc5911, rfc5912};
use der::asn1::{OctetString, SetOfVec};
use der::{Any, Decode, Encode, SliceReader};
use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::{PdfBuilder, incremental_update};
use pdf_codec::{PngColor, encode_png};
use pdf_core::{Dict, Document, ObjRef, Object, PdfString, SaveOptions};
use pdf_raster::Pixmap;
use pdf_render::{RenderOptions, render_page};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use signature::{SignatureEncoding, Signer};
use spki::AlgorithmIdentifierOwned;
use x509_cert::Certificate;
use x509_cert::attr::Attribute;
use x509_ocsp::{BasicOcspResponse, CertStatus, OcspResponse};

use pki::{Ca, Imprint, Key, Sealing, Services, Tsa, serve};

/// Cargo sets this for every test run; its value is the password of the test identities.
const PASSWORD_ENV: &str = "CARGO_PKG_NAME";
const SIGNER: &str = "CN=Goat Signer,O=Goat Test";
const SIGNER_SUBJECT: &str = "Common Name: Goat Signer, Organization: Goat Test";

fn password() -> String {
    std::env::var(PASSWORD_ENV).expect("cargo sets CARGO_PKG_NAME for tests")
}

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_sign::register(&mut registry);
    let cli = registry.into_cli(clap::Command::new("pdf-goat"), &["security"], &[]);
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let invocation = cli
        .parse(&args)
        .unwrap_or_else(|failure| panic!("{failure:?}"));
    invocation.run(&Ctx::new(home))
}

fn plain_pdf(dir: &Path) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text(72.0, 700.0, "Hello, signatures");
    builder
        .save(dir.join("plain.pdf"))
        .to_string_lossy()
        .into_owned()
}

/// A PKCS#12 file with a fresh certificate for `key` issued by `ca`, and the certificate.
fn identity(dir: &Path, ca: &Ca, key: &Key, sealing: Sealing) -> (String, Certificate) {
    let cert = ca.issue(SIGNER, key, |_| {});
    let path = dir.join("signer.p12");
    fs::write(
        &path,
        pki::p12(key, &cert, &[&ca.cert], &password(), sealing),
    )
    .expect("p12");
    (path.to_string_lossy().into_owned(), cert)
}

fn try_sign(
    dir: &Path,
    src: &str,
    p12: &str,
    extra: &[&str],
) -> Result<Map<String, Value>, GoatError> {
    let mut args = vec![
        "security",
        "sign",
        src,
        "--p12",
        p12,
        "--password-env",
        PASSWORD_ENV,
    ];
    args.extend_from_slice(extra);
    run(dir, &args)
}

fn sign(dir: &Path, src: &str, p12: &str, extra: &[&str]) -> Map<String, Value> {
    try_sign(dir, src, p12, extra).expect("sign succeeds")
}

fn output(result: &Map<String, Value>) -> String {
    result["outputs"][0]
        .as_str()
        .expect("one output")
        .to_owned()
}

fn only_signature(dir: &Path, file: &str) -> Value {
    let verified = run(dir, &["security", "verify", file]).expect("verify succeeds");
    assert_eq!(verified["signature_count"], 1);
    verified["signatures"][0].clone()
}

/// The `/ByteRange` of the file's only signature.
fn byte_range(data: &[u8]) -> [usize; 4] {
    let at = data
        .windows(10)
        .position(|w| w == b"/ByteRange")
        .expect("a /ByteRange");
    let open = at + data[at..].iter().position(|&b| b == b'[').expect("[");
    let close = open + data[open..].iter().position(|&b| b == b']').expect("]");
    let values: Vec<usize> = std::str::from_utf8(&data[open + 1..close])
        .expect("ASCII")
        .split_whitespace()
        .map(|v| v.parse().expect("offset"))
        .collect();
    values.try_into().expect("four offsets")
}

/// The signature dictionary of the file's only signature field.
fn signature_dict(data: &[u8]) -> Dict {
    let doc = Document::load(data.to_vec()).expect("load");
    let catalog = doc.catalog().expect("catalog");
    let form = doc
        .resolve_dict(catalog.get(b"AcroForm").expect("AcroForm"))
        .expect("resolve")
        .expect("form dict");
    let fields = doc
        .resolve_array(form.get(b"Fields").expect("Fields"))
        .expect("resolve")
        .expect("fields");
    let field = doc
        .resolve_dict(&fields[0])
        .expect("resolve")
        .expect("field");
    doc.resolve_dict(field.get(b"V").expect("V"))
        .expect("resolve")
        .expect("signature dict")
}

/// The CMS signed data of the file's only signature.
fn signed_data(data: &[u8]) -> SignedData {
    let [_, start, end, _] = byte_range(data);
    let hex = &data[start + 1..end - 1];
    let der: Vec<u8> = hex
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex"), 16).expect("hex digit")
        })
        .collect();
    let mut reader = SliceReader::new(&der).expect("reader");
    let info = ContentInfo::decode(&mut reader).expect("ContentInfo");
    info.content.decode_as().expect("SignedData")
}

fn signer_info(data: &[u8]) -> SignerInfo {
    signed_data(data).signer_infos.0.as_slice()[0].clone()
}

#[test]
fn every_supported_identity_signs_with_its_algorithm_and_verifies() {
    let ca = Ca::new("CN=Goat Test Root,O=Goat Test");
    let cases = [
        (
            Key::rsa(),
            Sealing::Modern,
            false,
            rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
        ),
        (Key::rsa(), Sealing::Legacy, true, rfc5912::ID_RSASSA_PSS),
        (
            Key::p256(),
            Sealing::Legacy,
            false,
            rfc5912::ECDSA_WITH_SHA_256,
        ),
        (
            Key::p384(),
            Sealing::Modern,
            false,
            rfc5912::ECDSA_WITH_SHA_384,
        ),
    ];
    for (key, sealing, pss, algorithm) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = plain_pdf(dir.path());
        let (p12, _) = identity(dir.path(), &ca, &key, sealing);
        let extra: &[&str] = if pss { &["--pss"] } else { &[] };
        let signed = sign(dir.path(), &src, &p12, extra);
        assert_eq!(signed["signer"], "Goat Signer", "{algorithm}");
        assert_eq!(signed["self_signed"], false, "{algorithm}");
        assert_eq!(signed["pades_level"], "B-B", "{algorithm}");
        let out = output(&signed);
        let used = signer_info(&fs::read(&out).expect("signed file")).signature_algorithm;
        assert_eq!(used.oid, algorithm);

        let sig = only_signature(dir.path(), &out);
        assert_eq!(sig["signer"], SIGNER_SUBJECT, "{algorithm}");
        assert_eq!(sig["intact"], true, "{algorithm}");
        assert_eq!(sig["valid"], true, "{algorithm}");
        assert_eq!(sig["trusted"], true, "{algorithm}");
        assert_eq!(
            sig["coverage"], "SignatureCoverageLevel.ENTIRE_FILE",
            "{algorithm}"
        );
    }
}

#[test]
fn a_pades_signature_is_cades_detached_with_the_time_in_the_dictionary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let out = output(&sign(dir.path(), &src, &p12, &[]));
    let data = fs::read(&out).expect("signed file");

    let dict = signature_dict(&data);
    assert_eq!(
        dict.get_name(b"SubFilter"),
        Some(&b"ETSI.CAdES.detached"[..])
    );
    let date = dict
        .get(b"M")
        .and_then(Object::as_string)
        .expect("/M")
        .to_text();
    assert!(date.starts_with("D:20"), "a PDF date: {date}");

    let attrs: Vec<ObjectIdentifier> = signer_info(&data)
        .signed_attrs
        .expect("signed attributes")
        .iter()
        .map(|attr| attr.oid)
        .collect();
    for required in [
        rfc5911::ID_CONTENT_TYPE,
        rfc5911::ID_MESSAGE_DIGEST,
        rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2,
    ] {
        assert!(attrs.contains(&required), "{required} in {attrs:?}");
    }
    assert!(
        !attrs.contains(&rfc5911::ID_SIGNING_TIME),
        "PAdES keeps the claimed time out of the signed attributes: {attrs:?}"
    );
}

#[test]
fn tampering_with_a_pades_signed_file_breaks_integrity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p384(), Sealing::Modern);
    let out = output(&sign(dir.path(), &src, &p12, &["--reason", "Untouched"]));
    let mut data = fs::read(&out).expect("signed file");
    let at = data
        .windows(9)
        .position(|w| w == b"Untouched")
        .expect("the reason is stored plain");
    data[at] = b'A';
    fs::write(&out, data).expect("rewrite");

    let sig = only_signature(dir.path(), &out);
    assert_eq!(sig["intact"], false);
    assert_eq!(sig["valid"], true);
    assert_eq!(sig["trusted"], false);
}

#[test]
fn a_wrong_p12_password_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let error = run(
        dir.path(),
        &[
            "security",
            "sign",
            &src,
            "--p12",
            &p12,
            "--password-env",
            "CARGO_PKG_VERSION",
        ],
    )
    .expect_err("the password is wrong");
    assert_eq!(
        error,
        GoatError::message("wrong password for the PKCS#12 file (its integrity check failed)")
    );
}

/// ESS `ESSCertIDv2` with the default SHA-256 and no issuer serial.
#[derive(der::Sequence)]
struct EssCertIdV2 {
    cert_hash: OctetString,
}

/// ESS `SigningCertificateV2`.
#[derive(der::Sequence)]
struct SigningCertificateV2 {
    certs: Vec<EssCertIdV2>,
}

fn attribute(oid: ObjectIdentifier, value: Any) -> Attribute {
    Attribute {
        oid,
        values: SetOfVec::try_from(vec![value]).expect("one value"),
    }
}

#[test]
fn a_signing_certificate_attribute_naming_another_certificate_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let key = Key::p256();
    let (p12, cert) = identity(dir.path(), &ca, &key, Sealing::Modern);
    let out = output(&sign(dir.path(), &src, &p12, &[]));
    let mut data = fs::read(&out).expect("signed file");

    // Re-sign the same bytes with the signer's own key, but name the CA's certificate in
    // the signing-certificate attribute: the signature itself verifies.
    let [_, start, end, tail] = byte_range(&data);
    let mut hasher = Sha256::new();
    hasher.update(&data[..start]);
    hasher.update(&data[end..end + tail]);
    let digest = hasher.finalize().to_vec();
    let other = SigningCertificateV2 {
        certs: vec![EssCertIdV2 {
            cert_hash: OctetString::new(Sha256::digest(ca.cert.to_der().expect("CA DER")).to_vec())
                .expect("hash"),
        }],
    };
    let mut attrs = SignedAttributes::new();
    for (oid, value) in [
        (
            rfc5911::ID_CONTENT_TYPE,
            Any::encode_from(&rfc5911::ID_DATA).expect("content type"),
        ),
        (
            rfc5911::ID_MESSAGE_DIGEST,
            Any::encode_from(&OctetString::new(digest).expect("digest")).expect("digest"),
        ),
        (
            rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2,
            Any::encode_from(&other).expect("ESS"),
        ),
    ] {
        attrs.insert(attribute(oid, value)).expect("attribute");
    }
    let Key::P256(secret) = &key else {
        unreachable!("a P-256 key")
    };
    let signature: p256::ecdsa::DerSignature =
        p256::ecdsa::SigningKey::from(secret).sign(&attrs.to_der().expect("attributes DER"));
    let sha256 = AlgorithmIdentifierOwned {
        oid: rfc5912::ID_SHA_256,
        parameters: None,
    };
    let info = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: cert.tbs_certificate.issuer.clone(),
            serial_number: cert.tbs_certificate.serial_number.clone(),
        }),
        digest_alg: sha256.clone(),
        signed_attrs: Some(attrs),
        signature_algorithm: AlgorithmIdentifierOwned {
            oid: rfc5912::ECDSA_WITH_SHA_256,
            parameters: None,
        },
        signature: OctetString::new(signature.to_vec()).expect("signature"),
        unsigned_attrs: None,
    };
    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![sha256]).expect("digest algorithms"),
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: rfc5911::ID_DATA,
            econtent: None,
        },
        certificates: Some(CertificateSet(
            SetOfVec::try_from(vec![CertificateChoices::Certificate(cert.clone())])
                .expect("certificates"),
        )),
        crls: None,
        signer_infos: SignerInfos(SetOfVec::try_from(vec![info]).expect("signer infos")),
    };
    let cms = ContentInfo {
        content_type: rfc5911::ID_SIGNED_DATA,
        content: Any::encode_from(&signed).expect("signed data"),
    }
    .to_der()
    .expect("CMS DER");
    let slot = &mut data[start + 1..end - 1];
    slot.fill(b'0');
    for (pair, byte) in slot.as_chunks_mut::<2>().0.iter_mut().zip(&cms) {
        *pair = format!("{byte:02x}")
            .into_bytes()
            .try_into()
            .expect("two digits");
    }
    fs::write(&out, data).expect("rewrite");

    let sig = only_signature(dir.path(), &out);
    assert_eq!(
        sig["validation_error"],
        format!(
            "Signing certificate attribute does not match selected signer's certificate for subject\"{SIGNER_SUBJECT}\"."
        )
    );
}

/// Two letter pages, each with a line of text.
fn two_page_pdf(dir: &Path) -> String {
    let mut builder = PdfBuilder::new();
    builder.page(612.0, 792.0).text(72.0, 700.0, "Cover page");
    builder
        .page(612.0, 792.0)
        .text(72.0, 700.0, "Hello, signatures");
    builder
        .save(dir.join("two-pages.pdf"))
        .to_string_lossy()
        .into_owned()
}

/// Page `index` of `file` at one pixel per point, with annotations.
fn render(file: &str, index: usize) -> Pixmap {
    let doc = Document::load(fs::read(file).expect("file")).expect("load");
    let page = doc.page(index).expect("page");
    render_page(&doc, &page, &RenderOptions::default()).expect("render")
}

/// Every pixel, as (x, y), where `after` differs from `before`.
fn changed(before: &Pixmap, after: &Pixmap) -> Vec<(u32, u32)> {
    assert_eq!(
        (before.width(), before.height()),
        (after.width(), after.height())
    );
    let mut pixels = Vec::new();
    for y in 0..before.height() {
        for x in 0..before.width() {
            if before.pixel(x, y) != after.pixel(x, y) {
                pixels.push((x, y));
            }
        }
    }
    pixels
}

#[test]
fn a_visible_signature_draws_its_text_across_its_box_on_its_page_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = two_page_pdf(dir.path());
    let signed = run(
        dir.path(),
        &[
            "security",
            "sign",
            &src,
            "--page",
            "2",
            "--rect",
            "300,600,500,650",
            "--appearance-text",
            "Approved by Goat",
        ],
    )
    .expect("sign succeeds");
    assert_eq!(signed["visible"], true);
    let out = output(&signed);
    assert_eq!(changed(&render(&src, 0), &render(&out, 0)), []);

    let drawn = changed(&render(&src, 1), &render(&out, 1));
    let (xs, ys): (Vec<u32>, Vec<u32>) = drawn.into_iter().unzip();
    let (left, right) = (
        xs.iter().min().expect("drawn"),
        xs.iter().max().expect("drawn"),
    );
    let (top, bottom) = (
        ys.iter().min().expect("drawn"),
        ys.iter().max().expect("drawn"),
    );
    assert!(
        *left >= 300 && *right < 500 && *top >= 600 && *bottom < 650,
        "inside the box: x {left}..={right}, y {top}..={bottom}"
    );
    assert!(
        right - left > 150,
        "the text is sized to the box's width: x {left}..={right}"
    );
}

#[test]
fn an_appearance_image_is_centred_in_its_box_keeping_its_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let red = dir.path().join("red.png");
    let png = encode_png(&[255, 0, 0].repeat(8), 4, 2, PngColor::Rgb, None).expect("PNG");
    fs::write(&red, png).expect("write PNG");
    let signed = run(
        dir.path(),
        &[
            "security",
            "sign",
            &src,
            "--rect",
            "100,100,300,150",
            "--appearance-image",
            red.to_str().expect("UTF-8 path"),
        ],
    )
    .expect("sign succeeds");
    let page = render(&output(&signed), 0);
    // The 2:1 image fills the height of the 196 × 46 space inside the box's 2 pt padding:
    // 92 × 46, centred on x 200.
    for x in [156, 200, 244] {
        assert_eq!(page.pixel(x, 125), Some([255, 0, 0, 255]), "x {x}");
    }
    for x in [110, 150, 250, 290] {
        assert_eq!(page.pixel(x, 125), Some([255, 255, 255, 255]), "x {x}");
    }
}

#[test]
fn signature_text_helvetica_cannot_show_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let error = run(
        dir.path(),
        &[
            "security",
            "sign",
            &src,
            "--rect",
            "100,100,300,150",
            "--appearance-text",
            "✔ Approved",
        ],
    )
    .expect_err("Helvetica has no check mark");
    assert_eq!(
        error,
        GoatError::message(
            "the signature text uses characters Helvetica cannot show: ✔ (U+2714); give --appearance-text without them"
        )
    );
}

/// A loopback time-stamp authority the root `ca` certified, stamping as `imprint` says,
/// and its URL.
fn tsa_server(ca: &Ca, imprint: Imprint) -> (Arc<Tsa>, String) {
    serve_tsa(Tsa::new(ca), imprint)
}

/// `tsa` stamping on a loopback port as `imprint` says, and its URL.
fn serve_tsa(tsa: Tsa, imprint: Imprint) -> (Arc<Tsa>, String) {
    let tsa = Arc::new(tsa);
    let serving = Arc::clone(&tsa);
    let url = serve(move |_, query| serving.reply(query, imprint));
    (tsa, url)
}

#[test]
fn a_time_stamped_signature_carries_the_authority_time_and_verifies_as_b_t() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (tsa, url) = tsa_server(&ca, Imprint::Asked);
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let signed = sign(dir.path(), &src, &p12, &["--tsa", &url]);
    assert_eq!(signed["pades_level"], "B-T");
    assert_eq!(signed["timestamp"], tsa.iso_time());

    let verified = only_signature(dir.path(), &output(&signed));
    assert_eq!(
        (&verified["intact"], &verified["valid"]),
        (&Value::Bool(true), &Value::Bool(true))
    );
    assert_eq!(verified["timestamp_valid"], true, "{verified}");
    assert_eq!(verified["timestamp"], tsa.iso_time());
    assert_eq!(verified["pades_level"], "B-T");
}

#[test]
fn a_time_stamp_over_another_signature_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (_, url) = tsa_server(&ca, Imprint::Other);
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let out = dir.path().join("stamped.pdf");
    let out = out.to_string_lossy();
    let error = run(
        dir.path(),
        &[
            "security",
            "sign",
            &src,
            "--p12",
            &p12,
            "--password-env",
            PASSWORD_ENV,
            "--tsa",
            &url,
            "-o",
            &out,
        ],
    )
    .expect_err("the token stamps another signature");
    assert_eq!(
        error,
        GoatError::message("the time-stamp does not cover this signature")
    );
    assert!(!Path::new(out.as_ref()).exists(), "no output is written");
}

/// `data` with a document time-stamp from `tsa` in a new revision, as another PAdES tool
/// writes one: an invisible signature field `DocTimeStamp1` whose `/DocTimeStamp` value
/// holds the authority's token over the revision's byte ranges.
fn add_document_time_stamp(data: &[u8], tsa: &Tsa) -> Vec<u8> {
    let mut doc = Document::load(data.to_vec()).expect("load");
    let mut stamp = Dict::new();
    stamp.insert("Type", Object::name("DocTimeStamp"));
    stamp.insert("Filter", Object::name("Adobe.PPKLite"));
    stamp.insert("SubFilter", Object::name("ETSI.RFC3161"));
    stamp.insert("Contents", PdfString::hex(vec![0; 8192]));
    stamp.insert("ByteRange", vec![Object::Integer(9_999_999_999); 4]);
    let stamp_ref = doc.add(stamp);
    let page = doc.page(0).expect("page");
    let mut field = Dict::new();
    field.insert("FT", Object::name("Sig"));
    field.insert("T", Object::text("DocTimeStamp1"));
    field.insert("Type", Object::name("Annot"));
    field.insert("Subtype", Object::name("Widget"));
    field.insert("F", 132);
    field.insert("Rect", vec![Object::Integer(0); 4]);
    field.insert("P", page.id);
    field.insert("V", stamp_ref);
    let field_ref = doc.add(field);
    let mut page_dict = page.dict.clone();
    let mut annots = page_dict
        .get(b"Annots")
        .map(|annots| doc.resolve_array(annots).expect("resolve").expect("annots"))
        .unwrap_or_default();
    annots.push(Object::Reference(field_ref));
    page_dict.insert("Annots", annots);
    doc.set(page.id, page_dict);
    let catalog_ref = doc.catalog_ref().expect("catalog");
    let mut catalog = doc.catalog().expect("catalog");
    let form_value = catalog.get(b"AcroForm").cloned().expect("AcroForm");
    let mut form = doc
        .resolve_dict(&form_value)
        .expect("resolve")
        .expect("form");
    let mut fields = doc
        .resolve_array(form.get(b"Fields").expect("Fields"))
        .expect("resolve")
        .expect("fields");
    fields.push(Object::Reference(field_ref));
    form.insert("Fields", fields);
    match form_value.as_reference() {
        Some(id) => doc.set(id, form),
        None => {
            catalog.insert("AcroForm", form);
            doc.set(catalog_ref, catalog);
        }
    }
    let saved = doc.save_incremental(false).expect("incremental save");
    let span = saved
        .signatures
        .iter()
        .find(|span| span.id == stamp_ref)
        .expect("the time-stamp span")
        .clone();
    let mut out = saved.data;
    let contents = span.contents;
    let ranges = format!(
        "[0 {} {} {}]",
        contents.start,
        contents.end,
        out.len() - contents.end
    );
    let slot = span.byte_range.expect("the /ByteRange span");
    let mut ranges = ranges.into_bytes();
    ranges.resize(slot.len(), b' ');
    out[slot].copy_from_slice(&ranges);
    let mut hasher = Sha256::new();
    hasher.update(&out[..contents.start]);
    hasher.update(&out[contents.end..]);
    let hex: String = tsa
        .token(&hasher.finalize())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    out[contents.start + 1..contents.start + 1 + hex.len()].copy_from_slice(hex.as_bytes());
    out
}

#[test]
fn a_document_time_stamp_is_checked_against_its_revision_and_is_an_archival_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let tsa = Tsa::new(&ca);
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let signed = output(&sign(dir.path(), &src, &p12, &["--reason", "Untouched"]));
    let data = add_document_time_stamp(&fs::read(&signed).expect("signed file"), &tsa);
    let stamped = path_in(dir.path(), "stamped.pdf");
    fs::write(&stamped, &data).expect("write");

    let rows = signature_rows(dir.path(), &stamped);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (signature, stamp) = (&rows[0], &rows[1]);
    assert_eq!(signature["kind"], "signature");
    assert_eq!(signature["intact"], true);
    assert_eq!(signature["modified"], "LTA_UPDATES");
    assert_eq!(signature["changes_allowed"], true);
    assert_eq!(stamp["kind"], "document_timestamp");
    assert_eq!(stamp["field"], "DocTimeStamp1");
    assert_eq!(
        stamp["signer"],
        "Common Name: Goat TSA, Organization: Goat Test"
    );
    assert_eq!(stamp["intact"], true, "{stamp}");
    assert_eq!(stamp["valid"], true, "{stamp}");
    assert_eq!(stamp["trusted"], true, "{stamp}");
    assert_eq!(stamp["coverage"], "SignatureCoverageLevel.ENTIRE_FILE");
    assert_eq!(stamp["timestamp"], tsa.iso_time());
    assert_eq!(stamp["timestamp_valid"], true);

    let mut tampered = data;
    let at = tampered
        .windows(9)
        .position(|w| w == b"Untouched")
        .expect("the reason is stored plain");
    tampered[at] = b'A';
    let tampered_path = path_in(dir.path(), "tampered.pdf");
    fs::write(&tampered_path, tampered).expect("write");
    let stamp = &signature_rows(dir.path(), &tampered_path)[1];
    assert_eq!(stamp["intact"], false, "{stamp}");
    assert_eq!(stamp["timestamp_valid"], false);
    assert_eq!(
        stamp["timestamp_error"],
        "the time-stamp does not cover the signed revision"
    );
}

/// A PKCS#12 file with a fresh P-256 certificate issued by `ca` that names the OCSP
/// responder of `services`, and the certificate.
fn revocable_identity(dir: &Path, ca: &Ca, services: &Services) -> (String, Certificate) {
    let key = Key::p256();
    let cert = ca.issue(SIGNER, &key, |builder| {
        builder
            .add_extension(&services.ocsp_pointer())
            .expect("OCSP pointer");
    });
    let path = dir.join("revocable.p12");
    fs::write(
        &path,
        pki::p12(&key, &cert, &[&ca.cert], &password(), Sealing::Modern),
    )
    .expect("p12");
    (path.to_string_lossy().into_owned(), cert)
}

/// The decoded streams the document security store of `file` lists under `key`.
fn dss_streams(file: &str, key: &str) -> Vec<Vec<u8>> {
    let doc = Document::load(fs::read(file).expect("signed file")).expect("load");
    let catalog = doc.catalog().expect("catalog");
    let dss = doc
        .resolve_dict(catalog.get(b"DSS").expect("a /DSS"))
        .expect("resolve")
        .expect("a /DSS dictionary");
    let items = doc
        .resolve_array(dss.get(key.as_bytes()).expect(key))
        .expect("resolve")
        .expect("an array");
    items
        .iter()
        .map(|item| match doc.resolve(item).expect("resolve") {
            Object::Stream(stream) => doc.decode_stream(&stream).expect("decode").data,
            other => panic!("{key} holds {other:?}"),
        })
        .collect()
}

#[test]
fn ltv_keeps_the_chain_and_the_signers_ocsp_answer_in_an_allowed_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let services = Services::new(&ca);
    let (p12, cert) = revocable_identity(dir.path(), &ca, &services);
    let signed = sign(dir.path(), &src, &p12, &["--ltv"]);
    assert_eq!(signed["pades_level"], "B-B");
    assert_eq!(
        signed["dss"],
        json!({"certificates": 2, "ocsp_responses": 1, "crls": 0, "unchecked": []})
    );

    let out = output(&signed);
    let mut certs = dss_streams(&out, "Certs");
    certs.sort();
    let mut chain = vec![
        cert.to_der().expect("signer DER"),
        ca.cert.to_der().expect("root DER"),
    ];
    chain.sort();
    assert_eq!(certs, chain);
    let ocsps = dss_streams(&out, "OCSPs");
    assert_eq!(ocsps.len(), 1);
    let response = OcspResponse::from_der(&ocsps[0]).expect("an OCSP response");
    let basic = BasicOcspResponse::from_der(
        response
            .response_bytes
            .expect("a basic response")
            .response
            .as_bytes(),
    )
    .expect("a basic response");
    let answers = &basic.tbs_response_data.responses;
    assert_eq!(answers.len(), 1);
    assert_eq!(
        answers[0].cert_id.serial_number,
        cert.tbs_certificate.serial_number
    );
    assert_eq!(answers[0].cert_status, CertStatus::good());

    let verified = only_signature(dir.path(), &out);
    assert_eq!(verified["intact"], true, "{verified}");
    assert_eq!(verified["modified"], "LTA_UPDATES");
    assert_eq!(verified["changes_allowed"], true);
}

#[test]
fn ltv_with_a_time_stamp_stamps_the_document_over_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let services = Services::new(&ca);
    let tsa = Tsa::issued(&ca, |builder| {
        builder
            .add_extension(&services.crl_pointer())
            .expect("CRL pointer");
    });
    let (tsa, url) = serve_tsa(tsa, Imprint::Asked);
    let (p12, _) = revocable_identity(dir.path(), &ca, &services);
    let signed = sign(dir.path(), &src, &p12, &["--tsa", &url, "--ltv"]);
    assert_eq!(signed["pades_level"], "B-LTA");
    assert_eq!(signed["timestamp"], tsa.iso_time());
    assert_eq!(signed["document_timestamp"], tsa.iso_time());
    assert_eq!(
        signed["dss"],
        json!({"certificates": 3, "ocsp_responses": 1, "crls": 1, "unchecked": []})
    );

    let rows = signature_rows(dir.path(), &output(&signed));
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (signature, stamp) = (&rows[0], &rows[1]);
    assert_eq!(signature["intact"], true, "{signature}");
    assert_eq!(signature["modified"], "LTA_UPDATES");
    assert_eq!(signature["timestamp_valid"], true);
    assert_eq!(stamp["kind"], "document_timestamp");
    assert_eq!(stamp["field"], "DocTimeStamp1");
    assert_eq!(stamp["intact"], true, "{stamp}");
    assert_eq!(stamp["timestamp_valid"], true, "{stamp}");
    assert_eq!(stamp["coverage"], "SignatureCoverageLevel.ENTIRE_FILE");
}

#[test]
fn verify_trusts_a_chain_only_once_its_root_is_an_anchor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let out = output(&sign(dir.path(), &src, &p12, &[]));
    let untrusted = only_signature(dir.path(), &out);
    assert_eq!(untrusted["chain_trusted"], false, "{untrusted}");

    let root = path_in(dir.path(), "root.der");
    fs::write(&root, ca.cert.to_der().expect("root DER")).expect("root file");
    let verified =
        run(dir.path(), &["security", "verify", &out, "--trust", &root]).expect("verify succeeds");
    let trusted = &verified["signatures"][0];
    assert_eq!(trusted["chain_trusted"], true, "{trusted}");
    assert_eq!(trusted.get("trust_error"), None, "{trusted}");
}

#[test]
fn verify_online_asks_the_responder_what_the_document_lacks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let services = Services::new(&ca);
    let (p12, cert) = revocable_identity(dir.path(), &ca, &services);
    let out = output(&sign(dir.path(), &src, &p12, &[]));
    services.revoke(&cert);
    let offline = only_signature(dir.path(), &out);
    assert_eq!(offline["revocation"], "unknown", "{offline}");
    assert_eq!(offline["revocation_source"], Value::Null);

    let verified =
        run(dir.path(), &["security", "verify", &out, "--online"]).expect("verify succeeds");
    let online = &verified["signatures"][0];
    assert_eq!(online["revocation"], "revoked", "{online}");
    assert_eq!(online["revocation_source"], "online");
    let error = online["revocation_error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with(&format!("{SIGNER_SUBJECT} was revoked at ")),
        "{online}"
    );
}

#[test]
fn verify_reads_the_store_offline_and_reaches_b_lt_then_b_lta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let services = Services::new(&ca);
    let tsa = Tsa::issued(&ca, |builder| {
        builder
            .add_extension(&services.crl_pointer())
            .expect("CRL pointer");
    });
    let (_tsa, url) = serve_tsa(tsa, Imprint::Asked);
    let (p12, cert) = revocable_identity(dir.path(), &ca, &services);
    let out = output(&sign(dir.path(), &src, &p12, &["--tsa", &url, "--ltv"]));
    // The responder now disagrees with the store; offline verification must not ask it.
    services.revoke(&cert);
    let rows = signature_rows(dir.path(), &out);
    let signature = &rows[0];
    assert_eq!(signature["revocation"], "good", "{signature}");
    assert_eq!(signature["revocation_source"], "dss");
    assert_eq!(signature["pades_level"], "B-LTA");

    // The file before its document time-stamp: the signature and the store only.
    let data = fs::read(&out).expect("signed file");
    let ends: Vec<usize> = data
        .windows(5)
        .enumerate()
        .filter(|(_, window)| window == b"%%EOF")
        .map(|(at, _)| at + 5)
        .collect();
    assert_eq!(
        ends.len(),
        4,
        "plain, signature, store, document time-stamp"
    );
    let before = path_in(dir.path(), "before-stamp.pdf");
    fs::write(&before, &data[..ends[2]]).expect("cut file");
    let rows = signature_rows(dir.path(), &before);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["pades_level"], "B-LT", "{:?}", rows[0]);
}

#[test]
fn ltv_refuses_a_revoked_signer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let services = Services::new(&ca);
    let (p12, cert) = revocable_identity(dir.path(), &ca, &services);
    services.revoke(&cert);
    let out = path_in(dir.path(), "revoked.pdf");
    let error = try_sign(dir.path(), &src, &p12, &["--ltv", "-o", &out])
        .expect_err("the signer's certificate is revoked")
        .to_string();
    assert!(
        error.starts_with(&format!("{SIGNER_SUBJECT} was revoked at ")),
        "{error}"
    );
    assert!(!Path::new(&out).exists(), "no output is written");
}

/// A one-page form with text fields `Name` and `Note` and an empty signature field
/// `Approver` whose widget covers `rect` (user space), carrying `lock` when given.
fn prepared_pdf(dir: &Path, rect: [f64; 4], lock: Option<Dict>) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(612.0, 792.0)
        .text_field("Name", [72.0, 600.0, 300.0, 620.0], "Ada")
        .text_field("Note", [72.0, 560.0, 300.0, 580.0], "Draft");
    let mut doc = builder.build();
    let page = doc.page(0).expect("page").id;
    let mut field = Dict::new();
    field.insert("FT", Object::name("Sig"));
    field.insert("T", Object::text("Approver"));
    field.insert("Type", Object::name("Annot"));
    field.insert("Subtype", Object::name("Widget"));
    field.insert("F", 4);
    field.insert("Rect", rect.map(Object::Real).to_vec());
    field.insert("P", page);
    if let Some(lock) = lock {
        field.insert("Lock", lock);
    }
    let field = doc.add(field);
    let mut page_dict = doc
        .resolve_dict(&Object::Reference(page))
        .expect("resolve")
        .expect("page dict");
    let mut annots = page_dict
        .get(b"Annots")
        .map(|annots| doc.resolve_array(annots).expect("resolve").expect("annots"))
        .unwrap_or_default();
    annots.push(Object::Reference(field));
    page_dict.insert("Annots", annots);
    doc.set(page, page_dict);
    let catalog_ref = doc.catalog_ref().expect("catalog");
    let mut catalog = doc.catalog().expect("catalog");
    let mut form = doc
        .resolve_dict(catalog.get(b"AcroForm").expect("AcroForm"))
        .expect("resolve")
        .expect("form");
    let mut fields = doc
        .resolve_array(form.get(b"Fields").expect("Fields"))
        .expect("resolve")
        .expect("fields");
    fields.push(Object::Reference(field));
    form.insert("Fields", fields);
    catalog.insert("AcroForm", form);
    doc.set(catalog_ref, catalog);
    let path = dir.join("prepared.pdf");
    fs::write(
        &path,
        doc.save_to_bytes(&SaveOptions::default()).expect("save"),
    )
    .expect("write");
    path.to_string_lossy().into_owned()
}

/// The top-level form fields, by object and dictionary, in `/Fields` order.
fn top_fields(doc: &Document) -> Vec<(ObjRef, Dict)> {
    let catalog = doc.catalog().expect("catalog");
    let form = doc
        .resolve_dict(catalog.get(b"AcroForm").expect("AcroForm"))
        .expect("resolve")
        .expect("form");
    doc.resolve_array(form.get(b"Fields").expect("Fields"))
        .expect("resolve")
        .expect("fields")
        .iter()
        .map(|field| {
            let id = field.as_reference().expect("an indirect field");
            let dict = doc.resolve_dict(field).expect("resolve").expect("field");
            (id, dict)
        })
        .collect()
}

fn field_name(field: &Dict) -> String {
    field.get_string(b"T").expect("/T").to_text()
}

/// `data` with the text field `name` set to `value` in an incremental update, as a form
/// filler saves it.
fn fill(data: &[u8], name: &str, value: &str) -> Vec<u8> {
    incremental_update(data, |doc| {
        let (id, mut field) = top_fields(doc)
            .into_iter()
            .find(|(_, field)| field_name(field) == name)
            .expect("the field");
        field.insert("V", Object::text(value));
        doc.set(id, field);
    })
}

fn signature_rows(dir: &Path, file: &str) -> Vec<Value> {
    let verified = run(dir, &["security", "verify", file]).expect("verify succeeds");
    verified["signatures"].as_array().expect("rows").clone()
}

fn path_in(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

#[test]
fn a_certification_allows_a_later_form_fill_from_level_two() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = prepared_pdf(dir.path(), [0.0; 4], None);
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    for (level, allowed) in [(1, false), (2, true)] {
        let certified = path_in(dir.path(), &format!("certified-{level}.pdf"));
        let signed = sign(
            dir.path(),
            &src,
            &p12,
            &["--certify", &level.to_string(), "-o", &certified],
        );
        assert_eq!(
            (&signed["certified"], &signed["docmdp_level"]),
            (&json!(true), &json!(level))
        );
        let untouched = only_signature(dir.path(), &certified);
        assert_eq!(
            (
                &untouched["certified"],
                &untouched["docmdp_level"],
                &untouched["changes_allowed"]
            ),
            (&json!(true), &json!(level), &json!(true)),
            "level {level}"
        );

        let filled = path_in(dir.path(), &format!("filled-{level}.pdf"));
        let data = fs::read(&certified).expect("certified file");
        fs::write(&filled, fill(&data, "Name", "Grace")).expect("write");
        let row = only_signature(dir.path(), &filled);
        assert_eq!(row["intact"], true, "level {level}");
        assert_eq!(row["modified"], "FORM_FILLING", "level {level}");
        assert_eq!(row["changes_allowed"], allowed, "level {level}: {row}");
    }
}

#[test]
fn signing_a_prepared_field_fills_it_and_draws_in_its_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    // User space [100 642 300 692] is pixels x 100..300, y 100..150 on the letter page.
    let src = prepared_pdf(dir.path(), [100.0, 642.0, 300.0, 692.0], None);
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let signed = sign(
        dir.path(),
        &src,
        &p12,
        &[
            "--field",
            "Approver",
            "--appearance-text",
            "Approved by Goat",
        ],
    );
    assert_eq!(
        (&signed["field_created"], &signed["visible"]),
        (&json!(false), &json!(true))
    );
    let out = output(&signed);

    let doc = Document::load(fs::read(&out).expect("signed file")).expect("load");
    let fields = top_fields(&doc);
    let names: Vec<String> = fields.iter().map(|(_, field)| field_name(field)).collect();
    assert_eq!(names, ["Name", "Note", "Approver"]);
    assert!(
        fields[2].1.get(b"V").is_some(),
        "Approver holds the signature"
    );

    let drawn = changed(&render(&src, 0), &render(&out, 0));
    let (xs, ys): (Vec<u32>, Vec<u32>) = drawn.into_iter().unzip();
    let (left, right) = (
        xs.iter().min().expect("drawn"),
        xs.iter().max().expect("drawn"),
    );
    let (top, bottom) = (
        ys.iter().min().expect("drawn"),
        ys.iter().max().expect("drawn"),
    );
    assert!(
        *left >= 100 && *right < 300 && *top >= 100 && *bottom < 150,
        "inside the field's box: x {left}..={right}, y {top}..={bottom}"
    );
    assert!(
        right - left > 150,
        "the text is sized to the field's box: x {left}..={right}"
    );

    let row = only_signature(dir.path(), &out);
    assert_eq!(row["field"], "Approver");
    assert_eq!(
        (&row["intact"], &row["valid"], &row["modified"]),
        (&json!(true), &json!(true), &json!("NONE"))
    );
}

#[test]
fn a_field_lock_forbids_changing_the_fields_it_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut lock = Dict::new();
    lock.insert("Type", Object::name("SigFieldLock"));
    lock.insert("Action", Object::name("Include"));
    lock.insert("Fields", vec![Object::text("Name")]);
    let src = prepared_pdf(dir.path(), [0.0; 4], Some(lock));
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let signed = sign(dir.path(), &src, &p12, &["--field", "Approver"]);
    let data = fs::read(output(&signed)).expect("signed file");
    for (name, allowed, modified) in [("Note", true, "FORM_FILLING"), ("Name", false, "OTHER")] {
        let filled = path_in(dir.path(), &format!("{name}.pdf"));
        fs::write(&filled, fill(&data, name, "Changed")).expect("write");
        let row = only_signature(dir.path(), &filled);
        assert_eq!(
            (&row["modified"], &row["changes_allowed"]),
            (&json!(modified), &json!(allowed)),
            "{name}: {row}"
        );
    }
}

#[test]
fn a_certified_document_takes_an_approval_signature_in_a_prepared_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = prepared_pdf(dir.path(), [100.0, 642.0, 300.0, 692.0], None);
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let certified = path_in(dir.path(), "certified.pdf");
    sign(
        dir.path(),
        &src,
        &p12,
        &["--certify", "2", "-o", &certified],
    );
    let approved = path_in(dir.path(), "approved.pdf");
    sign(
        dir.path(),
        &certified,
        &p12,
        &["--field", "Approver", "-o", &approved],
    );

    let rows: Vec<Value> = signature_rows(dir.path(), &approved)
        .iter()
        .map(|row| {
            json!([
                row["field"],
                row["intact"],
                row["valid"],
                row["certified"],
                row["changes_allowed"]
            ])
        })
        .collect();
    assert_eq!(
        rows,
        [
            json!(["Signature1", true, true, true, true]),
            json!(["Approver", true, true, false, true])
        ]
    );
}

#[test]
fn after_a_certification_a_new_visible_signature_field_is_not_allowed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let certified = path_in(dir.path(), "certified.pdf");
    sign(
        dir.path(),
        &src,
        &p12,
        &["--certify", "3", "-o", &certified],
    );
    let witnessed = path_in(dir.path(), "witnessed.pdf");
    sign(
        dir.path(),
        &certified,
        &p12,
        &[
            "--field",
            "Witness",
            "--rect",
            "100,100,300,150",
            "-o",
            &witnessed,
        ],
    );

    let rows = signature_rows(dir.path(), &witnessed);
    assert_eq!(
        (&rows[0]["intact"], &rows[0]["changes_allowed"]),
        (&json!(true), &json!(false)),
        "{}",
        rows[0]
    );
    assert_eq!(rows[1]["intact"], true);
}

#[test]
fn a_certification_comes_first_and_level_one_takes_no_later_signature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = plain_pdf(dir.path());
    let ca = Ca::new("CN=Goat Test Root");
    let (p12, _) = identity(dir.path(), &ca, &Key::p256(), Sealing::Modern);
    let approved = path_in(dir.path(), "approved.pdf");
    sign(dir.path(), &src, &p12, &["-o", &approved]);
    let error = try_sign(
        dir.path(),
        &approved,
        &p12,
        &["--certify", "2", "--field", "Signature2"],
    )
    .expect_err("a certification after an approval");
    assert_eq!(
        error,
        GoatError::message(
            "Certification signatures must be the first signature in a given document."
        )
    );

    let locked = path_in(dir.path(), "locked.pdf");
    sign(dir.path(), &src, &p12, &["--certify", "1", "-o", &locked]);
    let error = try_sign(dir.path(), &locked, &p12, &["--field", "Signature2"])
        .expect_err("a signature after a no-changes certification");
    assert_eq!(
        error,
        GoatError::message("Author signature forbids all changes")
    );
}
