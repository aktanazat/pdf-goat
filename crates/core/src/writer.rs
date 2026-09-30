//! Saving documents: full rewrites with a classic cross-reference table or
//! with object streams and a cross-reference stream, garbage collection and
//! renumbering, stream compression, `/ID`, encryption kept, removed, or
//! replaced, and incremental updates that report where objects and
//! signatures landed.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use md5::{Digest, Md5};
use pdf_crypt::{CryptMethod, DataKind, EncryptDict, NewEncryption, SecurityHandler};

use crate::document::{
    Change, Document, XrefEntry, XrefKind, crypt_dict, crypt_object, is_signature_dict,
    stream_data_kind,
};
use crate::error::{Error, Result};
use crate::filters::flate_encode;
use crate::object::{Dict, ObjRef, Object, PdfString, Stream};
use crate::parser::MAX_DEPTH;
use crate::serialize::{RefMap, write_dict, write_name, write_object, write_object_mapped};

mod linearize;

/// Objects packed into one object stream (qpdf's default).
const OBJECTS_PER_STREAM: usize = 100;

/// What a full rewrite does with the document's encryption.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Encryption {
    /// Keep the document's encryption: the same `/Encrypt` dictionary, key,
    /// and first `/ID` element. An unencrypted document stays unencrypted.
    #[default]
    Keep,
    /// Write decrypted, without `/Encrypt`.
    Remove,
    /// Encrypt with new passwords and permissions.
    New(NewEncryption),
}

/// Options for [`Document::save_to_bytes`] and [`Document::save`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SaveOptions {
    /// Flate-compress streams that have no filter, when that makes them
    /// smaller. XMP `/Metadata` streams stay uncompressed.
    pub compress_streams: bool,
    /// Pack non-stream objects into object streams and write a
    /// cross-reference stream (header raised to at least 1.5). Otherwise a
    /// classic xref table is written.
    pub object_streams: bool,
    /// Write only objects reachable from the trailer (`/Root`, `/Info`) and
    /// renumber them compactly from 1, all generation 0.
    pub garbage_collect: bool,
    /// Linearize (fast web view), with the page tree flattened and each
    /// page carrying its inherited attributes. Implies `garbage_collect`;
    /// `object_streams` is ignored and a classic xref table is written. A
    /// document without pages is written unlinearized.
    pub linearize: bool,
    /// Assign a fresh `/ID` instead of keeping the document's. A document
    /// without an `/ID` always gets one.
    pub new_id: bool,
    pub encryption: Encryption,
    /// Header version. Default: [`Document::version`]. It is raised when the
    /// output needs more: 1.5 for object streams, 1.6 for AES-128, 1.7 for
    /// AES-256.
    pub version: Option<(u8, u8)>,
}

/// An incremental update: the original bytes unchanged, then changed and
/// new objects, a cross-reference section of the same kind as the file's
/// (`/Prev` pointing at the old one), and a trailer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncrementalSave {
    /// The whole new file.
    pub data: Vec<u8>,
    /// Where the appended update starts in `data` (the original length).
    pub update_offset: usize,
    /// Byte offset in `data` of each changed or new object written in the
    /// update, in file order. A cross-reference stream is at `xref_offset`.
    pub offsets: Vec<(ObjRef, usize)>,
    /// Offset of the update's cross-reference section (the new startxref).
    pub xref_offset: usize,
    /// Byte spans of each signature dictionary written in the update: a
    /// changed object that is `/Type /Sig` or `/DocTimeStamp`, or any
    /// dictionary with `/ByteRange` and `/Contents`.
    pub signatures: Vec<SignatureSpan>,
}

/// Where a signature dictionary's placeholders landed in an
/// [`IncrementalSave`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureSpan {
    pub id: ObjRef,
    /// The `/Contents` string, from its `<` through its `>`. Create the
    /// placeholder as `PdfString::hex(vec![0; N])`. It is never encrypted.
    pub contents: Range<usize>,
    /// The `/ByteRange` array, from `[` through `]`, when present. Write wide
    /// placeholder integers, then overwrite them in place, padding with
    /// spaces.
    pub byte_range: Option<Range<usize>>,
}

impl Document {
    /// [`Document::save_to_bytes`], then write the file at `path`.
    pub fn save(&self, path: impl AsRef<Path>, options: &SaveOptions) -> Result<()> {
        let bytes = self.save_to_bytes(options)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// A full rewrite. Cross-reference streams, object streams, and
    /// linearization data of the source are rebuilt, never copied; an
    /// object that cannot be read is dropped and references to it are
    /// written as null. Fails with [`Error::NeedsPassword`] while the
    /// document is locked.
    pub fn save_to_bytes(&self, options: &SaveOptions) -> Result<Vec<u8>> {
        if self.needs_password() {
            return Err(Error::NeedsPassword);
        }
        self.catalog()?;
        if options.linearize {
            return self.write_linearized(options);
        }
        let plan = if options.garbage_collect {
            self.plan_reachable(&HashMap::new())?
        } else {
            self.plan_all()
        };
        self.write_full(options, &plan)
    }

    /// Append the changes since loading. Changed objects are encrypted with
    /// the document's handler; `compress_streams` Flate-compresses
    /// unfiltered streams in the update. With no change the original bytes
    /// come back unchanged. Fails with [`Error::NeedsPassword`] while
    /// locked, and with [`Error::Unsupported`] for a document that was not
    /// loaded from bytes. For a repaired file the appended section lists
    /// every object, so it does not depend on the damaged table.
    pub fn save_incremental(&self, compress_streams: bool) -> Result<IncrementalSave> {
        if self.needs_password() {
            return Err(Error::NeedsPassword);
        }
        if self.data.is_empty() {
            return Err(Error::Unsupported(
                "an incremental save of a document not loaded from bytes".into(),
            ));
        }
        let original = self.data.len();
        if self.changes.is_empty() && !self.is_modified() && !self.repaired {
            return Ok(IncrementalSave {
                data: self.data.to_vec(),
                update_offset: original,
                offsets: Vec::new(),
                xref_offset: self.startxref.unwrap_or(0),
                signatures: Vec::new(),
            });
        }
        let mut out = Vec::with_capacity(original.saturating_add(4096));
        out.extend_from_slice(&self.data);
        if !matches!(out.last(), Some(b'\n' | b'\r')) {
            out.push(b'\n');
        }
        let security = self.security.as_ref();
        let encrypt_num = security.and_then(|s| s.encrypt_num);
        let crypt = security
            .and_then(|s| s.handler.clone())
            .map(OutputCrypt::new);
        let emitter = Emitter {
            map: &keep_same,
            crypt: crypt.as_ref(),
            compress: compress_streams,
        };

        let mut entries = BTreeMap::new();
        if self.repaired {
            for (&num, entry) in &self.xref {
                match *entry {
                    XrefEntry::Offset { offset, generation } => {
                        entries.insert(num, XrefOut::InFile { offset, generation });
                    }
                    XrefEntry::Compressed { stream, index } => {
                        entries.insert(num, XrefOut::InStream { stream, index });
                    }
                    XrefEntry::Free => {}
                }
            }
        }
        entries.insert(
            0,
            XrefOut::Free {
                next: 0,
                generation: 65535,
            },
        );
        let mut offsets = Vec::with_capacity(self.changes.len());
        let mut signatures = Vec::new();
        for (&num, change) in &self.changes {
            match change {
                Change::Set { generation, object } => {
                    let id = ObjRef::new(num, *generation);
                    let offset = out.len();
                    entries.insert(
                        num,
                        XrefOut::InFile {
                            offset,
                            generation: *generation,
                        },
                    );
                    offsets.push((id, offset));
                    let crypt = crypt.as_ref().filter(|_| encrypt_num != Some(num));
                    match object.as_ref() {
                        Object::Dict(dict) if is_signature_dict(dict) => {
                            write_signature(&mut out, id, dict, crypt, &mut signatures);
                        }
                        other => emitter.indirect(&mut out, id, other, crypt.is_some()),
                    }
                }
                Change::Deleted { generation } => {
                    entries.insert(
                        num,
                        XrefOut::Free {
                            next: 0,
                            generation: generation.saturating_add(1),
                        },
                    );
                }
            }
        }

        let has_compressed = entries
            .values()
            .any(|e| matches!(e, XrefOut::InStream { .. }));
        let kind = if self.repaired && !has_compressed {
            XrefKind::Table
        } else if self.repaired {
            XrefKind::Stream
        } else {
            self.xref_kind
        };
        let xref_num = self.next_num;
        let size = match kind {
            XrefKind::Table => xref_num,
            XrefKind::Stream => xref_num.checked_add(1).ok_or_else(too_many_objects)?,
        };
        let mut trailer = Dict::new();
        trailer.insert("Size", size);
        for (key, value) in self.trailer.iter() {
            if !matches!(key.as_bytes(), b"Size" | b"Prev" | b"XRefStm" | b"ID") {
                trailer.insert(key, value.clone());
            }
        }
        if let Some(id) = self.update_id() {
            trailer.insert("ID", id_array(&id));
        }
        if let (false, Some(prev)) = (self.repaired, self.startxref) {
            trailer.insert("Prev", prev);
        }
        let xref_offset = out.len();
        match kind {
            XrefKind::Table => {
                write_xref_table(&mut out, &entries)?;
                write_trailer(&mut out, &trailer);
            }
            XrefKind::Stream => {
                let xref_id = ObjRef::new(xref_num, 0);
                entries.insert(
                    xref_id.num,
                    XrefOut::InFile {
                        offset: xref_offset,
                        generation: 0,
                    },
                );
                write_plain(
                    &mut out,
                    xref_id,
                    &Object::Stream(xref_stream(&entries, trailer)),
                );
            }
        }
        write_startxref(&mut out, xref_offset);
        Ok(IncrementalSave {
            data: out,
            update_offset: original,
            offsets,
            xref_offset,
            signatures,
        })
    }

    /// Every object in use, keeping numbers and generations.
    fn plan_all(&self) -> Plan {
        let encrypt_num = self.security.as_ref().and_then(|s| s.encrypt_num);
        let mut plan = Plan {
            objects: Vec::new(),
            map: HashMap::new(),
            next: 1,
        };
        for id in self.object_ids() {
            if encrypt_num == Some(id.num) {
                continue;
            }
            let Ok(Some(object)) = self.fetch(id.num, 0) else {
                continue;
            };
            if is_file_structure(&object) {
                continue;
            }
            plan.map.insert(id.num, id);
            plan.next = plan.next.max(id.num.saturating_add(1));
            plan.objects.push(Planned {
                source: id.num,
                id,
                object,
            });
        }
        plan
    }

    /// The objects reachable from the trailer, breadth first from `/Root`
    /// then `/Info`, numbered from 1 in that order. `replaced` stands in for
    /// the document's objects by number. Null objects are left out, so
    /// references to them are written as null.
    fn plan_reachable(&self, replaced: &HashMap<u32, Arc<Object>>) -> Result<Plan> {
        let encrypt_num = self.security.as_ref().and_then(|s| s.encrypt_num);
        let mut queue = VecDeque::new();
        for key in [b"Root".as_slice(), b"Info"] {
            if let Some(value) = self.trailer.get(key) {
                collect_refs(value, &mut queue, 0)?;
            }
        }
        for (key, value) in self.trailer.iter() {
            if !matches!(
                key.as_bytes(),
                b"Root" | b"Info" | b"Size" | b"Prev" | b"XRefStm" | b"ID" | b"Encrypt"
            ) {
                collect_refs(value, &mut queue, 0)?;
            }
        }
        let mut plan = Plan {
            objects: Vec::new(),
            map: HashMap::new(),
            next: 1,
        };
        let mut dropped = HashSet::new();
        while let Some(source) = queue.pop_front() {
            if plan.map.contains_key(&source.num) || dropped.contains(&source.num) {
                continue;
            }
            let object = match replaced.get(&source.num) {
                Some(object) => Arc::clone(object),
                None => match self.fetch(source.num, 0) {
                    Ok(Some(object))
                        if encrypt_num != Some(source.num)
                            && !is_file_structure(&object)
                            && !object.is_null() =>
                    {
                        object
                    }
                    _ => {
                        dropped.insert(source.num);
                        continue;
                    }
                },
            };
            let id = allocate(&mut plan.next)?;
            plan.map.insert(source.num, id);
            collect_refs(&object, &mut queue, 0)?;
            plan.objects.push(Planned {
                source: source.num,
                id,
                object,
            });
        }
        Ok(plan)
    }

    /// Write `plan` as a complete file.
    fn write_full(&self, options: &SaveOptions, plan: &Plan) -> Result<Vec<u8>> {
        let id = self.output_id(options);
        let crypt = self.full_crypt(&options.encryption, &id.0)?;
        let version = self.output_version(options, options.object_streams, crypt.as_ref());
        let lookup = |r: ObjRef| plan.map.get(&r.num).copied();
        let emitter = Emitter {
            map: &lookup,
            crypt: crypt.as_ref().map(|c| &c.crypt),
            compress: options.compress_streams,
        };

        let mut out = Vec::with_capacity(self.data.len().saturating_add(1024));
        write_header(&mut out, version);
        let mut entries = BTreeMap::new();
        let mut next = plan.next;
        let encrypt_id = match crypt {
            Some(_) => Some(allocate(&mut next)?),
            None => None,
        };
        let mut packed = Vec::new();
        for planned in &plan.objects {
            if options.object_streams && is_packable(planned) {
                packed.push(planned);
                continue;
            }
            entries.insert(
                planned.id.num,
                XrefOut::InFile {
                    offset: out.len(),
                    generation: planned.id.generation,
                },
            );
            emitter.indirect(&mut out, planned.id, &planned.object, true);
        }
        if let (Some(crypt), Some(encrypt_id)) = (&crypt, encrypt_id) {
            entries.insert(
                encrypt_id.num,
                XrefOut::InFile {
                    offset: out.len(),
                    generation: 0,
                },
            );
            write_plain(&mut out, encrypt_id, &crypt.dict);
        }
        for chunk in packed.chunks(OBJECTS_PER_STREAM) {
            let stream_id = allocate(&mut next)?;
            for (index, planned) in (0u32..).zip(chunk) {
                entries.insert(
                    planned.id.num,
                    XrefOut::InStream {
                        stream: stream_id.num,
                        index,
                    },
                );
            }
            entries.insert(
                stream_id.num,
                XrefOut::InFile {
                    offset: out.len(),
                    generation: 0,
                },
            );
            emitter.indirect(
                &mut out,
                stream_id,
                &Object::Stream(object_stream(chunk, &lookup)),
                true,
            );
        }

        if options.object_streams {
            let xref_id = allocate(&mut next)?;
            let xref_offset = out.len();
            entries.insert(
                xref_id.num,
                XrefOut::InFile {
                    offset: xref_offset,
                    generation: 0,
                },
            );
            fill_free(&mut entries, next);
            let trailer = self.full_trailer(next, &lookup, &id, encrypt_id)?;
            write_plain(
                &mut out,
                xref_id,
                &Object::Stream(xref_stream(&entries, trailer)),
            );
            write_startxref(&mut out, xref_offset);
        } else {
            fill_free(&mut entries, next);
            let xref_offset = out.len();
            write_xref_table(&mut out, &entries)?;
            write_trailer(
                &mut out,
                &self.full_trailer(next, &lookup, &id, encrypt_id)?,
            );
            write_startxref(&mut out, xref_offset);
        }
        Ok(out)
    }

    /// The `/ID` of a full rewrite. Kept encryption keeps the first element,
    /// which the key depends on.
    fn output_id(&self, options: &SaveOptions) -> (Vec<u8>, Vec<u8>) {
        let keep_first = options.encryption == Encryption::Keep && self.security.is_some();
        match self.file_id() {
            Some(existing) if !options.new_id => existing,
            existing => {
                let fresh = fresh_id(self);
                let first = match (keep_first, existing) {
                    (true, Some((first, _))) => first,
                    (true, None) => Vec::new(),
                    (false, _) => fresh.clone(),
                };
                (first, fresh)
            }
        }
    }

    /// The `/ID` of an incremental update: the first element kept and a new
    /// second one. `None` for an encrypted file without `/ID`, whose key was
    /// derived without one.
    fn update_id(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let fresh = fresh_id(self);
        match self.file_id() {
            Some((first, _)) => Some((first, fresh)),
            None if self.security.is_some() => None,
            None => Some((fresh.clone(), fresh)),
        }
    }

    fn full_crypt(&self, encryption: &Encryption, id0: &[u8]) -> Result<Option<FullCrypt>> {
        match encryption {
            Encryption::Remove => Ok(None),
            Encryption::Keep => {
                let Some(security) = &self.security else {
                    return Ok(None);
                };
                let handler = security.handler.clone().ok_or(Error::NeedsPassword)?;
                Ok(Some(FullCrypt {
                    crypt: OutputCrypt::new(handler),
                    dict: Object::Dict(self.direct_dict(&security.raw, 0)?),
                    version: required_version(&security.dict),
                }))
            }
            Encryption::New(params) => new_crypt(params, id0).map(Some),
        }
    }

    fn output_version(
        &self,
        options: &SaveOptions,
        object_streams: bool,
        crypt: Option<&FullCrypt>,
    ) -> (u8, u8) {
        let mut version = options.version.unwrap_or_else(|| self.version());
        if object_streams {
            version = version.max((1, 5));
        }
        if let Some(crypt) = crypt {
            version = version.max(crypt.version);
        }
        version
    }

    /// The trailer of a full rewrite: `/Size`, the document's trailer
    /// entries renumbered, `/Encrypt`, and `/ID`.
    fn full_trailer(
        &self,
        size: u32,
        map: &RefMap<'_>,
        id: &(Vec<u8>, Vec<u8>),
        encrypt: Option<ObjRef>,
    ) -> Result<Dict> {
        let mut trailer = Dict::new();
        trailer.insert("Size", size);
        for (key, value) in self.trailer.iter() {
            if !matches!(
                key.as_bytes(),
                b"Size" | b"Prev" | b"XRefStm" | b"ID" | b"Encrypt"
            ) {
                trailer.insert(key, remapped(value, map, 0)?);
            }
        }
        if let Some(encrypt) = encrypt {
            trailer.insert("Encrypt", encrypt);
        }
        trailer.insert("ID", id_array(id));
        Ok(trailer)
    }

    /// `dict` with every reference inside it resolved.
    fn direct_dict(&self, dict: &Dict, depth: usize) -> Result<Dict> {
        let mut out = Dict::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            out.insert(key, self.direct(value, depth + 1)?);
        }
        Ok(out)
    }

    fn direct(&self, object: &Object, depth: usize) -> Result<Object> {
        if depth > MAX_DEPTH {
            return Err(too_deep());
        }
        Ok(match self.resolve(object)? {
            Object::Array(items) => Object::Array(
                items
                    .iter()
                    .map(|item| self.direct(item, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            Object::Dict(dict) => Object::Dict(self.direct_dict(&dict, depth)?),
            other => other,
        })
    }
}

/// An object a full rewrite writes.
struct Planned {
    /// Its number in the document.
    source: u32,
    /// Its number and generation in the output.
    id: ObjRef,
    object: Arc<Object>,
}

/// The objects of a full rewrite in output order, and their renumbering.
struct Plan {
    objects: Vec<Planned>,
    /// Document object number to output reference.
    map: HashMap<u32, ObjRef>,
    /// The first output number not yet used.
    next: u32,
}

/// The handler that encrypts output objects.
struct OutputCrypt {
    handler: SecurityHandler,
    encrypts_metadata: bool,
}

impl OutputCrypt {
    fn new(handler: SecurityHandler) -> OutputCrypt {
        OutputCrypt {
            encrypts_metadata: handler.encrypts_metadata(),
            handler,
        }
    }

    fn encrypt(&self, id: ObjRef, kind: DataKind, data: &[u8]) -> Vec<u8> {
        self.handler.encrypt(id.num, id.generation, kind, data)
    }

    /// Encrypt the strings of `dict` (never a signature's `/Contents`) as
    /// part of object `id`.
    fn encrypt_dict(&self, dict: &mut Dict, id: ObjRef) {
        crypt_dict(
            dict,
            &|kind: DataKind, bytes: &[u8]| Some(self.encrypt(id, kind, bytes)),
            self.encrypts_metadata,
            0,
        );
    }

    fn encrypt_object(&self, object: &mut Object, id: ObjRef) {
        crypt_object(
            object,
            &|kind: DataKind, bytes: &[u8]| Some(self.encrypt(id, kind, bytes)),
            self.encrypts_metadata,
            0,
        );
    }
}

/// Encryption of a full rewrite: the handler, the `/Encrypt` dictionary
/// written for it, and the lowest version that defines it.
struct FullCrypt {
    crypt: OutputCrypt,
    dict: Object,
    version: (u8, u8),
}

/// A new handler and its `/Encrypt` dictionary.
fn new_crypt(params: &NewEncryption, id0: &[u8]) -> Result<FullCrypt> {
    let (handler, dict) = SecurityHandler::for_new_document(params, id0)?;
    Ok(FullCrypt {
        crypt: OutputCrypt::new(handler),
        dict: Object::Dict(encrypt_dictionary(&dict)),
        version: required_version(&dict),
    })
}

/// The `/Encrypt` dictionary for `fields`, with the standard crypt filter
/// `/StdCF` for V4 and V5.
fn encrypt_dictionary(fields: &EncryptDict) -> Dict {
    let mut dict = Dict::new();
    dict.insert("Filter", Object::name("Standard"));
    dict.insert("V", fields.v);
    dict.insert("R", fields.r);
    dict.insert("Length", fields.length_bits);
    if fields.v >= 4 {
        let (method, key_bytes) = match fields.stream_method {
            CryptMethod::AesV3 => ("AESV3", 32),
            CryptMethod::AesV2 => ("AESV2", 16),
            CryptMethod::Rc4 => ("V2", fields.length_bits / 8),
            CryptMethod::None => ("None", 0),
        };
        let mut standard = Dict::new();
        standard.insert("AuthEvent", Object::name("DocOpen"));
        standard.insert("CFM", Object::name(method));
        standard.insert("Length", key_bytes);
        let mut filters = Dict::new();
        filters.insert("StdCF", standard);
        dict.insert("CF", filters);
        dict.insert("StmF", Object::name("StdCF"));
        dict.insert("StrF", Object::name("StdCF"));
    }
    dict.insert("O", PdfString::hex(fields.o.clone()));
    dict.insert("U", PdfString::hex(fields.u.clone()));
    for (key, value) in [
        ("OE", &fields.oe),
        ("UE", &fields.ue),
        ("Perms", &fields.perms),
    ] {
        if let Some(value) = value {
            dict.insert(key, PdfString::hex(value.clone()));
        }
    }
    dict.insert("P", fields.p);
    if !fields.encrypt_metadata {
        dict.insert("EncryptMetadata", false);
    }
    dict
}

/// The lowest PDF version that defines this encryption.
fn required_version(fields: &EncryptDict) -> (u8, u8) {
    let methods = [
        fields.string_method,
        fields.stream_method,
        fields.embedded_file_method,
    ];
    if fields.v >= 5 || methods.contains(&CryptMethod::AesV3) {
        (1, 7)
    } else if methods.contains(&CryptMethod::AesV2) {
        (1, 6)
    } else if fields.v >= 4 {
        (1, 5)
    } else if fields.v >= 2 {
        (1, 4)
    } else {
        (1, 1)
    }
}

/// Serializes objects: references renumbered through `map`, unfiltered
/// streams compressed when `compress`, strings and streams encrypted with
/// `crypt`.
struct Emitter<'a> {
    map: &'a RefMap<'a>,
    crypt: Option<&'a OutputCrypt>,
    compress: bool,
}

impl Emitter<'_> {
    /// `N G obj`, the object, `endobj`. `encrypt` false writes it in the
    /// clear, as the `/Encrypt` dictionary must be.
    fn indirect(&self, out: &mut Vec<u8>, id: ObjRef, object: &Object, encrypt: bool) {
        write_object_header(out, id);
        let crypt = self.crypt.filter(|_| encrypt);
        match (object, crypt) {
            (Object::Stream(stream), _) => self.stream(out, id, stream, crypt),
            (_, Some(crypt)) => {
                let mut copy = object.clone();
                crypt.encrypt_object(&mut copy, id);
                write_object_mapped(out, &copy, self.map);
            }
            (_, None) => write_object_mapped(out, object, self.map),
        }
        out.extend_from_slice(b"\nendobj\n");
    }

    fn stream(&self, out: &mut Vec<u8>, id: ObjRef, stream: &Stream, crypt: Option<&OutputCrypt>) {
        let packed = if self.compress {
            compressed(stream)
        } else {
            None
        };
        let stream = packed.as_ref().unwrap_or(stream);
        let mut dict = Cow::Borrowed(&stream.dict);
        let mut data = Cow::Borrowed(stream.raw());
        if let Some(crypt) = crypt {
            crypt.encrypt_dict(dict.to_mut(), id);
            if let Some(kind) = stream_data_kind(&dict, crypt.encrypts_metadata) {
                data = Cow::Owned(crypt.encrypt(id, kind, &data));
            }
        }
        write_dict(out, &dict, Some(data.len()), self.map);
        out.extend_from_slice(b"\nstream\n");
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\nendstream");
    }
}

/// The reference map of an incremental update, which keeps every number.
fn keep_same(id: ObjRef) -> Option<ObjRef> {
    Some(id)
}

/// An unfiltered stream other than XMP metadata, which stays readable to
/// tools that scan files for it.
fn is_compressible(dict: &Dict) -> bool {
    let unfiltered = match dict.get(b"Filter") {
        None | Some(Object::Null) => true,
        Some(Object::Array(items)) => items.is_empty(),
        Some(_) => false,
    };
    unfiltered && !dict.has_type(b"Metadata")
}

/// `stream` Flate-compressed, when it is compressible and that makes it
/// smaller.
fn compressed(stream: &Stream) -> Option<Stream> {
    if !is_compressible(&stream.dict) {
        return None;
    }
    let packed = flate_encode(stream.raw());
    if packed.len() >= stream.raw().len() {
        return None;
    }
    let mut dict = stream.dict.clone();
    dict.insert("Filter", Object::name("FlateDecode"));
    dict.remove(b"DecodeParms");
    Some(Stream::new(dict, packed))
}

/// Cross-reference streams, object streams, and linearization dictionaries
/// describe the source file's layout; a rewrite builds its own.
fn is_file_structure(object: &Object) -> bool {
    match object {
        Object::Stream(stream) => stream.dict.has_type(b"XRef") || stream.dict.has_type(b"ObjStm"),
        Object::Dict(dict) => dict.contains_key(b"Linearized"),
        _ => false,
    }
}

/// Objects an object stream may hold: generation 0, not a stream, and not
/// a signature dictionary (whose `/Contents` stays unencrypted).
fn is_packable(planned: &Planned) -> bool {
    planned.id.generation == 0
        && match planned.object.as_ref() {
            Object::Stream(_) => false,
            Object::Dict(dict) => !is_signature_dict(dict),
            _ => true,
        }
}

/// Queue the references in `object` in order. A stream's `/Length` is
/// skipped: the writer puts the true length there.
fn collect_refs(object: &Object, queue: &mut VecDeque<ObjRef>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    match object {
        Object::Reference(id) => queue.push_back(*id),
        Object::Array(items) => {
            for item in items {
                collect_refs(item, queue, depth + 1)?;
            }
        }
        Object::Dict(dict) => {
            for (_, value) in dict.iter() {
                collect_refs(value, queue, depth + 1)?;
            }
        }
        Object::Stream(stream) => {
            for (key, value) in stream.dict.iter() {
                if key != "Length" {
                    collect_refs(value, queue, depth + 1)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// A copy of `object` with references passed through `map`.
fn remapped(object: &Object, map: &RefMap<'_>, depth: usize) -> Result<Object> {
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    Ok(match object {
        Object::Reference(id) => map(*id).map_or(Object::Null, Object::Reference),
        Object::Array(items) => Object::Array(
            items
                .iter()
                .map(|item| remapped(item, map, depth + 1))
                .collect::<Result<_>>()?,
        ),
        Object::Dict(dict) => {
            let mut out = Dict::with_capacity(dict.len());
            for (key, value) in dict.iter() {
                out.insert(key, remapped(value, map, depth + 1)?);
            }
            Object::Dict(out)
        }
        other => other.clone(),
    })
}

/// An object stream holding `members`, their references renumbered.
fn object_stream(members: &[&Planned], map: &RefMap<'_>) -> Stream {
    let mut header = Vec::new();
    let mut body = Vec::new();
    for planned in members {
        if !header.is_empty() {
            header.push(b' ');
        }
        header.extend_from_slice(format!("{} {}", planned.id.num, body.len()).as_bytes());
        write_object_mapped(&mut body, &planned.object, map);
        body.push(b'\n');
    }
    header.push(b'\n');
    let first = header.len();
    header.extend_from_slice(&body);
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("ObjStm"));
    dict.insert("N", members.len());
    dict.insert("First", first);
    dict.insert("Filter", Object::name("FlateDecode"));
    Stream::new(dict, flate_encode(&header))
}

/// Take the next free object number.
fn allocate(next: &mut u32) -> Result<ObjRef> {
    let id = ObjRef::new(*next, 0);
    *next = next.checked_add(1).ok_or_else(too_many_objects)?;
    Ok(id)
}

fn too_many_objects() -> Error {
    Error::LimitExceeded("more objects than a cross-reference section can number".into())
}

fn too_deep() -> Error {
    Error::LimitExceeded(format!("objects nested deeper than {MAX_DEPTH} levels"))
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new `/ID` element: MD5 of the time, a process-wide counter, and the
/// document's size, so two saves never share one.
fn fresh_id(doc: &Document) -> Vec<u8> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut md5 = Md5::new();
    md5.update(nanos.to_le_bytes());
    md5.update(ID_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    md5.update(std::process::id().to_le_bytes());
    md5.update((doc.data.len() as u64).to_le_bytes());
    md5.update(doc.next_num.to_le_bytes());
    md5.finalize().to_vec()
}

fn id_array(id: &(Vec<u8>, Vec<u8>)) -> Vec<Object> {
    vec![
        Object::String(PdfString::hex(id.0.clone())),
        Object::String(PdfString::hex(id.1.clone())),
    ]
}

/// A signature dictionary written key by key, recording where its
/// `/Contents` and `/ByteRange` values land.
fn write_signature(
    out: &mut Vec<u8>,
    id: ObjRef,
    dict: &Dict,
    crypt: Option<&OutputCrypt>,
    spans: &mut Vec<SignatureSpan>,
) {
    let dict = match crypt {
        Some(crypt) => {
            let mut copy = dict.clone();
            crypt.encrypt_dict(&mut copy, id);
            Cow::Owned(copy)
        }
        None => Cow::Borrowed(dict),
    };
    write_object_header(out, id);
    out.extend_from_slice(b"<<");
    let (mut contents, mut byte_range) = (None, None);
    for (key, value) in dict.iter() {
        write_name(out, key.as_bytes());
        let mut start = out.len();
        write_object(out, value);
        if out.get(start) == Some(&b' ') {
            start += 1;
        }
        match value {
            Object::String(_) if key == "Contents" => contents = Some(start..out.len()),
            Object::Array(_) if key == "ByteRange" => byte_range = Some(start..out.len()),
            _ => {}
        }
    }
    out.extend_from_slice(b">>\nendobj\n");
    if let Some(contents) = contents {
        spans.push(SignatureSpan {
            id,
            contents,
            byte_range,
        });
    }
}

fn write_header(out: &mut Vec<u8>, (major, minor): (u8, u8)) {
    out.extend_from_slice(format!("%PDF-{major}.{minor}\n").as_bytes());
    out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");
}

fn write_object_header(out: &mut Vec<u8>, id: ObjRef) {
    out.extend_from_slice(format!("{} {} obj\n", id.num, id.generation).as_bytes());
}

/// `N G obj`, `object` as it is, `endobj`.
fn write_plain(out: &mut Vec<u8>, id: ObjRef, object: &Object) {
    write_object_header(out, id);
    write_object(out, object);
    out.extend_from_slice(b"\nendobj\n");
}

fn write_trailer(out: &mut Vec<u8>, trailer: &Dict) {
    out.extend_from_slice(b"trailer\n");
    write_dict(out, trailer, None, &keep_same);
    out.push(b'\n');
}

fn write_startxref(out: &mut Vec<u8>, offset: usize) {
    out.extend_from_slice(format!("startxref\n{offset}\n%%EOF\n").as_bytes());
}

/// One entry of an output cross-reference section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XrefOut {
    Free { next: u32, generation: u16 },
    InFile { offset: usize, generation: u16 },
    InStream { stream: u32, index: u32 },
}

impl XrefOut {
    /// The type and two fields of a cross-reference stream entry.
    fn fields(self) -> (u8, u64, u64) {
        match self {
            XrefOut::Free { next, generation } => (0, u64::from(next), u64::from(generation)),
            XrefOut::InFile { offset, generation } => (1, offset as u64, u64::from(generation)),
            XrefOut::InStream { stream, index } => (2, u64::from(stream), u64::from(index)),
        }
    }
}

/// Mark every number below `size` that has no entry free, and chain the
/// free entries into the free list that starts at object 0.
fn fill_free(entries: &mut BTreeMap<u32, XrefOut>, size: u32) {
    entries.insert(
        0,
        XrefOut::Free {
            next: 0,
            generation: 65535,
        },
    );
    for num in 1..size {
        entries.entry(num).or_insert(XrefOut::Free {
            next: 0,
            generation: 0,
        });
    }
    let free: Vec<u32> = entries
        .iter()
        .filter(|(_, entry)| matches!(entry, XrefOut::Free { .. }))
        .map(|(&num, _)| num)
        .collect();
    for (i, num) in free.iter().enumerate() {
        let following = free.get(i + 1).copied().unwrap_or(0);
        if let Some(XrefOut::Free { next, .. }) = entries.get_mut(num) {
            *next = following;
        }
    }
}

/// Runs of consecutive object numbers, as (first, count).
fn runs(entries: &BTreeMap<u32, XrefOut>) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &num in entries.keys() {
        match runs.last_mut() {
            Some((first, count)) if u64::from(*first) + u64::from(*count) == u64::from(num) => {
                *count += 1
            }
            _ => runs.push((num, 1)),
        }
    }
    runs
}

/// A classic `xref` section, one subsection per run of numbers.
fn write_xref_table(out: &mut Vec<u8>, entries: &BTreeMap<u32, XrefOut>) -> Result<()> {
    out.extend_from_slice(b"xref\n");
    let mut values = entries.values();
    for (first, count) in runs(entries) {
        out.extend_from_slice(format!("{first} {count}\n").as_bytes());
        for entry in values.by_ref().take(count as usize) {
            let line = match *entry {
                XrefOut::Free { next, generation } => format!("{next:010} {generation:05} f \n"),
                XrefOut::InFile { offset, generation } => {
                    format!("{offset:010} {generation:05} n \n")
                }
                XrefOut::InStream { .. } => {
                    return Err(Error::Unsupported(
                        "a cross-reference table cannot list objects inside object streams".into(),
                    ));
                }
            };
            out.extend_from_slice(line.as_bytes());
        }
    }
    Ok(())
}

/// A cross-reference stream for `entries` (which include the stream's own
/// entry), its dictionary carrying the `trailer` entries.
fn xref_stream(entries: &BTreeMap<u32, XrefOut>, trailer: Dict) -> Stream {
    let (max2, max3) = entries.values().fold((0, 0), |(a, b), entry| {
        let (_, field2, field3) = entry.fields();
        (field2.max(a), field3.max(b))
    });
    let (w2, w3) = (byte_width(max2), byte_width(max3));
    let mut data = Vec::with_capacity(entries.len() * (1 + w2 + w3));
    for entry in entries.values() {
        let (kind, field2, field3) = entry.fields();
        data.push(kind);
        data.extend_from_slice(&field2.to_be_bytes()[8 - w2..]);
        data.extend_from_slice(&field3.to_be_bytes()[8 - w3..]);
    }
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XRef"));
    for (key, value) in trailer {
        dict.insert(key, value);
    }
    let index: Vec<Object> = runs(entries)
        .into_iter()
        .flat_map(|(first, count)| [Object::from(first), Object::from(count)])
        .collect();
    dict.insert("Index", index);
    dict.insert(
        "W",
        vec![Object::from(1), Object::from(w2), Object::from(w3)],
    );
    dict.insert("Filter", Object::name("FlateDecode"));
    Stream::new(dict, flate_encode(&data))
}

/// Bytes needed to hold `value` big-endian, at least one.
fn byte_width(value: u64) -> usize {
    let bits = (u64::BITS - value.leading_zeros()) as usize;
    bits.div_ceil(8).max(1)
}
