//! Linearized output (ISO 32000-2 Annex F) in the layout qpdf writes, so
//! that qpdf's own checks accept it: the page tree flattened, the first
//! page and the hint tables up front, and classic cross-reference tables.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter;
use std::ops::Range;
use std::sync::Arc;

use super::{
    Emitter, Planned, SaveOptions, XrefOut, allocate, compressed, id_array, write_header,
    write_plain, write_startxref, write_trailer, write_xref_table,
};
use crate::document::Document;
use crate::error::{Error, Result};
use crate::filters::flate_encode;
use crate::object::{Dict, ObjRef, Object, Stream};
use crate::pages::{INHERITABLE, Page};
use crate::serialize::{write_name, write_object};

/// Bytes reserved for the linearization dictionary object. It sits first
/// but is written last, once `/L`, `/H`, `/E`, and `/T` are known.
const LINDICT_WIDTH: usize = 200;
/// The first-page trailer's `startxref`, which readers ignore; the one at
/// the end of the file points at the first-page table.
const STARTXREF_ZERO: &[u8] = b"startxref\n0\n%%EOF\n";
/// Catalog keys whose objects a viewer needs to open the document.
const OPEN_DOCUMENT_KEYS: [&[u8]; 5] = [
    b"ViewerPreferences",
    b"PageMode",
    b"Threads",
    b"OpenAction",
    b"AcroForm",
];

impl Document {
    /// A linearized full rewrite; see [`SaveOptions::linearize`].
    pub(super) fn write_linearized(&self, options: &SaveOptions) -> Result<Vec<u8>> {
        let options = SaveOptions {
            object_streams: false,
            ..options.clone()
        };
        let pages = self.pages()?;
        if pages.is_empty() {
            return self.write_full(&options, &self.plan_reachable(&HashMap::new())?);
        }
        let mut plan = self.plan_reachable(&self.flattened(&pages)?)?;
        if options.compress_streams {
            // Compressed before the walk below, which must see the stream
            // dictionaries as they are written.
            for planned in &mut plan.objects {
                if let Object::Stream(stream) = planned.object.as_ref()
                    && let Some(packed) = compressed(stream)
                {
                    planned.object = Arc::new(Object::Stream(packed));
                }
            }
        }
        let objects = plan.objects.as_slice();
        let positions: HashMap<u32, usize> = objects
            .iter()
            .enumerate()
            .map(|(position, planned)| (planned.source, position))
            .collect();
        let position = |id: ObjRef| {
            positions
                .get(&id.num)
                .copied()
                .ok_or(Error::MissingObject(id))
        };
        let catalog = position(self.catalog_ref()?)?;
        let page_positions = pages
            .iter()
            .map(|page| position(page.id))
            .collect::<Result<Vec<_>>>()?;
        let users = Users::new(objects, &positions, &page_positions, &self.trailer, catalog);
        let layout = Layout::new(&users, &page_positions, catalog)?;

        let id = self.output_id(&options);
        let crypt = self.full_crypt(&options.encryption, &id.0)?;
        let version = self.output_version(&options, false, crypt.as_ref());

        // Parts 7 to 9 are numbered from 1; then come the linearization
        // dictionary, part 4, /Encrypt, the hint stream, and part 6.
        let (part6, later) = layout.back.split_at(layout.first_page);
        let mut numbers = vec![0; objects.len()];
        let mut next = 1;
        for &p in later {
            numbers[p] = allocate(&mut next)?.num;
        }
        let lindict = allocate(&mut next)?;
        for &p in &layout.open {
            numbers[p] = allocate(&mut next)?.num;
        }
        let encrypt_id = match crypt {
            Some(_) => Some(allocate(&mut next)?),
            None => None,
        };
        let hint_id = allocate(&mut next)?;
        for &p in part6 {
            numbers[p] = allocate(&mut next)?.num;
        }
        let lookup = |r: ObjRef| positions.get(&r.num).map(|&p| ObjRef::new(numbers[p], 0));
        let emitter = Emitter {
            map: &lookup,
            crypt: crypt.as_ref().map(|c| &c.crypt),
            compress: false,
        };

        // Part 4 and /Encrypt, in front of the hint stream.
        let mut front = Vec::new();
        let mut front_offsets = Vec::with_capacity(layout.open.len() + 1);
        for &p in &layout.open {
            let id = ObjRef::new(numbers[p], 0);
            front_offsets.push((id.num, front.len()));
            emitter.indirect(&mut front, id, &objects[p].object, true);
        }
        if let (Some(crypt), Some(id)) = (&crypt, encrypt_id) {
            front_offsets.push((id.num, front.len()));
            write_plain(&mut front, id, &crypt.dict);
        }
        // Parts 6 to 9, behind it.
        let mut back = Vec::with_capacity(self.data.len());
        let mut spans = Vec::with_capacity(layout.back.len());
        for &p in &layout.back {
            let start = back.len();
            emitter.indirect(
                &mut back,
                ObjRef::new(numbers[p], 0),
                &objects[p].object,
                true,
            );
            spans.push(start..back.len());
        }

        let trailer = self.full_trailer(next, &lookup, &id, encrypt_id)?;
        let mut out =
            Vec::with_capacity(back.len().saturating_add(front.len()).saturating_add(4096));
        write_header(&mut out, version);
        let lindict_offset = out.len();
        let first_xref = lindict_offset + LINDICT_WIDTH + 1;
        let front_offset = first_xref
            + table_len(lindict.num, next - lindict.num)
            + first_trailer(&trailer, 0).len()
            + STARTXREF_ZERO.len();
        let hint_offset = front_offset + front.len();
        let tables = hint_tables(&layout, &spans, &numbers, hint_offset)?;
        let mut hint_dict = Dict::new();
        hint_dict.insert("Filter", Object::name("FlateDecode"));
        hint_dict.insert("S", tables.shared_at);
        if let Some(outlines_at) = tables.outlines_at {
            hint_dict.insert("O", outlines_at);
        }
        let mut hint = Vec::new();
        emitter.indirect(
            &mut hint,
            hint_id,
            &Object::Stream(Stream::new(hint_dict, flate_encode(&tables.data))),
            true,
        );
        // Where part 6 starts, and the main cross-reference table.
        let behind = hint_offset + hint.len();
        let main_xref = behind + back.len();

        let in_file = |offset| XrefOut::InFile {
            offset,
            generation: 0,
        };
        let mut first = BTreeMap::from([
            (lindict.num, in_file(lindict_offset)),
            (hint_id.num, in_file(hint_offset)),
        ]);
        first.extend(
            front_offsets
                .iter()
                .map(|&(num, offset)| (num, in_file(front_offset + offset))),
        );
        let mut main = BTreeMap::from([(
            0,
            XrefOut::Free {
                next: 0,
                generation: 65535,
            },
        )]);
        for (index, (&p, span)) in layout.back.iter().zip(&spans).enumerate() {
            let table = if index < layout.first_page {
                &mut first
            } else {
                &mut main
            };
            table.insert(numbers[p], in_file(behind + span.start));
        }
        let mut tail = Vec::new();
        write_xref_table(&mut tail, &main)?;
        let mut main_trailer = Dict::new();
        main_trailer.insert("Size", lindict.num);
        main_trailer.insert("ID", id_array(&id));
        write_trailer(&mut tail, &main_trailer);
        write_startxref(&mut tail, first_xref);

        let first_page_end = behind + spans[..layout.first_page].last().map_or(0, |span| span.end);
        // The whitespace before the main table's first entry.
        let zero_entry = main_xref + format!("xref\n0 {}", lindict.num).len();
        let file_len = main_xref + tail.len();
        let dict = format!(
            "{} 0 obj\n<< /Linearized 1 /L {file_len} /H [ {hint_offset} {} ] /O {} /E {first_page_end} /N {} /T {zero_entry} >>\nendobj\n",
            lindict.num,
            hint.len(),
            numbers[page_positions[0]],
            pages.len(),
        );
        let padding = LINDICT_WIDTH.checked_sub(dict.len()).ok_or_else(|| {
            Error::LimitExceeded("a linearization dictionary wider than its reserved space".into())
        })?;
        out.extend_from_slice(dict.as_bytes());
        out.resize(out.len() + padding, b' ');
        out.push(b'\n');
        write_xref_table(&mut out, &first)?;
        out.extend_from_slice(&first_trailer(&trailer, main_xref));
        out.extend_from_slice(STARTXREF_ZERO);
        out.extend_from_slice(&front);
        out.extend_from_slice(&hint);
        out.extend_from_slice(&back);
        out.extend_from_slice(&tail);
        Ok(out)
    }

    /// Replacement objects, by number, that flatten the page tree: every
    /// page directly under the root `/Pages` node with its inherited
    /// attributes set on it, as linearization requires. A direct
    /// `/Outlines` is made indirect, and a new root node takes the next
    /// free number when the catalog's `/Pages` is itself a page.
    fn flattened(&self, pages: &[Page]) -> Result<HashMap<u32, Arc<Object>>> {
        let tree = self.page_tree()?;
        let mut catalog = self.catalog()?;
        let mut catalog_changed = false;
        let mut next = self.next_num;
        let mut replaced = HashMap::new();
        let (root, mut node) = if pages.iter().any(|page| page.id.num == tree.root.num) {
            let root = allocate(&mut next)?;
            catalog.insert("Pages", root);
            catalog_changed = true;
            (root, Dict::new())
        } else {
            (
                tree.root,
                self.resolve_dict(&Object::Reference(tree.root))?
                    .unwrap_or_default(),
            )
        };
        for key in [b"Kids".as_slice(), b"Count", b"Parent"]
            .into_iter()
            .chain(INHERITABLE)
        {
            node.remove(key);
        }
        node.insert("Type", Object::name("Pages"));
        node.insert(
            "Kids",
            pages
                .iter()
                .map(|page| Object::Reference(page.id))
                .collect::<Vec<_>>(),
        );
        node.insert("Count", pages.len());
        replaced.insert(root.num, Arc::new(Object::Dict(node)));
        for page in pages {
            let mut dict = page.dict.clone();
            dict.insert("Type", Object::name("Page"));
            dict.insert("Parent", root);
            // Other readers take a dictionary with /Kids for a tree node.
            dict.remove(b"Kids");
            replaced.insert(page.id.num, Arc::new(Object::Dict(dict)));
        }
        if let Some(Object::Dict(outlines)) = catalog.get(b"Outlines") {
            let id = allocate(&mut next)?;
            replaced.insert(id.num, Arc::new(Object::Dict(outlines.clone())));
            catalog.insert("Outlines", id);
            catalog_changed = true;
        }
        if catalog_changed {
            replaced.insert(self.catalog_ref()?.num, Arc::new(Object::Dict(catalog)));
        }
        Ok(replaced)
    }
}

/// Who uses each object, counted the way qpdf's `QPDF::optimize` counts:
/// each page, each page's thumbnail, each trailer and catalog key, and the
/// catalog itself.
struct Users<'a> {
    objects: &'a [Planned],
    positions: &'a HashMap<u32, usize>,
    /// By plan position.
    usage: Vec<Usage>,
    /// What each page uses, in the order its walk reached them.
    by_page: Vec<Vec<usize>>,
    /// What the catalog's `/Outlines` uses, in the order reached.
    by_outlines: Vec<usize>,
    /// The outline dictionary.
    outline_root: Option<usize>,
    /// `/PageMode /UseOutlines`: the outlines go with the first page.
    outlines_first: bool,
}

/// One object's users.
#[derive(Clone, Copy, Default)]
struct Usage {
    catalog: bool,
    outlines: bool,
    open_document: bool,
    first_page: bool,
    other_pages: u32,
    thumbnails: u32,
    others: u32,
    /// Distinct users of every kind.
    users: u32,
}

#[derive(Clone, Copy)]
enum User {
    Catalog,
    Page(usize),
    Thumbnail,
    Outlines,
    OpenDocument,
    Other,
}

/// Where an object goes by its users, in qpdf's order of precedence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Catalog,
    Outlines,
    OpenDocument,
    FirstPagePrivate,
    FirstPageShared,
    OtherPagePrivate,
    OtherPageShared,
    /// Thumbnails, objects shared outside the pages, and objects no user
    /// reaches.
    Rest,
}

impl Usage {
    fn class(&self) -> Class {
        let private = self.others == 0 && self.thumbnails == 0;
        if self.catalog {
            Class::Catalog
        } else if self.outlines {
            Class::Outlines
        } else if self.open_document {
            Class::OpenDocument
        } else if self.first_page && private && self.other_pages == 0 {
            Class::FirstPagePrivate
        } else if self.first_page {
            Class::FirstPageShared
        } else if self.other_pages == 1 && private {
            Class::OtherPagePrivate
        } else if self.other_pages > 1 {
            Class::OtherPageShared
        } else {
            Class::Rest
        }
    }
}

impl<'a> Users<'a> {
    fn new(
        objects: &'a [Planned],
        positions: &'a HashMap<u32, usize>,
        pages: &[usize],
        trailer: &'a Dict,
        catalog: usize,
    ) -> Users<'a> {
        let mut users = Users {
            objects,
            positions,
            usage: vec![Usage::default(); objects.len()],
            by_page: vec![Vec::new(); pages.len()],
            by_outlines: Vec::new(),
            outline_root: None,
            outlines_first: false,
        };
        for (page, &position) in pages.iter().enumerate() {
            users.record(position, User::Page(page));
            users.walk(objects[position].object.as_ref(), User::Page(page));
        }
        for (key, value) in trailer.iter() {
            if !matches!(
                key.as_bytes(),
                b"Root" | b"Size" | b"Prev" | b"XRefStm" | b"ID" | b"Encrypt"
            ) {
                users.walk(value, User::Other);
            }
        }
        if let Object::Dict(dict) = objects[catalog].object.as_ref() {
            for (key, value) in dict.iter() {
                let user = if key == "Outlines" {
                    User::Outlines
                } else if OPEN_DOCUMENT_KEYS.contains(&key.as_bytes()) {
                    User::OpenDocument
                } else {
                    User::Other
                };
                users.walk(value, user);
            }
            users.outline_root = dict
                .get_ref(b"Outlines")
                .and_then(|id| positions.get(&id.num).copied());
            let page_mode = dict.get(b"PageMode").and_then(|mode| users.resolved(mode));
            users.outlines_first = users.outline_root.is_some()
                && matches!(page_mode, Some(Object::Name(mode)) if mode == "UseOutlines");
        }
        users.record(catalog, User::Catalog);
        users
    }

    /// qpdf's walk from one user: depth first, a dictionary's values in key
    /// order, pages other than the starting one skipped, a page's `/Parent`
    /// skipped and its `/Thumb` counted for the thumbnail.
    fn walk(&mut self, start: &'a Object, user: User) {
        let (objects, positions) = (self.objects, self.positions);
        let mut visited = HashSet::new();
        let mut pending = vec![(start, user, true)];
        while let Some((item, user, top)) = pending.pop() {
            let (object, position) = match item {
                Object::Reference(id) => match positions.get(&id.num) {
                    Some(&position) => (objects[position].object.as_ref(), Some(position)),
                    None => continue,
                },
                direct => (direct, None),
            };
            let page = matches!(object, Object::Dict(dict) if dict.has_type(b"Page"));
            if page && !top {
                continue;
            }
            if let Some(position) = position {
                if !visited.insert(position) {
                    continue;
                }
                self.record(position, user);
            }
            let (dict, stream) = match object {
                Object::Array(items) => {
                    pending.extend(items.iter().map(|item| (item, user, false)));
                    continue;
                }
                Object::Dict(dict) => (dict, false),
                // The written /Length is always direct.
                Object::Stream(stream) => (&stream.dict, true),
                _ => continue,
            };
            let mut entries: Vec<_> = dict
                .iter()
                .filter(|&(key, value)| !(stream && key == "Length") && !is_null(value, positions))
                .collect();
            entries.sort_by_key(|&(key, _)| key);
            for (key, value) in entries {
                if page && key == "Parent" {
                    continue;
                }
                let user = if page && key == "Thumb" {
                    User::Thumbnail
                } else {
                    user
                };
                pending.push((value, user, false));
            }
        }
    }

    fn record(&mut self, position: usize, user: User) {
        let usage = &mut self.usage[position];
        usage.users += 1;
        match user {
            User::Catalog => usage.catalog = true,
            User::Page(0) => usage.first_page = true,
            User::Page(_) => usage.other_pages += 1,
            User::Thumbnail => usage.thumbnails += 1,
            User::Outlines => usage.outlines = true,
            User::OpenDocument => usage.open_document = true,
            User::Other => usage.others += 1,
        }
        match user {
            User::Page(page) => self.by_page[page].push(position),
            User::Outlines => self.by_outlines.push(position),
            _ => {}
        }
    }

    /// `value`, following a reference; `None` for null.
    fn resolved(&self, value: &'a Object) -> Option<&'a Object> {
        let objects = self.objects;
        match value {
            Object::Reference(id) => self
                .positions
                .get(&id.num)
                .map(|&p| objects[p].object.as_ref()),
            Object::Null => None,
            direct => Some(direct),
        }
    }
}

/// A value qpdf's walk skips: null, or a reference to an object that is
/// not written.
fn is_null(value: &Object, positions: &HashMap<u32, usize>) -> bool {
    match value {
        Object::Null => true,
        Object::Reference(id) => !positions.contains_key(&id.num),
        _ => false,
    }
}

/// The objects in file order by linearization part (ISO 32000-2 F.3), as
/// plan positions.
struct Layout {
    /// Part 4: the catalog, then what opening the document needs.
    open: Vec<usize>,
    /// Parts 6 to 9, in file order behind the hint stream.
    back: Vec<usize>,
    /// Objects in part 6: the first page, everything it uses, and the
    /// outlines when the document opens showing them.
    first_page: usize,
    /// Size of each later page's group in part 7: the page, then what only
    /// it uses.
    page_groups: Vec<usize>,
    /// Objects in part 8, which several later pages share.
    shared: usize,
    /// The outline objects, as a range of `back`.
    outlines: Range<usize>,
    /// Each page's entries in the shared object hint table.
    page_shared: Vec<Vec<u64>>,
}

impl Layout {
    fn new(users: &Users<'_>, pages: &[usize], catalog: usize) -> Result<Layout> {
        let Some(&first) = pages.first() else {
            return Err(Error::invalid("linearizing a document without pages"));
        };
        let classes: Vec<Class> = users.usage.iter().map(Usage::class).collect();
        for (index, &page) in pages.iter().enumerate() {
            let private = if index == 0 {
                Class::FirstPagePrivate
            } else {
                Class::OtherPagePrivate
            };
            if classes[page] != private {
                return Err(Error::Unsupported(format!(
                    "linearizing a document whose page {} is referenced from outside the page tree",
                    index + 1
                )));
            }
        }
        let open: Vec<usize> = iter::once(catalog)
            .chain((0..classes.len()).filter(|&p| classes[p] == Class::OpenDocument))
            .collect();
        let outline_group: Vec<usize> = match users.outline_root {
            Some(root) if classes[root] == Class::Outlines => iter::once(root)
                .chain(
                    users
                        .by_outlines
                        .iter()
                        .copied()
                        .filter(|&p| p != root && classes[p] == Class::Outlines),
                )
                .collect(),
            _ => Vec::new(),
        };

        let mut back = vec![first];
        for class in [Class::FirstPagePrivate, Class::FirstPageShared] {
            back.extend(
                users.by_page[0]
                    .iter()
                    .copied()
                    .filter(|&p| p != first && classes[p] == class),
            );
        }
        let mut outlines = 0..0;
        if users.outlines_first {
            outlines = back.len()..back.len() + outline_group.len();
            back.extend_from_slice(&outline_group);
        }
        let first_page = back.len();
        let mut page_groups = Vec::with_capacity(pages.len() - 1);
        for (used, &page) in users.by_page.iter().zip(pages).skip(1) {
            let start = back.len();
            back.push(page);
            back.extend(
                used.iter()
                    .copied()
                    .filter(|&p| p != page && classes[p] == Class::OtherPagePrivate),
            );
            page_groups.push(back.len() - start);
        }
        let shared_start = back.len();
        let mut seen = HashSet::new();
        for used in users.by_page.iter().skip(1) {
            back.extend(
                used.iter()
                    .copied()
                    .filter(|&p| classes[p] == Class::OtherPageShared && seen.insert(p)),
            );
        }
        let shared = back.len() - shared_start;
        if !users.outlines_first {
            outlines = back.len()..back.len() + outline_group.len();
            back.extend_from_slice(&outline_group);
        }
        let mut placed = vec![false; classes.len()];
        for &p in open.iter().chain(&back) {
            placed[p] = true;
        }
        back.extend((0..classes.len()).filter(|&p| !placed[p]));

        // Shared object table entries: part 6, then part 8, one object each.
        let shared_index: HashMap<usize, u64> = back[..first_page]
            .iter()
            .chain(&back[shared_start..shared_start + shared])
            .zip(0..)
            .map(|(&p, index)| (p, index))
            .collect();
        let page_shared = users
            .by_page
            .iter()
            .enumerate()
            .map(|(page, used)| {
                // The first page lists none (F.4.1).
                if page == 0 {
                    return Vec::new();
                }
                used.iter()
                    .filter(|&&p| users.usage[p].users > 1)
                    .filter_map(|p| shared_index.get(p).copied())
                    .collect()
            })
            .collect();
        Ok(Layout {
            open,
            back,
            first_page,
            page_groups,
            shared,
            outlines,
            page_shared,
        })
    }
}

/// The hint stream's contents (ISO 32000-2 F.4): the page offset table, the
/// shared object table at `shared_at`, and the outline table at
/// `outlines_at`.
struct HintTables {
    data: Vec<u8>,
    shared_at: usize,
    outlines_at: Option<usize>,
}

/// The hint tables for `layout`, whose objects span `spans` of the bytes
/// behind the hint stream. Offsets are the ones the file would have without
/// the hint stream, which starts at `base`.
fn hint_tables(
    layout: &Layout,
    spans: &[Range<usize>],
    numbers: &[u32],
    base: usize,
) -> Result<HintTables> {
    let length =
        |range: Range<usize>| -> u64 { spans[range].iter().map(|span| span.len() as u64).sum() };
    let offset = |index: usize| (base + spans[index].start) as u64;
    let number = |index: usize| u64::from(numbers[layout.back[index]]);

    // Page offset table. The first page's group is all of part 6.
    let mut groups = Vec::with_capacity(layout.page_groups.len() + 1);
    groups.push(0..layout.first_page);
    for &count in &layout.page_groups {
        let start = groups.last().map_or(0, |group| group.end);
        groups.push(start..start + count);
    }
    let objects: Vec<u64> = groups.iter().map(|group| group.len() as u64).collect();
    let lengths: Vec<u64> = groups.iter().map(|group| length(group.clone())).collect();
    let (least_objects, most_objects) = bounds(&objects);
    let (least_length, most_length) = bounds(&lengths);
    let most_shared = layout.page_shared.iter().map(Vec::len).max().unwrap_or(0) as u64;
    let shared_total = (layout.first_page + layout.shared) as u64;
    let object_bits = nbits(most_objects - least_objects);
    let length_bits = nbits(most_length - least_length);
    let count_bits = nbits(most_shared);
    let id_bits = nbits(shared_total);
    let mut bits = BitWriter::default();
    bits.write32(least_objects)?;
    bits.write32(offset(0))?;
    bits.write(u64::from(object_bits), 16);
    bits.write32(least_length)?;
    bits.write(u64::from(length_bits), 16);
    // Content offsets and lengths as Acrobat writes them: offset 0 and the
    // page lengths (implementation notes 126 and 127).
    bits.write(0, 32);
    bits.write(0, 16);
    bits.write32(least_length)?;
    bits.write(u64::from(length_bits), 16);
    bits.write(u64::from(count_bits), 16);
    bits.write(u64::from(id_bits), 16);
    // Numerators take no bits; the denominator is then unused.
    bits.write(0, 16);
    bits.write(4, 16);
    bits.row(objects.iter().map(|&n| n - least_objects), object_bits);
    bits.row(lengths.iter().map(|&n| n - least_length), length_bits);
    bits.row(
        layout.page_shared.iter().map(|ids| ids.len() as u64),
        count_bits,
    );
    bits.row(layout.page_shared.iter().flatten().copied(), id_bits);
    // The numerator and content offset rows take no bits.
    bits.row(lengths.iter().map(|&n| n - least_length), length_bits);
    let shared_at = bits.bytes.len();

    // Shared object table: part 6, then part 8, one object per group.
    let shared_start = groups.last().map_or(0, |group| group.end);
    let shared = shared_start..shared_start + layout.shared;
    let entries: Vec<u64> = (0..layout.first_page)
        .chain(shared.clone())
        .map(|index| spans[index].len() as u64)
        .collect();
    let (least_group, most_group) = bounds(&entries);
    let group_bits = nbits(most_group - least_group);
    let (first_shared, first_shared_offset) = if shared.is_empty() {
        (0, 0)
    } else {
        (number(shared.start), offset(shared.start))
    };
    bits.write32(first_shared)?;
    bits.write32(first_shared_offset)?;
    bits.write32(layout.first_page as u64)?;
    bits.write32(shared_total)?;
    // Object counts minus one, all zero, take no bits.
    bits.write(0, 16);
    bits.write32(least_group)?;
    bits.write(u64::from(group_bits), 16);
    bits.row(entries.iter().map(|&n| n - least_group), group_bits);
    // No group carries an MD5 signature.
    bits.row(entries.iter().map(|_| 0), 1);

    let outlines_at = if layout.outlines.is_empty() {
        None
    } else {
        let at = bits.bytes.len();
        bits.write32(number(layout.outlines.start))?;
        bits.write32(offset(layout.outlines.start))?;
        bits.write32(layout.outlines.len() as u64)?;
        bits.write32(length(layout.outlines.clone()))?;
        Some(at)
    };
    bits.flush();
    Ok(HintTables {
        data: bits.bytes,
        shared_at,
        outlines_at,
    })
}

/// The least and greatest of `values`, zero when empty.
fn bounds(values: &[u64]) -> (u64, u64) {
    (
        values.iter().copied().min().unwrap_or(0),
        values.iter().copied().max().unwrap_or(0),
    )
}

/// Bits needed to write `value`: none for zero.
fn nbits(value: u64) -> u32 {
    u64::BITS - value.leading_zeros()
}

/// Hint table bits, most significant first.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    pending: u8,
    used: u32,
}

impl BitWriter {
    fn write(&mut self, value: u64, bits: u32) {
        for shift in (0..bits).rev() {
            self.pending = (self.pending << 1) | u8::from((value >> shift) & 1 == 1);
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.pending);
                self.pending = 0;
                self.used = 0;
            }
        }
    }

    fn write32(&mut self, value: u64) -> Result<()> {
        if value > u64::from(u32::MAX) {
            return Err(Error::LimitExceeded(
                "linearized output beyond the 4 GiB its hint tables address".into(),
            ));
        }
        self.write(value, 32);
        Ok(())
    }

    /// One value per entry, then padding to a byte boundary, where each row
    /// of a hint table starts.
    fn row(&mut self, values: impl IntoIterator<Item = u64>, bits: u32) {
        for value in values {
            self.write(value, bits);
        }
        self.flush();
    }

    fn flush(&mut self) {
        if self.used > 0 {
            self.bytes.push(self.pending << (8 - self.used));
            self.pending = 0;
            self.used = 0;
        }
    }
}

/// Length of a one-subsection cross-reference table as `write_xref_table`
/// writes it.
fn table_len(first: u32, count: u32) -> usize {
    format!("xref\n{first} {count}\n").len() + 20 * count as usize
}

/// The first-page trailer: `trailer`, then `/Prev` padded so that the
/// length does not depend on the offset.
fn first_trailer(trailer: &Dict, prev: usize) -> Vec<u8> {
    let mut out = b"trailer\n<<".to_vec();
    for (key, value) in trailer.iter() {
        write_name(&mut out, key.as_bytes());
        write_object(&mut out, value);
    }
    out.extend_from_slice(format!(" /Prev {prev:<21}>>\n").as_bytes());
    out
}
