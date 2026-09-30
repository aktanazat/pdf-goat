//! Revisions 5 and 6: AES-256 with SHA-2 password hashing (ISO 32000-2
//! algorithms 2.A, 2.B, 8, 9, 10; revision 5 is the Adobe extension level 3
//! draft that hashes with plain SHA-256).

use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::CryptError;
use crate::aescbc::{self, Key};

/// Fields of a revision 5 or 6 encryption dictionary, already cut to size.
pub(crate) struct Params<'a> {
    /// 5 or 6.
    pub revision: u8,
    pub o: &'a [u8; 48],
    pub u: &'a [u8; 48],
    pub oe: Option<&'a [u8; 32]>,
    pub ue: Option<&'a [u8; 32]>,
    pub perms: Option<&'a [u8; 16]>,
    pub p: i32,
    pub encrypt_metadata: bool,
}

pub(crate) struct Authenticated {
    pub key: [u8; 32],
    pub is_owner: bool,
    pub perms_valid: bool,
}

/// Everything `for_new_document` needs to write; the handler itself comes
/// from authenticating against these entries.
pub(crate) struct NewEntries {
    pub o: [u8; 48],
    pub u: [u8; 48],
    pub oe: [u8; 32],
    pub ue: [u8; 32],
    pub perms: [u8; 16],
}

/// Algorithm 2.B (revision 6) or plain SHA-256 (revision 5) of
/// `password || salt || udata`.
pub(crate) fn hash(revision: u8, password: &[u8], salt: &[u8], udata: &[u8]) -> [u8; 32] {
    let mut k: Vec<u8> = Sha256::new()
        .chain_update(password)
        .chain_update(salt)
        .chain_update(udata)
        .finalize()
        .to_vec();
    if revision == 5 {
        return first32(&k);
    }
    let mut round = 0usize;
    loop {
        round += 1;
        let unit = password.len() + k.len() + udata.len();
        let mut k1 = Vec::with_capacity(unit * 64);
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let e = aescbc::cbc_encrypt(&Key::Aes128(first16(&k)), &second16(&k), &k1);
        // The first 16 bytes as a big-endian integer mod 3; 256 = 1 mod 3, so
        // the byte sum has the same remainder.
        let remainder = e[..16].iter().map(|&b| u32::from(b)).sum::<u32>() % 3;
        k = match remainder {
            0 => Sha256::digest(&e).to_vec(),
            1 => Sha384::digest(&e).to_vec(),
            _ => Sha512::digest(&e).to_vec(),
        };
        let last = usize::from(e.last().copied().unwrap_or(0));
        if round >= 64 && last <= round - 32 {
            return first32(&k);
        }
    }
}

fn first16(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[..16]);
    out
}

fn second16(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[16..32]);
    out
}

fn first32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    out
}

/// Unwraps `/OE` or `/UE` with the intermediate key (AES-256 CBC, zero IV,
/// no padding).
fn unwrap_file_key(intermediate: [u8; 32], wrapped: &[u8; 32]) -> Result<[u8; 32], CryptError> {
    let plain = aescbc::cbc_decrypt(&Key::Aes256(intermediate), &[0u8; 16], wrapped)?;
    Ok(first32(&plain))
}

fn wrap_file_key(intermediate: [u8; 32], key: &[u8; 32]) -> [u8; 32] {
    first32(&aescbc::cbc_encrypt(
        &Key::Aes256(intermediate),
        &[0u8; 16],
        key,
    ))
}

fn split48(entry: &[u8; 48]) -> (&[u8], &[u8], &[u8]) {
    (&entry[..32], &entry[32..40], &entry[40..48])
}

/// Algorithm 2.A: `password` is already SASLprep'd and cut to 127 bytes.
/// Both the user and the owner check run so `is_owner` is right when the two
/// passwords coincide; `/Perms` is verified but a mismatch is only reported.
pub(crate) fn authenticate(
    params: &Params<'_>,
    password: &[u8],
) -> Result<Authenticated, CryptError> {
    let (u_hash, u_validation_salt, u_key_salt) = split48(params.u);
    let (o_hash, o_validation_salt, o_key_salt) = split48(params.o);
    let is_user = hash(params.revision, password, u_validation_salt, &[]) == u_hash;
    let is_owner = hash(params.revision, password, o_validation_salt, params.u) == o_hash;
    let key = if is_user {
        let Some(ue) = params.ue else {
            return Err(CryptError::Unsupported(
                "user password matches but /UE is missing".into(),
            ));
        };
        unwrap_file_key(hash(params.revision, password, u_key_salt, &[]), ue)?
    } else if is_owner {
        let Some(oe) = params.oe else {
            return Err(CryptError::Unsupported(
                "owner password matches but /OE is missing".into(),
            ));
        };
        unwrap_file_key(hash(params.revision, password, o_key_salt, params.u), oe)?
    } else {
        return Err(CryptError::WrongPassword);
    };
    let perms_valid = params.perms.is_some_and(|perms| {
        let block = aescbc::ecb_decrypt(&Key::Aes256(key), perms);
        let p = i32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        &block[9..12] == b"adb"
            && p == params.p
            && block[8] == metadata_flag(params.encrypt_metadata)
    });
    Ok(Authenticated {
        key,
        is_owner,
        perms_valid,
    })
}

fn metadata_flag(encrypt_metadata: bool) -> u8 {
    if encrypt_metadata { b'T' } else { b'F' }
}

/// Algorithms 8, 9, and 10 for a revision 6 document with a fresh random
/// file key. Passwords are already SASLprep'd and cut to 127 bytes.
pub(crate) fn new_document(
    user_password: &[u8],
    owner_password: &[u8],
    p: i32,
    encrypt_metadata: bool,
) -> NewEntries {
    let key: [u8; 32] = aescbc::random();
    let (u, ue) = key_entries(user_password, &[], &key);
    let (o, oe) = key_entries(owner_password, &u, &key);

    let mut perms_block = [0u8; 16];
    perms_block[..4].copy_from_slice(&p.to_le_bytes());
    perms_block[4..8].copy_from_slice(&[0xff; 4]);
    perms_block[8] = metadata_flag(encrypt_metadata);
    perms_block[9..12].copy_from_slice(b"adb");
    perms_block[12..].copy_from_slice(&aescbc::random::<4>());
    let perms = aescbc::ecb_encrypt(&Key::Aes256(key), &perms_block);

    NewEntries {
        o,
        u,
        oe,
        ue,
        perms,
    }
}

/// Algorithm 8 (`udata` empty) or 9 (`udata` = `/U`): the 48-byte entry and
/// its wrapped file key.
fn key_entries(password: &[u8], udata: &[u8], key: &[u8; 32]) -> ([u8; 48], [u8; 32]) {
    let salts: [u8; 16] = aescbc::random();
    let (validation_salt, key_salt) = salts.split_at(8);
    let mut entry = [0u8; 48];
    entry[..32].copy_from_slice(&hash(6, password, validation_salt, udata));
    entry[32..].copy_from_slice(&salts);
    let wrapped = wrap_file_key(hash(6, password, key_salt, udata), key);
    (entry, wrapped)
}
