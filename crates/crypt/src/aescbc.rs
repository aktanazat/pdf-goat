//! AES-CBC over the `aes` and `cbc` crates: the IV-prefixed PKCS#5 form used
//! for strings and streams, the unpadded zero-IV form used by the revision 5
//! and 6 key wrapping, and single ECB blocks for `/Perms`.

use aes::cipher::block_padding::{NoPadding, Pkcs7};
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{
    BlockDecrypt, BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit,
};

use crate::CryptError;

pub(crate) const BLOCK: usize = 16;

#[derive(Clone, Copy)]
pub(crate) enum Key {
    Aes128([u8; 16]),
    Aes256([u8; 32]),
}

/// Fills `buf` from the operating system's random source.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    // Not PDF or user data: the OS random source failing is a broken host,
    // and the pinned `encrypt` signature has no error channel for it.
    getrandom::getrandom(&mut buf).expect("operating system random source");
    buf
}

/// String/stream form: a random 16-byte IV followed by CBC ciphertext with
/// PKCS#5 padding (always at least one padding byte).
pub(crate) fn encrypt_with_iv(key: &Key, data: &[u8]) -> Vec<u8> {
    let iv: [u8; BLOCK] = random();
    let mut out = Vec::with_capacity(BLOCK + data.len() + BLOCK);
    out.extend_from_slice(&iv);
    let body = match key {
        Key::Aes128(k) => cbc::Encryptor::<aes::Aes128>::new(k.into(), (&iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(data),
        Key::Aes256(k) => cbc::Encryptor::<aes::Aes256>::new(k.into(), (&iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(data),
    };
    out.extend_from_slice(&body);
    out
}

/// Inverse of [`encrypt_with_iv`], tolerant the way real files require: empty
/// input is empty output, a trailing partial block is ignored, and padding
/// that is missing or malformed leaves the decrypted blocks untouched
/// instead of failing. Only input too short to hold an IV is an error.
pub(crate) fn decrypt_with_iv(key: &Key, data: &[u8]) -> Result<Vec<u8>, CryptError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.len() < BLOCK {
        return Err(CryptError::Corrupt(format!(
            "AES data of {} bytes is shorter than its 16-byte IV",
            data.len()
        )));
    }
    let (iv, body) = data.split_at(BLOCK);
    let iv: [u8; BLOCK] = iv
        .try_into()
        .map_err(|_| CryptError::Corrupt("AES IV".into()))?;
    let body = &body[..body.len() - body.len() % BLOCK];
    let mut plain = cbc_decrypt(key, &iv, body)?;
    strip_pkcs5(&mut plain);
    Ok(plain)
}

fn strip_pkcs5(buf: &mut Vec<u8>) {
    let Some(&last) = buf.last() else { return };
    let pad = usize::from(last);
    if (1..=BLOCK).contains(&pad)
        && pad <= buf.len()
        && buf[buf.len() - pad..].iter().all(|&b| b == last)
    {
        buf.truncate(buf.len() - pad);
    }
}

/// CBC without padding; `body.len()` must be a multiple of 16.
pub(crate) fn cbc_decrypt(key: &Key, iv: &[u8; BLOCK], body: &[u8]) -> Result<Vec<u8>, CryptError> {
    let result = match key {
        Key::Aes128(k) => cbc::Decryptor::<aes::Aes128>::new(k.into(), iv.into())
            .decrypt_padded_vec_mut::<NoPadding>(body),
        Key::Aes256(k) => cbc::Decryptor::<aes::Aes256>::new(k.into(), iv.into())
            .decrypt_padded_vec_mut::<NoPadding>(body),
    };
    result.map_err(|_| {
        CryptError::Corrupt(format!(
            "AES data length {} is not a multiple of 16",
            body.len()
        ))
    })
}

/// CBC without padding; a trailing partial block of `body` is dropped.
pub(crate) fn cbc_encrypt(key: &Key, iv: &[u8; BLOCK], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut prev = *iv;
    for chunk in body.as_chunks::<BLOCK>().0 {
        let mut block = [0u8; BLOCK];
        for ((b, c), p) in block.iter_mut().zip(chunk).zip(prev) {
            *b = c ^ p;
        }
        prev = ecb_encrypt(key, &block);
        out.extend_from_slice(&prev);
    }
    out
}

pub(crate) fn ecb_encrypt(key: &Key, block: &[u8; BLOCK]) -> [u8; BLOCK] {
    let mut ga = GenericArray::from(*block);
    match key {
        Key::Aes128(k) => aes::Aes128::new(k.into()).encrypt_block(&mut ga),
        Key::Aes256(k) => aes::Aes256::new(k.into()).encrypt_block(&mut ga),
    }
    ga.into()
}

pub(crate) fn ecb_decrypt(key: &Key, block: &[u8; BLOCK]) -> [u8; BLOCK] {
    let mut ga = GenericArray::from(*block);
    match key {
        Key::Aes128(k) => aes::Aes128::new(k.into()).decrypt_block(&mut ga),
        Key::Aes256(k) => aes::Aes256::new(k.into()).decrypt_block(&mut ga),
    }
    ga.into()
}

#[cfg(test)]
mod tests {
    use super::{Key, cbc_decrypt, cbc_encrypt, decrypt_with_iv, encrypt_with_iv};

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // NIST SP 800-38A, F.2.1 (CBC-AES128) and F.2.5 (CBC-AES256): four blocks,
    // IV 000102...0f.
    const IV: &str = "000102030405060708090a0b0c0d0e0f";
    const PLAIN: &str = "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51\
                         30c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710";
    const KEY128: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const KEY256: &str = "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4";
    const CIPHER256: &str = "f58c4c04d6e5f1ba779eabfb5f7bfbd69cfc4e967edb808d679f777bc6702c7d\
                             39f23369a9d9bacfa530e26304231461b2eb05e2c39be9fcda6c19078c6a9d1b";
    const CIPHER128: &str = "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2\
                             73bed6b8e3c1743b7116e69e222295163ff1caa1681fac09120eca307586e1a7";

    fn key128() -> Key {
        Key::Aes128(unhex(KEY128).try_into().unwrap())
    }

    fn key256() -> Key {
        Key::Aes256(unhex(KEY256).try_into().unwrap())
    }

    fn iv() -> [u8; 16] {
        unhex(IV).try_into().unwrap()
    }

    #[test]
    fn cbc_matches_nist_aes128_vector() {
        assert_eq!(
            cbc_encrypt(&key128(), &iv(), &unhex(PLAIN)),
            unhex(CIPHER128)
        );
        assert_eq!(
            cbc_decrypt(&key128(), &iv(), &unhex(CIPHER128)).unwrap(),
            unhex(PLAIN)
        );
    }

    #[test]
    fn cbc_matches_nist_aes256_vector() {
        assert_eq!(
            cbc_encrypt(&key256(), &iv(), &unhex(PLAIN)),
            unhex(CIPHER256)
        );
        assert_eq!(
            cbc_decrypt(&key256(), &iv(), &unhex(CIPHER256)).unwrap(),
            unhex(PLAIN)
        );
    }

    #[test]
    fn iv_prefixed_decrypt_of_nist_vector_keeps_unpadded_blocks() {
        // The vector carries no PKCS#5 padding and its last block does not
        // look like one, so every byte comes back.
        let mut data = unhex(IV);
        data.extend(unhex(CIPHER128));
        assert_eq!(decrypt_with_iv(&key128(), &data).unwrap(), unhex(PLAIN));
    }

    #[test]
    fn iv_prefixed_round_trip_strips_padding_for_every_length() {
        for len in [0usize, 1, 15, 16, 17, 31, 32, 100] {
            let plain: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let cipher = encrypt_with_iv(&key256(), &plain);
            assert_eq!(cipher.len(), 16 + (len / 16 + 1) * 16, "len {len}");
            assert_eq!(
                decrypt_with_iv(&key256(), &cipher).unwrap(),
                plain,
                "len {len}"
            );
        }
    }

    #[test]
    fn iv_prefixed_decrypt_tolerates_trailing_partial_block_and_empty_input() {
        let plain = b"twenty bytes of text";
        let mut cipher = encrypt_with_iv(&key128(), plain);
        cipher.extend_from_slice(b"\r\n");
        assert_eq!(decrypt_with_iv(&key128(), &cipher).unwrap(), plain);
        assert_eq!(decrypt_with_iv(&key128(), &[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn iv_prefixed_decrypt_rejects_input_shorter_than_iv() {
        let err = decrypt_with_iv(&key128(), &[1, 2, 3]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "corrupt encrypted data: AES data of 3 bytes is shorter than its 16-byte IV"
        );
    }
}
