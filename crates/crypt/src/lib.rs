//! PDF standard security handler: RC4 and AES, revisions 2 to 6.
//!
//! INTERFACE PIN: pdf-core compiles against the signatures below. The crypt
//! owner may add items, but must not change a pinned signature without
//! telling pdf-core.
//!
//! Algorithms follow ISO 32000-1 7.6.3 (revisions 2 to 4: MD5, RC4, AES-128
//! through `/AESV2`) and ISO 32000-2 7.6.4 (revision 6: AES-256, hash
//! algorithm 2.B; revision 5 is the Adobe extension level 3 draft with plain
//! SHA-256). Passwords for revisions 2 to 4 are bytes as given (PDFDocEncoding
//! is the caller's job); revisions 5 and 6 SASLprep them and cut them to 127
//! bytes of UTF-8.

mod aes256;
mod aescbc;
mod legacy;
mod rc4;
mod saslprep;

#[cfg(test)]
mod oracle_tests;

use aescbc::Key;

/// How one class of data (strings, streams, embedded files) is encrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptMethod {
    /// `/Identity` or no crypt filter: bytes pass through.
    None,
    /// `/V2` RC4 with the file key length.
    Rc4,
    /// `/AESV2`: AES-128-CBC, random 16-byte IV prefix, PKCS#5 padding.
    AesV2,
    /// `/AESV3`: AES-256-CBC, random 16-byte IV prefix, PKCS#5 padding.
    AesV3,
}

/// The fields of an `/Encrypt` dictionary (filter `/Standard`) after pdf-core
/// has resolved them. Crypt filters are already resolved to methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptDict {
    pub v: i32,
    pub r: i32,
    /// Key length in bits (`/Length`, default 40).
    pub length_bits: u32,
    pub o: Vec<u8>,
    pub u: Vec<u8>,
    pub oe: Option<Vec<u8>>,
    pub ue: Option<Vec<u8>>,
    pub perms: Option<Vec<u8>>,
    pub p: i32,
    pub encrypt_metadata: bool,
    pub string_method: CryptMethod,
    pub stream_method: CryptMethod,
    pub embedded_file_method: CryptMethod,
}

/// Which kind of object data is being transformed; selects the method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataKind {
    String,
    Stream,
    EmbeddedFile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewMethod {
    Rc4_128,
    Aes128,
    Aes256,
}

/// Parameters for encrypting a document on save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEncryption {
    pub method: NewMethod,
    pub user_password: Vec<u8>,
    pub owner_password: Vec<u8>,
    /// `/P` value: bits per ISO 32000-2 Table 22, reserved bits set as required.
    pub permissions: i32,
    pub encrypt_metadata: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptError {
    /// Neither the user nor the owner password opens the document.
    WrongPassword,
    /// A revision, method, or dictionary shape this handler does not support.
    Unsupported(String),
    /// Ciphertext that cannot be decrypted (bad length or padding).
    Corrupt(String),
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptError::WrongPassword => f.write_str("incorrect password"),
            CryptError::Unsupported(what) => write!(f, "unsupported encryption: {what}"),
            CryptError::Corrupt(what) => write!(f, "corrupt encrypted data: {what}"),
        }
    }
}

impl std::error::Error for CryptError {}

/// An authenticated handler holding the file key.
#[derive(Clone, Debug)]
pub struct SecurityHandler {
    /// 5 to 16 bytes for revisions 2 to 4, 32 bytes for 5 and 6.
    key: Vec<u8>,
    v: i32,
    r: i32,
    /// Key length in bits for the description when the stream method is
    /// `None`: what `/Length` declares after the byte-vs-bit repair.
    declared_bits: u32,
    p: i32,
    encrypt_metadata: bool,
    is_owner: bool,
    /// `None` below revision 5, where there is no `/Perms`.
    perms_valid: Option<bool>,
    string_method: CryptMethod,
    stream_method: CryptMethod,
    embedded_file_method: CryptMethod,
}

/// The dictionary after validation: which algorithm family runs and how long
/// the file key is.
struct Shape {
    /// 2 to 6 (`/R` 1 is treated as 2).
    revision: u8,
    /// File key length in bytes.
    key_len: usize,
    declared_bits: u32,
    string_method: CryptMethod,
    stream_method: CryptMethod,
    embedded_file_method: CryptMethod,
}

impl Shape {
    fn resolve(dict: &EncryptDict) -> Result<Shape, CryptError> {
        let revision = match dict.r {
            1 | 2 => 2,
            3..=6 => dict.r as u8,
            r => return Err(CryptError::Unsupported(format!("revision {r}"))),
        };
        if !(1..=5).contains(&dict.v) {
            return Err(CryptError::Unsupported(format!("V {}", dict.v)));
        }
        // Below V4 there are no crypt filters: everything is RC4.
        let (string_method, stream_method, embedded_file_method) = if dict.v < 4 {
            (CryptMethod::Rc4, CryptMethod::Rc4, CryptMethod::Rc4)
        } else {
            (
                dict.string_method,
                dict.stream_method,
                dict.embedded_file_method,
            )
        };
        let methods = [string_method, stream_method, embedded_file_method];
        if revision < 5 && methods.contains(&CryptMethod::AesV3) {
            return Err(CryptError::Unsupported(format!(
                "AESV3 crypt filter with revision {}",
                dict.r
            )));
        }

        // Some producers write /Length in bytes; MuPDF applies the same repair.
        let mut declared_bits = dict.length_bits;
        if declared_bits < 40 {
            declared_bits *= 8;
        }
        let key_len = if revision >= 5 {
            declared_bits = 256;
            32
        } else if methods.contains(&CryptMethod::AesV2) {
            16
        } else if dict.v == 1 {
            declared_bits = 40;
            5
        } else {
            if !declared_bits.is_multiple_of(8) || !(40..=128).contains(&declared_bits) {
                return Err(CryptError::Unsupported(format!(
                    "key length {} bits",
                    dict.length_bits
                )));
            }
            declared_bits as usize / 8
        };
        Ok(Shape {
            revision,
            key_len,
            declared_bits,
            string_method,
            stream_method,
            embedded_file_method,
        })
    }
}

fn fixed<const N: usize>(entry: &[u8], name: &str) -> Result<[u8; N], CryptError> {
    let Some(head) = entry.get(..N) else {
        return Err(CryptError::Unsupported(format!(
            "{name} is {} bytes, {N} needed",
            entry.len()
        )));
    };
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Ok(out)
}

fn fixed_opt<const N: usize>(entry: Option<&Vec<u8>>) -> Option<[u8; N]> {
    let head = entry?.get(..N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Some(out)
}

fn key128(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = bytes.len().min(16);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

fn key256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = bytes.len().min(32);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

fn method_word(method: CryptMethod) -> &'static str {
    match method {
        CryptMethod::None => "None",
        CryptMethod::Rc4 => "RC4",
        CryptMethod::AesV2 | CryptMethod::AesV3 => "AES",
    }
}

impl SecurityHandler {
    /// Try `password` as the user password, then as the owner password.
    /// `file_id0` is the first element of the trailer `/ID` (empty if absent).
    ///
    /// Both checks always run, so [`is_owner`](Self::is_owner) is true whenever
    /// the password is the owner password, including when the two passwords
    /// coincide. A revision 5 or 6 password that SASLprep rejects cannot be
    /// the document's password and reports `WrongPassword`.
    pub fn authenticate(
        dict: &EncryptDict,
        file_id0: &[u8],
        password: &[u8],
    ) -> Result<SecurityHandler, CryptError> {
        let shape = Shape::resolve(dict)?;
        let (key, is_owner, perms_valid) = if shape.revision >= 5 {
            let o: [u8; 48] = fixed(&dict.o, "/O")?;
            let u: [u8; 48] = fixed(&dict.u, "/U")?;
            let oe: Option<[u8; 32]> = fixed_opt(dict.oe.as_ref());
            let ue: Option<[u8; 32]> = fixed_opt(dict.ue.as_ref());
            let perms: Option<[u8; 16]> = fixed_opt(dict.perms.as_ref());
            let params = aes256::Params {
                revision: shape.revision,
                o: &o,
                u: &u,
                oe: oe.as_ref(),
                ue: ue.as_ref(),
                perms: perms.as_ref(),
                p: dict.p,
                encrypt_metadata: dict.encrypt_metadata,
            };
            let password = saslprep::prepare(password).map_err(|_| CryptError::WrongPassword)?;
            let auth = aes256::authenticate(&params, &password)?;
            (auth.key.to_vec(), auth.is_owner, Some(auth.perms_valid))
        } else {
            let o: [u8; 32] = fixed(&dict.o, "/O")?;
            let u: [u8; 32] = fixed(&dict.u, "/U")?;
            let params = legacy::Params {
                revision: shape.revision,
                key_len: shape.key_len,
                o: &o,
                p: dict.p,
                id0: file_id0,
                encrypt_metadata: dict.encrypt_metadata,
            };
            let as_owner = legacy::check_owner(&params, &u, password);
            let key = match legacy::check_user(&params, &u, password) {
                Some(key) => key,
                None => as_owner.clone().ok_or(CryptError::WrongPassword)?,
            };
            (key, as_owner.is_some(), None)
        };
        Ok(SecurityHandler {
            key,
            v: dict.v,
            r: dict.r,
            declared_bits: shape.declared_bits,
            p: dict.p,
            encrypt_metadata: dict.encrypt_metadata,
            is_owner,
            perms_valid,
            string_method: shape.string_method,
            stream_method: shape.stream_method,
            embedded_file_method: shape.embedded_file_method,
        })
    }

    /// Build a handler and the dictionary to write for a newly encrypted file.
    ///
    /// `Rc4_128` writes V2 R3, `Aes128` V4 R4 with `/AESV2` crypt filters,
    /// `Aes256` V5 R6 with `/AESV3`. An empty owner password means the user
    /// password is also the owner password. The returned handler authenticated
    /// with the owner password against the dictionary it returns.
    pub fn for_new_document(
        params: &NewEncryption,
        file_id0: &[u8],
    ) -> Result<(SecurityHandler, EncryptDict), CryptError> {
        let p = params.permissions;
        let user = &params.user_password;
        let owner = if params.owner_password.is_empty() {
            user
        } else {
            &params.owner_password
        };
        let mut dict = EncryptDict {
            v: 0,
            r: 0,
            length_bits: 0,
            o: Vec::new(),
            u: Vec::new(),
            oe: None,
            ue: None,
            perms: None,
            p,
            encrypt_metadata: params.encrypt_metadata,
            string_method: CryptMethod::None,
            stream_method: CryptMethod::None,
            embedded_file_method: CryptMethod::None,
        };
        match params.method {
            NewMethod::Rc4_128 | NewMethod::Aes128 => {
                let (v, r, method) = if params.method == NewMethod::Rc4_128 {
                    if !params.encrypt_metadata {
                        return Err(CryptError::Unsupported(
                            "RC4-128 (V2 R3) cannot leave metadata unencrypted; use AES-128 or AES-256".into(),
                        ));
                    }
                    (2, 3, CryptMethod::Rc4)
                } else {
                    (4, 4, CryptMethod::AesV2)
                };
                let o = legacy::compute_o(r as u8, 16, owner, user);
                let legacy = legacy::Params {
                    revision: r as u8,
                    key_len: 16,
                    o: &o,
                    p,
                    id0: file_id0,
                    encrypt_metadata: params.encrypt_metadata,
                };
                let key = legacy::file_key(&legacy, user);
                dict.v = v;
                dict.r = r;
                dict.length_bits = 128;
                dict.o = o.to_vec();
                dict.u = legacy::compute_u(&legacy, &key).to_vec();
                dict.string_method = method;
                dict.stream_method = method;
                dict.embedded_file_method = method;
            }
            NewMethod::Aes256 => {
                let user = saslprep::prepare(user).map_err(CryptError::Unsupported)?;
                let owner = saslprep::prepare(owner).map_err(CryptError::Unsupported)?;
                let entries = aes256::new_document(&user, &owner, p, params.encrypt_metadata);
                dict.v = 5;
                dict.r = 6;
                dict.length_bits = 256;
                dict.o = entries.o.to_vec();
                dict.u = entries.u.to_vec();
                dict.oe = Some(entries.oe.to_vec());
                dict.ue = Some(entries.ue.to_vec());
                dict.perms = Some(entries.perms.to_vec());
                dict.string_method = CryptMethod::AesV3;
                dict.stream_method = CryptMethod::AesV3;
                dict.embedded_file_method = CryptMethod::AesV3;
            }
        }
        let handler = SecurityHandler::authenticate(&dict, file_id0, owner)?;
        Ok((handler, dict))
    }

    fn method_for(&self, kind: DataKind) -> CryptMethod {
        match kind {
            DataKind::String => self.string_method,
            DataKind::Stream => self.stream_method,
            DataKind::EmbeddedFile => self.embedded_file_method,
        }
    }

    /// Algorithm 1: the per-object key for RC4 and `/AESV2`.
    fn object_key(&self, num: u32, generation: u16, aes: bool) -> Vec<u8> {
        legacy::object_key(&self.key, num, generation, aes)
    }

    pub fn decrypt(
        &self,
        num: u32,
        generation: u16,
        kind: DataKind,
        data: &[u8],
    ) -> Result<Vec<u8>, CryptError> {
        match self.method_for(kind) {
            CryptMethod::None => Ok(data.to_vec()),
            CryptMethod::Rc4 => Ok(rc4::apply(&self.object_key(num, generation, false), data)),
            CryptMethod::AesV2 => aescbc::decrypt_with_iv(
                &Key::Aes128(key128(&self.object_key(num, generation, true))),
                data,
            ),
            CryptMethod::AesV3 => aescbc::decrypt_with_iv(&Key::Aes256(key256(&self.key)), data),
        }
    }

    pub fn encrypt(&self, num: u32, generation: u16, kind: DataKind, data: &[u8]) -> Vec<u8> {
        match self.method_for(kind) {
            CryptMethod::None => data.to_vec(),
            CryptMethod::Rc4 => rc4::apply(&self.object_key(num, generation, false), data),
            CryptMethod::AesV2 => aescbc::encrypt_with_iv(
                &Key::Aes128(key128(&self.object_key(num, generation, true))),
                data,
            ),
            CryptMethod::AesV3 => aescbc::encrypt_with_iv(&Key::Aes256(key256(&self.key)), data),
        }
    }

    /// True when the owner password authenticated.
    pub fn is_owner(&self) -> bool {
        self.is_owner
    }

    /// The `/P` permission bits.
    pub fn permissions(&self) -> i32 {
        self.p
    }

    /// Revision 5 and 6 only: whether `/Perms` decrypted to the `adb` marker
    /// with the same permissions and metadata flag as the dictionary. `None`
    /// below revision 5. A mismatch does not stop authentication.
    pub fn perms_valid(&self) -> Option<bool> {
        self.perms_valid
    }

    /// Human description in MuPDF's format, e.g. `Standard V4 R4 128-bit AES`
    /// or `Standard V2 R3 128-bit RC4`. When streams and strings use different
    /// methods MuPDF names both: `Standard V4 R4 128-bit streams: AES strings: None`.
    pub fn description(&self) -> String {
        let bits = match self.stream_method {
            CryptMethod::None => self.declared_bits,
            _ => self.key.len() as u32 * 8,
        };
        let streams = method_word(self.stream_method);
        let strings = method_word(self.string_method);
        if streams == strings {
            format!("Standard V{} R{} {bits}-bit {streams}", self.v, self.r)
        } else {
            format!(
                "Standard V{} R{} {bits}-bit streams: {streams} strings: {strings}",
                self.v, self.r
            )
        }
    }

    pub fn encrypts_metadata(&self) -> bool {
        self.encrypt_metadata
    }
}
