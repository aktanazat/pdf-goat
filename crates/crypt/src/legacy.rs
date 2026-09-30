//! Revisions 2 to 4: ISO 32000-1 algorithms 2 to 7 (MD5 and RC4 key
//! derivation, `/O` and `/U`, user and owner authentication).

use md5::{Digest, Md5};

use crate::rc4;

/// The standard 32-byte password padding (ISO 32000-1 7.6.3.3).
pub(crate) const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

/// What the algorithms read from the encryption dictionary and trailer.
pub(crate) struct Params<'a> {
    /// 2, 3, or 4.
    pub revision: u8,
    /// File key length in bytes, 5 for revision 2 and 5..=16 otherwise.
    pub key_len: usize,
    pub o: &'a [u8; 32],
    pub p: i32,
    pub id0: &'a [u8],
    pub encrypt_metadata: bool,
}

fn pad_password(password: &[u8]) -> [u8; 32] {
    let mut out = PAD;
    let n = password.len().min(32);
    out[..n].copy_from_slice(&password[..n]);
    out[n..].copy_from_slice(&PAD[..32 - n]);
    out
}

fn rehash_50(hash: [u8; 16], n: usize) -> [u8; 16] {
    // Adobe hashes only the first n bytes each round (qpdf, MuPDF, PDFBox
    // agree); hashing all 16 makes 40-bit R3 owner passwords fail in Acrobat.
    (0..50).fold(hash, |h, _| Md5::digest(&h[..n]).into())
}

fn xor_key(key: &[u8], i: u8) -> Vec<u8> {
    key.iter().map(|b| b ^ i).collect()
}

/// Algorithm 2: the file key from the user password.
pub(crate) fn file_key(params: &Params<'_>, user_password: &[u8]) -> Vec<u8> {
    let mut md5 = Md5::new();
    md5.update(pad_password(user_password));
    md5.update(params.o);
    md5.update((params.p as u32).to_le_bytes());
    md5.update(params.id0);
    if params.revision >= 4 && !params.encrypt_metadata {
        md5.update([0xff; 4]);
    }
    let mut hash: [u8; 16] = md5.finalize().into();
    let n = params.key_len;
    if params.revision >= 3 {
        hash = rehash_50(hash, n);
    }
    hash[..n].to_vec()
}

/// Algorithm 3 steps a to d: the RC4 key derived from the owner password.
fn owner_key(revision: u8, key_len: usize, owner_password: &[u8]) -> Vec<u8> {
    let mut hash: [u8; 16] = Md5::digest(pad_password(owner_password)).into();
    if revision >= 3 {
        hash = rehash_50(hash, key_len);
    }
    hash[..key_len].to_vec()
}

/// Algorithm 3: the `/O` entry. An empty owner password means the user
/// password doubles as owner password.
pub(crate) fn compute_o(
    revision: u8,
    key_len: usize,
    owner_password: &[u8],
    user_password: &[u8],
) -> [u8; 32] {
    let owner_password = if owner_password.is_empty() {
        user_password
    } else {
        owner_password
    };
    let key = owner_key(revision, key_len, owner_password);
    let mut data = rc4::apply(&key, &pad_password(user_password));
    if revision >= 3 {
        for i in 1..=19 {
            data = rc4::apply(&xor_key(&key, i), &data);
        }
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&data);
    out
}

/// Algorithms 4 (revision 2) and 5 (revision 3 and 4): the `/U` entry.
pub(crate) fn compute_u(params: &Params<'_>, file_key: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if params.revision == 2 {
        out.copy_from_slice(&rc4::apply(file_key, &PAD));
    } else {
        let mut md5 = Md5::new();
        md5.update(PAD);
        md5.update(params.id0);
        let mut data = rc4::apply(file_key, &md5.finalize());
        for i in 1..=19 {
            data = rc4::apply(&xor_key(file_key, i), &data);
        }
        // The remaining 16 bytes are arbitrary padding.
        out[..16].copy_from_slice(&data);
    }
    out
}

/// Algorithm 6: the file key when `password` is the user password.
pub(crate) fn check_user(params: &Params<'_>, u: &[u8; 32], password: &[u8]) -> Option<Vec<u8>> {
    let key = file_key(params, password);
    let computed = compute_u(params, &key);
    let n = if params.revision == 2 { 32 } else { 16 };
    (computed[..n] == u[..n]).then_some(key)
}

/// Algorithm 7: the file key when `password` is the owner password, found by
/// recovering the padded user password from `/O`.
pub(crate) fn check_owner(params: &Params<'_>, u: &[u8; 32], password: &[u8]) -> Option<Vec<u8>> {
    let key = owner_key(params.revision, params.key_len, password);
    let mut user_password = params.o.to_vec();
    if params.revision == 2 {
        user_password = rc4::apply(&key, &user_password);
    } else {
        for i in (0..=19).rev() {
            user_password = rc4::apply(&xor_key(&key, i), &user_password);
        }
    }
    check_user(params, u, &user_password)
}

/// Algorithm 1: the per-object key; `aes` appends the `sAlT` bytes.
pub(crate) fn object_key(file_key: &[u8], num: u32, generation: u16, aes: bool) -> Vec<u8> {
    let mut md5 = Md5::new();
    md5.update(file_key);
    md5.update(&num.to_le_bytes()[..3]);
    md5.update(generation.to_le_bytes());
    if aes {
        md5.update(b"sAlT");
    }
    let hash = md5.finalize();
    let n = (file_key.len() + 5).min(16);
    hash[..n].to_vec()
}
