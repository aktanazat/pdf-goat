//! Opening documents: the header, cross-reference tables and streams, `/Prev`
//! chains, hybrid files, object streams, reconstruction of damaged files,
//! and decryption. Objects load lazily into a cache shared across threads.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use pdf_crypt::{CryptError, CryptMethod, DataKind, EncryptDict, SecurityHandler};

use crate::error::{Error, Result};
use crate::filters::{self, DEFAULT_DECODE_LIMIT, DecodedStream};
use crate::lexer::{Lexer, Token, find, is_regular, is_whitespace, rfind};
use crate::object::{Dict, Name, ObjRef, Object, Stream};
use crate::pages::PageTree;
use crate::parser::{MAX_DEPTH, Parser, object_stream_index, parse_indirect};

/// Longest chain of references followed while loading or resolving.
pub(crate) const MAX_REF_DEPTH: usize = 32;
/// Most cross-reference sections read through `/Prev`.
const MAX_XREF_SECTIONS: usize = 4096;
/// Highest object count ISO 32000 allows; bounds a lying trailer `/Size`.
const MAX_OBJECTS: u32 = 8_388_607;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XrefEntry {
    Free,
    Offset { offset: usize, generation: u16 },
    Compressed { stream: u32, index: u32 },
}

/// The kind of the newest cross-reference section; an incremental update
/// appends the same kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XrefKind {
    Table,
    Stream,
}

#[derive(Clone, Debug)]
pub(crate) enum Change {
    Set {
        generation: u16,
        object: Arc<Object>,
    },
    Deleted {
        generation: u16,
    },
}

pub(crate) struct Security {
    pub dict: EncryptDict,
    /// The `/Encrypt` dictionary as stored, written back when encryption is
    /// kept on save.
    pub raw: Dict,
    /// Object number of the `/Encrypt` dictionary when it is indirect.
    pub encrypt_num: Option<u32>,
    pub handler: Option<SecurityHandler>,
}

/// A PDF document: the original bytes, the merged cross-reference table,
/// lazily loaded objects, and pending changes. `Send + Sync`; reads take
/// `&self`, changes take `&mut self`.
pub struct Document {
    pub(crate) data: Arc<Vec<u8>>,
    pub(crate) header_version: (u8, u8),
    pub(crate) xref: BTreeMap<u32, XrefEntry>,
    pub(crate) xref_kind: XrefKind,
    /// Offset of the newest cross-reference section, when the chain was read.
    pub(crate) startxref: Option<usize>,
    pub(crate) trailer: Dict,
    pub(crate) repaired: bool,
    /// Object streams found while reconstructing, indexed once decryptable.
    pending_objstms: Vec<u32>,
    cache: RwLock<HashMap<u32, Arc<Object>>>,
    pub(crate) changes: BTreeMap<u32, Change>,
    pub(crate) next_num: u32,
    pub(crate) security: Option<Security>,
    pub(crate) page_tree: RwLock<Option<Arc<PageTree>>>,
    modified: bool,
    decode_limit: usize,
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("version", &self.header_version)
            .field("bytes", &self.data.len())
            .field("objects", &self.xref.len())
            .field("changes", &self.changes.len())
            .field("encrypted", &self.security.is_some())
            .field("repaired", &self.repaired)
            .finish()
    }
}

impl Default for Document {
    fn default() -> Document {
        Document::new()
    }
}

impl Document {
    fn empty(data: Arc<Vec<u8>>, version: (u8, u8)) -> Document {
        Document {
            data,
            header_version: version,
            xref: BTreeMap::new(),
            xref_kind: XrefKind::Table,
            startxref: None,
            trailer: Dict::new(),
            repaired: false,
            pending_objstms: Vec::new(),
            cache: RwLock::new(HashMap::new()),
            changes: BTreeMap::new(),
            next_num: 1,
            security: None,
            page_tree: RwLock::new(None),
            modified: false,
            decode_limit: DEFAULT_DECODE_LIMIT,
        }
    }

    /// An empty PDF 1.7 document: a catalog (`1 0 R`) and an empty page tree
    /// (`2 0 R`).
    pub fn new() -> Document {
        let mut doc = Document::empty(Arc::new(Vec::new()), (1, 7));
        let root = doc.reserve();
        let mut pages = Dict::new();
        pages.insert("Type", Object::name("Pages"));
        pages.insert("Kids", Vec::<Object>::new());
        pages.insert("Count", 0);
        let pages = doc.add(pages);
        let mut catalog = Dict::new();
        catalog.insert("Type", Object::name("Catalog"));
        catalog.insert("Pages", pages);
        doc.set(root, catalog);
        doc.trailer.insert("Root", root);
        doc
    }

    /// Read and open the file at `path`, trying the empty user password when
    /// it is encrypted.
    pub fn open(path: impl AsRef<Path>) -> Result<Document> {
        Document::load(std::fs::read(path)?)
    }

    /// [`Document::open`] trying `password` first, then the empty password.
    /// Fails with [`Error::WrongPassword`] when neither opens the file.
    pub fn open_with_password(path: impl AsRef<Path>, password: &[u8]) -> Result<Document> {
        Document::load_with_password(std::fs::read(path)?, password)
    }

    /// Open a document held in memory. An encrypted document that the empty
    /// user password does not open loads with [`Document::needs_password`]
    /// set.
    pub fn load(data: Vec<u8>) -> Result<Document> {
        Document::load_inner(data, None)
    }

    /// [`Document::load`] trying `password` first, then the empty password.
    pub fn load_with_password(data: Vec<u8>, password: &[u8]) -> Result<Document> {
        Document::load_inner(data, Some(password))
    }

    fn load_inner(data: Vec<u8>, password: Option<&[u8]>) -> Result<Document> {
        let (header_offset, version) = parse_header(&data);
        let data = Arc::new(data);
        let mut doc = Document::empty(Arc::clone(&data), version);
        match read_xref_chain(&data, header_offset) {
            Ok(chain) => {
                doc.xref = chain.entries;
                doc.trailer = chain.trailer;
                doc.xref_kind = chain.kind;
                doc.startxref = Some(chain.startxref);
                let broken = doc.fix_offsets(header_offset);
                if broken || chain.incomplete || !doc.trailer.contains_key(b"Root") {
                    doc.repair();
                }
            }
            Err(_) => doc.repair(),
        }
        doc.xref.remove(&0);
        doc.update_next_num();
        doc.setup_security(password)?;
        if !doc.needs_password() {
            if doc.catalog().is_err() && !doc.repaired {
                doc.repair();
                doc.update_next_num();
                doc.clear_cache();
            }
            doc.index_object_streams();
            doc.catalog()?;
        }
        Ok(doc)
    }

    fn update_next_num(&mut self) {
        let max_key = self.xref.keys().next_back().copied().unwrap_or(0);
        let size = self
            .trailer
            .get_i64(b"Size")
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        self.next_num = max_key.saturating_add(1).max(size.min(MAX_OBJECTS)).max(1);
    }

    /// Check that each in-file entry points at its object's header, fixing
    /// entries shifted by junk before `%PDF-`. True when some entry is still
    /// wrong.
    fn fix_offsets(&mut self, header_offset: usize) -> bool {
        let data = Arc::clone(&self.data);
        let mut broken = false;
        for (num, entry) in self.xref.iter_mut() {
            let XrefEntry::Offset { offset, .. } = entry else {
                continue;
            };
            if header_number(&data, *offset) == Some(*num) {
                continue;
            }
            if header_offset > 0 && header_number(&data, *offset + header_offset) == Some(*num) {
                *offset += header_offset;
                continue;
            }
            broken = true;
        }
        broken
    }

    /// Rebuild the cross-reference table by scanning the file for `N G obj`
    /// and trailers. Valid entries already read are kept.
    fn repair(&mut self) {
        let scan = scan_file(&self.data);
        let chain_read = !self.xref.is_empty();
        for (num, entry) in scan.entries {
            let keep = match self.xref.get(&num) {
                Some(XrefEntry::Offset { offset, .. }) => {
                    header_number(&self.data, *offset) == Some(num)
                }
                Some(XrefEntry::Compressed { .. }) => true,
                Some(XrefEntry::Free) => chain_read,
                None => false,
            };
            if !keep {
                self.xref.insert(num, entry);
            }
        }
        let data = Arc::clone(&self.data);
        for (num, entry) in self.xref.iter_mut() {
            if let XrefEntry::Offset { offset, .. } = entry
                && header_number(&data, *offset) != Some(*num)
            {
                *entry = XrefEntry::Free;
            }
        }
        for (key, value) in scan.trailer {
            if !self.trailer.contains_key(key.as_bytes()) {
                self.trailer.insert(key, value);
            }
        }
        let root_known = match self.trailer.get(b"Root") {
            Some(Object::Reference(id)) => matches!(
                self.xref.get(&id.num),
                Some(XrefEntry::Offset { .. } | XrefEntry::Compressed { .. })
            ),
            _ => false,
        };
        if let (false, Some(catalog)) = (root_known, scan.catalog) {
            self.trailer.insert("Root", catalog);
        }
        self.pending_objstms = scan.objstms;
        self.repaired = true;
    }

    /// Index the objects inside object streams found by reconstruction.
    fn index_object_streams(&mut self) {
        let streams = std::mem::take(&mut self.pending_objstms);
        if streams.is_empty() {
            return;
        }
        let mut found = BTreeMap::new();
        for stream_num in streams {
            let Ok(Some(object)) = self.fetch(stream_num, 0) else {
                continue;
            };
            let Object::Stream(stream) = object.as_ref() else {
                continue;
            };
            let (Some(count), Some(first)) =
                (stream.dict.get_i64(b"N"), stream.dict.get_i64(b"First"))
            else {
                continue;
            };
            let (Ok(count), Ok(first)) = (usize::try_from(count), usize::try_from(first)) else {
                continue;
            };
            let Ok(decoded) = self.decode_stream_depth(stream, 1) else {
                continue;
            };
            let Ok(index) = object_stream_index(&decoded.data, count, first) else {
                continue;
            };
            for (i, (num, _)) in index.into_iter().enumerate() {
                let Ok(i) = u32::try_from(i) else { break };
                found.insert(
                    num,
                    XrefEntry::Compressed {
                        stream: stream_num,
                        index: i,
                    },
                );
            }
        }
        for (num, entry) in found {
            if num != 0 && !matches!(self.xref.get(&num), Some(XrefEntry::Offset { .. })) {
                self.xref.insert(num, entry);
            }
        }
        self.update_next_num();
        self.clear_cache();
    }

    fn setup_security(&mut self, password: Option<&[u8]>) -> Result<()> {
        let (encrypt_num, raw) = match self.trailer.get(b"Encrypt").cloned() {
            None | Some(Object::Null) => return Ok(()),
            Some(Object::Reference(id)) => match self.fetch(id.num, 0)?.as_deref() {
                Some(Object::Dict(dict)) => (Some(id.num), dict.clone()),
                _ => return Err(Error::invalid("the /Encrypt entry is not a dictionary")),
            },
            Some(Object::Dict(dict)) => (None, dict),
            Some(_) => return Err(Error::invalid("the /Encrypt entry is not a dictionary")),
        };
        match raw.get_name(b"Filter") {
            Some(b"Standard") => {}
            Some(other) => {
                return Err(Error::Unsupported(format!(
                    "encryption filter /{}",
                    String::from_utf8_lossy(other)
                )));
            }
            None => return Err(Error::invalid("the /Encrypt dictionary has no /Filter")),
        }
        let dict = self.encrypt_dict_from(&raw)?;
        let id0 = self.file_id0();
        let mut result = Err(CryptError::WrongPassword);
        if let Some(password) = password {
            result = SecurityHandler::authenticate(&dict, &id0, password);
        }
        if matches!(result, Err(CryptError::WrongPassword)) {
            result = SecurityHandler::authenticate(&dict, &id0, b"");
        }
        let handler = match result {
            Ok(handler) => Some(handler),
            Err(CryptError::WrongPassword) if password.is_none() => None,
            Err(err) => return Err(err.into()),
        };
        self.security = Some(Security {
            dict,
            raw,
            encrypt_num,
            handler,
        });
        self.clear_cache();
        Ok(())
    }

    /// Map an `/Encrypt` dictionary onto [`EncryptDict`], resolving crypt
    /// filters to methods. For V4 and V5 the key length comes from the
    /// stream (or string) crypt filter's `/Length` when present.
    fn encrypt_dict_from(&self, raw: &Dict) -> Result<EncryptDict> {
        let get = |key: &[u8]| self.resolve_key(raw, key);
        let bytes = |key: &[u8]| -> Result<Option<Vec<u8>>> {
            Ok(get(key)?.as_string().map(|s| s.bytes.clone()))
        };
        let int = |key: &[u8]| -> Result<Option<i64>> { Ok(get(key)?.as_i64()) };
        let v = int(b"V")?.unwrap_or(0);
        let r = int(b"R")?.unwrap_or(0);
        let o = bytes(b"O")?.ok_or_else(|| Error::invalid("the /Encrypt dictionary has no /O"))?;
        let u = bytes(b"U")?.ok_or_else(|| Error::invalid("the /Encrypt dictionary has no /U"))?;
        let p = int(b"P")?.unwrap_or(0);
        let mut length = int(b"Length")?;
        let (string_method, stream_method, embedded_file_method) = if v >= 4 {
            let filters = self.resolve_dict(&get(b"CF")?)?.unwrap_or_default();
            let name = |key: &[u8], default: &[u8]| -> Result<Vec<u8>> {
                Ok(get(key)?.as_name().unwrap_or(default).to_vec())
            };
            let stmf = name(b"StmF", b"Identity")?;
            let strf = name(b"StrF", b"Identity")?;
            let eff = name(b"EFF", &stmf)?;
            let mut methods = [CryptMethod::None; 3];
            let mut lengths = [None; 3];
            for (i, filter_name) in [&strf, &stmf, &eff].into_iter().enumerate() {
                if filter_name.as_slice() == b"Identity" {
                    continue;
                }
                let filter = match filters.get(filter_name) {
                    Some(value) => self.resolve_dict(value)?.unwrap_or_default(),
                    None => Dict::new(),
                };
                lengths[i] = self.resolve_key(&filter, b"Length")?.as_i64();
                methods[i] = match filter.get_name(b"CFM") {
                    Some(b"V2") => CryptMethod::Rc4,
                    Some(b"AESV2") => CryptMethod::AesV2,
                    Some(b"AESV3") => CryptMethod::AesV3,
                    _ => CryptMethod::None,
                };
            }
            // The stream filter's key length wins, then the string filter's.
            length = lengths[1]
                .or(lengths[0])
                .or(lengths[2])
                .or(length)
                .or(Some(128));
            (methods[0], methods[1], methods[2])
        } else {
            (CryptMethod::Rc4, CryptMethod::Rc4, CryptMethod::Rc4)
        };
        Ok(EncryptDict {
            v: i32::try_from(v).unwrap_or(0),
            r: i32::try_from(r).unwrap_or(0),
            length_bits: length.and_then(|n| u32::try_from(n).ok()).unwrap_or(40),
            o,
            u,
            oe: bytes(b"OE")?,
            ue: bytes(b"UE")?,
            perms: bytes(b"Perms")?,
            // `/P` is a signed 32-bit field; some writers store it unsigned.
            p: p as u32 as i32,
            encrypt_metadata: get(b"EncryptMetadata")?.as_bool().unwrap_or(true),
            string_method,
            stream_method,
            embedded_file_method,
        })
    }

    /// The raw first element of the trailer `/ID`, or empty.
    pub(crate) fn file_id0(&self) -> Vec<u8> {
        self.file_id().map(|(first, _)| first).unwrap_or_default()
    }

    /// Both elements of the trailer `/ID`, when present.
    pub fn file_id(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let id = self.resolve_key(&self.trailer, b"ID").ok()?;
        let items = id.as_array()?;
        let first = items.first()?.as_string()?.bytes.clone();
        let second = items
            .get(1)
            .and_then(Object::as_string)
            .map_or_else(|| first.clone(), |s| s.bytes.clone());
        Some((first, second))
    }

    // ----- properties -------------------------------------------------------

    /// The effective version: the header's, raised by a catalog `/Version`.
    pub fn version(&self) -> (u8, u8) {
        let catalog = self.catalog().ok();
        let declared = catalog
            .as_ref()
            .and_then(|c| c.get_name(b"Version"))
            .and_then(parse_version_name);
        match declared {
            Some(v) if v > self.header_version => v,
            _ => self.header_version,
        }
    }

    /// The version in the `%PDF-x.y` header.
    pub fn header_version(&self) -> (u8, u8) {
        self.header_version
    }

    /// True when the cross-reference data was damaged and rebuilt by scanning.
    pub fn was_repaired(&self) -> bool {
        self.repaired
    }

    /// The bytes the document was loaded from (empty for [`Document::new`]).
    pub fn source_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Bound on the decoded size of one stream (default 512 MiB).
    pub fn set_decode_limit(&mut self, limit: usize) {
        self.decode_limit = limit;
    }

    pub fn decode_limit(&self) -> usize {
        self.decode_limit
    }

    // ----- encryption -------------------------------------------------------

    pub fn is_encrypted(&self) -> bool {
        self.security.is_some()
    }

    /// True when the document is encrypted and no password has opened it.
    /// Until then strings and stream data are the encrypted bytes and
    /// [`Document::decode_stream`] fails with [`Error::NeedsPassword`].
    pub fn needs_password(&self) -> bool {
        self.security.as_ref().is_some_and(|s| s.handler.is_none())
    }

    /// Try `password` as the user, then the owner password. On success
    /// objects load decrypted from then on.
    pub fn authenticate(&mut self, password: &[u8]) -> Result<()> {
        let id0 = self.file_id0();
        let Some(security) = self.security.as_mut() else {
            return Ok(());
        };
        let handler = SecurityHandler::authenticate(&security.dict, &id0, password)?;
        security.handler = Some(handler);
        self.clear_cache();
        self.index_object_streams();
        Ok(())
    }

    /// True when the owner password opened the document.
    pub fn is_owner(&self) -> bool {
        self.security
            .as_ref()
            .and_then(|s| s.handler.as_ref())
            .is_some_and(SecurityHandler::is_owner)
    }

    /// The `/P` permission bits (ISO 32000-2 Table 22), or `None` when the
    /// document is not encrypted.
    pub fn permissions(&self) -> Option<i32> {
        self.security.as_ref().map(|s| {
            s.handler
                .as_ref()
                .map_or(s.dict.p, SecurityHandler::permissions)
        })
    }

    /// MuPDF-style description such as `Standard V4 R4 128-bit AES`, once a
    /// password has opened the document.
    pub fn encryption_description(&self) -> Option<String> {
        self.security
            .as_ref()?
            .handler
            .as_ref()
            .map(SecurityHandler::description)
    }

    /// The parsed `/Encrypt` dictionary.
    pub fn encrypt_dict(&self) -> Option<&EncryptDict> {
        self.security.as_ref().map(|s| &s.dict)
    }

    /// The authenticated handler, for encrypting new data consistently.
    pub fn security_handler(&self) -> Option<&SecurityHandler> {
        self.security.as_ref()?.handler.as_ref()
    }

    // ----- objects ----------------------------------------------------------

    fn cache_read(&self) -> RwLockReadGuard<'_, HashMap<u32, Arc<Object>>> {
        self.cache.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn cache_write(&self) -> RwLockWriteGuard<'_, HashMap<u32, Arc<Object>>> {
        self.cache.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn clear_cache(&mut self) {
        self.cache
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.invalidate_pages();
    }

    pub(crate) fn invalidate_pages(&mut self) {
        *self
            .page_tree
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The object `num` (generation ignored), `None` when free or absent.
    pub(crate) fn fetch(&self, num: u32, depth: usize) -> Result<Option<Arc<Object>>> {
        if depth > MAX_REF_DEPTH {
            return Err(Error::LimitExceeded(format!(
                "references nested deeper than {MAX_REF_DEPTH} levels"
            )));
        }
        if let Some(change) = self.changes.get(&num) {
            return Ok(match change {
                Change::Set { object, .. } => Some(Arc::clone(object)),
                Change::Deleted { .. } => None,
            });
        }
        if let Some(hit) = self.cache_read().get(&num) {
            return Ok(Some(Arc::clone(hit)));
        }
        let object = match self.xref.get(&num).copied() {
            None | Some(XrefEntry::Free) => return Ok(None),
            Some(XrefEntry::Compressed { stream, .. }) => {
                return self.load_compressed(num, stream, depth);
            }
            Some(XrefEntry::Offset { offset, .. }) => {
                let found =
                    parse_indirect(&self.data, offset, &|id| self.length_value(id, depth + 1))?;
                if found.id.num != num {
                    return Err(Error::syntax(
                        offset,
                        format!("expected object {num}, found {}", found.id),
                    ));
                }
                let mut object = found.object;
                self.decrypt(found.id, &mut object);
                Arc::new(object)
            }
        };
        self.cache_write()
            .entry(num)
            .or_insert_with(|| Arc::clone(&object));
        Ok(Some(object))
    }

    fn length_value(&self, id: ObjRef, depth: usize) -> Option<usize> {
        match self.fetch(id.num, depth).ok()??.as_ref() {
            Object::Integer(n) => usize::try_from(*n).ok(),
            _ => None,
        }
    }

    /// Load `num` from object stream `stream_num`, caching every object the
    /// stream holds that the cross-reference table still places there.
    fn load_compressed(
        &self,
        num: u32,
        stream_num: u32,
        depth: usize,
    ) -> Result<Option<Arc<Object>>> {
        if self.needs_password() {
            return Err(Error::NeedsPassword);
        }
        let Some(container) = self.fetch(stream_num, depth + 1)? else {
            return Ok(None);
        };
        let Object::Stream(stream) = container.as_ref() else {
            return Err(Error::invalid(format!(
                "object stream {stream_num} is not a stream"
            )));
        };
        let count = self
            .resolve_depth(stream.dict.get(b"N").unwrap_or(&Object::Null), depth + 1)?
            .as_i64();
        let first = self
            .resolve_depth(
                stream.dict.get(b"First").unwrap_or(&Object::Null),
                depth + 1,
            )?
            .as_i64();
        let (Some(Ok(count)), Some(Ok(first))) =
            (count.map(usize::try_from), first.map(usize::try_from))
        else {
            return Err(Error::invalid(format!(
                "object stream {stream_num} has no valid /N and /First"
            )));
        };
        let decoded = self.decode_stream_depth(stream, depth + 1)?.data;
        let index = object_stream_index(&decoded, count, first)?;
        let mut wanted = None;
        let mut parsed = Vec::new();
        for (i, &(obj_num, at)) in index.iter().enumerate() {
            let listed_here = u32::try_from(i).is_ok_and(|i| {
                self.xref.get(&obj_num)
                    == Some(&XrefEntry::Compressed {
                        stream: stream_num,
                        index: i,
                    })
            });
            if !listed_here && obj_num != num {
                continue;
            }
            let object = match Parser::new(&decoded, at).parse_object() {
                Ok(object) => Arc::new(object),
                Err(err) if obj_num == num && wanted.is_none() && listed_here => return Err(err),
                Err(_) => continue,
            };
            if obj_num == num && (listed_here || wanted.is_none()) {
                wanted = Some(Arc::clone(&object));
            }
            if listed_here {
                parsed.push((obj_num, object));
            }
        }
        let mut cache = self.cache_write();
        for (obj_num, object) in parsed {
            cache.entry(obj_num).or_insert(object);
        }
        Ok(wanted)
    }

    fn decrypt(&self, id: ObjRef, object: &mut Object) {
        let Some(security) = &self.security else {
            return;
        };
        let Some(handler) = &security.handler else {
            return;
        };
        if security.encrypt_num == Some(id.num) {
            return;
        }
        if matches!(object, Object::Stream(stream) if stream.dict.has_type(b"XRef")) {
            return;
        }
        let transform =
            |kind: DataKind, bytes: &[u8]| handler.decrypt(id.num, id.generation, kind, bytes).ok();
        crypt_object(object, &transform, handler.encrypts_metadata(), 0);
    }

    /// The object `id` (generation ignored). Strings and streams come back
    /// decrypted. Fails with [`Error::MissingObject`] when it is free or
    /// absent.
    pub fn get(&self, id: ObjRef) -> Result<Object> {
        match self.fetch(id.num, 0)? {
            Some(object) => Ok(object.as_ref().clone()),
            None => Err(Error::MissingObject(id)),
        }
    }

    /// True when `id` names an object that is in use.
    pub fn has_object(&self, id: ObjRef) -> bool {
        match self.changes.get(&id.num) {
            Some(Change::Set { .. }) => true,
            Some(Change::Deleted { .. }) => false,
            None => matches!(
                self.xref.get(&id.num),
                Some(XrefEntry::Offset { .. } | XrefEntry::Compressed { .. })
            ),
        }
    }

    /// Every object in use, by number, with its generation.
    pub fn object_ids(&self) -> Vec<ObjRef> {
        let mut ids = BTreeMap::new();
        for (&num, entry) in &self.xref {
            match entry {
                XrefEntry::Offset { generation, .. } => {
                    ids.insert(num, *generation);
                }
                XrefEntry::Compressed { .. } => {
                    ids.insert(num, 0);
                }
                XrefEntry::Free => {}
            }
        }
        for (&num, change) in &self.changes {
            match change {
                Change::Set { generation, .. } => {
                    ids.insert(num, *generation);
                }
                Change::Deleted { .. } => {
                    ids.remove(&num);
                }
            }
        }
        ids.into_iter()
            .map(|(num, generation)| ObjRef::new(num, generation))
            .collect()
    }

    /// One more than the highest object number (the trailer `/Size`).
    pub fn xref_size(&self) -> u32 {
        self.next_num
    }

    /// Follow references to a direct object. A reference to a free or absent
    /// object resolves to null.
    pub fn resolve(&self, object: &Object) -> Result<Object> {
        self.resolve_depth(object, 0)
    }

    fn resolve_depth(&self, object: &Object, depth: usize) -> Result<Object> {
        let Object::Reference(start) = object else {
            return Ok(object.clone());
        };
        let mut id = *start;
        for hop in 0..MAX_REF_DEPTH {
            match self.fetch(id.num, depth + hop)? {
                None => return Ok(Object::Null),
                Some(found) => match found.as_ref() {
                    Object::Reference(next) => id = *next,
                    other => return Ok(other.clone()),
                },
            }
        }
        Err(Error::LimitExceeded(format!(
            "reference chain longer than {MAX_REF_DEPTH} at {id}"
        )))
    }

    /// `dict[key]` with references followed; null when absent.
    pub fn resolve_key(&self, dict: &Dict, key: &[u8]) -> Result<Object> {
        match dict.get(key) {
            Some(value) => self.resolve(value),
            None => Ok(Object::Null),
        }
    }

    /// The dictionary `object` resolves to, if it is one.
    pub fn resolve_dict(&self, object: &Object) -> Result<Option<Dict>> {
        Ok(match self.resolve(object)? {
            Object::Dict(dict) => Some(dict),
            _ => None,
        })
    }

    /// The array `object` resolves to, if it is one.
    pub fn resolve_array(&self, object: &Object) -> Result<Option<Vec<Object>>> {
        Ok(match self.resolve(object)? {
            Object::Array(items) => Some(items),
            _ => None,
        })
    }

    /// The stream `object` resolves to, if it is one.
    pub fn resolve_stream(&self, object: &Object) -> Result<Option<Stream>> {
        Ok(match self.resolve(object)? {
            Object::Stream(stream) => Some(stream),
            _ => None,
        })
    }

    /// The integer `object` resolves to (a real is truncated).
    pub fn resolve_i64(&self, object: &Object) -> Result<Option<i64>> {
        Ok(self.resolve(object)?.as_i64())
    }

    /// The number `object` resolves to.
    pub fn resolve_f64(&self, object: &Object) -> Result<Option<f64>> {
        Ok(self.resolve(object)?.as_f64())
    }

    /// The name `object` resolves to.
    pub fn resolve_name(&self, object: &Object) -> Result<Option<Name>> {
        Ok(match self.resolve(object)? {
            Object::Name(name) => Some(name),
            _ => None,
        })
    }

    /// Run the stream's filters, resolving `/Filter` and `/DecodeParms`.
    /// Decoding stops before an image filter (DCT, JPX, JBIG2, CCITTFax),
    /// which is reported in [`DecodedStream::stopped`].
    pub fn decode_stream(&self, stream: &Stream) -> Result<DecodedStream> {
        if self.needs_password() {
            return Err(Error::NeedsPassword);
        }
        self.decode_stream_depth(stream, 0)
    }

    fn decode_stream_depth(&self, stream: &Stream, depth: usize) -> Result<DecodedStream> {
        let filter = match stream.dict.get(b"Filter") {
            Some(value) => Some(self.resolve_filter_value(value, depth)?),
            None => None,
        };
        let parms = match stream.dict.get(b"DecodeParms") {
            Some(value) => Some(self.resolve_filter_value(value, depth)?),
            None => None,
        };
        let chain = filters::filter_chain(filter.as_ref(), parms.as_ref());
        filters::decode(stream.raw(), &chain, self.decode_limit)
    }

    /// Resolve a `/Filter` or `/DecodeParms` value, its array items, and the
    /// values of the parameter dictionaries.
    fn resolve_filter_value(&self, value: &Object, depth: usize) -> Result<Object> {
        let resolve_parms = |object: Object| -> Result<Object> {
            match object {
                Object::Dict(dict) => {
                    let mut out = Dict::with_capacity(dict.len());
                    for (key, value) in dict {
                        let value = self.resolve_depth(&value, depth)?;
                        out.insert(key, value);
                    }
                    Ok(Object::Dict(out))
                }
                other => Ok(other),
            }
        };
        match self.resolve_depth(value, depth)? {
            Object::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in &items {
                    out.push(resolve_parms(self.resolve_depth(item, depth)?)?);
                }
                Ok(Object::Array(out))
            }
            other => resolve_parms(other),
        }
    }

    // ----- trailer and catalog ----------------------------------------------

    /// The merged trailer: `/Root`, `/Info`, `/ID`, `/Encrypt`, `/Size`.
    pub fn trailer(&self) -> &Dict {
        &self.trailer
    }

    /// Edit the trailer, invalidating cached pages because `/Root` may change.
    pub fn trailer_mut(&mut self) -> &mut Dict {
        self.modified = true;
        self.invalidate_pages();
        &mut self.trailer
    }

    /// The reference in the trailer's `/Root`.
    pub fn catalog_ref(&self) -> Result<ObjRef> {
        self.trailer
            .get_ref(b"Root")
            .ok_or_else(|| Error::invalid("the trailer has no /Root reference"))
    }

    /// The document catalog.
    pub fn catalog(&self) -> Result<Dict> {
        let root = self.catalog_ref()?;
        self.resolve_dict(&Object::Reference(root))?.ok_or_else(|| {
            Error::invalid(format!("the document catalog {root} is not a dictionary"))
        })
    }

    /// The `/Info` dictionary, when present.
    pub fn info(&self) -> Result<Option<Dict>> {
        match self.trailer.get(b"Info") {
            Some(info) => self.resolve_dict(info),
            None => Ok(None),
        }
    }

    /// Replace the `/Info` dictionary, writing it in place when `/Info` is
    /// an indirect object and adding a new object otherwise.
    pub fn set_info(&mut self, info: Dict) {
        match self
            .trailer
            .get_ref(b"Info")
            .filter(|id| self.has_object(*id))
        {
            Some(id) => self.set(id, info),
            None => {
                let id = self.add(info);
                self.trailer.insert("Info", id);
            }
        }
    }

    // ----- changes ----------------------------------------------------------

    /// Replace (or create) object `id`.
    pub fn set(&mut self, id: ObjRef, object: impl Into<Object>) {
        self.changes.insert(
            id.num,
            Change::Set {
                generation: id.generation,
                object: Arc::new(object.into()),
            },
        );
        self.next_num = self.next_num.max(id.num.saturating_add(1));
        self.modified = true;
        self.invalidate_pages();
    }

    /// Add `object` under a new number (generation 0).
    pub fn add(&mut self, object: impl Into<Object>) -> ObjRef {
        let id = ObjRef::new(self.next_num, 0);
        self.set(id, object);
        id
    }

    /// Allocate a new number holding null, to be filled with
    /// [`Document::set`]; for objects that refer to each other.
    pub fn reserve(&mut self) -> ObjRef {
        self.add(Object::Null)
    }

    /// Free object `id`.
    pub fn delete(&mut self, id: ObjRef) {
        let generation = match (self.changes.get(&id.num), self.xref.get(&id.num)) {
            (Some(Change::Set { generation, .. } | Change::Deleted { generation }), _) => {
                *generation
            }
            (None, Some(XrefEntry::Offset { generation, .. })) => *generation,
            _ => 0,
        };
        self.changes.insert(id.num, Change::Deleted { generation });
        self.modified = true;
        self.invalidate_pages();
    }

    /// True after any change through this API.
    pub fn is_modified(&self) -> bool {
        self.modified
    }

    pub fn mark_modified(&mut self) {
        self.modified = true;
    }

    /// Numbers set or deleted since loading, with their generations.
    pub fn changed_ids(&self) -> Vec<ObjRef> {
        self.changes
            .iter()
            .map(|(&num, change)| match change {
                Change::Set { generation, .. } | Change::Deleted { generation } => {
                    ObjRef::new(num, *generation)
                }
            })
            .collect()
    }
}

/// A string or stream transformation: encryption or decryption of one
/// object's data, `None` to leave the data as it is.
pub(crate) type CryptFn<'a> = dyn Fn(DataKind, &[u8]) -> Option<Vec<u8>> + 'a;

/// Apply `transform` to every string and stream in `object`, as encryption
/// and decryption do: signature `/Contents`, cross-reference streams,
/// unencrypted metadata, and `/Identity` crypt-filter streams are skipped.
pub(crate) fn crypt_object(
    object: &mut Object,
    transform: &CryptFn<'_>,
    encrypts_metadata: bool,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match object {
        Object::String(string) => {
            if let Some(out) = transform(DataKind::String, &string.bytes) {
                string.bytes = out;
            }
        }
        Object::Array(items) => {
            for item in items {
                crypt_object(item, transform, encrypts_metadata, depth + 1);
            }
        }
        Object::Dict(dict) => crypt_dict(dict, transform, encrypts_metadata, depth + 1),
        Object::Stream(stream) => {
            crypt_dict(&mut stream.dict, transform, encrypts_metadata, depth + 1);
            if let Some(kind) = stream_data_kind(&stream.dict, encrypts_metadata)
                && let Some(out) = transform(kind, stream.raw())
            {
                stream.set_raw(out);
            }
        }
        _ => {}
    }
}

pub(crate) fn crypt_dict(
    dict: &mut Dict,
    transform: &CryptFn<'_>,
    encrypts_metadata: bool,
    depth: usize,
) {
    let signature = is_signature_dict(dict);
    for (key, value) in dict.iter_mut() {
        if signature && key == "Contents" {
            continue;
        }
        crypt_object(value, transform, encrypts_metadata, depth);
    }
}

/// Signature dictionaries keep `/Contents` unencrypted (ISO 32000-2 7.6.2).
pub(crate) fn is_signature_dict(dict: &Dict) -> bool {
    dict.has_type(b"Sig")
        || dict.has_type(b"DocTimeStamp")
        || (dict.contains_key(b"ByteRange") && dict.contains_key(b"Contents"))
}

/// Which key a stream's data is encrypted under, or `None` when it is
/// stored in the clear.
pub(crate) fn stream_data_kind(dict: &Dict, encrypts_metadata: bool) -> Option<DataKind> {
    if dict.has_type(b"XRef")
        || (!encrypts_metadata && dict.has_type(b"Metadata"))
        || uses_identity_crypt_filter(dict)
    {
        return None;
    }
    Some(if dict.has_type(b"EmbeddedFile") {
        DataKind::EmbeddedFile
    } else {
        DataKind::Stream
    })
}

fn uses_identity_crypt_filter(dict: &Dict) -> bool {
    let first = match dict.get(b"Filter") {
        Some(Object::Name(name)) => Some(name.as_bytes()),
        Some(Object::Array(items)) => items.first().and_then(Object::as_name),
        _ => None,
    };
    if first != Some(b"Crypt".as_slice()) {
        return false;
    }
    let parms = match dict.get(b"DecodeParms") {
        Some(Object::Dict(parms)) => Some(parms),
        Some(Object::Array(items)) => items.first().and_then(Object::as_dict),
        _ => None,
    };
    parms
        .and_then(|p| p.get_name(b"Name"))
        .is_none_or(|name| name == b"Identity")
}

fn parse_version_name(name: &[u8]) -> Option<(u8, u8)> {
    match name {
        [major, b'.', minor] if major.is_ascii_digit() && minor.is_ascii_digit() => {
            Some((major - b'0', minor - b'0'))
        }
        _ => None,
    }
}

/// Where `%PDF-` starts (within the first KiB) and the version it declares;
/// (0, 1.4) when there is no header.
fn parse_header(data: &[u8]) -> (usize, (u8, u8)) {
    let window = &data[..data.len().min(1024)];
    let Some(at) = find(window, b"%PDF-", 0) else {
        return (0, (1, 4));
    };
    let rest = &data[at + 5..];
    let version = match rest {
        [major, b'.', minor, ..] if major.is_ascii_digit() && minor.is_ascii_digit() => {
            (major - b'0', minor - b'0')
        }
        _ => (1, 4),
    };
    (at, version)
}

/// The object number in the `N G obj` header at `offset`.
fn header_number(data: &[u8], offset: usize) -> Option<u32> {
    if offset >= data.len() {
        return None;
    }
    Parser::new(data, offset)
        .object_header()
        .ok()
        .map(|id| id.num)
}

fn is_trailer_key(key: &Name) -> bool {
    matches!(
        key.as_bytes(),
        b"Root" | b"Info" | b"ID" | b"Encrypt" | b"Size"
    )
}

struct Chain {
    entries: BTreeMap<u32, XrefEntry>,
    trailer: Dict,
    kind: XrefKind,
    startxref: usize,
    /// An older section could not be read.
    incomplete: bool,
}

struct Section {
    entries: BTreeMap<u32, XrefEntry>,
    trailer: Dict,
    kind: XrefKind,
}

fn find_startxref(data: &[u8]) -> Option<usize> {
    let at = rfind(data, b"startxref", data.len())?;
    match Lexer::new(data, at + b"startxref".len()).next_token() {
        Some(Token::Integer(offset)) => usize::try_from(offset).ok(),
        _ => None,
    }
}

/// Read the newest section and follow `/Prev`; newer entries win.
fn read_xref_chain(data: &Arc<Vec<u8>>, header_offset: usize) -> Result<Chain> {
    let startxref = find_startxref(data).ok_or_else(|| Error::invalid("no startxref"))?;
    let mut chain = Chain {
        entries: BTreeMap::new(),
        trailer: Dict::new(),
        kind: XrefKind::Table,
        startxref,
        incomplete: false,
    };
    let mut seen = HashSet::new();
    let mut next = Some(startxref);
    while let Some(offset) = next.take() {
        if seen.len() >= MAX_XREF_SECTIONS || !seen.insert(offset) {
            break;
        }
        let section = match read_section(data, offset, header_offset) {
            Ok(section) => section,
            Err(err) if seen.len() == 1 => return Err(err),
            Err(_) => {
                chain.incomplete = true;
                break;
            }
        };
        if seen.len() == 1 {
            chain.kind = section.kind;
        }
        next = section
            .trailer
            .get_i64(b"Prev")
            .and_then(|prev| usize::try_from(prev).ok());
        for (num, entry) in section.entries {
            chain.entries.entry(num).or_insert(entry);
        }
        for (key, value) in section.trailer {
            if is_trailer_key(&key) && !chain.trailer.contains_key(key.as_bytes()) {
                chain.trailer.insert(key, value);
            }
        }
    }
    Ok(chain)
}

fn read_section(data: &Arc<Vec<u8>>, offset: usize, header_offset: usize) -> Result<Section> {
    let candidates = if header_offset > 0 {
        vec![offset, offset + header_offset]
    } else {
        vec![offset]
    };
    for candidate in candidates {
        let mut lex = Lexer::new(data, candidate);
        lex.skip_whitespace();
        let at = lex.pos();
        if data[at..].starts_with(b"xref") {
            return read_table(data, at);
        }
        if let Ok(section) = read_xref_stream(data, at) {
            return Ok(section);
        }
    }
    Err(Error::syntax(
        offset,
        "no cross-reference section at the startxref offset",
    ))
}

/// A classic `xref` table and its trailer; a hybrid file's `/XRefStm`
/// fills entries the table leaves free or absent.
fn read_table(data: &Arc<Vec<u8>>, at: usize) -> Result<Section> {
    let mut lex = Lexer::new(data, at + b"xref".len());
    let mut entries = BTreeMap::new();
    loop {
        lex.skip_whitespace();
        let start = lex.pos();
        match lex.next_token() {
            Some(Token::Keyword(b"trailer")) => break,
            Some(Token::Integer(first)) => {
                let Some(Token::Integer(count)) = lex.next_token() else {
                    return Err(Error::syntax(
                        start,
                        "bad cross-reference subsection header",
                    ));
                };
                let (Ok(mut first), Ok(count)) = (u32::try_from(first), usize::try_from(count))
                else {
                    return Err(Error::syntax(
                        start,
                        "bad cross-reference subsection header",
                    ));
                };
                // Each entry takes at least 18 bytes.
                if count > data.len().saturating_sub(lex.pos()) / 18 + 1 {
                    return Err(Error::syntax(
                        start,
                        "cross-reference subsection longer than the file",
                    ));
                }
                for i in 0..count {
                    let entry_start = lex.pos();
                    let (
                        Some(Token::Integer(field1)),
                        Some(Token::Integer(field2)),
                        Some(Token::Keyword(kind)),
                    ) = (lex.next_token(), lex.next_token(), lex.next_token())
                    else {
                        return Err(Error::syntax(entry_start, "bad cross-reference entry"));
                    };
                    // Some writers number the subsection holding object 0 from 1.
                    if i == 0 && first == 1 && kind == b"f" && field2 == 65535 {
                        first = 0;
                    }
                    let Some(num) = u32::try_from(i).ok().and_then(|i| first.checked_add(i)) else {
                        break;
                    };
                    let entry = match (kind, usize::try_from(field1)) {
                        (b"n", Ok(offset)) => XrefEntry::Offset {
                            offset,
                            generation: u16::try_from(field2).unwrap_or(0),
                        },
                        (b"n" | b"f", _) => XrefEntry::Free,
                        _ => {
                            return Err(Error::syntax(
                                entry_start,
                                "bad cross-reference entry type",
                            ));
                        }
                    };
                    entries.insert(num, entry);
                }
            }
            _ => return Err(Error::syntax(start, "bad cross-reference table")),
        }
    }
    let trailer_at = lex.pos();
    let trailer = match Parser::new(data, trailer_at).parse_object()? {
        Object::Dict(dict) => dict,
        _ => return Err(Error::syntax(trailer_at, "the trailer is not a dictionary")),
    };
    if let Some(stream_at) = trailer
        .get_i64(b"XRefStm")
        .and_then(|v| usize::try_from(v).ok())
        && let Ok(extra) = read_xref_stream(data, stream_at)
    {
        for (num, entry) in extra.entries {
            if matches!(entries.get(&num), None | Some(XrefEntry::Free)) {
                entries.insert(num, entry);
            }
        }
    }
    Ok(Section {
        entries,
        trailer,
        kind: XrefKind::Table,
    })
}

/// A cross-reference stream (`/W`, `/Index`, `/Size`); its dictionary is
/// the section's trailer.
fn read_xref_stream(data: &Arc<Vec<u8>>, at: usize) -> Result<Section> {
    let found = parse_indirect(data, at, &|_| None)?;
    let Object::Stream(stream) = found.object else {
        return Err(Error::syntax(
            at,
            "the cross-reference stream is not a stream",
        ));
    };
    let widths: Vec<usize> = stream
        .dict
        .get_array(b"W")
        .unwrap_or(&[])
        .iter()
        .filter_map(Object::as_i64)
        .filter_map(|w| usize::try_from(w).ok())
        .collect();
    if widths.len() < 3 || widths.iter().any(|&w| w > 8) {
        return Err(Error::syntax(at, "bad /W in cross-reference stream"));
    }
    let row = widths[0] + widths[1] + widths[2];
    if row == 0 {
        return Err(Error::syntax(at, "bad /W in cross-reference stream"));
    }
    let size = stream.dict.get_i64(b"Size").unwrap_or(0).max(0);
    let index: Vec<i64> = match stream.dict.get_array(b"Index") {
        Some(items) => items.iter().filter_map(Object::as_i64).collect(),
        None => vec![0, size],
    };
    let chain = filters::filter_chain(stream.dict.get(b"Filter"), stream.dict.get(b"DecodeParms"));
    let decoded = filters::decode(stream.raw(), &chain, DEFAULT_DECODE_LIMIT)?.data;
    let mut rows = decoded.chunks_exact(row);
    let mut entries = BTreeMap::new();
    for [start, count] in index.as_chunks::<2>().0 {
        let (Ok(start), Ok(count)) = (u32::try_from(*start), u64::try_from(*count)) else {
            continue;
        };
        for i in 0..count {
            let Some(bytes) = rows.next() else { break };
            let Some(num) = u32::try_from(i).ok().and_then(|i| start.checked_add(i)) else {
                break;
            };
            let field = |from: usize, width: usize| {
                bytes[from..from + width]
                    .iter()
                    .fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
            };
            let kind = if widths[0] == 0 {
                1
            } else {
                field(0, widths[0])
            };
            let second = field(widths[0], widths[1]);
            let third = field(widths[0] + widths[1], widths[2]);
            let entry = match kind {
                0 => XrefEntry::Free,
                1 => XrefEntry::Offset {
                    offset: usize::try_from(second).unwrap_or(usize::MAX),
                    generation: u16::try_from(third).unwrap_or(0),
                },
                2 => XrefEntry::Compressed {
                    stream: u32::try_from(second).unwrap_or(0),
                    index: u32::try_from(third).unwrap_or(0),
                },
                // Unknown types are references to null.
                _ => continue,
            };
            entries.insert(num, entry);
        }
    }
    Ok(Section {
        entries,
        trailer: stream.dict,
        kind: XrefKind::Stream,
    })
}

#[derive(Default)]
struct Scan {
    entries: BTreeMap<u32, XrefEntry>,
    trailer: Dict,
    catalog: Option<ObjRef>,
    objstms: Vec<u32>,
}

/// Start of the `N G` before an `obj` keyword at `obj_at`.
fn header_start(data: &[u8], obj_at: usize) -> Option<usize> {
    let mut p = obj_at;
    let skip_space = |p: &mut usize| {
        let before = *p;
        while *p > 0 && is_whitespace(data[*p - 1]) {
            *p -= 1;
        }
        *p < before
    };
    let skip_digits = |p: &mut usize, max: usize| {
        let before = *p;
        while *p > 0 && data[*p - 1].is_ascii_digit() {
            *p -= 1;
        }
        *p < before && before - *p <= max
    };
    if !skip_space(&mut p)
        || !skip_digits(&mut p, 5)
        || !skip_space(&mut p)
        || !skip_digits(&mut p, 10)
    {
        return None;
    }
    if p > 0 && is_regular(data[p - 1]) {
        return None;
    }
    Some(p)
}

/// Scan the whole file for indirect objects and trailers. Later
/// definitions win; stream data is skipped.
fn scan_file(data: &Arc<Vec<u8>>) -> Scan {
    let mut scan = Scan::default();
    let mut trailers: Vec<(usize, Dict)> = Vec::new();
    let mut pos = 0;
    while let Some(at) = find(data, b"obj", pos) {
        pos = at + 3;
        if data.get(at + 3).is_some_and(|&b| is_regular(b)) {
            continue;
        }
        let Some(start) = header_start(data, at) else {
            continue;
        };
        let Ok(found) = parse_indirect(data, start, &|_| None) else {
            continue;
        };
        if found.id.num == 0 {
            continue;
        }
        scan.entries.insert(
            found.id.num,
            XrefEntry::Offset {
                offset: start,
                generation: found.id.generation,
            },
        );
        match &found.object {
            Object::Stream(stream) => {
                if stream.dict.has_type(b"ObjStm") {
                    scan.objstms.push(found.id.num);
                }
                if stream.dict.has_type(b"XRef") {
                    trailers.push((start, stream.dict.clone()));
                }
                pos = pos.max(found.end);
            }
            Object::Dict(dict) if dict.has_type(b"Catalog") => scan.catalog = Some(found.id),
            _ => {}
        }
    }
    let mut pos = 0;
    while let Some(at) = find(data, b"trailer", pos) {
        pos = at + b"trailer".len();
        if let Ok(Object::Dict(dict)) = Parser::new(data, pos).parse_object() {
            trailers.push((at, dict));
        }
    }
    trailers.sort_by_key(|(at, _)| *at);
    for (_, dict) in trailers {
        for (key, value) in dict {
            if is_trailer_key(&key) {
                scan.trailer.insert(key, value);
            }
        }
    }
    scan
}
