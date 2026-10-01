//! PAdES signatures (`ETSI.CAdES.detached`) with a PKCS#12 identity: CMS signed data over
//! the `/ByteRange` digest carrying the signed attributes PAdES B-B requires
//! (content-type, message-digest, and ESS signing-certificate-v2). There is no
//! signing-time attribute: PAdES puts the claimed time in the signature dictionary's /M.

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedAttributes, SignedData, SignerIdentifier,
    SignerInfo, SignerInfos,
};
use const_oid::ObjectIdentifier;
use const_oid::db::{rfc5911, rfc5912};
use der::asn1::{OctetString, SetOfVec};
use der::{Any, Encode};
use signature::{RandomizedSigner, SignatureEncoding, Signer};
use spki::AlgorithmIdentifierOwned;
use x509_cert::Certificate;
use x509_cert::attr::Attribute;
use x509_cert::ext::pkix::name::GeneralName;

use crate::p12::{Identity, PrivateKey};
use crate::pkcs7::{EssCertIdV2, Hash, IssuerSerial, SigningCertificateV2};

/// Room for the fixed parts of the signed data around the certificates and signature:
/// the signed attributes, algorithm identifiers, and DER headers.
const OVERHEAD_BYTES: usize = 2048;
/// The largest DER ECDSA P-384 signature.
const P384_SIGNATURE_BYTES: usize = 104;
/// The largest DER ECDSA P-256 signature.
const P256_SIGNATURE_BYTES: usize = 72;
/// The RSASSA-PSS salt: the SHA-256 output length, as the algorithm identifier states.
const PSS_SALT_BYTES: usize = 32;

/// A PKCS#12 identity ready to sign.
pub struct CadesSigner {
    identity: Identity,
    pss: bool,
}

impl CadesSigner {
    /// `pss` selects RSASSA-PSS for an RSA key; a key restricted to PSS always uses it.
    pub fn new(identity: Identity, pss: bool) -> Result<CadesSigner, String> {
        if pss && !matches!(identity.key, PrivateKey::Rsa(_) | PrivateKey::RsaPss(_)) {
            return Err("--pss applies only to RSA keys".to_owned());
        }
        Ok(CadesSigner { identity, pss })
    }

    /// The signing certificate.
    pub fn certificate(&self) -> &Certificate {
        &self.identity.cert
    }

    /// The digest of the `/ByteRange` and of the signed attributes: SHA-384 for a P-384
    /// key, SHA-256 otherwise.
    pub fn digest(&self) -> Hash {
        match self.identity.key {
            PrivateKey::P384(_) => Hash::Sha384,
            PrivateKey::Rsa(_) | PrivateKey::RsaPss(_) | PrivateKey::P256(_) => Hash::Sha256,
        }
    }

    /// An upper bound on the DER size of [`CadesSigner::sign`]'s result.
    pub fn size_hint(&self) -> Result<usize, String> {
        let signature = match &self.identity.key {
            PrivateKey::Rsa(key) | PrivateKey::RsaPss(key) => {
                rsa::traits::PublicKeyParts::size(key)
            }
            PrivateKey::P256(_) => P256_SIGNATURE_BYTES,
            PrivateKey::P384(_) => P384_SIGNATURE_BYTES,
        };
        let mut total = signature + OVERHEAD_BYTES;
        for cert in self.certificates() {
            let len = cert.encoded_len().map_err(|e| e.to_string())?;
            total += usize::try_from(len).map_err(|e| e.to_string())?;
        }
        Ok(total)
    }

    /// The certificates the signed data carries: the signer's first, then its chain,
    /// each once.
    fn certificates(&self) -> Vec<&Certificate> {
        let mut out: Vec<&Certificate> = vec![&self.identity.cert];
        for cert in &self.identity.chain {
            if !out.contains(&cert) {
                out.push(cert);
            }
        }
        out
    }

    /// A detached `ContentInfo` (DER) over `message_digest`, the digest of the signed byte
    /// ranges with [`CadesSigner::digest`].
    pub fn sign(&self, message_digest: &[u8]) -> Result<Vec<u8>, String> {
        let cert = &self.identity.cert;
        let digest_alg = AlgorithmIdentifierOwned {
            oid: self.digest().oid(),
            parameters: None,
        };
        let mut attrs = SignedAttributes::new();
        let content_type = Any::encode_from(&rfc5911::ID_DATA).map_err(|e| e.to_string())?;
        let digest = OctetString::new(message_digest).map_err(|e| e.to_string())?;
        let digest = Any::encode_from(&digest).map_err(|e| e.to_string())?;
        let ess = Any::encode_from(&signing_certificate_v2(cert)?).map_err(|e| e.to_string())?;
        for (oid, value) in [
            (rfc5911::ID_CONTENT_TYPE, content_type),
            (rfc5911::ID_MESSAGE_DIGEST, digest),
            (rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2, ess),
        ] {
            attrs
                .insert(attribute(oid, value)?)
                .map_err(|e| e.to_string())?;
        }
        let (signature_algorithm, signature) =
            self.sign_bytes(&attrs.to_der().map_err(|e| e.to_string())?)?;
        let signer_info = SignerInfo {
            version: CmsVersion::V1,
            sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
                issuer: cert.tbs_certificate.issuer.clone(),
                serial_number: cert.tbs_certificate.serial_number.clone(),
            }),
            digest_alg: digest_alg.clone(),
            signed_attrs: Some(attrs),
            signature_algorithm,
            signature: OctetString::new(signature).map_err(|e| e.to_string())?,
            unsigned_attrs: None,
        };
        let mut certificates = SetOfVec::new();
        for cert in self.certificates() {
            certificates
                .insert(CertificateChoices::Certificate(cert.clone()))
                .map_err(|e| e.to_string())?;
        }
        let signed = SignedData {
            version: CmsVersion::V1,
            digest_algorithms: SetOfVec::try_from(vec![digest_alg]).map_err(|e| e.to_string())?,
            encap_content_info: EncapsulatedContentInfo {
                econtent_type: rfc5911::ID_DATA,
                econtent: None,
            },
            certificates: Some(CertificateSet(certificates)),
            crls: None,
            signer_infos: SignerInfos(
                SetOfVec::try_from(vec![signer_info]).map_err(|e| e.to_string())?,
            ),
        };
        ContentInfo {
            content_type: rfc5911::ID_SIGNED_DATA,
            content: Any::encode_from(&signed).map_err(|e| e.to_string())?,
        }
        .to_der()
        .map_err(|e| e.to_string())
    }

    /// The signature algorithm and the signature over `data` (the DER signed attributes).
    fn sign_bytes(&self, data: &[u8]) -> Result<(AlgorithmIdentifierOwned, Vec<u8>), String> {
        let mut rng = rand::thread_rng();
        match &self.identity.key {
            PrivateKey::Rsa(key) if !self.pss => {
                let signer = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(key.clone());
                Ok((
                    AlgorithmIdentifierOwned {
                        oid: rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
                        parameters: Some(Any::null()),
                    },
                    signer.sign_with_rng(&mut rng, data).to_vec(),
                ))
            }
            PrivateKey::Rsa(key) | PrivateKey::RsaPss(key) => {
                let signer = rsa::pss::BlindedSigningKey::<sha2::Sha256>::new_with_salt_len(
                    key.clone(),
                    PSS_SALT_BYTES,
                );
                let algorithm = rsa::pss::get_default_pss_signature_algo_id::<sha2::Sha256>()
                    .map_err(|e| e.to_string())?;
                Ok((algorithm, signer.sign_with_rng(&mut rng, data).to_vec()))
            }
            PrivateKey::P256(key) => {
                let signer = p256::ecdsa::SigningKey::from(key);
                let signature: p256::ecdsa::DerSignature = signer.sign(data);
                Ok((ecdsa(rfc5912::ECDSA_WITH_SHA_256), signature.to_vec()))
            }
            PrivateKey::P384(key) => {
                let signer = p384::ecdsa::SigningKey::from(key);
                let signature: p384::ecdsa::DerSignature = signer.sign(data);
                Ok((ecdsa(rfc5912::ECDSA_WITH_SHA_384), signature.to_vec()))
            }
        }
    }
}

fn ecdsa(oid: ObjectIdentifier) -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid,
        parameters: None,
    }
}

fn attribute(oid: ObjectIdentifier, value: Any) -> Result<Attribute, String> {
    Ok(Attribute {
        oid,
        values: SetOfVec::try_from(vec![value]).map_err(|e| e.to_string())?,
    })
}

/// ESS signing-certificate-v2 naming `cert` by its SHA-256 hash (the default algorithm,
/// so omitted) and its issuer and serial number.
fn signing_certificate_v2(cert: &Certificate) -> Result<SigningCertificateV2, String> {
    let der = cert.to_der().map_err(|e| e.to_string())?;
    Ok(SigningCertificateV2 {
        certs: vec![EssCertIdV2 {
            hash_algorithm: None,
            cert_hash: OctetString::new(Hash::Sha256.digest_bytes(&der))
                .map_err(|e| e.to_string())?,
            issuer_serial: Some(IssuerSerial {
                issuer: vec![GeneralName::DirectoryName(
                    cert.tbs_certificate.issuer.clone(),
                )],
                serial_number: cert.tbs_certificate.serial_number.clone(),
            }),
        }],
        policies: None,
    })
}
