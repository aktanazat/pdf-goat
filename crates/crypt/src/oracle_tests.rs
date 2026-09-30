//! Known-answer tests taken once from files pikepdf 10.9.1 (qpdf 12.3.2)
//! wrote for a one-page document with `/Title (Hello, crypt!)` and a
//! 33-byte content stream. Only the `/Encrypt` entries, `/ID[0]`, and the two
//! ciphertexts are kept; the expected file key is pikepdf's
//! `EncryptionInfo.encryption_key`, the description is pymupdf 1.27's
//! `metadata["encryption"]` for the same file.

use crate::{CryptError, CryptMethod, DataKind, EncryptDict, SecurityHandler};

struct Fixture {
    name: &'static str,
    user: &'static str,
    owner: &'static str,
    v: i32,
    r: i32,
    length_bits: u32,
    p: i32,
    encrypt_metadata: bool,
    o: &'static str,
    u: &'static str,
    oe: Option<&'static str>,
    ue: Option<&'static str>,
    perms: Option<&'static str>,
    string_method: CryptMethod,
    stream_method: CryptMethod,
    id0: &'static str,
    file_key: &'static str,
    description: &'static str,
    string_obj: (u32, u16),
    string_cipher: &'static str,
    string_plain: &'static str,
    stream_obj: (u32, u16),
    stream_cipher: &'static str,
    stream_plain: &'static str,
}

const FIXTURES: &[Fixture] = &[
    // V1 and V2 files carry no crypt filters; pdf-core may hand over `None`
    // and the handler still runs RC4.
    Fixture {
        name: "rc4_40_r2",
        user: "user pw",
        owner: "owner-secret",
        v: 1,
        r: 2,
        length_bits: 40,
        p: -12,
        encrypt_metadata: true,
        o: "89a62de6c2de3bae192d2e13a1e4a081c56034b602b10956b9920c22ef3009b1",
        u: "284afcd47aa733d1064b7cef0b99d207a565c2bde3568cd063750940540ffc9c",
        oe: None,
        ue: None,
        perms: None,
        string_method: CryptMethod::None,
        stream_method: CryptMethod::None,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "24e2cf4367",
        description: "Standard V1 R2 40-bit RC4",
        string_obj: (2, 0),
        string_cipher: "daaeb5176025a2a5ebdf4d4e54",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "aa83527649412dfaddd47c93551e7d41197d36857faea12117308284a102051e8b",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    Fixture {
        name: "rc4_128_r3",
        user: "user pw",
        owner: "owner-secret",
        v: 2,
        r: 3,
        length_bits: 128,
        p: -1028,
        encrypt_metadata: true,
        o: "ccf72c13356068255023b239e3e75a7b551e3bdd8b6fc9dbc1cd7db8b078a87b",
        u: "e483a3d635d72117683a80095172139c0021446990b9e4114071a4d9104984c1",
        oe: None,
        ue: None,
        perms: None,
        string_method: CryptMethod::None,
        stream_method: CryptMethod::None,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "cfd9c485169f20caca7ec018e333b1e8",
        description: "Standard V2 R3 128-bit RC4",
        string_obj: (2, 0),
        string_cipher: "c21a613784b9e55a7adde29739",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "0a610bc4659209c3d5d3b5c74b3637c4ad2a0458d5e24fb37ea9bc4d1ef1369619",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    // /EncryptMetadata false: algorithm 2 hashes the extra ff ff ff ff.
    Fixture {
        name: "rc4_128_r4",
        user: "user pw",
        owner: "owner-secret",
        v: 4,
        r: 4,
        length_bits: 128,
        p: -1028,
        encrypt_metadata: false,
        o: "ccf72c13356068255023b239e3e75a7b551e3bdd8b6fc9dbc1cd7db8b078a87b",
        u: "6741a3a9e0c00c84f00f409acd05aad10021446990b9e4114071a4d9104984c1",
        oe: None,
        ue: None,
        perms: None,
        string_method: CryptMethod::Rc4,
        stream_method: CryptMethod::Rc4,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "453929d739727010b985811249b82202",
        description: "Standard V4 R4 128-bit RC4",
        string_obj: (2, 0),
        string_cipher: "8550096b0a28e21a664922ac9d",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "ac519f86822b9011deed60e67866bca38defa50d918bef2c4049c8bb3ace240253",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    Fixture {
        name: "aes_128_r4",
        user: "user pw",
        owner: "owner-secret",
        v: 4,
        r: 4,
        length_bits: 128,
        p: -1028,
        encrypt_metadata: true,
        o: "ccf72c13356068255023b239e3e75a7b551e3bdd8b6fc9dbc1cd7db8b078a87b",
        u: "e483a3d635d72117683a80095172139c0021446990b9e4114071a4d9104984c1",
        oe: None,
        ue: None,
        perms: None,
        string_method: CryptMethod::AesV2,
        stream_method: CryptMethod::AesV2,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "cfd9c485169f20caca7ec018e333b1e8",
        description: "Standard V4 R4 128-bit AES",
        string_obj: (2, 0),
        string_cipher: "680fd1c78c5603817c6d6fc07e05db1b547d90415deb5e63c40444d436dcf0b4",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "4e52a0c5ceef4fc1cc4c17d08b12d6a3edd67617c992928213ecb65aea19cd94\
                        f1d62039791b8a6705bc36750ed91248b27eac97f432ee4dcb011cb42338f8b8",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    Fixture {
        name: "aes_256_r6",
        user: "user pw",
        owner: "owner-secret",
        v: 5,
        r: 6,
        length_bits: 256,
        p: -1028,
        encrypt_metadata: true,
        o: "904e902e3ce3af3a76a9db11833c99ca327085059ec3e1ad84c0fd34b6632c0e\
            c7ff280789c8ffeb37c2019a87474088",
        u: "1e21879bf41c2f63fedf2d6d621e6a1bda2f849da0c171e082c5c2a2f8430c24\
            389825375b359c0c11938590024b56a3",
        oe: Some("6b80e88c3a7325c2d7c642da6548121ef3db02b929c0822ebe8e4aaec1a21bae"),
        ue: Some("d59a4398a005b2e601a0829d0d98ff35fd618fd403a69694881bb2a21d993d60"),
        perms: Some("9768277e5cc4fc4490c724226184720f"),
        string_method: CryptMethod::AesV3,
        stream_method: CryptMethod::AesV3,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "d6c0274defc77a8ac3d1afd0a93c14c48b7924a97be3699249c539b3d5297fea",
        description: "Standard V5 R6 256-bit AES",
        string_obj: (2, 0),
        string_cipher: "f3b82eee3639a19a3389c8c722210848b6541777060e65e3e85f2eded476b7dc",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "8642e885e678381bdddd4ab8323dc2ddb7d41fca25ae0526bedd1dcd1fb1999f\
                        42284d987a8f2e2f08588f843efb4f4b7cf79852a7397d8a6ecc11d869dbfb4d",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    // /EncryptMetadata false: /Perms byte 8 is 'F'.
    Fixture {
        name: "aes_256_r6_nometa",
        user: "user pw",
        owner: "owner-secret",
        v: 5,
        r: 6,
        length_bits: 256,
        p: -1028,
        encrypt_metadata: false,
        o: "c6ea17b73c3d6d6b78ceed032922bac41b89fc6fd41754b34dbd3fcee937f531\
            56c9727b7535699d89aad18ebabd4b74",
        u: "10fcbb775af11792c7c3394103d845ff868f66c516f668f223f2986d288bc7c8\
            98049cf778294e1de31156fa535cd3e4",
        oe: Some("1d9b40cf4746a5418c1362a6c29cb44997951bc3d19b6e0853fd5e3d901bca97"),
        ue: Some("834c45251b3440f2f425062ec101c0bfcf7d7c1886554cdf45b44b6be096adce"),
        perms: Some("6dc8f26f67f8a6c8d3a1984a3eaec389"),
        string_method: CryptMethod::AesV3,
        stream_method: CryptMethod::AesV3,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "6a88585d05cb816149fd882caa5b651bba671e255a4272a17bb4407af6a13877",
        description: "Standard V5 R6 256-bit AES",
        string_obj: (2, 0),
        string_cipher: "83df8aee5ceb60d159fca092eeb89329117f20060521afb04c978a8809385b2d",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "cb112738f42a8b1d7f462ee971d08595db7ef8f79a056ea80390ca066a6b77f2\
                        5c662fd8dc2104d0e0b11a07aa11974636f49cd38098e5fb72743921c51b801f",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    // Same user and owner password: pymupdf authenticates as both (6).
    Fixture {
        name: "aes_256_r6_same_pw",
        user: "user pw",
        owner: "user pw",
        v: 5,
        r: 6,
        length_bits: 256,
        p: -1028,
        encrypt_metadata: true,
        o: "809c0287cdaf0aac24e1d863503d5f3848a0b36cfc23ff076ad765aee2bfbf84\
            6b60f475de68a87fbeda4a0ebf8335ea",
        u: "7345faaf8bd1d38263fa35fd76092cec1cde27051c629231923dc938f8c95638\
            a34f2585c37769c6e0a22d344403d3a6",
        oe: Some("3beca8e4d76e11114f315f3e60c42a1a49025978741ff7e729ebadfce9b71475"),
        ue: Some("126c831732f0f371f7c5f763ca201575aa279cbfda195e85265fffbefd3bdc07"),
        perms: Some("1b4a82a94e9cb91f735fef12e1d704b5"),
        string_method: CryptMethod::AesV3,
        stream_method: CryptMethod::AesV3,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "0badc876f29bb4e65f49686b0580afbdb51e5cf69da6e20bc9abb58e24f77666",
        description: "Standard V5 R6 256-bit AES",
        string_obj: (2, 0),
        string_cipher: "97d565475f10d3e06d16093e08654ad38a5f298460c047f4dd93c7401f33769b",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "a277262e3a11a6feb48f0d6b205f064f2c2b754430155159c3a384b23c5cfdb5\
                        7ef9294c0fd65f6065e7fc858a5541386a1c5dd11365ee770aec9804a3151898",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
    Fixture {
        name: "aes_256_r6_empty_user",
        user: "",
        owner: "owner-secret",
        v: 5,
        r: 6,
        length_bits: 256,
        p: -1028,
        encrypt_metadata: true,
        o: "b8d8f9b62a8155968c7175dae54d646330839fb2859ba6a557123aee5f3c5e66\
            e4eb994edcfd9372f97aef7336a5f6c5",
        u: "fd9260334c7d34fd1020d9116063118d29ac909b92e5392220ba99eaaad0cd2d\
            eee70f11dfe82f5d6a32055dc089f264",
        oe: Some("61f5d61b0bef368e5ef01bd9e41caa816022c0aad99b91acdbd559729ce77eb6"),
        ue: Some("17a3e4cffbc49f264189a919ae3122b9b5d5bd8bb3e4c9ee5e3c1da61a6ad850"),
        perms: Some("b4e2ae3f6a9b8dc4f970664e139b031c"),
        string_method: CryptMethod::AesV3,
        stream_method: CryptMethod::AesV3,
        id0: "58b570be96759e0dec4bebfc46e2c5c7",
        file_key: "6cdd5a312b308526fc2458b4896783739f4acc1017e3b1e11d284007654eba1a",
        description: "Standard V5 R6 256-bit AES",
        string_obj: (2, 0),
        string_cipher: "e02b8aaf7f5328684e9b9148694d39821bf0f875a4256f4700864d271d44a02b",
        string_plain: "Hello, crypt!",
        stream_obj: (5, 0),
        stream_cipher: "f350b7d5c2588473de836382a8a4980e1d935bca4adcdd6a1c5c0f524e814173\
                        4ceb565cb13a7c9400eed0934f74224c4cf11a6a68704107b171cfb4d0674150",
        stream_plain: "BT /F1 12 Tf 72 700 Td (Hi) Tj ET",
    },
];

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn find(name: &str) -> &'static Fixture {
    FIXTURES.iter().find(|f| f.name == name).unwrap()
}

fn dict(f: &Fixture) -> EncryptDict {
    EncryptDict {
        v: f.v,
        r: f.r,
        length_bits: f.length_bits,
        o: unhex(f.o),
        u: unhex(f.u),
        oe: f.oe.map(unhex),
        ue: f.ue.map(unhex),
        perms: f.perms.map(unhex),
        p: f.p,
        encrypt_metadata: f.encrypt_metadata,
        string_method: f.string_method,
        stream_method: f.stream_method,
        embedded_file_method: f.stream_method,
    }
}

fn open(f: &Fixture, password: &str) -> Result<SecurityHandler, CryptError> {
    SecurityHandler::authenticate(&dict(f), &unhex(f.id0), password.as_bytes())
}

macro_rules! oracle_cases {
    ($($name:ident),* $(,)?) => { $(
        mod $name {
            use super::*;

            fn fixture() -> &'static Fixture {
                find(stringify!($name))
            }

            #[test]
            fn user_password_derives_oracle_file_key() {
                let f = fixture();
                let handler = open(f, f.user).unwrap();
                assert_eq!(handler.key, unhex(f.file_key));
                assert_eq!(handler.is_owner(), f.user == f.owner);
                assert_eq!(handler.permissions(), f.p);
                assert_eq!(handler.encrypts_metadata(), f.encrypt_metadata);
                assert_eq!(handler.perms_valid(), (f.r >= 5).then_some(true));
            }

            #[test]
            fn owner_password_authenticates_as_owner_with_same_key() {
                let f = fixture();
                let handler = open(f, f.owner).unwrap();
                assert!(handler.is_owner());
                assert_eq!(handler.key, unhex(f.file_key));
            }

            #[test]
            fn wrong_password_is_rejected() {
                let f = fixture();
                assert_eq!(open(f, "not the password").unwrap_err(), CryptError::WrongPassword);
            }

            #[test]
            fn decrypts_oracle_string_and_stream() {
                let f = fixture();
                let handler = open(f, f.user).unwrap();
                let (num, generation) = f.string_obj;
                let string = handler.decrypt(num, generation, DataKind::String, &unhex(f.string_cipher)).unwrap();
                assert_eq!(String::from_utf8(string).unwrap(), f.string_plain);
                let (num, generation) = f.stream_obj;
                let stream = handler.decrypt(num, generation, DataKind::Stream, &unhex(f.stream_cipher)).unwrap();
                assert_eq!(String::from_utf8(stream).unwrap(), f.stream_plain);
            }

            #[test]
            fn description_matches_mupdf() {
                let f = fixture();
                assert_eq!(open(f, f.user).unwrap().description(), f.description);
            }
        }
    )* };
}

oracle_cases!(
    rc4_40_r2,
    rc4_128_r3,
    rc4_128_r4,
    aes_128_r4,
    aes_256_r6,
    aes_256_r6_nometa,
    aes_256_r6_same_pw,
    aes_256_r6_empty_user,
);

#[test]
fn r6_perms_mismatch_is_reported_not_fatal() {
    let f = find("aes_256_r6");
    let mut d = dict(f);
    d.p = -1;
    let handler = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap();
    assert_eq!(handler.perms_valid(), Some(false));
    assert_eq!(handler.permissions(), -1);
    d.p = f.p;
    d.encrypt_metadata = false;
    let handler = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap();
    assert_eq!(handler.perms_valid(), Some(false));
    d.encrypt_metadata = true;
    d.perms = None;
    let handler = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap();
    assert_eq!(handler.perms_valid(), Some(false));
    assert_eq!(handler.key, unhex(f.file_key));
}

#[test]
fn r6_user_password_needs_ue_and_owner_needs_oe() {
    let f = find("aes_256_r6");
    let mut d = dict(f);
    d.ue = None;
    let err = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: user password matches but /UE is missing"
    );
    assert!(SecurityHandler::authenticate(&d, &unhex(f.id0), f.owner.as_bytes()).is_ok());
    d.ue = f.ue.map(unhex);
    d.oe = None;
    let err = SecurityHandler::authenticate(&d, &unhex(f.id0), f.owner.as_bytes()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: owner password matches but /OE is missing"
    );
}

#[test]
fn r6_password_is_saslprepped_before_hashing() {
    // NFKC folds U+2168 (Roman numeral nine) to "IX", so a password written as
    // "IX" opens with either spelling; a control character can never match.
    let (handler, d) = SecurityHandler::for_new_document(
        &crate::NewEncryption {
            method: crate::NewMethod::Aes256,
            user_password: b"IX".to_vec(),
            owner_password: b"owner".to_vec(),
            permissions: -1,
            encrypt_metadata: true,
        },
        b"",
    )
    .unwrap();
    let opened = SecurityHandler::authenticate(&d, b"", "\u{2168}".as_bytes()).unwrap();
    assert_eq!(opened.key, handler.key);
    assert!(!opened.is_owner());
    assert_eq!(
        SecurityHandler::authenticate(&d, b"", b"I\tX").unwrap_err(),
        CryptError::WrongPassword
    );
}

#[test]
fn length_in_bytes_is_repaired_and_bad_lengths_are_unsupported() {
    let f = find("rc4_128_r3");
    let mut d = dict(f);
    d.length_bits = 16;
    let handler = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap();
    assert_eq!(handler.key, unhex(f.file_key));
    assert_eq!(handler.description(), f.description);
    d.length_bits = 100;
    let err = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encryption: key length 100 bits"
    );
}

#[test]
fn v4_file_with_identity_strings_describes_both_methods() {
    let f = find("aes_128_r4");
    let mut d = dict(f);
    d.string_method = CryptMethod::None;
    let handler = SecurityHandler::authenticate(&d, &unhex(f.id0), f.user.as_bytes()).unwrap();
    assert_eq!(
        handler.description(),
        "Standard V4 R4 128-bit streams: AES strings: None"
    );
    let raw = b"(plain)";
    assert_eq!(handler.decrypt(2, 0, DataKind::String, raw).unwrap(), raw);
}
