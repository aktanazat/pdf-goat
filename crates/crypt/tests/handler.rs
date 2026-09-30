//! Contract tests through the public API: a document encrypted with
//! `for_new_document` opens with its passwords, refuses others, and carries
//! object data through encrypt/decrypt for every method.

use pdf_crypt::{
    CryptError, CryptMethod, DataKind, EncryptDict, NewEncryption, NewMethod, SecurityHandler,
};

const ID0: &[u8] = b"\x01\x23\x45\x67\x89\xab\xcd\xef\xfe\xdc\xba\x98\x76\x54\x32\x10";
const PERMISSIONS: i32 = -3904; // print, copy, annotate, fill forms cleared: 0xFFFFF0C0

fn params(method: NewMethod) -> NewEncryption {
    NewEncryption {
        method,
        user_password: b"user pw".to_vec(),
        owner_password: b"owner-secret".to_vec(),
        permissions: PERMISSIONS,
        encrypt_metadata: true,
    }
}

fn new_document(method: NewMethod) -> (SecurityHandler, EncryptDict) {
    SecurityHandler::for_new_document(&params(method), ID0).unwrap()
}

macro_rules! method_cases {
    ($($name:ident: $method:expr => ($v:expr, $r:expr, $bits:expr, $crypt:expr, $description:expr);)*) => { $(
        mod $name {
            use super::*;

            #[test]
            fn dictionary_has_the_pinned_shape() {
                let (_, dict) = new_document($method);
                assert_eq!((dict.v, dict.r, dict.length_bits), ($v, $r, $bits));
                assert_eq!(
                    (dict.string_method, dict.stream_method, dict.embedded_file_method),
                    ($crypt, $crypt, $crypt)
                );
                assert_eq!(dict.p, PERMISSIONS);
                assert!(dict.encrypt_metadata);
                let aes256 = $method == NewMethod::Aes256;
                assert_eq!((dict.o.len(), dict.u.len()), if aes256 { (48, 48) } else { (32, 32) });
                assert_eq!(dict.oe.as_ref().map(Vec::len), aes256.then_some(32));
                assert_eq!(dict.ue.as_ref().map(Vec::len), aes256.then_some(32));
                assert_eq!(dict.perms.as_ref().map(Vec::len), aes256.then_some(16));
            }

            #[test]
            fn user_password_opens_without_owner_rights() {
                let (_, dict) = new_document($method);
                let handler = SecurityHandler::authenticate(&dict, ID0, b"user pw").unwrap();
                assert!(!handler.is_owner());
                assert_eq!(handler.permissions(), PERMISSIONS);
                assert!(handler.encrypts_metadata());
                assert_eq!(handler.description(), $description);
            }

            #[test]
            fn owner_password_opens_with_owner_rights() {
                let (created, dict) = new_document($method);
                let handler = SecurityHandler::authenticate(&dict, ID0, b"owner-secret").unwrap();
                assert!(handler.is_owner());
                assert!(created.is_owner());
                // Same file key: what the owner encrypts, the user reads.
                let cipher = handler.encrypt(9, 0, DataKind::Stream, b"owner wrote this");
                let user = SecurityHandler::authenticate(&dict, ID0, b"user pw").unwrap();
                assert_eq!(user.decrypt(9, 0, DataKind::Stream, &cipher).unwrap(), b"owner wrote this");
            }

            #[test]
            fn wrong_password_is_rejected() {
                let (_, dict) = new_document($method);
                assert_eq!(
                    SecurityHandler::authenticate(&dict, ID0, b"user pw ").unwrap_err(),
                    CryptError::WrongPassword
                );
                assert_eq!(SecurityHandler::authenticate(&dict, ID0, b"").unwrap_err(), CryptError::WrongPassword);
            }

            #[test]
            fn empty_owner_password_makes_the_user_password_owner() {
                let mut p = params($method);
                p.owner_password.clear();
                let (created, dict) = SecurityHandler::for_new_document(&p, ID0).unwrap();
                assert!(created.is_owner());
                assert!(SecurityHandler::authenticate(&dict, ID0, b"user pw").unwrap().is_owner());
            }

            #[test]
            fn object_data_round_trips_for_every_kind_and_object() {
                let (handler, _) = new_document($method);
                let plain: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
                for kind in [DataKind::String, DataKind::Stream, DataKind::EmbeddedFile] {
                    for (num, generation) in [(1, 0), (70000, 3), (u32::MAX, u16::MAX)] {
                        let cipher = handler.encrypt(num, generation, kind, &plain);
                        assert_ne!(cipher, plain, "{kind:?} {num} {generation}");
                        assert_eq!(handler.decrypt(num, generation, kind, &cipher).unwrap(), plain);
                    }
                }
                assert_eq!(handler.decrypt(1, 0, DataKind::String, &handler.encrypt(1, 0, DataKind::String, b"")).unwrap(), b"");
            }
        }
    )* };
}

method_cases! {
    rc4_128: NewMethod::Rc4_128 => (2, 3, 128, CryptMethod::Rc4, "Standard V2 R3 128-bit RC4");
    aes_128: NewMethod::Aes128 => (4, 4, 128, CryptMethod::AesV2, "Standard V4 R4 128-bit AES");
    aes_256: NewMethod::Aes256 => (5, 6, 256, CryptMethod::AesV3, "Standard V5 R6 256-bit AES");
}

#[test]
fn rc4_and_aes128_keys_depend_on_the_object_number() {
    for method in [NewMethod::Rc4_128, NewMethod::Aes128] {
        let (handler, _) = new_document(method);
        let cipher = handler.encrypt(1, 0, DataKind::Stream, b"object one");
        let other = handler
            .decrypt(2, 0, DataKind::Stream, &cipher)
            .unwrap_or_default();
        assert_ne!(other, b"object one", "{method:?}");
    }
}

#[test]
fn aes256_uses_the_file_key_for_every_object() {
    // ISO 32000-2: /AESV3 has no per-object key, so the object number and
    // generation do not take part in the ciphertext.
    let (handler, _) = new_document(NewMethod::Aes256);
    let cipher = handler.encrypt(1, 0, DataKind::Stream, b"object one");
    assert_eq!(
        handler.decrypt(2, 5, DataKind::String, &cipher).unwrap(),
        b"object one"
    );
}

#[test]
fn aes_ciphertext_uses_a_fresh_iv_each_time() {
    let (handler, _) = new_document(NewMethod::Aes256);
    let a = handler.encrypt(1, 0, DataKind::Stream, b"same plaintext");
    let b = handler.encrypt(1, 0, DataKind::Stream, b"same plaintext");
    assert_ne!(a, b);
    assert_eq!(a.len(), 32);
    assert_eq!(
        handler.decrypt(1, 0, DataKind::Stream, &b).unwrap(),
        b"same plaintext"
    );
}

#[test]
fn aes_256_document_with_clear_metadata_records_the_flag() {
    let mut p = params(NewMethod::Aes256);
    p.encrypt_metadata = false;
    let (handler, dict) = SecurityHandler::for_new_document(&p, ID0).unwrap();
    assert!(!dict.encrypt_metadata);
    assert!(!handler.encrypts_metadata());
    assert_eq!(handler.perms_valid(), Some(true));
    let reopened = SecurityHandler::authenticate(&dict, ID0, b"user pw").unwrap();
    assert_eq!(reopened.perms_valid(), Some(true));
    assert!(!reopened.encrypts_metadata());
}

#[test]
fn aes_128_document_with_clear_metadata_derives_a_different_key() {
    let mut p = params(NewMethod::Aes128);
    p.encrypt_metadata = false;
    let (clear, dict_clear) = SecurityHandler::for_new_document(&p, ID0).unwrap();
    let (_, dict_encrypted) = new_document(NewMethod::Aes128);
    assert_eq!(dict_clear.o, dict_encrypted.o);
    assert_ne!(dict_clear.u, dict_encrypted.u);
    assert!(!clear.encrypts_metadata());
    let mut mislabeled = dict_clear.clone();
    mislabeled.encrypt_metadata = true;
    assert_eq!(
        SecurityHandler::authenticate(&mislabeled, ID0, b"user pw").unwrap_err(),
        CryptError::WrongPassword
    );
}

#[test]
fn rc4_128_cannot_leave_metadata_clear() {
    let mut p = params(NewMethod::Rc4_128);
    p.encrypt_metadata = false;
    let err = SecurityHandler::for_new_document(&p, ID0).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: RC4-128 (V2 R3) cannot leave metadata unencrypted; use AES-128 or AES-256"
    );
}

#[test]
fn aes_256_rejects_a_password_saslprep_prohibits() {
    let mut p = params(NewMethod::Aes256);
    p.user_password = b"line\nbreak".to_vec();
    let err = SecurityHandler::for_new_document(&p, ID0).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: password contains U+000A, which SASLprep prohibits"
    );
}

#[test]
fn passwords_longer_than_the_algorithm_limit_are_truncated() {
    // Revisions 2-4 keep 32 bytes, revision 6 keeps 127 bytes of UTF-8.
    for (method, limit) in [(NewMethod::Aes128, 32usize), (NewMethod::Aes256, 127)] {
        let mut p = params(method);
        p.user_password = vec![b'x'; limit + 5];
        let (_, dict) = SecurityHandler::for_new_document(&p, ID0).unwrap();
        assert!(
            SecurityHandler::authenticate(&dict, ID0, &vec![b'x'; limit]).is_ok(),
            "{method:?}"
        );
        assert_eq!(
            SecurityHandler::authenticate(&dict, ID0, &vec![b'x'; limit - 1]).unwrap_err(),
            CryptError::WrongPassword,
            "{method:?}"
        );
    }
}

#[test]
fn unsupported_shapes_are_named() {
    let (_, dict) = new_document(NewMethod::Aes128);
    let mut bad = dict.clone();
    bad.r = 7;
    let err = SecurityHandler::authenticate(&bad, ID0, b"user pw").unwrap_err();
    assert_eq!(err.to_string(), "unsupported encryption: revision 7");
    let mut bad = dict.clone();
    bad.stream_method = CryptMethod::AesV3;
    let err = SecurityHandler::authenticate(&bad, ID0, b"user pw").unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: AESV3 crypt filter with revision 4"
    );
    let mut bad = dict.clone();
    bad.o.truncate(20);
    let err = SecurityHandler::authenticate(&bad, ID0, b"user pw").unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: /O is 20 bytes, 32 needed"
    );
    let mut bad = dict;
    bad.v = 6;
    let err = SecurityHandler::authenticate(&bad, ID0, b"user pw").unwrap_err();
    assert_eq!(err.to_string(), "unsupported encryption: V 6");
}

#[test]
fn aes_decrypt_tolerates_bad_padding_and_rejects_missing_iv() {
    let (handler, _) = new_document(NewMethod::Aes128);
    let mut cipher = handler.encrypt(4, 0, DataKind::Stream, b"exactly sixteen!");
    // Corrupt the padding block: the sixteen real bytes still come back.
    let len = cipher.len();
    cipher[len - 1] ^= 0x55;
    let plain = handler.decrypt(4, 0, DataKind::Stream, &cipher).unwrap();
    assert_eq!(&plain[..16], b"exactly sixteen!");
    assert_eq!(plain.len(), 32);
    assert_eq!(handler.decrypt(4, 0, DataKind::Stream, b"").unwrap(), b"");
    let err = handler
        .decrypt(4, 0, DataKind::Stream, b"short")
        .unwrap_err();
    assert!(matches!(err, CryptError::Corrupt(_)), "{err}");
}
