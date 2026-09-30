//! `security verify` on files pyhanko signed, checked against what the Python CLI reports
//! for the same files: integrity, the algorithm policy, coverage, and the incremental-update
//! review all have to agree with the reference implementation, not just with this crate's
//! own signer.
//!
//! These small golden PDFs are the integrator-approved fixture exception: generated
//! public test text and self-signed test identities only, produced by pyhanko/PyMuPDF.
//! Every file is below 64 KiB; none contains a private key or personal document.
//! The additional algorithm fixtures use certificates valid from 2020 through 2120.
//! Their `-bad-signature` companions change one signature byte, not the signed PDF ranges.

use std::path::PathBuf;

use serde_json::{Map, Value};

fn fixture(name: &str) -> Vec<u8> {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "fixtures", name]
        .iter()
        .collect();
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn verify(name: &str) -> Vec<Map<String, Value>> {
    pdf_sign::verify_file(&fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn expect(
    sig: &Map<String, Value>,
    field: &str,
    signer: &str,
    flags: (bool, bool, bool),
    coverage: &str,
    modified: &str,
) {
    assert_eq!(sig["field"], field);
    assert_eq!(sig["signer"], signer);
    assert_eq!(
        (
            sig["intact"].as_bool(),
            sig["valid"].as_bool(),
            sig["trusted"].as_bool()
        ),
        (Some(flags.0), Some(flags.1), Some(flags.2)),
        "{sig:?}"
    );
    assert_eq!(
        sig["coverage"],
        format!("SignatureCoverageLevel.{coverage}")
    );
    assert_eq!(sig["modified"], modified);
}

#[test]
fn a_pyhanko_signature_over_the_whole_file_is_intact_valid_and_trusted() {
    let sigs = verify("py-signed.pdf");
    assert_eq!(sigs.len(), 1);
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: pdf-goat demo",
        (true, true, true),
        "ENTIRE_FILE",
        "NONE",
    );
}

#[test]
fn a_byte_changed_inside_the_signed_range_breaks_integrity_only() {
    let sigs = verify("tampered.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: pdf-goat demo",
        (false, true, false),
        "ENTIRE_FILE",
        "NONE",
    );
}

#[test]
fn bytes_appended_without_a_new_revision_leave_a_contiguous_block() {
    let sigs = verify("modified.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: pdf-goat demo",
        (false, true, false),
        "CONTIGUOUS_BLOCK_FROM_START",
        "OTHER",
    );
}

#[test]
fn an_annotation_added_in_a_later_revision_is_an_unexplained_modification() {
    let sigs = verify("modified-incr.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: pdf-goat demo",
        (true, true, true),
        "ENTIRE_REVISION",
        "OTHER",
    );
}

#[test]
fn two_signatures_are_listed_in_signing_order_with_the_first_at_form_filling() {
    let sigs = verify("twice.pdf");
    assert_eq!(sigs.len(), 2);
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: first signer",
        (true, true, true),
        "ENTIRE_REVISION",
        "FORM_FILLING",
    );
    expect(
        &sigs[1],
        "Signature2",
        "Common Name: second signer",
        (true, true, true),
        "ENTIRE_FILE",
        "NONE",
    );
}

#[test]
fn a_metadata_only_update_after_signing_is_an_lta_update() {
    let sigs = verify("meta-incr.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: pdf-goat demo",
        (true, true, true),
        "ENTIRE_REVISION",
        "LTA_UPDATES",
    );
}

#[test]
fn a_text_field_value_set_after_signing_is_form_filling() {
    let sigs = verify("form-vfill.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: form signer",
        (true, true, true),
        "ENTIRE_REVISION",
        "FORM_FILLING",
    );
}

#[test]
fn an_ecdsa_p256_signature_verifies() {
    let sigs = verify("ecdsa.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: ecdsa one",
        (true, true, true),
        "ENTIRE_FILE",
        "NONE",
    );
}

#[test]
fn a_sha1_signature_is_intact_but_fails_the_algorithm_policy() {
    let sigs = verify("sha1.pdf");
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: sha1 one",
        (true, true, false),
        "ENTIRE_FILE",
        "NONE",
    );
}

#[test]
fn the_signer_name_lists_subject_components_most_specific_first() {
    let sigs = verify("multiname.pdf");
    assert_eq!(
        sigs[0]["signer"],
        "Common Name: Multi Name; Email Address: goat@example.com; Organizational Unit: Docs; Organization: Goat, Inc; Country: US"
    );
    let sigs = verify("unicode.pdf");
    assert_eq!(sigs[0]["signer"], "Common Name: Zoë Ünïcode");
}

const ALGORITHMS: &[&str] = &[
    "rsa-pss",
    "rsa-pss-4096",
    "rsa-pss-mixed-mask",
    "rsa-pss-2049-zero-salt",
    "ecdsa-p384",
    "ecdsa-p521",
    "ecdsa-k256",
    "ed25519",
    "ed448",
    "dsa",
    "rsa-sha224",
];

#[test]
fn independently_signed_algorithms_preserve_integrity_and_key_strength_verdicts() {
    for name in ALGORITHMS {
        let sigs = verify(&format!("{name}.pdf"));
        assert_eq!(sigs.len(), 1, "{name}");
        expect(
            &sigs[0],
            "Signature1",
            &format!("Common Name: {name}"),
            (true, true, *name != "dsa"),
            "ENTIRE_FILE",
            "NONE",
        );
    }
}

#[test]
fn altered_signature_bytes_fail_verification_without_breaking_document_integrity() {
    for name in ALGORITHMS {
        let sigs = verify(&format!("{name}-bad-signature.pdf"));
        assert_eq!(sigs.len(), 1, "{name}");
        expect(
            &sigs[0],
            "Signature1",
            &format!("Common Name: {name}"),
            (true, false, false),
            "ENTIRE_FILE",
            "NONE",
        );
    }
}

#[test]
fn an_ec_key_with_a_different_declared_curve_is_not_accepted_as_p256() {
    let sigs = verify("ecdsa-wrong-curve.pdf");
    assert_eq!(sigs.len(), 1);
    assert!(sigs[0].get("validation_error").is_some(), "{:?}", sigs[0]);
}

#[test]
fn a_high_s_secp256k1_signature_is_also_valid() {
    let sigs = verify("ecdsa-k256-high-s.pdf");
    assert_eq!(sigs.len(), 1);
    expect(
        &sigs[0],
        "Signature1",
        "Common Name: ecdsa-k256",
        (true, true, true),
        "ENTIRE_FILE",
        "NONE",
    );
}
