//! CMS (PKCS#7) detached signatures: the self-signed RSA signer `security sign` uses, and
//! the integrity checks `security verify` reports (pyhanko's `validate_sig_integrity` and
//! its default `DisallowWeakAlgorithmsPolicy`).

use std::time::{Duration, SystemTime};

use cms::builder::{SignedDataBuilder, SignerInfoBuilder, create_signing_time_attribute};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::ContentInfo;
use cms::signed_data::{EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo};
use const_oid::db::{rfc4519, rfc5911, rfc5912, rfc8410};
use const_oid::{AssociatedOid, ObjectIdentifier};
use der::asn1::{Ia5StringRef, OctetString, PrintableStringRef, TeletexStringRef, Utf8StringRef};
use der::referenced::OwnedToRef;
use der::{Any, Decode, Encode, Tag, Tagged};
use rsa::pkcs8::EncodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use sha3::digest::{ExtendableOutput, XofReader};
use signature::hazmat::PrehashVerifier;
use spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use x509_cert::Certificate;
use x509_cert::attr::Attributes;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::ext::pkix::name::{GeneralName, GeneralNames};
use x509_cert::ext::pkix::{BasicConstraints, KeyUsage, KeyUsages, SubjectKeyIdentifier};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::{Time, Validity};

const RSA_BITS: usize = 2048;
const DAY: u64 = 86_400;
/// `ecdsa-with-SHA1`, which const-oid's database lacks.
const ECDSA_WITH_SHA_1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.1");
const SHAKE_256_LEN: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.18");
const SECP_256_K_1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.10");
/// Retain the reference policy's DSA threshold, independently of cryptographic validity.
const MIN_DSA_BITS: usize = 3192;

/// A freshly generated RSA-2048 key with a self-signed certificate for `CN=<name>`, as
/// `_self_signed` builds: valid from a day ago for ten years, `BasicConstraints(ca=False)`
/// and `KeyUsage(digitalSignature, nonRepudiation)`.
pub struct SelfSigned {
    key: RsaPrivateKey,
    cert: Certificate,
}

impl SelfSigned {
    pub fn generate(common_name: &str) -> Result<SelfSigned, String> {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, RSA_BITS).map_err(|e| e.to_string())?;
        let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key.clone());
        let public_der = key
            .to_public_key()
            .to_public_key_der()
            .map_err(|e| e.to_string())?;
        let spki = SubjectPublicKeyInfoOwned::from_der(public_der.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut serial_bytes = rand::random::<[u8; 16]>();
        serial_bytes[0] &= 0x7f;
        let serial = SerialNumber::new(&serial_bytes).map_err(|e| e.to_string())?;
        let now = SystemTime::now();
        let validity = Validity {
            not_before: Time::try_from(now - Duration::from_secs(DAY))
                .map_err(|e| e.to_string())?,
            not_after: Time::try_from(now + Duration::from_secs(3650 * DAY))
                .map_err(|e| e.to_string())?,
        };
        let subject = subject_name(common_name)?;
        let mut builder = CertificateBuilder::new(
            Profile::Manual { issuer: None },
            serial,
            validity,
            subject,
            spki,
            &signer,
        )
        .map_err(|e| e.to_string())?;
        builder
            .add_extension(&BasicConstraints {
                ca: false,
                path_len_constraint: None,
            })
            .map_err(|e| e.to_string())?;
        builder
            .add_extension(&KeyUsage(
                KeyUsages::DigitalSignature | KeyUsages::NonRepudiation,
            ))
            .map_err(|e| e.to_string())?;
        let cert = builder
            .build::<rsa::pkcs1v15::Signature>()
            .map_err(|e| e.to_string())?;
        Ok(SelfSigned { key, cert })
    }

    /// A detached `SignedData` (DER) over `message_digest`, the SHA-256 of the signed byte
    /// ranges: version 1, `IssuerAndSerialNumber`, sha256WithRSAEncryption, and the
    /// signed attributes contentType, messageDigest, and signingTime.
    pub fn sign_detached(&self, message_digest: &[u8]) -> Result<Vec<u8>, String> {
        let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(self.key.clone());
        let eci = EncapsulatedContentInfo {
            econtent_type: rfc5911::ID_DATA,
            econtent: None,
        };
        let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: self.cert.tbs_certificate.issuer.clone(),
            serial_number: self.cert.tbs_certificate.serial_number.clone(),
        });
        let digest_alg = AlgorithmIdentifierOwned {
            oid: rfc5912::ID_SHA_256,
            parameters: None,
        };
        let mut signer_info =
            SignerInfoBuilder::new(&signer, sid, digest_alg.clone(), &eci, Some(message_digest))
                .map_err(|e| e.to_string())?;
        signer_info
            .add_signed_attribute(create_signing_time_attribute().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let mut builder = SignedDataBuilder::new(&eci);
        let content = builder
            .add_digest_algorithm(digest_alg)
            .map_err(|e| e.to_string())?
            .add_certificate(CertificateChoices::Certificate(self.cert.clone()))
            .map_err(|e| e.to_string())?
            .add_signer_info::<rsa::pkcs1v15::SigningKey<Sha256>, rsa::pkcs1v15::Signature>(
                signer_info,
            )
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?;
        content.to_der().map_err(|e| e.to_string())
    }
}

/// RFC 4514 escaping for the value of a `CN=` component.
fn escape_rdn_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        let special = matches!(ch, ',' | '+' | '"' | '\\' | '<' | '>' | ';' | '=')
            || (index == 0 && matches!(ch, ' ' | '#'));
        if special {
            out.push('\\');
        }
        out.push(ch);
    }
    if out.ends_with(' ') {
        out.insert(out.len() - 1, '\\');
    }
    out
}

/// `x509.Name.build({"common_name": name})`: a single `CN=` component, UTF8String.
fn subject_name(common_name: &str) -> Result<Name, String> {
    use std::str::FromStr;
    Name::from_str(&format!("CN={}", escape_rdn_value(common_name)))
        .map_err(|e| format!("subject name: {e}"))
}

/// Digest algorithms a PDF signature can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Shake256512,
}

impl Hash {
    pub(crate) fn from_oid(oid: &ObjectIdentifier) -> Option<Hash> {
        match *oid {
            rfc5912::ID_SHA_1 => Some(Hash::Sha1),
            rfc5912::ID_SHA_224 => Some(Hash::Sha224),
            rfc5912::ID_SHA_256 => Some(Hash::Sha256),
            rfc5912::ID_SHA_384 => Some(Hash::Sha384),
            rfc5912::ID_SHA_512 => Some(Hash::Sha512),
            SHAKE_256_LEN => Some(Hash::Shake256512),
            _ => None,
        }
    }

    pub(crate) fn from_identifier(algorithm: &AlgorithmIdentifierOwned) -> Result<Hash, String> {
        let hash = Self::from_oid(&algorithm.oid)
            .ok_or_else(|| format!("unsupported digest algorithm {}", algorithm.oid))?;
        if hash == Hash::Shake256512 {
            let bits: u32 = algorithm
                .parameters
                .as_ref()
                .ok_or("SHAKE256-LEN requires an output length")?
                .decode_as()
                .map_err(|e| format!("SHAKE256-LEN output length: {e}"))?;
            if bits != 512 {
                return Err(format!("unsupported SHAKE256-LEN output length {bits}"));
            }
        }
        Ok(hash)
    }

    /// The digest's algorithm identifier; SHAKE256-LEN also needs its length parameter.
    pub(crate) fn oid(self) -> ObjectIdentifier {
        match self {
            Hash::Sha1 => rfc5912::ID_SHA_1,
            Hash::Sha224 => rfc5912::ID_SHA_224,
            Hash::Sha256 => rfc5912::ID_SHA_256,
            Hash::Sha384 => rfc5912::ID_SHA_384,
            Hash::Sha512 => rfc5912::ID_SHA_512,
            Hash::Shake256512 => SHAKE_256_LEN,
        }
    }

    /// The digest of `data`.
    pub fn digest_bytes(self, data: &[u8]) -> Vec<u8> {
        self.digest_ranges(data, &[(0, data.len())])
    }

    /// The digest of the `(offset, length)` slices of `data` in order; a slice that runs
    /// past the end contributes what exists, as a short read does.
    pub fn digest_ranges(self, data: &[u8], ranges: &[(usize, usize)]) -> Vec<u8> {
        let parts = || {
            ranges.iter().map(|&(offset, length)| {
                let start = offset.min(data.len());
                let end = offset.saturating_add(length).min(data.len());
                &data[start..end]
            })
        };
        fn run<'a, D: Digest>(parts: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
            let mut hasher = D::new();
            for part in parts {
                hasher.update(part);
            }
            hasher.finalize().to_vec()
        }
        match self {
            Hash::Sha1 => run::<Sha1>(parts()),
            Hash::Sha224 => run::<Sha224>(parts()),
            Hash::Sha256 => run::<Sha256>(parts()),
            Hash::Sha384 => run::<Sha384>(parts()),
            Hash::Sha512 => run::<Sha512>(parts()),
            Hash::Shake256512 => {
                let mut hasher = sha3::Shake256::default();
                for part in parts() {
                    sha3::digest::Update::update(&mut hasher, part);
                }
                let mut output = vec![0; 64];
                hasher.finalize_xof().read(&mut output);
                output
            }
        }
    }

    /// pyhanko's default `algorithm_usage_policy`: SHA-1 is no longer acceptable.
    pub(crate) fn is_weak(self) -> bool {
        self == Hash::Sha1
    }
}

/// The signature algorithm family, with the hash the algorithm identifier implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SigAlg {
    /// `rsaEncryption`: the hash is the signer's digest algorithm.
    RsaPlain,
    RsaPkcs1(Hash),
    RsaPss {
        hash: Hash,
        mask_hash: Hash,
        salt_len: usize,
    },
    Ecdsa(Hash),
    Dsa(Hash),
    Ed25519,
    Ed448,
}

impl SigAlg {
    fn from_identifier(algorithm: &AlgorithmIdentifierOwned) -> Result<SigAlg, String> {
        let alg = match algorithm.oid {
            rfc5912::RSA_ENCRYPTION => SigAlg::RsaPlain,
            rfc5912::SHA_1_WITH_RSA_ENCRYPTION => SigAlg::RsaPkcs1(Hash::Sha1),
            rfc5912::SHA_224_WITH_RSA_ENCRYPTION => SigAlg::RsaPkcs1(Hash::Sha224),
            rfc5912::SHA_256_WITH_RSA_ENCRYPTION => SigAlg::RsaPkcs1(Hash::Sha256),
            rfc5912::SHA_384_WITH_RSA_ENCRYPTION => SigAlg::RsaPkcs1(Hash::Sha384),
            rfc5912::SHA_512_WITH_RSA_ENCRYPTION => SigAlg::RsaPkcs1(Hash::Sha512),
            rfc5912::ID_RSASSA_PSS => return parse_pss(algorithm),
            ECDSA_WITH_SHA_1 => SigAlg::Ecdsa(Hash::Sha1),
            rfc5912::ECDSA_WITH_SHA_224 => SigAlg::Ecdsa(Hash::Sha224),
            rfc5912::ECDSA_WITH_SHA_256 => SigAlg::Ecdsa(Hash::Sha256),
            rfc5912::ECDSA_WITH_SHA_384 => SigAlg::Ecdsa(Hash::Sha384),
            rfc5912::ECDSA_WITH_SHA_512 => SigAlg::Ecdsa(Hash::Sha512),
            rfc5912::DSA_WITH_SHA_1 => SigAlg::Dsa(Hash::Sha1),
            rfc5912::DSA_WITH_SHA_224 => SigAlg::Dsa(Hash::Sha224),
            rfc5912::DSA_WITH_SHA_256 => SigAlg::Dsa(Hash::Sha256),
            rfc8410::ID_ED_25519 => SigAlg::Ed25519,
            rfc8410::ID_ED_448 => SigAlg::Ed448,
            _ => return Err(format!("unsupported signature algorithm {}", algorithm.oid)),
        };
        Ok(alg)
    }

    /// The digest the algorithm identifier implies; `rsaEncryption` implies none, so the
    /// signer's digest algorithm applies.
    fn hash(self) -> Option<Hash> {
        match self {
            SigAlg::RsaPlain => None,
            SigAlg::RsaPkcs1(hash)
            | SigAlg::RsaPss { hash, .. }
            | SigAlg::Ecdsa(hash)
            | SigAlg::Dsa(hash) => Some(hash),
            SigAlg::Ed25519 => Some(Hash::Sha512),
            SigAlg::Ed448 => Some(Hash::Shake256512),
        }
    }
}

#[derive(der::Sequence)]
struct PssParameters<'a> {
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    hash: Option<spki::AlgorithmIdentifierRef<'a>>,
    #[asn1(context_specific = "1", tag_mode = "EXPLICIT", optional = "true")]
    mask: Option<spki::AlgorithmIdentifier<spki::AlgorithmIdentifierRef<'a>>>,
    #[asn1(context_specific = "2", tag_mode = "EXPLICIT", optional = "true")]
    salt_len: Option<u32>,
    #[asn1(context_specific = "3", tag_mode = "EXPLICIT", optional = "true")]
    trailer: Option<u32>,
}

fn parse_pss(algorithm: &AlgorithmIdentifierOwned) -> Result<SigAlg, String> {
    let parameters: PssParameters<'_> = algorithm
        .parameters
        .as_ref()
        .ok_or("RSA-PSS parameters missing")?
        .decode_as()
        .map_err(|e| format!("RSA-PSS parameters: {e}"))?;
    let hash = parameters
        .hash
        .map(|value| {
            Hash::from_oid(&value.oid)
                .ok_or_else(|| format!("unsupported RSA-PSS digest {}", value.oid))
        })
        .transpose()?
        .unwrap_or(Hash::Sha1);
    let mask_hash = match parameters.mask {
        None => Hash::Sha1,
        Some(mask) => {
            if mask.oid != rfc5912::ID_MGF_1 {
                return Err(format!(
                    "unsupported RSA-PSS mask generation algorithm {}",
                    mask.oid
                ));
            }
            let digest = mask.parameters.ok_or("RSA-PSS MGF1 digest missing")?;
            Hash::from_oid(&digest.oid)
                .ok_or_else(|| format!("unsupported RSA-PSS MGF1 digest {}", digest.oid))?
        }
    };
    if parameters.trailer.unwrap_or(1) != 1 {
        return Err("unsupported RSA-PSS trailer field".to_owned());
    }
    let salt_len = usize::try_from(parameters.salt_len.unwrap_or(20))
        .map_err(|e| format!("RSA-PSS salt length: {e}"))?;
    Ok(SigAlg::RsaPss {
        hash,
        mask_hash,
        salt_len,
    })
}

/// A parsed CMS signature: what `validate_sig_integrity` needs before hashing the signed
/// data.
pub struct Signature {
    /// `human_friendly` of the signer certificate's subject.
    pub signer: String,
    /// The signer's digest algorithm: the one the `/ByteRange` digest uses.
    pub digest: Hash,
    signer_info: SignerInfo,
    cert: Certificate,
    /// Every certificate the signed data carries, the signer's included.
    certs: Vec<Certificate>,
    message_digest: Vec<u8>,
    econtent: Option<Any>,
}

/// The three integrity verdicts of one signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The `/ByteRange` digest equals the signed messageDigest attribute.
    pub intact: bool,
    /// The signature over the signed attributes verifies with the embedded certificate.
    pub valid: bool,
    /// `valid`, `intact`, and no weak-algorithm policy violation.
    pub trusted: bool,
}

impl Signature {
    /// Parses a detached PDF signature, signed data over id-data, and locates the signer
    /// certificate. Errors are the texts pyhanko's exceptions carry into
    /// `validation_error`.
    pub fn parse(cms_der: &[u8]) -> Result<Signature, String> {
        Self::parse_as(cms_der, rfc5911::ID_DATA)
    }

    /// [`Signature::parse`] for signed data whose content type is `content_type`, such as
    /// an RFC 3161 time-stamp token over its TSTInfo.
    pub fn parse_as(cms_der: &[u8], content_type: ObjectIdentifier) -> Result<Signature, String> {
        let content = ContentInfo::from_der(trim_der(cms_der)?)
            .map_err(|e| format!("Failed to parse CMS content: {e}"))?;
        if content.content_type != rfc5911::ID_SIGNED_DATA {
            return Err("CMS content is not SignedData".to_owned());
        }
        let signed: SignedData = content
            .content
            .decode_as()
            .map_err(|e| format!("Failed to parse SignedData: {e}"))?;
        let mut signer_infos = signed.signer_infos.0.iter();
        let signer_info = signer_infos
            .next()
            .cloned()
            .ok_or("SignedData has no signer")?;
        if signer_infos.next().is_some() {
            return Err("signed_data should contain exactly one signer info".to_owned());
        }
        let certs: Vec<Certificate> = signed
            .certificates
            .iter()
            .flat_map(|set| set.0.iter())
            .filter_map(|choice| match choice {
                CertificateChoices::Certificate(cert) => Some(cert.clone()),
                CertificateChoices::Other(_) => None,
            })
            .collect();
        let cert = match &signer_info.sid {
            SignerIdentifier::IssuerAndSerialNumber(iasn) => certs.iter().find(|cert| {
                cert.tbs_certificate.issuer == iasn.issuer
                    && cert.tbs_certificate.serial_number == iasn.serial_number
            }),
            SignerIdentifier::SubjectKeyIdentifier(ski) => certs
                .iter()
                .find(|cert| subject_key_id(cert).as_ref() == Some(ski))
                .or_else(|| certs.first().filter(|_| certs.len() == 1)),
        }
        .cloned()
        .ok_or("signer certificate not included in signed data")?;
        let digest = Hash::from_identifier(&signer_info.digest_alg)?;
        let signed_attrs = signer_info
            .signed_attrs
            .as_ref()
            .ok_or("signature has no signed attributes")?;
        let mut message_digest = None;
        let mut signed_type = None;
        for attr in signed_attrs.iter() {
            let value = attr.values.iter().next();
            if attr.oid == rfc5911::ID_MESSAGE_DIGEST {
                let octets: OctetString = value
                    .ok_or("messageDigest attribute has no value")?
                    .decode_as()
                    .map_err(|e| format!("messageDigest attribute: {e}"))?;
                message_digest = Some(octets.as_bytes().to_vec());
            } else if attr.oid == rfc5911::ID_CONTENT_TYPE {
                let oid: ObjectIdentifier = value
                    .ok_or("contentType attribute has no value")?
                    .decode_as()
                    .map_err(|e| format!("contentType attribute: {e}"))?;
                signed_type = Some(oid);
            }
        }
        let message_digest =
            message_digest.ok_or("Signature does not contain a message digest attribute")?;
        if signed_type.is_some_and(|oid| oid != content_type) {
            return Err("Content type mismatch".to_owned());
        }
        Ok(Signature {
            signer: human_friendly(&cert.tbs_certificate.subject),
            digest,
            signer_info,
            cert,
            certs,
            message_digest,
            econtent: signed.encap_content_info.econtent,
        })
    }

    /// The signer certificate.
    pub fn certificate(&self) -> &Certificate {
        &self.cert
    }

    /// Every certificate the signed data carries, the signer's included.
    pub fn certificates(&self) -> &[Certificate] {
        &self.certs
    }

    /// The signature value octets, which a signature time-stamp covers.
    pub fn signature_value(&self) -> &[u8] {
        self.signer_info.signature.as_bytes()
    }

    /// The encapsulated content, when the signed data carries it (a time-stamp token's
    /// TSTInfo).
    pub fn content(&self) -> Result<Option<Vec<u8>>, String> {
        self.econtent
            .as_ref()
            .map(|any| {
                any.decode_as::<OctetString>()
                    .map(OctetString::into_bytes)
                    .map_err(|e| format!("encapsulated content: {e}"))
            })
            .transpose()
    }

    /// The first value of the signed attribute `oid`.
    pub fn signed_attribute(&self, oid: ObjectIdentifier) -> Option<&Any> {
        first_value(self.signer_info.signed_attrs.as_ref(), oid)
    }

    /// The first value of the unsigned attribute `oid`.
    pub fn unsigned_attribute(&self, oid: ObjectIdentifier) -> Option<&Any> {
        first_value(self.signer_info.unsigned_attrs.as_ref(), oid)
    }

    /// The verdicts given the digest of the signed data computed with
    /// [`Signature::digest`].
    pub fn verify(&self, byte_range_digest: &[u8]) -> Result<Verdict, String> {
        let intact = byte_range_digest == self.message_digest.as_slice();
        let alg = SigAlg::from_identifier(&self.signer_info.signature_algorithm)?;
        let sig_hash = alg.hash().unwrap_or(self.digest);
        if sig_hash != self.digest {
            return Err(format!(
                "Digest algorithm mismatch: the signature algorithm implies {sig_hash:?} but the signer digest is {:?}",
                self.digest
            ));
        }
        let signed_attrs = self
            .signer_info
            .signed_attrs
            .as_ref()
            .ok_or("signature has no signed attributes")?;
        check_signing_certificate(&self.cert, signed_attrs)?;
        let signed = signed_attrs.to_der().map_err(|e| e.to_string())?;
        let (valid, weak_key) = verify_with(
            alg,
            sig_hash,
            &self.cert.tbs_certificate.subject_public_key_info,
            &signed,
            self.signer_info.signature.as_bytes(),
        )?;
        let policy_ok = !self.digest.is_weak() && !sig_hash.is_weak() && !weak_key;
        Ok(Verdict {
            intact,
            valid,
            trusted: valid && intact && policy_ok,
        })
    }
}

fn first_value(attrs: Option<&Attributes>, oid: ObjectIdentifier) -> Option<&Any> {
    attrs?
        .iter()
        .find(|attr| attr.oid == oid)
        .and_then(|attr| attr.values.iter().next())
}

/// The certificate's subject key identifier extension.
pub(crate) fn subject_key_id(cert: &Certificate) -> Option<SubjectKeyIdentifier> {
    cert.tbs_certificate
        .get::<SubjectKeyIdentifier>()
        .ok()
        .flatten()
        .map(|(_, ski)| ski)
}

/// Checks `signature` over `data` (hashed with `hash`) and reports it with whether the
/// key is below the policy's minimum size.
fn verify_with(
    alg: SigAlg,
    hash: Hash,
    spki: &SubjectPublicKeyInfoOwned,
    data: &[u8],
    signature: &[u8],
) -> Result<(bool, bool), String> {
    Ok(match alg {
        SigAlg::RsaPlain | SigAlg::RsaPkcs1(_) | SigAlg::RsaPss { .. } => {
            let key = RsaPublicKey::try_from(spki.owned_to_ref())
                .map_err(|e| format!("signer public key: {e}"))?;
            let weak = key.n().bits() < RSA_BITS;
            let pss = match alg {
                SigAlg::RsaPss {
                    mask_hash,
                    salt_len,
                    ..
                } => Some((mask_hash, salt_len)),
                _ => None,
            };
            (
                rsa_verify(key, hash, pss, &hash.digest_bytes(data), signature),
                weak,
            )
        }
        SigAlg::Ecdsa(_) => (
            ecdsa_verify(spki, &hash.digest_bytes(data), signature)?,
            false,
        ),
        SigAlg::Dsa(_) => {
            let key = dsa::VerifyingKey::try_from(spki.owned_to_ref())
                .map_err(|e| format!("signer public key: {e}"))?;
            let weak = key.components().p().bits() < MIN_DSA_BITS;
            let valid = dsa::Signature::try_from(signature)
                .is_ok_and(|sig| key.verify_prehash(&hash.digest_bytes(data), &sig).is_ok());
            (valid, weak)
        }
        SigAlg::Ed25519 => {
            let key = ed25519_dalek::VerifyingKey::try_from(spki.owned_to_ref())
                .map_err(|e| format!("signer public key: {e}"))?;
            let valid = ed25519_dalek::Signature::try_from(signature)
                .is_ok_and(|sig| key.verify_strict(data, &sig).is_ok());
            (valid, false)
        }
        SigAlg::Ed448 => {
            let key = ed448_goldilocks_plus::VerifyingKey::try_from(spki.owned_to_ref())
                .map_err(|e| format!("signer public key: {e}"))?;
            let valid = ed448_goldilocks_plus::Signature::try_from(signature)
                .is_ok_and(|sig| key.verify_raw(&sig, data).is_ok());
            (valid, false)
        }
    })
}

/// Checks an X.509 signature (a certificate's, a CRL's, or an OCSP response's) made with
/// `algorithm` over `data` by the holder of `spki`. Fails when it does not verify;
/// otherwise returns why it falls short of the weak-algorithm policy CMS signatures meet,
/// if it does.
pub(crate) fn verify_signed(
    algorithm: &AlgorithmIdentifierOwned,
    spki: &SubjectPublicKeyInfoOwned,
    data: &[u8],
    signature: &[u8],
) -> Result<Option<&'static str>, String> {
    let alg = SigAlg::from_identifier(algorithm)?;
    let hash = alg
        .hash()
        .ok_or("the signature algorithm names no digest")?;
    let (valid, weak_key) = verify_with(alg, hash, spki, data, signature)?;
    if !valid {
        return Err("the signature does not verify".to_owned());
    }
    Ok(if hash.is_weak() {
        Some("the signature uses SHA-1, which is no longer acceptable")
    } else if weak_key {
        Some("the signing key is too short")
    } else {
        None
    })
}

/// ESS `IssuerSerial` (RFC 5035).
#[derive(Clone, Debug, der::Sequence)]
pub(crate) struct IssuerSerial {
    pub issuer: GeneralNames,
    pub serial_number: SerialNumber,
}

/// ESS `ESSCertIDv2` (RFC 5035). The hash algorithm is DEFAULT SHA-256, so DER omits it
/// then: `None` means SHA-256.
#[derive(Clone, Debug, der::Sequence)]
pub(crate) struct EssCertIdV2 {
    #[asn1(optional = "true")]
    pub hash_algorithm: Option<AlgorithmIdentifierOwned>,
    pub cert_hash: OctetString,
    #[asn1(optional = "true")]
    pub issuer_serial: Option<IssuerSerial>,
}

/// ESS `SigningCertificateV2` (RFC 5035), the PAdES signing-certificate attribute.
#[derive(Clone, Debug, der::Sequence)]
pub(crate) struct SigningCertificateV2 {
    pub certs: Vec<EssCertIdV2>,
    #[asn1(optional = "true")]
    pub policies: Option<Vec<Any>>,
}

/// ESS `ESSCertID` (RFC 2634), always SHA-1.
#[derive(Clone, Debug, der::Sequence)]
struct EssCertId {
    cert_hash: OctetString,
    #[asn1(optional = "true")]
    issuer_serial: Option<IssuerSerial>,
}

/// ESS `SigningCertificate` (RFC 2634), which time-stamp tokens still carry.
#[derive(Clone, Debug, der::Sequence)]
struct SigningCertificate {
    certs: Vec<EssCertId>,
    #[asn1(optional = "true")]
    policies: Option<Vec<Any>>,
}

/// pyhanko's `_check_signing_certificate`: the first certificate a signing-certificate
/// (v2, else v1) attribute names must be the signer's; without the attribute there is
/// nothing to check.
fn check_signing_certificate(cert: &Certificate, signed_attrs: &Attributes) -> Result<(), String> {
    let (hash, cert_hash, issuer_serial) = if let Some(value) =
        first_value(Some(signed_attrs), rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2)
    {
        let attr: SigningCertificateV2 = value
            .decode_as()
            .map_err(|e| format!("signing-certificate-v2 attribute: {e}"))?;
        let id = attr
            .certs
            .into_iter()
            .next()
            .ok_or("signing-certificate-v2 attribute names no certificate")?;
        (
            match &id.hash_algorithm {
                Some(algorithm) => Hash::from_identifier(algorithm)?,
                None => Hash::Sha256,
            },
            id.cert_hash,
            id.issuer_serial,
        )
    } else if let Some(value) = first_value(Some(signed_attrs), rfc5911::ID_AA_SIGNING_CERTIFICATE)
    {
        let attr: SigningCertificate = value
            .decode_as()
            .map_err(|e| format!("signing-certificate attribute: {e}"))?;
        let id = attr
            .certs
            .into_iter()
            .next()
            .ok_or("signing-certificate attribute names no certificate")?;
        (Hash::Sha1, id.cert_hash, id.issuer_serial)
    } else {
        return Ok(());
    };
    let der = cert.to_der().map_err(|e| e.to_string())?;
    let tbs = &cert.tbs_certificate;
    let matches = hash.digest_bytes(&der) == cert_hash.as_bytes()
        && issuer_serial.is_none_or(|id| {
            id.serial_number == tbs.serial_number
                && id
                    .issuer
                    .iter()
                    .any(|name| matches!(name, GeneralName::DirectoryName(dn) if *dn == tbs.issuer))
        });
    if matches {
        Ok(())
    } else {
        Err(format!(
            "Signing certificate attribute does not match selected signer's certificate for subject\"{}\".",
            human_friendly(&tbs.subject)
        ))
    }
}

/// The first DER value in `bytes`, without the zero padding a `/Contents` placeholder
/// leaves after it (asn1crypto's `load` ignores trailing data the same way).
fn trim_der(bytes: &[u8]) -> Result<&[u8], String> {
    use der::{Header, Reader, SliceReader};
    let mut reader =
        SliceReader::new(bytes).map_err(|e| format!("Failed to parse CMS content: {e}"))?;
    let header =
        Header::decode(&mut reader).map_err(|e| format!("Failed to parse CMS content: {e}"))?;
    let body =
        usize::try_from(header.length).map_err(|e| format!("Failed to parse CMS content: {e}"))?;
    let start = usize::try_from(reader.position())
        .map_err(|e| format!("Failed to parse CMS content: {e}"))?;
    bytes
        .get(..start.saturating_add(body))
        .ok_or_else(|| "Failed to parse CMS content: truncated".to_owned())
}

fn rsa_verify(
    key: RsaPublicKey,
    hash: Hash,
    pss: Option<(Hash, usize)>,
    prehash: &[u8],
    signature: &[u8],
) -> bool {
    fn run<D: Digest + AssociatedOid>(
        key: RsaPublicKey,
        pss: Option<(Hash, usize)>,
        prehash: &[u8],
        signature: &[u8],
    ) -> bool {
        match pss {
            Some((mask_hash, salt_len)) => {
                rsa_pss_verify::<D>(&key, mask_hash, salt_len, prehash, signature)
            }
            None => rsa::pkcs1v15::Signature::try_from(signature).is_ok_and(|sig| {
                rsa::pkcs1v15::VerifyingKey::<D>::new(key)
                    .verify_prehash(prehash, &sig)
                    .is_ok()
            }),
        }
    }
    match hash {
        Hash::Sha1 => run::<Sha1>(key, pss, prehash, signature),
        Hash::Sha224 => run::<Sha224>(key, pss, prehash, signature),
        Hash::Sha256 => run::<Sha256>(key, pss, prehash, signature),
        Hash::Sha384 => run::<Sha384>(key, pss, prehash, signature),
        Hash::Sha512 => run::<Sha512>(key, pss, prehash, signature),
        Hash::Shake256512 => false,
    }
}

/// RFC 8017 §9.1.2 permits the message and MGF1 to use different digests.
fn rsa_pss_verify<D: Digest>(
    key: &RsaPublicKey,
    mask_hash: Hash,
    salt_len: usize,
    prehash: &[u8],
    signature: &[u8],
) -> bool {
    let hash_len = <D as Digest>::output_size();
    let em_bits = key.n().bits().saturating_sub(1);
    let em_len = em_bits.div_ceil(8);
    if signature.len() != key.size() || prehash.len() != hash_len {
        return false;
    }
    let Some(max_salt_len) = em_len.checked_sub(hash_len + 2) else {
        return false;
    };
    if salt_len > max_salt_len {
        return false;
    }
    let representative = rsa::BigUint::from_bytes_be(signature);
    if &representative >= key.n() {
        return false;
    }
    let Ok(message) = rsa::hazmat::rsa_encrypt(key, &representative) else {
        return false;
    };
    let mut encoded = message.to_bytes_be();
    if encoded.len() > em_len {
        return false;
    }
    if encoded.len() < em_len {
        let padding = em_len - encoded.len();
        encoded.resize(em_len, 0);
        encoded.copy_within(..em_len - padding, padding);
        encoded[..padding].fill(0);
    }
    let (masked_db, tail) = encoded.split_at_mut(em_len - hash_len - 1);
    let (expected_hash, trailer) = tail.split_at(hash_len);
    let top_mask = 0xff_u8 >> (8 * em_len - em_bits);
    if trailer != [0xbc] || masked_db[0] & !top_mask != 0 {
        return false;
    }
    if !mgf1_xor(mask_hash, masked_db, expected_hash) {
        return false;
    }
    masked_db[0] &= top_mask;
    let salt_start = masked_db.len() - salt_len;
    if masked_db[salt_start - 1] != 1 || masked_db[..salt_start - 1].iter().any(|byte| *byte != 0) {
        return false;
    }
    let mut hasher = D::new();
    hasher.update([0; 8]);
    hasher.update(prehash);
    hasher.update(&masked_db[salt_start..]);
    hasher.finalize().as_slice() == expected_hash
}

fn mgf1_xor(hash: Hash, masked: &mut [u8], seed: &[u8]) -> bool {
    fn run<D: Digest>(masked: &mut [u8], seed: &[u8]) {
        for (counter, chunk) in (0_u32..).zip(masked.chunks_mut(<D as Digest>::output_size())) {
            let mut hasher = D::new();
            hasher.update(seed);
            hasher.update(counter.to_be_bytes());
            for (byte, mask) in chunk.iter_mut().zip(hasher.finalize()) {
                *byte ^= mask;
            }
        }
    }
    match hash {
        Hash::Sha1 => run::<Sha1>(masked, seed),
        Hash::Sha224 => run::<Sha224>(masked, seed),
        Hash::Sha256 => run::<Sha256>(masked, seed),
        Hash::Sha384 => run::<Sha384>(masked, seed),
        Hash::Sha512 => run::<Sha512>(masked, seed),
        Hash::Shake256512 => return false,
    }
    true
}

fn ecdsa_verify(
    spki: &SubjectPublicKeyInfoOwned,
    prehash: &[u8],
    signature: &[u8],
) -> Result<bool, String> {
    if spki.algorithm.oid != rfc5912::ID_EC_PUBLIC_KEY {
        return Err("signer public key is not an EC key".to_owned());
    }
    let curve: ObjectIdentifier = spki
        .algorithm
        .parameters
        .as_ref()
        .ok_or("EC named curve missing")?
        .decode_as()
        .map_err(|e| format!("EC named curve: {e}"))?;
    let public = spki
        .subject_public_key
        .as_bytes()
        .ok_or("EC public key is not byte-aligned")?;
    macro_rules! verify_curve {
        ($curve:ident) => {{
            let key = $curve::ecdsa::VerifyingKey::from_sec1_bytes(public)
                .map_err(|e| format!("signer public key: {e}"))?;
            Ok($curve::ecdsa::Signature::from_der(signature)
                .is_ok_and(|sig| key.verify_prehash(prehash, &sig).is_ok()))
        }};
    }
    match curve {
        rfc5912::SECP_256_R_1 => verify_curve!(p256),
        rfc5912::SECP_384_R_1 => verify_curve!(p384),
        rfc5912::SECP_521_R_1 => verify_curve!(p521),
        SECP_256_K_1 => {
            let key = k256::ecdsa::VerifyingKey::from_sec1_bytes(public)
                .map_err(|e| format!("signer public key: {e}"))?;
            Ok(
                k256::ecdsa::Signature::from_der(signature).is_ok_and(|sig| {
                    // CMS accepts both ECDSA representatives; k256 verifies only low-S signatures.
                    let sig = sig.normalize_s().unwrap_or(sig);
                    key.verify_prehash(prehash, &sig).is_ok()
                }),
            )
        }
        _ => Err(format!("unsupported EC named curve {curve}")),
    }
}

/// asn1crypto's `Name.human_friendly`: `Label: value` per attribute, joined with `, `
/// (`; ` when any value holds a comma) in reverse order; a name ending in a country
/// component reverses the labels first, and repeated labels collect their values.
pub fn human_friendly(name: &Name) -> String {
    let mut data: Vec<(String, Vec<String>)> = Vec::new();
    let mut last_label = None;
    for rdn in name.0.iter() {
        for atv in rdn.0.iter() {
            let label = attribute_label(&atv.oid);
            let value = any_to_text(&atv.value);
            match data.iter_mut().find(|(existing, _)| *existing == label) {
                Some((_, values)) => values.insert(0, value),
                None => data.push((label.clone(), vec![value])),
            }
            last_label = Some(label);
        }
    }
    if last_label.as_deref() == Some("Country") {
        data.reverse();
    }
    let to_join: Vec<String> = data
        .iter()
        .map(|(label, values)| format!("{label}: {}", values.join(", ")))
        .collect();
    let separator = if to_join.iter().any(|element| element.contains(',')) {
        "; "
    } else {
        ", "
    };
    to_join
        .iter()
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(separator)
}

/// The first common-name value of `name`.
pub(crate) fn common_name(name: &Name) -> Option<String> {
    name.0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .find(|atv| atv.oid == rfc4519::CN)
        .map(|atv| any_to_text(&atv.value))
}

fn attribute_label(oid: &ObjectIdentifier) -> String {
    let label = match oid.to_string().as_str() {
        "2.5.4.3" => "Common Name",
        "2.5.4.4" => "Surname",
        "2.5.4.5" => "Serial Number",
        "2.5.4.6" => "Country",
        "2.5.4.7" => "Locality",
        "2.5.4.8" => "State/Province",
        "2.5.4.9" => "Street Address",
        "2.5.4.10" => "Organization",
        "2.5.4.11" => "Organizational Unit",
        "2.5.4.12" => "Title",
        "2.5.4.15" => "Business Category",
        "2.5.4.17" => "Postal Code",
        "2.5.4.20" => "Telephone Number",
        "2.5.4.41" => "Name",
        "2.5.4.42" => "Given Name",
        "2.5.4.43" => "Initials",
        "2.5.4.44" => "Generation Qualifier",
        "2.5.4.45" => "Unique Identifier",
        "2.5.4.46" => "DN Qualifier",
        "2.5.4.65" => "Pseudonym",
        "2.5.4.97" => "Organization Identifier",
        "1.2.840.113549.1.9.1" => "Email Address",
        "0.9.2342.19200300.100.1.1" => "User ID",
        "0.9.2342.19200300.100.1.25" => "Domain Component",
        other => return other.to_owned(),
    };
    label.to_owned()
}

/// The text of a directory string of any ASN.1 string type.
fn any_to_text(value: &Any) -> String {
    let decoded = match value.tag() {
        Tag::Utf8String => value
            .decode_as::<Utf8StringRef<'_>>()
            .ok()
            .map(|s| s.as_str().to_owned()),
        Tag::PrintableString => value
            .decode_as::<PrintableStringRef<'_>>()
            .ok()
            .map(|s| s.as_str().to_owned()),
        Tag::Ia5String => value
            .decode_as::<Ia5StringRef<'_>>()
            .ok()
            .map(|s| s.as_str().to_owned()),
        Tag::TeletexString => value
            .decode_as::<TeletexStringRef<'_>>()
            .ok()
            .map(|s| s.as_str().to_owned()),
        Tag::BmpString => {
            let units: Vec<u16> = value
                .value()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_be_bytes(*pair))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    };
    decoded.unwrap_or_else(|| String::from_utf8_lossy(value.value()).into_owned())
}
