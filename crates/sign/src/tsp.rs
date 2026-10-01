//! RFC 3161 time-stamps: the request `security sign --tsa` sends for the signature value
//! (PAdES B-T), the token it embeds as an unsigned attribute, and the checks
//! `security verify` applies to an embedded token or a document time-stamp.

use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerInfos};
use const_oid::ObjectIdentifier;
use const_oid::db::{rfc5280, rfc5911};
use der::asn1::{BitString, Int, OctetString, SetOfVec};
use der::{Any, Decode, Encode, Sequence, Tag, Tagged};
use pdf_core::PdfDate;
use spki::AlgorithmIdentifierOwned;
use x509_cert::attr::Attribute;
use x509_cert::ext::Extension;
use x509_cert::ext::pkix::ExtendedKeyUsage;
use x509_cert::ext::pkix::name::GeneralName;

use crate::net::Http;
use crate::pkcs7::{Hash, Signature, Verdict};

/// `id-aa-timeStampToken` (RFC 3161 appendix A), the unsigned attribute holding a
/// signature time-stamp.
pub const ID_AA_TIME_STAMP_TOKEN: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.14");
/// `id-ct-TSTInfo`, the content type of a time-stamp token.
const ID_CT_TST_INFO: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.1.4");

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
struct MessageImprint {
    hash_algorithm: AlgorithmIdentifierOwned,
    hashed_message: OctetString,
}

#[derive(Sequence)]
struct TimeStampReq {
    version: u8,
    message_imprint: MessageImprint,
    #[asn1(optional = "true")]
    req_policy: Option<ObjectIdentifier>,
    #[asn1(optional = "true")]
    nonce: Option<Int>,
    /// DEFAULT FALSE; the request always asks for the TSA's certificate.
    cert_req: bool,
}

#[derive(Sequence)]
struct PkiStatusInfo {
    status: u32,
    #[asn1(optional = "true")]
    status_string: Option<Vec<String>>,
    #[asn1(optional = "true")]
    fail_info: Option<BitString>,
}

#[derive(Sequence)]
struct TimeStampResp {
    status: PkiStatusInfo,
    #[asn1(optional = "true")]
    time_stamp_token: Option<Any>,
}

#[derive(Sequence)]
struct Accuracy {
    #[asn1(optional = "true")]
    seconds: Option<u32>,
    #[asn1(context_specific = "0", tag_mode = "IMPLICIT", optional = "true")]
    millis: Option<u16>,
    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", optional = "true")]
    micros: Option<u16>,
}

#[derive(Sequence)]
struct TstInfo {
    version: u8,
    policy: ObjectIdentifier,
    message_imprint: MessageImprint,
    serial_number: Int,
    /// Kept raw: der's GeneralizedTime rejects the fractional seconds TSAs may add.
    gen_time: Any,
    #[asn1(optional = "true")]
    accuracy: Option<Accuracy>,
    #[asn1(optional = "true")]
    ordering: Option<bool>,
    #[asn1(optional = "true")]
    nonce: Option<Int>,
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    tsa: Option<GeneralName>,
    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", optional = "true")]
    extensions: Option<Vec<Extension>>,
}

/// A time-stamp token: signed data over a TSTInfo.
pub struct Token {
    pub signature: Signature,
    /// genTime in UTC, fractions of a second dropped.
    pub time: PdfDate,
    content: Vec<u8>,
    imprint: MessageImprint,
    nonce: Option<Int>,
}

impl Token {
    pub fn parse(der: &[u8]) -> Result<Token, String> {
        let signature = Signature::parse_as(der, ID_CT_TST_INFO)?;
        let content = signature
            .content()?
            .ok_or("the time-stamp token carries no TSTInfo")?;
        let info = TstInfo::from_der(&content).map_err(|e| format!("TSTInfo: {e}"))?;
        let time = generalized_time(&info.gen_time)?;
        Ok(Token {
            signature,
            time,
            content,
            imprint: info.message_imprint,
            nonce: info.nonce,
        })
    }

    /// The digest algorithm of the token's message imprint.
    pub fn imprint_hash(&self) -> Result<Hash, String> {
        Hash::from_identifier(&self.imprint.hash_algorithm)
    }

    /// Checks that the token time-stamps `message`: the imprint is the digest of
    /// `message`, the TSA's signature over the TSTInfo verifies, and the TSA's certificate
    /// allows time-stamping.
    pub fn check(&self, message: &[u8]) -> Result<(), String> {
        if self.imprint_hash()?.digest_bytes(message) != self.imprint.hashed_message.as_bytes() {
            return Err("the time-stamp does not cover this signature".to_owned());
        }
        self.check_token()
    }

    /// [`Token::check`] for a document time-stamp, given `digest`: the
    /// [`Token::imprint_hash`] digest of the byte ranges of the revision it ends.
    pub fn check_revision(&self, digest: &[u8]) -> Result<(), String> {
        if digest != self.imprint.hashed_message.as_bytes() {
            return Err("the time-stamp does not cover the signed revision".to_owned());
        }
        self.check_token()
    }

    /// The verdicts of a document time-stamp, given `digest` as for
    /// [`Token::check_revision`]: intact when the imprint is `digest` and the TSTInfo
    /// matches its signed message digest; trusted when also valid, with no weak algorithm
    /// and a certificate allowed to issue time-stamps.
    pub fn verdict(&self, digest: &[u8]) -> Result<Verdict, String> {
        let token = self.token_verdict()?;
        let covers = digest == self.imprint.hashed_message.as_bytes();
        Ok(Verdict {
            intact: token.intact && covers,
            valid: token.valid,
            trusted: token.trusted
                && covers
                && !self.imprint_hash()?.is_weak()
                && self.may_time_stamp()?,
        })
    }

    /// The TSA's signature over the TSTInfo verifies and its certificate allows
    /// time-stamping.
    fn check_token(&self) -> Result<(), String> {
        let verdict = self.token_verdict()?;
        if !verdict.intact {
            return Err(
                "the time-stamp token's TSTInfo does not match its message digest".to_owned(),
            );
        }
        if !verdict.valid {
            return Err("the time-stamp authority's signature does not verify".to_owned());
        }
        if !self.may_time_stamp()? {
            return Err(
                "the time-stamp certificate is not allowed to issue time-stamps".to_owned(),
            );
        }
        Ok(())
    }

    fn token_verdict(&self) -> Result<Verdict, String> {
        self.signature
            .verify(&self.signature.digest.digest_bytes(&self.content))
    }

    /// Whether the TSA's certificate carries the time-stamping extended key usage.
    fn may_time_stamp(&self) -> Result<bool, String> {
        Ok(self
            .signature
            .certificate()
            .tbs_certificate
            .get::<ExtendedKeyUsage>()
            .map_err(|e| format!("time-stamp certificate extended key usage: {e}"))?
            .is_some_and(|(_, usage)| usage.0.contains(&rfc5280::ID_KP_TIME_STAMPING)))
    }
}

/// `cms_der` with a signature time-stamp from the TSA at `url` added to its signer, and
/// the token's time. The token's imprint is the `hash` of the signature value; it goes in
/// an unsigned attribute, so the signature itself is unchanged.
pub fn stamp(
    http: &Http,
    url: &str,
    hash: Hash,
    cms_der: &[u8],
) -> Result<(Vec<u8>, PdfDate), String> {
    let content = ContentInfo::from_der(cms_der).map_err(|e| format!("signed data: {e}"))?;
    let mut signed: SignedData = content
        .content
        .decode_as()
        .map_err(|e| format!("signed data: {e}"))?;
    let mut infos = signed.signer_infos.0.into_vec();
    let info = infos.first_mut().ok_or("the signed data has no signer")?;
    let (token, time) = request(http, url, hash, info.signature.as_bytes())?;
    let mut unsigned = info.unsigned_attrs.take().unwrap_or_default();
    let value = Any::from_der(&token).map_err(|e| format!("time-stamp token: {e}"))?;
    unsigned
        .insert(Attribute {
            oid: ID_AA_TIME_STAMP_TOKEN,
            values: SetOfVec::try_from(vec![value]).map_err(|e| e.to_string())?,
        })
        .map_err(|e| e.to_string())?;
    info.unsigned_attrs = Some(unsigned);
    signed.signer_infos = SignerInfos(SetOfVec::try_from(infos).map_err(|e| e.to_string())?);
    let der = ContentInfo {
        content_type: rfc5911::ID_SIGNED_DATA,
        content: Any::encode_from(&signed).map_err(|e| e.to_string())?,
    }
    .to_der()
    .map_err(|e| e.to_string())?;
    Ok((der, time))
}

/// Asks the TSA at `url` for a token over `message` with `hash`, and returns it as DER
/// with its time after checking it answers the request.
fn request(
    http: &Http,
    url: &str,
    hash: Hash,
    message: &[u8],
) -> Result<(Vec<u8>, PdfDate), String> {
    let (token, parsed) = ask(http, url, hash, hash.digest_bytes(message))?;
    parsed.check(message)?;
    Ok((token, parsed.time))
}

/// A document time-stamp token from the TSA at `url`, as DER: its imprint is `digest`,
/// the `hash` digest of the byte ranges of the revision it ends.
pub fn document_stamp(
    http: &Http,
    url: &str,
    hash: Hash,
    digest: &[u8],
) -> Result<Vec<u8>, String> {
    let (token, parsed) = ask(http, url, hash, digest.to_vec())?;
    parsed.check_revision(digest)?;
    Ok(token)
}

/// Sends the TSA at `url` a request for a token over `digest`, made with `hash`, and
/// returns the token that answers it, checked against the request's nonce.
fn ask(http: &Http, url: &str, hash: Hash, digest: Vec<u8>) -> Result<(Vec<u8>, Token), String> {
    let imprint = MessageImprint {
        hash_algorithm: AlgorithmIdentifierOwned {
            oid: hash.oid(),
            parameters: None,
        },
        hashed_message: OctetString::new(digest).map_err(|e| e.to_string())?,
    };
    let mut nonce = rand::random::<[u8; 8]>();
    // A positive integer in its shortest encoding.
    nonce[0] = (nonce[0] & 0x7f).max(1);
    let nonce = Int::new(&nonce).map_err(|e| e.to_string())?;
    let query = TimeStampReq {
        version: 1,
        message_imprint: imprint,
        req_policy: None,
        nonce: Some(nonce.clone()),
        cert_req: true,
    }
    .to_der()
    .map_err(|e| e.to_string())?;
    let body = http.post(url, "application/timestamp-query", &query)?;
    let reply = TimeStampResp::from_der(&body)
        .map_err(|e| format!("{url} did not answer with a time-stamp response: {e}"))?;
    if reply.status.status > 1 {
        let text = reply.status.status_string.unwrap_or_default().join("; ");
        return Err(format!(
            "{url} refused the time-stamp request (status {}{}{text})",
            reply.status.status,
            if text.is_empty() { "" } else { ": " }
        ));
    }
    let token = reply
        .time_stamp_token
        .ok_or_else(|| format!("{url} answered without a time-stamp token"))?
        .to_der()
        .map_err(|e| e.to_string())?;
    let parsed = Token::parse(&token)?;
    if parsed.nonce.as_ref() != Some(&nonce) {
        return Err(format!(
            "{url} answered a different time-stamp request (nonce mismatch)"
        ));
    }
    Ok((token, parsed))
}

/// The signature time-stamp token `signature` carries, if any.
pub fn embedded_token(signature: &Signature) -> Option<Result<Token, String>> {
    signature
        .unsigned_attribute(ID_AA_TIME_STAMP_TOKEN)
        .map(|value| {
            value
                .to_der()
                .map_err(|e| format!("time-stamp token: {e}"))
                .and_then(|der| Token::parse(&der))
        })
}

/// `YYYYMMDDHHMMSS[.fraction]Z` as a UTC date.
fn generalized_time(value: &Any) -> Result<PdfDate, String> {
    let bad = || "the time-stamp token's genTime is not a UTC GeneralizedTime".to_owned();
    if value.tag() != Tag::GeneralizedTime {
        return Err(bad());
    }
    let text = std::str::from_utf8(value.value()).map_err(|_| bad())?;
    let text = text.strip_suffix('Z').ok_or_else(bad)?;
    let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if whole.len() != 14 || !digits(whole) || !digits(fraction) {
        return Err(bad());
    }
    PdfDate::parse(&format!("D:{whole}Z")).ok_or_else(bad)
}

/// A UTC date as ISO 8601 (`2026-09-30T12:00:00Z`).
pub fn iso_utc(date: &PdfDate) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        date.year, date.month, date.day, date.hour, date.minute, date.second
    )
}
