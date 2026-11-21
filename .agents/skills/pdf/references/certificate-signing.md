# Certificate signing and verification

`IN` the finished PDF, `W` an empty work dir, `P12` the PKCS#12 identity, its password in env var `P12_PW` (never on the command line), `CA` an optional PEM root, `SIG` an optional image. Sign last: any later change except another `security sign` makes `verify` report `intact: false`.

## 1. Sign with a visible box

Place the box on empty space (check the last text line with `search` or a render).

```bash
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --field Member --page 1 --rect 62,330,330,390 --appearance-image "$SIG" --appearance-text $'Jane Q. Member\nSigned 2026-09-30' --reason "Membership application" -o "$W/signed.pdf"
```

- No `--rect`: invisible. The box is opaque over page text.
- `--appearance-text` breaks only on a real newline: use bash `$'…\n…'`.
- `--field` signs an existing empty field; otherwise a new one (`Signature1`).

## 2. Verify and report

```bash
pdf-goat --agent security verify "$W/signed.pdf"
pdf-goat --agent security verify "$W/signed.pdf" --trust "$CA"
pdf-goat --agent form list "$W/signed.pdf"
pdf-goat --agent render "$W/signed.pdf" --pages 1 --dpi 144 --clip 50,240,570,400 --mark 62,330,330,390 -o "$W/look"
```

- Per signature: `intact` (bytes unchanged), `valid` (crypto), `trusted` (local policy), `chain_trusted`, `trust_error`, `modified`, `certified`, `docmdp_level`, `timestamp`, `revocation`, `pades_level`.
- `--trust` adds roots to the system's; repeatable.
- Report fields as returned, e.g. "signed by Jane Q. Member; intact and valid; chain not trusted (ends at Example Club Root); PAdES B-B; no timestamp". Never "valid signature".

## 3. Timestamp and long-term validation

```bash
# needs the network: the time-stamp authority and OCSP responders
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --tsa http://timestamp.digicert.com -o "$W/signed-t.pdf"
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --tsa http://timestamp.digicert.com --ltv -o "$W/signed-lta.pdf"
pdf-goat --agent security verify "$W/signed-lta.pdf" --trust "$CA" --online
```

- `--tsa`: `B-T`. `--ltv` stores chains and revocation data in `dss`; with `--tsa` adds a document timestamp (`DocTimeStamp1`), first signature then `modified: "LTA_UPDATES"`.
- A certificate with no OCSP/CRL lands in `dss.unchecked` and holds the level at `B-T`. Never upgrade the level yourself.
- `--online` fetches missing revocation answers; `--timeout N` per request (default 30 s).

Offline, `--ltv` alone still stores the chain:

```bash
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --ltv -o "$W/signed-lt.pdf"
```

## 4. Certify, countersign, PSS

```bash
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --certify 2 -o "$W/certified.pdf"
pdf-goat --agent security verify "$W/certified.pdf" --trust "$CA"
pdf-goat --agent security sign "$W/signed.pdf" --p12 "$P12" --password-env P12_PW --field Witness --page 1 --rect 340,330,560,390 -o "$W/countersigned.pdf"
pdf-goat --agent security verify "$W/countersigned.pdf" --trust "$CA"
pdf-goat --agent security sign "$IN" --p12 "$P12" --password-env P12_PW --pss -o "$W/signed-pss.pdf"
pdf-goat --agent security verify "$W/signed-pss.pdf" --trust "$CA"
```

- `--certify 1|2|3`: no changes / form filling and signing / also annotations.
- A countersignature leaves the first `intact` with `modified: "FORM_FILLING"`.

## 5. Demo identity

```bash
pdf-goat --agent security sign "$IN" -o "$W/demo-signed.pdf"
```

- Signer `pdf-goat demo`, `self_signed: true`: layout tests only, never deliver.
