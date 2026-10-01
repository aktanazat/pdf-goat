//! PKCS#12 (`.p12`, `.pfx`) identities for `security sign --p12`: the integrity MAC checked
//! with the password, the certificate and key bags read (PBES2 with PBKDF2 and AES or triple
//! DES, as OpenSSL 3 writes them, or the legacy PKCS#12 PBE with triple DES or RC2, as macOS
//! and `openssl -legacy` write them), and the key matched to its certificate.

use aes::{Aes128, Aes192, Aes256};
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, InnerIvInit, KeyIvInit};
use cms::content_info::ContentInfo;
use cms::encrypted_data::EncryptedData;
use const_oid::ObjectIdentifier;
use const_oid::db::{rfc5911, rfc5912, rfc6268};
use der::asn1::OctetString;
use der::{AnyRef, Decode, Sequence};
use hmac::{Hmac, Mac};
use pkcs12::cert_type::CertBag;
use pkcs12::kdf::{Pkcs12KeyType, derive_key_utf8};
use pkcs12::pbe_params::{EncryptedPrivateKeyInfo, Pbes2Params, Pkcs12PbeParams};
use pkcs12::pfx::Pfx;
use pkcs12::safe_bag::SafeBag;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs8::DecodePrivateKey;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use spki::AlgorithmIdentifierOwned;
use x509_cert::Certificate;

/// `pkcs5PBES2`, which const-oid's database lacks.
const PBES2: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.5.13");
/// PKCS#5 `hmacWithSHA1`, the PBKDF2 default; const-oid's `HMAC_SHA_1` is the IPsec one.
const HMAC_WITH_SHA_1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.2.7");
/// Nested bags, one level at a time, as far as any real file goes.
const MAX_BAG_DEPTH: usize = 8;

/// A private key `security sign` can sign with.
pub enum PrivateKey {
    Rsa(RsaPrivateKey),
    /// An RSA key whose PKCS#8 algorithm is id-RSASSA-PSS: it signs only with PSS.
    RsaPss(RsaPrivateKey),
    P256(p256::SecretKey),
    P384(p384::SecretKey),
}

/// The key, its certificate, and the other certificates of the file, issuers first in
/// chain order from the certificate up.
pub struct Identity {
    pub key: PrivateKey,
    pub cert: Certificate,
    pub chain: Vec<Certificate>,
}

/// PBKDF2-params (RFC 8018 A.2). The `pkcs12` crate's version requires `prf`, which the
/// RFC makes DEFAULT hmacWithSHA1, so files that omit it would not parse.
#[derive(Sequence)]
struct Pbkdf2Params {
    salt: OctetString,
    iteration_count: u32,
    #[asn1(optional = "true")]
    key_length: Option<u32>,
    #[asn1(optional = "true")]
    prf: Option<AlgorithmIdentifierOwned>,
}

/// Reads the identity in the PKCS#12 file `data` with `password` (empty when the file has
/// none).
pub fn load(data: &[u8], password: &str) -> Result<Identity, String> {
    let pfx = Pfx::from_der(data).map_err(|e| format!("not a PKCS#12 file: {e}"))?;
    if pfx.auth_safe.content_type != rfc5911::ID_DATA {
        return Err(
            "the PKCS#12 file is protected with a public key; only password protection is supported"
                .to_owned(),
        );
    }
    let auth_safe: OctetString = pfx
        .auth_safe
        .content
        .decode_as()
        .map_err(|e| format!("PKCS#12 content: {e}"))?;
    if let Some(mac) = &pfx.mac_data {
        verify_mac(mac, password, auth_safe.as_bytes())?;
    }
    let contents = Vec::<ContentInfo>::from_der(auth_safe.as_bytes())
        .map_err(|e| format!("PKCS#12 content: {e}"))?;
    let mut keys = Vec::new();
    let mut certs = Vec::new();
    for info in contents {
        let safe_contents = if info.content_type == rfc5911::ID_DATA {
            info.content
                .decode_as::<OctetString>()
                .map_err(|e| format!("PKCS#12 content: {e}"))?
                .into_bytes()
        } else if info.content_type == rfc5911::ID_ENCRYPTED_DATA {
            let encrypted: EncryptedData = info
                .content
                .decode_as()
                .map_err(|e| format!("PKCS#12 encrypted content: {e}"))?;
            let sealed = encrypted
                .enc_content_info
                .encrypted_content
                .ok_or("PKCS#12 encrypted content is empty")?;
            decrypt(
                &encrypted.enc_content_info.content_enc_alg,
                password,
                sealed.as_bytes(),
            )?
        } else {
            return Err(format!(
                "unsupported PKCS#12 content type {}",
                info.content_type
            ));
        };
        read_bags(&safe_contents, password, &mut keys, &mut certs, 0)?;
    }
    let mut keys = keys.into_iter();
    let key = keys.next().ok_or("the PKCS#12 file holds no private key")?;
    if keys.next().is_some() {
        return Err("the PKCS#12 file holds more than one private key".to_owned());
    }
    let position = certs
        .iter()
        .position(|cert| key_matches(&key, cert))
        .ok_or("no certificate in the PKCS#12 file matches its private key")?;
    let cert = certs.remove(position);
    let chain = chain_order(&cert, certs);
    Ok(Identity { key, cert, chain })
}

fn read_bags(
    safe_contents: &[u8],
    password: &str,
    keys: &mut Vec<PrivateKey>,
    certs: &mut Vec<Certificate>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_BAG_DEPTH {
        return Err("PKCS#12 bags nest too deeply".to_owned());
    }
    let bags = Vec::<SafeBag>::from_der(safe_contents).map_err(|e| format!("PKCS#12 bags: {e}"))?;
    for bag in bags {
        // `bag_value` is the whole `[0] EXPLICIT` value.
        let wrapper = AnyRef::from_der(&bag.bag_value).map_err(|e| format!("PKCS#12 bag: {e}"))?;
        let value = wrapper.value();
        if bag.bag_id == pkcs12::PKCS_12_CERT_BAG_OID {
            let cert_bag =
                CertBag::from_der(value).map_err(|e| format!("PKCS#12 certificate: {e}"))?;
            if cert_bag.cert_id == pkcs12::PKCS_12_X509_CERT_OID {
                certs.push(
                    Certificate::from_der(cert_bag.cert_value.as_bytes())
                        .map_err(|e| format!("PKCS#12 certificate: {e}"))?,
                );
            }
        } else if bag.bag_id == pkcs12::PKCS_12_PKCS8_KEY_BAG_OID {
            let sealed = EncryptedPrivateKeyInfo::from_der(value)
                .map_err(|e| format!("PKCS#12 encrypted key: {e}"))?;
            let plain = decrypt(
                &sealed.encryption_algorithm,
                password,
                sealed.encrypted_data.as_bytes(),
            )?;
            keys.push(private_key(&plain)?);
        } else if bag.bag_id == pkcs12::PKCS_12_KEY_BAG_OID {
            keys.push(private_key(value)?);
        } else if bag.bag_id == pkcs12::PKCS_12_SAFE_CONTENTS_BAG_OID {
            read_bags(value, password, keys, certs, depth + 1)?;
        }
    }
    Ok(())
}

fn verify_mac(mac: &pkcs12::mac_data::MacData, password: &str, data: &[u8]) -> Result<(), String> {
    let salt = mac.mac_salt.as_bytes();
    let rounds = mac.iterations;
    let expected = mac.mac.digest.as_bytes();
    macro_rules! check {
        ($digest:ty) => {{
            let key = derive_key_utf8::<$digest>(
                password,
                salt,
                Pkcs12KeyType::Mac,
                rounds,
                <$digest as Digest>::output_size(),
            )
            .map_err(|e| format!("PKCS#12 password: {e}"))?;
            let mut hmac = <Hmac<$digest> as Mac>::new_from_slice(&key)
                .map_err(|e| format!("PKCS#12 MAC key: {e}"))?;
            hmac.update(data);
            hmac.verify_slice(expected).is_ok()
        }};
    }
    let oid = mac.mac.algorithm.oid;
    let matched = if oid == rfc5912::ID_SHA_1 {
        check!(Sha1)
    } else if oid == rfc5912::ID_SHA_224 {
        check!(Sha224)
    } else if oid == rfc5912::ID_SHA_256 {
        check!(Sha256)
    } else if oid == rfc5912::ID_SHA_384 {
        check!(Sha384)
    } else if oid == rfc5912::ID_SHA_512 {
        check!(Sha512)
    } else {
        return Err(format!("unsupported PKCS#12 MAC digest {oid}"));
    };
    if matched {
        Ok(())
    } else {
        Err("wrong password for the PKCS#12 file (its integrity check failed)".to_owned())
    }
}

/// Decrypts `data` sealed with the password-based scheme `algorithm`.
fn decrypt(
    algorithm: &AlgorithmIdentifierOwned,
    password: &str,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    let wrong = |_| "wrong password for the PKCS#12 file (decryption failed)".to_owned();
    let bad_key = |e: cbc::cipher::InvalidLength| format!("PKCS#12 cipher key: {e}");
    let oid = algorithm.oid;
    if oid == PBES2 {
        return pbes2(algorithm, password, data);
    }
    let (key_len, iv_len) = if oid == pkcs12::PKCS_12_PBE_WITH_SHAAND3_KEY_TRIPLE_DES_CBC {
        (24, 8)
    } else if oid == pkcs12::PKCS_12_PBE_WITH_SHAAND2_KEY_TRIPLE_DES_CBC {
        (16, 8)
    } else if oid == pkcs12::PKCS_12_PBEWITH_SHAAND40_BIT_RC2_CBC {
        (5, 8)
    } else if oid == pkcs12::PKCS_12_PBE_WITH_SHAAND128_BIT_RC2_CBC {
        (16, 8)
    } else {
        return Err(format!("unsupported PKCS#12 encryption algorithm {oid}"));
    };
    let params: Pkcs12PbeParams = algorithm
        .parameters
        .as_ref()
        .ok_or("PKCS#12 encryption parameters missing")?
        .decode_as()
        .map_err(|e| format!("PKCS#12 encryption parameters: {e}"))?;
    let derive = |kind, len| {
        derive_key_utf8::<Sha1>(
            password,
            params.salt.as_bytes(),
            kind,
            params.iterations,
            len,
        )
        .map_err(|e| format!("PKCS#12 password: {e}"))
    };
    let key = derive(Pkcs12KeyType::EncryptionKey, key_len)?;
    let iv = derive(Pkcs12KeyType::Iv, iv_len)?;
    if oid == pkcs12::PKCS_12_PBE_WITH_SHAAND3_KEY_TRIPLE_DES_CBC {
        cbc::Decryptor::<des::TdesEde3>::new_from_slices(&key, &iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else if oid == pkcs12::PKCS_12_PBE_WITH_SHAAND2_KEY_TRIPLE_DES_CBC {
        cbc::Decryptor::<des::TdesEde2>::new_from_slices(&key, &iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else {
        let cipher = rc2::Rc2::new_with_eff_key_len(&key, key_len * 8);
        cbc::Decryptor::<rc2::Rc2>::inner_iv_slice_init(cipher, &iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    }
}

/// PBES2 (RFC 8018 §6.2) with PBKDF2 over the UTF-8 password, as RFC 9579 and OpenSSL use
/// it inside PKCS#12.
fn pbes2(
    algorithm: &AlgorithmIdentifierOwned,
    password: &str,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    let wrong = |_| "wrong password for the PKCS#12 file (decryption failed)".to_owned();
    let bad_key = |e: cbc::cipher::InvalidLength| format!("PKCS#12 cipher key: {e}");
    let params: Pbes2Params = algorithm
        .parameters
        .as_ref()
        .ok_or("PBES2 parameters missing")?
        .decode_as()
        .map_err(|e| format!("PBES2 parameters: {e}"))?;
    if params.kdf.oid != rfc5911::ID_PBKDF_2 {
        return Err(format!(
            "unsupported PBES2 key derivation {}",
            params.kdf.oid
        ));
    }
    let kdf: Pbkdf2Params = params
        .kdf
        .parameters
        .as_ref()
        .ok_or("PBKDF2 parameters missing")?
        .decode_as()
        .map_err(|e| format!("PBKDF2 parameters: {e}"))?;
    let scheme = params.encryption.oid;
    let key_len = if scheme == rfc5911::ID_AES_128_CBC {
        16
    } else if scheme == rfc5911::ID_AES_192_CBC || scheme == rfc5911::DES_EDE_3_CBC {
        24
    } else if scheme == rfc5911::ID_AES_256_CBC {
        32
    } else {
        return Err(format!("unsupported PBES2 cipher {scheme}"));
    };
    if kdf
        .key_length
        .is_some_and(|len| usize::try_from(len).ok() != Some(key_len))
    {
        return Err("PBKDF2 key length does not fit the cipher".to_owned());
    }
    let iv: OctetString = params
        .encryption
        .parameters
        .as_ref()
        .ok_or("PBES2 cipher IV missing")?
        .decode_as()
        .map_err(|e| format!("PBES2 cipher IV: {e}"))?;
    let mut key = vec![0_u8; key_len];
    let salt = kdf.salt.as_bytes();
    let rounds = kdf.iteration_count;
    let prf = kdf.prf.as_ref().map_or(HMAC_WITH_SHA_1, |prf| prf.oid);
    let secret = password.as_bytes();
    if prf == HMAC_WITH_SHA_1 {
        pbkdf2::pbkdf2_hmac::<Sha1>(secret, salt, rounds, &mut key);
    } else if prf == rfc6268::ID_HMAC_WITH_SHA_224 {
        pbkdf2::pbkdf2_hmac::<Sha224>(secret, salt, rounds, &mut key);
    } else if prf == rfc6268::ID_HMAC_WITH_SHA_256 {
        pbkdf2::pbkdf2_hmac::<Sha256>(secret, salt, rounds, &mut key);
    } else if prf == rfc6268::ID_HMAC_WITH_SHA_384 {
        pbkdf2::pbkdf2_hmac::<Sha384>(secret, salt, rounds, &mut key);
    } else if prf == rfc6268::ID_HMAC_WITH_SHA_512 {
        pbkdf2::pbkdf2_hmac::<Sha512>(secret, salt, rounds, &mut key);
    } else {
        return Err(format!("unsupported PBKDF2 function {prf}"));
    }
    let iv = iv.as_bytes();
    if scheme == rfc5911::ID_AES_128_CBC {
        cbc::Decryptor::<Aes128>::new_from_slices(&key, iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else if scheme == rfc5911::ID_AES_192_CBC {
        cbc::Decryptor::<Aes192>::new_from_slices(&key, iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else if scheme == rfc5911::ID_AES_256_CBC {
        cbc::Decryptor::<Aes256>::new_from_slices(&key, iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else {
        cbc::Decryptor::<des::TdesEde3>::new_from_slices(&key, iv)
            .map_err(bad_key)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    }
}

/// A PKCS#8 `PrivateKeyInfo` (v1 or v2) as a signing key.
fn private_key(der: &[u8]) -> Result<PrivateKey, String> {
    let info = rsa::pkcs8::PrivateKeyInfo::try_from(der)
        .map_err(|e| format!("PKCS#12 private key: {e}"))?;
    let oid = info.algorithm.oid;
    if oid == rfc5912::RSA_ENCRYPTION || oid == rfc5912::ID_RSASSA_PSS {
        let key = RsaPrivateKey::from_pkcs1_der(info.private_key)
            .map_err(|e| format!("PKCS#12 RSA key: {e}"))?;
        return Ok(if oid == rfc5912::ID_RSASSA_PSS {
            PrivateKey::RsaPss(key)
        } else {
            PrivateKey::Rsa(key)
        });
    }
    if oid != rfc5912::ID_EC_PUBLIC_KEY {
        return Err(format!(
            "unsupported key type {oid}: use an RSA, P-256, or P-384 key"
        ));
    }
    let curve = info
        .algorithm
        .parameters_oid()
        .map_err(|e| format!("PKCS#12 EC key curve: {e}"))?;
    if curve == rfc5912::SECP_256_R_1 {
        p256::SecretKey::from_pkcs8_der(der)
            .map(PrivateKey::P256)
            .map_err(|e| format!("PKCS#12 P-256 key: {e}"))
    } else if curve == rfc5912::SECP_384_R_1 {
        p384::SecretKey::from_pkcs8_der(der)
            .map(PrivateKey::P384)
            .map_err(|e| format!("PKCS#12 P-384 key: {e}"))
    } else {
        Err(format!(
            "unsupported EC curve {curve}: use an RSA, P-256, or P-384 key"
        ))
    }
}

/// True when `cert` carries the public half of `key`.
fn key_matches(key: &PrivateKey, cert: &Certificate) -> bool {
    let public = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes();
    match key {
        PrivateKey::Rsa(key) | PrivateKey::RsaPss(key) => {
            RsaPublicKey::from_pkcs1_der(public).is_ok_and(|found| found == key.to_public_key())
        }
        PrivateKey::P256(key) => {
            p256::PublicKey::from_sec1_bytes(public).is_ok_and(|found| found == key.public_key())
        }
        PrivateKey::P384(key) => {
            p384::PublicKey::from_sec1_bytes(public).is_ok_and(|found| found == key.public_key())
        }
    }
}

/// `others` with the issuers of `leaf` first, each followed by its own issuer.
fn chain_order(leaf: &Certificate, mut others: Vec<Certificate>) -> Vec<Certificate> {
    let mut ordered = Vec::with_capacity(others.len());
    let mut current = leaf.clone();
    while current.tbs_certificate.issuer != current.tbs_certificate.subject {
        let Some(position) = others
            .iter()
            .position(|cert| cert.tbs_certificate.subject == current.tbs_certificate.issuer)
        else {
            break;
        };
        current = others.remove(position);
        ordered.push(current.clone());
    }
    ordered.extend(others);
    ordered
}
