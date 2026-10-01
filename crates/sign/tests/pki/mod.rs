//! An in-test public-key infrastructure: a root certificate authority, signing keys of
//! each supported type, PKCS#12 files holding them in the modern (PBES2, AES-256,
//! HMAC-SHA-256) and legacy (triple DES and RC2, HMAC-SHA-1) encodings, an RFC 3161
//! time-stamp authority, and the root's OCSP responder and CRL, each served over HTTP on
//! a loopback port.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockEncryptMut, InnerIvInit, KeyIvInit};
use cms::builder::{SignedDataBuilder, SignerInfoBuilder};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::encrypted_data::EncryptedData;
use cms::enveloped_data::EncryptedContentInfo;
use cms::signed_data::{EncapsulatedContentInfo, SignerIdentifier};
use const_oid::ObjectIdentifier;
use const_oid::db::{rfc5280, rfc5911, rfc5912, rfc6268};
use der::asn1::{Ia5String, Int, OctetString};
use der::{Any, DateTime, Decode, Encode, Sequence, Tag};
use hmac::{Hmac, Mac};
use pkcs12::cert_type::CertBag;
use pkcs12::digest_info::DigestInfo;
use pkcs12::kdf::{Pkcs12KeyType, derive_key_utf8};
use pkcs12::mac_data::MacData;
use pkcs12::pbe_params::{EncryptedPrivateKeyInfo, Pbes2Params, Pbkdf2Params, Pkcs12PbeParams};
use pkcs12::pfx::{Pfx, Version};
use pkcs12::safe_bag::SafeBag;
use rand::RngCore;
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
use rsa::signature::Signer;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use spki::{
    AlgorithmIdentifierOwned, DynSignatureAlgorithmIdentifier, SignatureBitStringEncoding,
    SubjectPublicKeyInfoOwned,
};
use x509_cert::Certificate;
use x509_cert::Version as CertVersion;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::crl::{CertificateList, RevokedCert, TbsCertList};
use x509_cert::ext::pkix::crl::dp::DistributionPoint;
use x509_cert::ext::pkix::name::{DistributionPointName, GeneralName};
use x509_cert::ext::pkix::{
    AccessDescription, AuthorityInfoAccessSyntax, CrlDistributionPoints, ExtendedKeyUsage,
};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::{Time, Validity};
use x509_ocsp::builder::OcspResponseBuilder;
use x509_ocsp::{CertStatus, OcspGeneralizedTime, OcspRequest, RevokedInfo, SingleResponse};

/// `pkcs5PBES2`, which const-oid's database lacks.
const PBES2: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.5.13");
/// `id-ct-TSTInfo`, the content type of a time-stamp token.
const ID_CT_TST_INFO: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.1.4");
/// The policy the in-test time-stamp authority stamps under.
const TSA_POLICY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1");
const ITERATIONS: u16 = 2048;
const DAY: Duration = Duration::from_secs(86_400);
const MINUTE: Duration = Duration::from_secs(60);
/// How long before now the revocation services date a revocation.
const REVOKED_AGO: Duration = Duration::from_secs(2 * 3600);

fn generate_rsa() -> RsaPrivateKey {
    RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("RSA key generation")
}

/// A signing key of one of the types `security sign --p12` accepts.
pub enum Key {
    Rsa(Box<RsaPrivateKey>),
    P256(p256::SecretKey),
    P384(p384::SecretKey),
}

impl Key {
    /// An RSA-2048 key, generated once per test process.
    pub fn rsa() -> Key {
        static RSA: LazyLock<RsaPrivateKey> = LazyLock::new(generate_rsa);
        Key::Rsa(Box::new(RSA.clone()))
    }

    pub fn p256() -> Key {
        Key::P256(p256::SecretKey::random(&mut rand::thread_rng()))
    }

    pub fn p384() -> Key {
        Key::P384(p384::SecretKey::random(&mut rand::thread_rng()))
    }

    fn spki(&self) -> SubjectPublicKeyInfoOwned {
        let der = match self {
            Key::Rsa(key) => key.to_public_key().to_public_key_der(),
            Key::P256(key) => key.public_key().to_public_key_der(),
            Key::P384(key) => key.public_key().to_public_key_der(),
        }
        .expect("public key DER");
        SubjectPublicKeyInfoOwned::from_der(der.as_bytes()).expect("public key info")
    }

    /// The PKCS#8 `PrivateKeyInfo` DER.
    pub fn pkcs8(&self) -> Vec<u8> {
        match self {
            Key::Rsa(key) => key.to_pkcs8_der(),
            Key::P256(key) => key.to_pkcs8_der(),
            Key::P384(key) => key.to_pkcs8_der(),
        }
        .expect("private key DER")
        .as_bytes()
        .to_vec()
    }
}

/// A self-signed root certificate authority with an RSA-2048 key.
#[derive(Clone)]
pub struct Ca {
    pub cert: Certificate,
    key: rsa::pkcs1v15::SigningKey<Sha256>,
}

impl Ca {
    pub fn new(subject: &str) -> Ca {
        static KEY: LazyLock<RsaPrivateKey> = LazyLock::new(generate_rsa);
        let key = rsa::pkcs1v15::SigningKey::<Sha256>::new(KEY.clone());
        let builder = CertificateBuilder::new(
            Profile::Root,
            serial(),
            validity(),
            name(subject),
            Key::Rsa(Box::new(KEY.clone())).spki(),
            &key,
        )
        .expect("root builder");
        let cert = builder
            .build::<rsa::pkcs1v15::Signature>()
            .expect("root certificate");
        Ca { cert, key }
    }

    /// An end-entity certificate for `key`, with `extend` adding extensions beyond the
    /// leaf profile's key usage (digital signature, non-repudiation).
    pub fn issue(
        &self,
        subject: &str,
        key: &Key,
        extend: impl FnOnce(&mut CertificateBuilder<'_, rsa::pkcs1v15::SigningKey<Sha256>>),
    ) -> Certificate {
        let profile = Profile::Leaf {
            issuer: self.cert.tbs_certificate.subject.clone(),
            enable_key_agreement: false,
            enable_key_encipherment: false,
            include_subject_key_identifier: true,
        };
        let mut builder = CertificateBuilder::new(
            profile,
            serial(),
            validity(),
            name(subject),
            key.spki(),
            &self.key,
        )
        .expect("leaf builder");
        extend(&mut builder);
        builder
            .build::<rsa::pkcs1v15::Signature>()
            .expect("leaf certificate")
    }

    /// The OCSP response, signed by the root itself, to the DER request `query`: each
    /// certificate asked about is good unless `revoked` holds its serial. The answer is
    /// current from a minute ago for a day and echoes the request's nonce.
    fn ocsp(&self, query: &[u8], revoked: &[SerialNumber]) -> Vec<u8> {
        let request = OcspRequest::from_der(query).expect("OCSP request");
        let now = SystemTime::now();
        let mut builder = OcspResponseBuilder::new(self.cert.tbs_certificate.subject.clone());
        for single in &request.tbs_request.request_list {
            let id = single.req_cert.clone();
            let status = if revoked.contains(&id.serial_number) {
                CertStatus::revoked(RevokedInfo {
                    revocation_time: ocsp_time(now - REVOKED_AGO),
                    revocation_reason: None,
                })
            } else {
                CertStatus::good()
            };
            builder = builder.with_single_response(
                SingleResponse::new(id, status, ocsp_time(now - MINUTE))
                    .with_next_update(ocsp_time(now + DAY)),
            );
        }
        if let Some(nonce) = request.nonce() {
            builder = builder.with_extension(nonce).expect("nonce echo");
        }
        let mut key = self.key.clone();
        builder
            .sign::<_, rsa::pkcs1v15::Signature>(&mut key, None, ocsp_time(now))
            .expect("OCSP response")
            .to_der()
            .expect("OCSP response DER")
    }

    /// The root's CRL, listing the serials in `revoked`, current from a minute ago for a
    /// day.
    fn crl(&self, revoked: &[SerialNumber]) -> Vec<u8> {
        let now = SystemTime::now();
        let entries: Vec<RevokedCert> = revoked
            .iter()
            .map(|serial| RevokedCert {
                serial_number: serial.clone(),
                revocation_date: Time::try_from(now - REVOKED_AGO).expect("revocation date"),
                crl_entry_extensions: None,
            })
            .collect();
        let signature_algorithm = self
            .key
            .signature_algorithm_identifier()
            .expect("CRL signature algorithm");
        let tbs_cert_list = TbsCertList {
            version: CertVersion::V2,
            signature: signature_algorithm.clone(),
            issuer: self.cert.tbs_certificate.subject.clone(),
            this_update: Time::try_from(now - MINUTE).expect("this update"),
            next_update: Some(Time::try_from(now + DAY).expect("next update")),
            revoked_certificates: (!entries.is_empty()).then_some(entries),
            crl_extensions: None,
        };
        let signature: rsa::pkcs1v15::Signature = self
            .key
            .sign(&tbs_cert_list.to_der().expect("CRL contents"));
        CertificateList {
            tbs_cert_list,
            signature_algorithm,
            signature: signature.to_bitstring().expect("CRL signature"),
        }
        .to_der()
        .expect("CRL DER")
    }
}

fn ocsp_time(time: SystemTime) -> OcspGeneralizedTime {
    OcspGeneralizedTime::try_from(time).expect("OCSP time")
}

/// The root's revocation services on a loopback port: an OCSP responder at `/ocsp` and a
/// CRL at `/crl`. They report as revoked two hours ago each certificate
/// [`Services::revoke`] names.
pub struct Services {
    url: String,
    revoked: Arc<Mutex<Vec<SerialNumber>>>,
}

impl Services {
    pub fn new(ca: &Ca) -> Services {
        let revoked = Arc::new(Mutex::new(Vec::new()));
        let listed = Arc::clone(&revoked);
        let ca = ca.clone();
        let url = serve(move |path, body| {
            let revoked = listed.lock().expect("revoked serials").clone();
            match path {
                "/ocsp" => ca.ocsp(body, &revoked),
                "/crl" => ca.crl(&revoked),
                _ => Vec::new(),
            }
        });
        Services { url, revoked }
    }

    /// Makes every later answer report `cert` as revoked.
    pub fn revoke(&self, cert: &Certificate) {
        self.revoked
            .lock()
            .expect("revoked serials")
            .push(cert.tbs_certificate.serial_number.clone());
    }

    /// The extension that names the OCSP responder.
    pub fn ocsp_pointer(&self) -> AuthorityInfoAccessSyntax {
        AuthorityInfoAccessSyntax(vec![AccessDescription {
            access_method: rfc5280::ID_AD_OCSP,
            access_location: self.address("/ocsp"),
        }])
    }

    /// The extension that names the CRL.
    pub fn crl_pointer(&self) -> CrlDistributionPoints {
        CrlDistributionPoints(vec![DistributionPoint {
            distribution_point: Some(DistributionPointName::FullName(vec![self.address("/crl")])),
            reasons: None,
            crl_issuer: None,
        }])
    }

    fn address(&self, path: &str) -> GeneralName {
        GeneralName::UniformResourceIdentifier(
            Ia5String::new(&format!("{}{path}", self.url)).expect("service URL"),
        )
    }
}

fn name(subject: &str) -> Name {
    Name::from_str(subject).expect("subject name")
}

fn serial() -> SerialNumber {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[0] &= 0x7f;
    bytes[0] |= 0x01;
    SerialNumber::new(&bytes).expect("serial number")
}

fn validity() -> Validity {
    let now = SystemTime::now();
    Validity {
        not_before: Time::try_from(now - DAY).expect("not before"),
        not_after: Time::try_from(now + 365 * DAY).expect("not after"),
    }
}

/// Answers every HTTP request to a loopback port with `answer(path, body)` until the test
/// process exits, and returns the server's URL.
pub fn serve(answer: impl Fn(&str, &[u8]) -> Vec<u8> + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("connection");
            let mut reader = BufReader::new(stream.try_clone().expect("stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).expect("header");
                if line.trim_end().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("request body");
            let reply = answer(&path, &body);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            )
            .expect("response head");
            stream.write_all(&reply).expect("response body");
        }
    });
    url
}

#[derive(Sequence)]
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
    #[asn1(optional = "true")]
    cert_req: Option<bool>,
}

#[derive(Sequence)]
struct TstInfo {
    version: u8,
    policy: ObjectIdentifier,
    message_imprint: MessageImprint,
    serial_number: Int,
    gen_time: Any,
    #[asn1(optional = "true")]
    nonce: Option<Int>,
}

#[derive(Sequence)]
struct PkiStatusInfo {
    status: u8,
}

#[derive(Sequence)]
struct TimeStampResp {
    status: PkiStatusInfo,
    time_stamp_token: Any,
}

/// What a [`Tsa`] time-stamps.
#[derive(Clone, Copy)]
pub enum Imprint {
    /// The digest the request asks for.
    Asked,
    /// Another digest of the same length: a token for some other signature.
    Other,
}

/// A time-stamp authority the root certified for time-stamping; every token it issues
/// attests the same time, an hour ago.
pub struct Tsa {
    cert: Certificate,
    key: rsa::pkcs1v15::SigningKey<Sha256>,
    time: DateTime,
}

impl Tsa {
    pub fn new(ca: &Ca) -> Tsa {
        Tsa::issued(ca, |_| {})
    }

    /// An authority whose certificate `extend` adds extensions to beyond time-stamping
    /// usage.
    pub fn issued(
        ca: &Ca,
        extend: impl FnOnce(&mut CertificateBuilder<'_, rsa::pkcs1v15::SigningKey<Sha256>>),
    ) -> Tsa {
        static KEY: LazyLock<RsaPrivateKey> = LazyLock::new(generate_rsa);
        let cert = ca.issue(
            "CN=Goat TSA,O=Goat Test",
            &Key::Rsa(Box::new(KEY.clone())),
            |builder| {
                builder
                    .add_extension(&ExtendedKeyUsage(vec![rfc5280::ID_KP_TIME_STAMPING]))
                    .expect("time-stamping usage");
                extend(builder);
            },
        );
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        Tsa {
            cert,
            key: rsa::pkcs1v15::SigningKey::new(KEY.clone()),
            time: DateTime::from_system_time(an_hour_ago).expect("token time"),
        }
    }

    /// The time the tokens attest, as ISO 8601 in UTC.
    pub fn iso_time(&self) -> String {
        let t = &self.time;
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minutes(),
            t.seconds()
        )
    }

    /// The RFC 3161 response to the DER request `query`.
    pub fn reply(&self, query: &[u8], imprint: Imprint) -> Vec<u8> {
        let request = TimeStampReq::from_der(query).expect("time-stamp request");
        let mut message_imprint = request.message_imprint;
        if let Imprint::Other = imprint {
            let other: Vec<u8> = message_imprint
                .hashed_message
                .as_bytes()
                .iter()
                .map(|b| !b)
                .collect();
            message_imprint.hashed_message = OctetString::new(other).expect("other imprint");
        }
        let token = self.sign(message_imprint, request.nonce);
        TimeStampResp {
            status: PkiStatusInfo { status: 0 },
            time_stamp_token: Any::encode_from(&token).expect("token value"),
        }
        .to_der()
        .expect("time-stamp response")
    }

    /// A token over the SHA-256 digest `digest`, as the authority hands one out.
    pub fn token(&self, digest: &[u8]) -> Vec<u8> {
        let message_imprint = MessageImprint {
            hash_algorithm: AlgorithmIdentifierOwned {
                oid: rfc5912::ID_SHA_256,
                parameters: None,
            },
            hashed_message: OctetString::new(digest).expect("imprint"),
        };
        self.sign(message_imprint, None)
            .to_der()
            .expect("time-stamp token")
    }

    /// The token attesting the authority's time for `message_imprint`.
    fn sign(&self, message_imprint: MessageImprint, nonce: Option<Int>) -> ContentInfo {
        let t = &self.time;
        // With a fraction of a second, as some authorities send it.
        let gen_time = format!(
            "{:04}{:02}{:02}{:02}{:02}{:02}.25Z",
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minutes(),
            t.seconds()
        );
        let info = TstInfo {
            version: 1,
            policy: TSA_POLICY,
            message_imprint,
            serial_number: Int::new(&[1]).expect("token serial"),
            gen_time: Any::new(Tag::GeneralizedTime, gen_time.as_bytes()).expect("genTime"),
            nonce,
        };
        let eci = EncapsulatedContentInfo {
            econtent_type: ID_CT_TST_INFO,
            econtent: Some(
                Any::new(Tag::OctetString, info.to_der().expect("TSTInfo"))
                    .expect("TSTInfo octets"),
            ),
        };
        let digest_alg = AlgorithmIdentifierOwned {
            oid: rfc5912::ID_SHA_256,
            parameters: None,
        };
        let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: self.cert.tbs_certificate.issuer.clone(),
            serial_number: self.cert.tbs_certificate.serial_number.clone(),
        });
        let signer_info = SignerInfoBuilder::new(&self.key, sid, digest_alg.clone(), &eci, None)
            .expect("TSA signer info");
        let mut builder = SignedDataBuilder::new(&eci);
        builder
            .add_digest_algorithm(digest_alg)
            .expect("digest algorithm")
            .add_certificate(CertificateChoices::Certificate(self.cert.clone()))
            .expect("TSA certificate")
            .add_signer_info::<rsa::pkcs1v15::SigningKey<Sha256>, rsa::pkcs1v15::Signature>(
                signer_info,
            )
            .expect("TSA signature")
            .build()
            .expect("time-stamp token")
    }
}

/// How a PKCS#12 file seals its contents.
#[derive(Clone, Copy, Debug)]
pub enum Sealing {
    /// OpenSSL 3's default: PBES2 with PBKDF2-HMAC-SHA-256 and AES-256-CBC, HMAC-SHA-256.
    Modern,
    /// macOS Keychain and `openssl -legacy`: the key under triple DES, the certificates
    /// under 40-bit RC2, HMAC-SHA-1.
    Legacy,
}

/// The PKCS#12 integrity MAC (RFC 7292 appendix B key derivation, then HMAC) of `data`.
macro_rules! pkcs12_mac {
    ($digest:ty, $password:expr, $salt:expr, $data:expr) => {{
        let key = derive_key_utf8::<$digest>(
            $password,
            $salt,
            Pkcs12KeyType::Mac,
            i32::from(ITERATIONS),
            <$digest as Digest>::output_size(),
        )
        .expect("MAC key");
        let mut mac = <Hmac<$digest> as Mac>::new_from_slice(&key).expect("HMAC key");
        mac.update($data);
        mac.finalize().into_bytes().to_vec()
    }};
}

/// A PKCS#12 file holding `key`, its certificate `cert`, and `others`.
pub fn p12(
    key: &Key,
    cert: &Certificate,
    others: &[&Certificate],
    password: &str,
    sealing: Sealing,
) -> Vec<u8> {
    let bags: Vec<SafeBag> = std::iter::once(cert)
        .chain(others.iter().copied())
        .map(|cert| SafeBag {
            bag_id: pkcs12::PKCS_12_CERT_BAG_OID,
            bag_value: CertBag {
                cert_id: pkcs12::PKCS_12_X509_CERT_OID,
                cert_value: OctetString::new(cert.to_der().expect("certificate DER"))
                    .expect("certificate octets"),
            }
            .to_der()
            .expect("certificate bag"),
            bag_attributes: None,
        })
        .collect();
    let (algorithm, sealed) = seal(
        sealing,
        pkcs12::PKCS_12_PBEWITH_SHAAND40_BIT_RC2_CBC,
        password,
        &bags.to_der().expect("certificate bags"),
    );
    let encrypted = EncryptedData {
        version: CmsVersion::V0,
        enc_content_info: EncryptedContentInfo {
            content_type: rfc5911::ID_DATA,
            content_enc_alg: algorithm,
            encrypted_content: Some(OctetString::new(sealed).expect("sealed certificates")),
        },
        unprotected_attrs: None,
    };
    let certificates = ContentInfo {
        content_type: rfc5911::ID_ENCRYPTED_DATA,
        content: Any::encode_from(&encrypted).expect("encrypted data"),
    };

    let (algorithm, sealed) = seal(
        sealing,
        pkcs12::PKCS_12_PBE_WITH_SHAAND3_KEY_TRIPLE_DES_CBC,
        password,
        &key.pkcs8(),
    );
    let shrouded = EncryptedPrivateKeyInfo {
        encryption_algorithm: algorithm,
        encrypted_data: OctetString::new(sealed).expect("sealed key"),
    };
    let key_bags = vec![SafeBag {
        bag_id: pkcs12::PKCS_12_PKCS8_KEY_BAG_OID,
        bag_value: shrouded.to_der().expect("shrouded key"),
        bag_attributes: None,
    }];
    let keys = ContentInfo {
        content_type: rfc5911::ID_DATA,
        content: octets(key_bags.to_der().expect("key bags")),
    };

    let auth_safe = vec![certificates, keys]
        .to_der()
        .expect("authenticated safe");
    let mut salt = [0_u8; 8];
    rand::thread_rng().fill_bytes(&mut salt);
    let (digest_oid, mac) = match sealing {
        Sealing::Modern => (
            rfc5912::ID_SHA_256,
            pkcs12_mac!(Sha256, password, &salt, &auth_safe),
        ),
        Sealing::Legacy => (
            rfc5912::ID_SHA_1,
            pkcs12_mac!(Sha1, password, &salt, &auth_safe),
        ),
    };
    Pfx {
        version: Version::V3,
        auth_safe: ContentInfo {
            content_type: rfc5911::ID_DATA,
            content: octets(auth_safe),
        },
        mac_data: Some(MacData {
            mac: DigestInfo {
                algorithm: AlgorithmIdentifierOwned {
                    oid: digest_oid,
                    parameters: Some(Any::null()),
                },
                digest: OctetString::new(mac).expect("MAC octets"),
            },
            mac_salt: OctetString::new(salt.to_vec()).expect("MAC salt"),
            iterations: i32::from(ITERATIONS),
        }),
    }
    .to_der()
    .expect("PKCS#12 DER")
}

fn octets(data: Vec<u8>) -> Any {
    Any::encode_from(&OctetString::new(data).expect("octets")).expect("octets value")
}

/// Encrypts `data`: PBES2 AES-256 when modern, else the legacy scheme `legacy`.
fn seal(
    sealing: Sealing,
    legacy: ObjectIdentifier,
    password: &str,
    data: &[u8],
) -> (AlgorithmIdentifierOwned, Vec<u8>) {
    let mut rng = rand::thread_rng();
    let mut salt = [0_u8; 8];
    rng.fill_bytes(&mut salt);
    match sealing {
        Sealing::Modern => {
            let mut key = [0_u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(
                password.as_bytes(),
                &salt,
                u32::from(ITERATIONS),
                &mut key,
            );
            let mut iv = [0_u8; 16];
            rng.fill_bytes(&mut iv);
            let sealed = cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &iv)
                .expect("AES key")
                .encrypt_padded_vec_mut::<Pkcs7>(data);
            let kdf = Pbkdf2Params {
                salt: OctetString::new(salt.to_vec()).expect("salt"),
                iteration_count: u32::from(ITERATIONS),
                key_length: None,
                prf: AlgorithmIdentifierOwned {
                    oid: rfc6268::ID_HMAC_WITH_SHA_256,
                    parameters: Some(Any::null()),
                },
            };
            let params = Pbes2Params {
                kdf: AlgorithmIdentifierOwned {
                    oid: rfc5911::ID_PBKDF_2,
                    parameters: Some(Any::encode_from(&kdf).expect("PBKDF2 parameters")),
                },
                encryption: AlgorithmIdentifierOwned {
                    oid: rfc5911::ID_AES_256_CBC,
                    parameters: Some(
                        Any::encode_from(&OctetString::new(iv.to_vec()).expect("IV"))
                            .expect("IV value"),
                    ),
                },
            };
            (
                AlgorithmIdentifierOwned {
                    oid: PBES2,
                    parameters: Some(Any::encode_from(&params).expect("PBES2 parameters")),
                },
                sealed,
            )
        }
        Sealing::Legacy => {
            let rc2 = legacy == pkcs12::PKCS_12_PBEWITH_SHAAND40_BIT_RC2_CBC;
            let key_len = if rc2 { 5 } else { 24 };
            let rounds = i32::from(ITERATIONS);
            let key = derive_key_utf8::<Sha1>(
                password,
                &salt,
                Pkcs12KeyType::EncryptionKey,
                rounds,
                key_len,
            )
            .expect("legacy key");
            let iv = derive_key_utf8::<Sha1>(password, &salt, Pkcs12KeyType::Iv, rounds, 8)
                .expect("legacy IV");
            let sealed = if rc2 {
                let cipher = rc2::Rc2::new_with_eff_key_len(&key, key_len * 8);
                cbc::Encryptor::<rc2::Rc2>::inner_iv_slice_init(cipher, &iv)
                    .expect("RC2 IV")
                    .encrypt_padded_vec_mut::<Pkcs7>(data)
            } else {
                cbc::Encryptor::<des::TdesEde3>::new_from_slices(&key, &iv)
                    .expect("triple DES key")
                    .encrypt_padded_vec_mut::<Pkcs7>(data)
            };
            let params = Pkcs12PbeParams {
                salt: OctetString::new(salt.to_vec()).expect("salt"),
                iterations: rounds,
            };
            (
                AlgorithmIdentifierOwned {
                    oid: legacy,
                    parameters: Some(Any::encode_from(&params).expect("PBE parameters")),
                },
                sealed,
            )
        }
    }
}
