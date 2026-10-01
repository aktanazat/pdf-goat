# Certificate signing and verification

A cryptographic signature with the signer's PKCS#12 identity (.p12 or .pfx). Set `IN` to the finished PDF, `W` to an empty work directory, `P12` to the identity file, and put its password in an environment variable (here `P12_PW`); never on the command line. `CA` is a PEM root to trust when the identity comes from a private authority. `SIG` is an optional signature image.

Sign last. Any later change, including `form fill` on a document certified for form filling, rewrites the file, and `security verify` then reports `intact: false`. A further `security sign` is the one change that keeps earlier signatures intact.

## 1. Sign with a visible box

Put the box on empty space: find the last line of text with `search` or look at a render. In the example page the last line ("Visit Acme for details.") ends at y 297.9, so the box goes at y 330 to 390.

```bash
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --field Member --page 1 --rect 62,330,330,390 --appearance-image "$SIG" --appearance-text $'Jane Q. Member\nSigned 2026-09-30' --reason "Membership application" -o "$W/signed.pdf"
```

- Returns `signer` (the certificate's common name), `self_signed`, `pades_level`, `visible`, `field`, `field_created`, `timestamp`, `certified`, `docmdp_level`, `dss` and `document_timestamp`. Here: signer `Jane Q. Member`, `self_signed: false`, `pades_level` `B-B`, `visible: true`, field `Member`, no timestamp.
- `--rect` is in the search frame. Without it the signature is invisible. The box is opaque: it covers any page text under it.
- `--appearance-image` (PNG or JPEG) fills the left part; `--appearance-text` sits on the right and wraps to fit. Lines break at a real newline only, so use bash `$'…\n…'`; a typed `\n` prints as is. Without either option the box reads "Digitally signed by <signer>" and the signing time in UTC.
- `--field` names an existing empty signature field to sign in place; otherwise a new field is made (default `Signature1`).
- Errors to report as they come: `wrong password for the PKCS#12 file (its integrity check failed)`, `the environment variable P12_PW named by --password-env is not set`.

## 2. Verify and report

```bash
pdf-goat --agent security verify "$W/signed.pdf"
pdf-goat --agent security verify "$W/signed.pdf" --trust "$CA"
pdf-goat --agent form list "$W/signed.pdf"
pdf-goat --agent render "$W/signed.pdf" --pages 1 --dpi 144 --clip 50,240,570,400 --mark 62,330,330,390 -o "$W/look"
```

- `signature_count`, then per signature in `signatures[]`: `field`, `kind` (`signature` or `document_timestamp`), `signer`, `intact` (the signed bytes are unchanged), `valid` (the cryptography checks out), `trusted` (local policy only), `coverage`, `modified`, `certified`, `docmdp_level`, `changes_allowed`, `timestamp`, `timestamp_valid`, `chain_trusted`, `revocation`, `revocation_source`, `pades_level`, and `trust_error` or `modification_error` when present.
- Here, without `--trust`: `intact` and `valid` true, `chain_trusted: false` with `trust_error` "the chain ends at Common Name: Example Club Root, Organization: Example Club, which is not a trusted root". With `--trust "$CA"`: `chain_trusted: true`. `--trust` adds roots to the system's; repeat it for more.
- `form list` shows the field with `type` `Signature`. Open the render: the image on the left and the text on the right (here wrapped to three lines: "Jane Q. Member", "Signed", "2026-09-30") inside the magenta outline, clear of the page text.
- Report the fields as returned, for example: "signed by Jane Q. Member; intact and valid; chain not trusted (ends at Example Club Root, not a trusted root); PAdES B-B; no timestamp". Never shorten that to "valid signature": the fields are the answer, not your inference. A certificate carrying a critical extension pdf-goat does not process makes `chain_trusted: false`, and `trust_error` names the certificate and the extension.

## 3. Timestamp and long-term validation

```bash
# needs the network: the time-stamp authority and OCSP responders
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --tsa http://timestamp.digicert.com -o "$W/signed-t.pdf"
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --tsa http://timestamp.digicert.com --ltv -o "$W/signed-lta.pdf"
pdf-goat --agent security verify "$W/signed-lta.pdf" --trust "$CA" --online
```

- `--tsa` adds an RFC 3161 timestamp: `timestamp` is set and `pades_level` reads `B-T`.
- `--ltv` stores certificate chains with the OCSP responses and CRLs their certificates name, in `dss` (`certificates`, `ocsp_responses`, `crls`, `unchecked`). With `--tsa` it also adds a document timestamp: `document_timestamp` is set, and `verify` lists two signatures, the second `kind: "document_timestamp"` (field `DocTimeStamp1`), the first with `modified: "LTA_UPDATES"`.
- `pades_level` follows the evidence stored in the file. `B-LTA` needs the document timestamp plus a revocation answer for every certificate below the root, or a certificate that by RFC 9608 needs none. A certificate that names no OCSP responder or CRL is listed in `dss.unchecked` and keeps the signature at `B-T`. With a private test authority that publishes no revocation service, `unchecked` names the signer and the level is `B-T`. Report the level and `unchecked` as returned; never upgrade the level yourself.
- `verify --online` asks the responders for revocation answers the file lacks. `revocation` and `revocation_source` show what came back; `revocation` is `unknown` for a certificate nothing answers for. `--timeout N` bounds each request (default 30 s).

Without the network, `--ltv` alone still stores the chain:

```bash
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --ltv -o "$W/signed-lt.pdf"
```

- Here `dss` holds 2 certificates and no revocation answers, `unchecked` names the signer, and `pades_level` stays `B-B`.

## 4. Certify, countersign, PSS

```bash
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --certify 2 -o "$W/certified.pdf"
pdf-goat --agent security verify "$W/certified.pdf" --trust "$CA"
pdf-goat --agent security sign "$W/signed.pdf" --p12 "$P12" --password-env P12_PW --field Witness --page 1 --rect 340,330,560,390 -o "$W/countersigned.pdf"
pdf-goat --agent security verify "$W/countersigned.pdf" --trust "$CA"
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --pss -o "$W/signed-pss.pdf"
pdf-goat --agent security verify "$W/signed-pss.pdf" --trust "$CA"
```

- `--certify 1|2|3` allows, after signing, no changes, form filling and signing, or also annotations: `certified: true` and `docmdp_level` in both sign and verify.
- A countersignature keeps the first one intact: verify shows `signature_count: 2`, the first with `modified: "FORM_FILLING"` and `changes_allowed: true`, the second covering the whole file.
- `--pss` signs an RSA key with RSASSA-PSS; verify must still read `intact` and `valid` true.

## 5. Demo identity

```bash
pdf-goat --agent security sign "$IN" -o "$W/demo-signed.pdf"
```

- Without `--p12` the signer is `pdf-goat demo` with `self_signed: true`. Use it only to test a layout; never deliver it as a signature.
