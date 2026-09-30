//! Disposable per-page extraction cache.
//!
//! The SQLite schema, keys, and encodings retain the on-disk format of existing
//! installations. Every failure disables or skips the cache and never fails the verb.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blake2::{Blake2b512, Digest};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::ctx::Ctx;
use crate::error::GoatError;
use crate::parse::parse_pages;
use crate::paths::mkdir_parents;
use crate::pool::{PageTask, PoolTuning, Store, map_with_store};
use crate::py::parse_float;

/// The cache file inside the state directory.
pub const CACHE_FILE: &str = "cache.sqlite";

const MIB: u64 = 1024 * 1024;
/// The budget when `PDF_GOAT_CACHE_MB` is unset or unreadable.
pub const DEFAULT_CAP_BYTES: u64 = 256 * MIB;
const HASH_WINDOW: u64 = 2 * MIB;
/// Budgeted per row for the SQLite record and key fields, so empty pages still count.
const ROW_OVERHEAD: i64 = 64;
const SQL_VARIABLE_LIMIT: usize = 900;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    size_bytes INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    digest BLOB NOT NULL,
    path TEXT NOT NULL,
    page_count INTEGER NOT NULL,
    row_bytes INTEGER NOT NULL DEFAULT 0,
    last_used INTEGER NOT NULL,
    UNIQUE(size_bytes, mtime_ns, digest)
);
CREATE TABLE IF NOT EXISTS pages (
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    form TEXT NOT NULL,
    page_index INTEGER NOT NULL,
    text_value TEXT,
    char_count INTEGER,
    word_count INTEGER,
    word_text TEXT,
    rects BLOB,
    lines BLOB,
    row_bytes INTEGER NOT NULL,
    PRIMARY KEY(document_id, form, page_index)
);
";

/// A document's disposable identity: size, modification time, and a BLAKE2b-512 digest of
/// the whole file up to 2 MiB, else of its first and last MiB.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DocumentKey {
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub digest: Vec<u8>,
}

/// `document_key(path)`: the key, or `None` when the file cannot be read.
pub fn document_key(path: &Path) -> Option<DocumentKey> {
    let metadata = fs::metadata(path).ok()?;
    let size = metadata.len();
    let mtime_ns = metadata
        .mtime()
        .checked_mul(1_000_000_000)?
        .checked_add(metadata.mtime_nsec())?;
    let mut file = File::open(path).ok()?;
    let mut digest = Blake2b512::new();
    let mut buffer = Vec::new();
    if size <= HASH_WINDOW {
        file.read_to_end(&mut buffer).ok()?;
        digest.update(&buffer);
    } else {
        (&mut file).take(MIB).read_to_end(&mut buffer).ok()?;
        digest.update(&buffer);
        buffer.clear();
        file.seek(SeekFrom::End(-(MIB as i64))).ok()?;
        (&mut file).take(MIB).read_to_end(&mut buffer).ok()?;
        digest.update(&buffer);
    }
    Some(DocumentKey {
        size_bytes: i64::try_from(size).ok()?,
        mtime_ns,
        digest: digest.finalize().to_vec(),
    })
}

/// `_cache_limit()`: the byte budget for a `PDF_GOAT_CACHE_MB` value (`None` when unset).
/// Zero or less disables the cache; an unreadable value means the 256 MiB default.
pub fn cache_limit_bytes(megabytes: Option<&str>) -> u64 {
    let Ok(megabytes) = parse_float(megabytes.unwrap_or("256")) else {
        return DEFAULT_CAP_BYTES;
    };
    let bytes = megabytes * MIB as f64;
    if !bytes.is_finite() {
        return DEFAULT_CAP_BYTES;
    }
    // `int()` truncates toward zero; the float-to-integer cast saturates past `u64::MAX`.
    if bytes <= 0.0 {
        0
    } else {
        bytes.trunc() as u64
    }
}

/// One cached form of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Form {
    /// The page's plain text.
    Text,
    /// Character and word counts.
    Count,
    /// Word columns, shared by search and meaning search.
    Words,
}

impl Form {
    /// The `form` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Count => "count",
            Self::Words => "words",
        }
    }
}

/// One page's cached value.
#[derive(Clone, Debug, PartialEq)]
pub enum PageEntry {
    Text(String),
    Count { chars: i64, words: i64 },
    Words(WordColumns),
}

/// A page's words as columns: `text` holds the words joined by `\n`, `rects` four floats
/// per word, and `lines` two integers per word.
#[derive(Clone, Debug, PartialEq)]
pub struct WordColumns {
    pub text: String,
    pub rects: Vec<f64>,
    pub lines: Vec<i32>,
}

impl PageEntry {
    /// The form this value is stored under.
    pub fn form(&self) -> Form {
        match self {
            Self::Text(_) => Form::Text,
            Self::Count { .. } => Form::Count,
            Self::Words(_) => Form::Words,
        }
    }
}

/// A cached document row: its id and the page count stored with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachedDocument {
    pub id: i64,
    pub page_count: usize,
}

/// The best-effort SQLite cache; a failure disables only the cache.
#[derive(Debug)]
pub struct Cache {
    path: PathBuf,
    cap_bytes: u64,
    connection: Option<Connection>,
}

impl Cache {
    /// Opens or creates the cache file at `path` with a budget of `cap_bytes`. A zero
    /// budget, or any failure to create the directory, open, or migrate, leaves it disabled.
    pub fn open(path: impl Into<PathBuf>, cap_bytes: u64) -> Self {
        let path = path.into();
        let connection = if cap_bytes == 0 { None } else { connect(&path) };
        Self {
            path,
            cap_bytes,
            connection,
        }
    }

    /// Whether the cache has a working connection.
    pub fn enabled(&self) -> bool {
        self.connection.is_some()
    }

    /// The cache file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The byte budget.
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// The row for `key`, deleting a row the cache can prove damaged: a page count above the
    /// file size in bytes, or a stored page index at or above the count. Fewer rows than the
    /// count is legal, since `--pages` primes part of a document.
    pub fn document(&self, key: &DocumentKey) -> Option<CachedDocument> {
        let connection = self.connection.as_ref()?;
        let row = connection
            .query_row(
                "SELECT id, page_count, \
                 (SELECT MAX(page_index) FROM pages WHERE document_id=documents.id) \
                 FROM documents WHERE size_bytes=? AND mtime_ns=? AND digest=?",
                params![key.size_bytes, key.mtime_ns, key.digest],
                |row| {
                    Ok((
                        row.get::<_, SqlValue>(0)?,
                        row.get::<_, SqlValue>(1)?,
                        row.get::<_, SqlValue>(2)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let valid = match row {
            (SqlValue::Integer(id), SqlValue::Integer(count), highest) => {
                let contradicted =
                    matches!(highest, SqlValue::Integer(highest) if highest >= count);
                (id >= 1 && count >= 0 && count <= key.size_bytes && !contradicted)
                    .then(|| {
                        usize::try_from(count)
                            .ok()
                            .map(|page_count| CachedDocument { id, page_count })
                    })
                    .flatten()
            }
            _ => None,
        };
        if valid.is_none() {
            self.discard_document(key);
        }
        valid
    }

    /// Deletes the row for `key` and, through the foreign key, its pages.
    pub fn discard_document(&self, key: &DocumentKey) {
        if let Some(connection) = &self.connection {
            // A failed delete leaves the row for the next run to reject again.
            let _ = connection.execute(
                "DELETE FROM documents WHERE size_bytes=? AND mtime_ns=? AND digest=?",
                params![key.size_bytes, key.mtime_ns, key.digest],
            );
        }
    }

    /// The stored `form` values among `indices`. Rows of the wrong shape are skipped, and
    /// any read failure returns nothing.
    pub fn lookup(
        &self,
        key: &DocumentKey,
        form: Form,
        indices: &[usize],
    ) -> HashMap<usize, PageEntry> {
        let Some(connection) = &self.connection else {
            return HashMap::new();
        };
        if indices.is_empty() {
            return HashMap::new();
        }
        let Some(document) = self.document(key) else {
            return HashMap::new();
        };
        let wanted = unique(indices);
        let wanted_set: HashSet<usize> = wanted.iter().copied().collect();
        let mut values = HashMap::new();
        for chunk in wanted.chunks(SQL_VARIABLE_LIMIT - 2) {
            let read = read_chunk(
                connection,
                document.id,
                form,
                chunk,
                &wanted_set,
                &mut values,
            );
            if read.is_err() {
                return HashMap::new();
            }
        }
        if !values.is_empty() {
            // Recency is bookkeeping: a failed touch keeps the rows already decoded.
            let _ = connection.execute(
                "UPDATE documents SET last_used=? WHERE id=?",
                params![now_ns(), document.id],
            );
        }
        values
    }

    /// Stores `entries` for a document of `page_count` pages, then evicts least recently
    /// used documents until the cache fits its budget, possibly this one. Entries of a
    /// form other than `form` abandon the whole write, as does any failure.
    pub fn write<'e>(
        &self,
        key: &DocumentKey,
        source: &Path,
        page_count: usize,
        form: Form,
        entries: impl IntoIterator<Item = (usize, &'e PageEntry)>,
    ) {
        let Some(connection) = &self.connection else {
            return;
        };
        let encoded: Option<Vec<(i64, Encoded<'e>)>> = entries
            .into_iter()
            .map(|(index, entry)| Some((i64::try_from(index).ok()?, encode(form, entry)?)))
            .collect();
        let Some(encoded) = encoded.filter(|encoded| !encoded.is_empty()) else {
            return;
        };
        let Ok(page_count) = i64::try_from(page_count) else {
            return;
        };
        // An abandoned transaction rolls back when dropped.
        let _ = self.write_encoded(connection, key, source, page_count, form, &encoded);
    }

    fn write_encoded(
        &self,
        connection: &Connection,
        key: &DocumentKey,
        source: &Path,
        page_count: i64,
        form: Form,
        encoded: &[(i64, Encoded<'_>)],
    ) -> rusqlite::Result<()> {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO documents(
                size_bytes, mtime_ns, digest, path, page_count, row_bytes, last_used
            ) VALUES(?,?,?,?,?,?,?)
            ON CONFLICT(size_bytes, mtime_ns, digest) DO UPDATE SET
                path=excluded.path,
                page_count=excluded.page_count",
            params![
                key.size_bytes,
                key.mtime_ns,
                key.digest,
                source.to_string_lossy(),
                page_count,
                0,
                now_ns()
            ],
        )?;
        let document = transaction
            .query_row(
                "SELECT id FROM documents WHERE size_bytes=? AND mtime_ns=? AND digest=?",
                params![key.size_bytes, key.mtime_ns, key.digest],
                |row| row.get::<_, SqlValue>(0),
            )
            .optional()?;
        let Some(document_id) = document else {
            return transaction.commit();
        };
        for (index, row) in encoded {
            transaction.execute(
                "INSERT INTO pages(
                    document_id, form, page_index, text_value, char_count,
                    word_count, word_text, rects, lines, row_bytes
                ) VALUES(?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(document_id, form, page_index) DO UPDATE SET
                    text_value=excluded.text_value,
                    char_count=excluded.char_count,
                    word_count=excluded.word_count,
                    word_text=excluded.word_text,
                    rects=excluded.rects,
                    lines=excluded.lines,
                    row_bytes=excluded.row_bytes",
                params![
                    document_id,
                    form.as_str(),
                    index,
                    row.text_value,
                    row.char_count,
                    row.word_count,
                    row.word_text,
                    row.rects,
                    row.lines,
                    row.row_bytes
                ],
            )?;
        }
        let row_bytes: SqlValue = transaction.query_row(
            "SELECT COALESCE(SUM(row_bytes), 0) FROM pages WHERE document_id=?",
            [&document_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE documents SET row_bytes=?, last_used=? WHERE id=?",
            params![row_bytes, now_ns(), document_id],
        )?;
        let mut total = i128::from(transaction.query_row(
            "SELECT COALESCE(SUM(row_bytes), 0) FROM documents",
            [],
            |row| row.get::<_, i64>(0),
        )?);
        while total > i128::from(self.cap_bytes) {
            let oldest = transaction
                .query_row(
                    "SELECT id, row_bytes FROM documents ORDER BY last_used, id LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, SqlValue>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            let Some((id, bytes)) = oldest else {
                break;
            };
            transaction.execute("DELETE FROM documents WHERE id=?", [&id])?;
            total -= i128::from(bytes);
        }
        transaction.commit()
    }
}

fn connect(path: &Path) -> Option<Connection> {
    if let Some(parent) = path.parent() {
        mkdir_parents(parent).ok()?;
    }
    let connection = Connection::open(path).ok()?;
    connection.busy_timeout(Duration::from_secs(2)).ok()?;
    connection
        .execute_batch(
            "PRAGMA busy_timeout=2000;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=OFF;
             PRAGMA foreign_keys=ON;",
        )
        .ok()?;
    connection.execute_batch(SCHEMA).ok()?;
    Some(connection)
}

/// The `pages` columns of one entry.
struct Encoded<'e> {
    text_value: Option<&'e str>,
    char_count: Option<i64>,
    word_count: Option<i64>,
    word_text: Option<&'e str>,
    rects: Option<Vec<u8>>,
    lines: Option<Vec<u8>>,
    row_bytes: i64,
}

/// `_encode(form, value)`, or `None` for a value of another form.
fn encode(form: Form, entry: &PageEntry) -> Option<Encoded<'_>> {
    let empty = Encoded {
        text_value: None,
        char_count: None,
        word_count: None,
        word_text: None,
        rects: None,
        lines: None,
        row_bytes: ROW_OVERHEAD,
    };
    match (form, entry) {
        (Form::Text, PageEntry::Text(text)) => Some(Encoded {
            text_value: Some(text),
            row_bytes: byte_len(text.len()) + ROW_OVERHEAD,
            ..empty
        }),
        (Form::Count, PageEntry::Count { chars, words }) => Some(Encoded {
            char_count: Some(*chars),
            word_count: Some(*words),
            row_bytes: 16 + ROW_OVERHEAD,
            ..empty
        }),
        (Form::Words, PageEntry::Words(columns)) => {
            let rects: Vec<u8> = columns
                .rects
                .iter()
                .flat_map(|value| value.to_ne_bytes())
                .collect();
            let lines: Vec<u8> = columns
                .lines
                .iter()
                .flat_map(|value| value.to_ne_bytes())
                .collect();
            let row_bytes = byte_len(columns.text.len())
                + byte_len(rects.len())
                + byte_len(lines.len())
                + ROW_OVERHEAD;
            Some(Encoded {
                word_text: Some(&columns.text),
                rects: Some(rects),
                lines: Some(lines),
                row_bytes,
                ..empty
            })
        }
        _ => None,
    }
}

fn byte_len(length: usize) -> i64 {
    i64::try_from(length).unwrap_or(i64::MAX)
}

/// Reads one chunk of a lookup into `values`. A text column that is not UTF-8 fails the
/// whole lookup, as Python's decoder does.
fn read_chunk(
    connection: &Connection,
    document_id: i64,
    form: Form,
    chunk: &[usize],
    wanted: &HashSet<usize>,
    values: &mut HashMap<usize, PageEntry>,
) -> Result<(), ()> {
    let placeholders = vec!["?"; chunk.len()].join(",");
    let sql = format!(
        "SELECT page_index, text_value, char_count, word_count, word_text, rects, lines \
         FROM pages WHERE document_id=? AND form=? AND page_index IN ({placeholders})"
    );
    let mut arguments = Vec::with_capacity(chunk.len() + 2);
    arguments.push(SqlValue::Integer(document_id));
    arguments.push(SqlValue::Text(form.as_str().to_owned()));
    arguments.extend(
        chunk
            .iter()
            .filter_map(|index| i64::try_from(*index).ok())
            .map(SqlValue::Integer),
    );
    let mut statement = connection.prepare(&sql).map_err(drop)?;
    let mut rows = statement
        .query(params_from_iter(arguments.iter()))
        .map_err(drop)?;
    while let Some(row) = rows.next().map_err(drop)? {
        if !decodable(row) {
            return Err(());
        }
        let Ok(ValueRef::Integer(index)) = row.get_ref(0) else {
            continue;
        };
        let Some(index) = usize::try_from(index)
            .ok()
            .filter(|index| wanted.contains(index))
        else {
            continue;
        };
        if let Some(entry) = decode(row, form) {
            values.insert(index, entry);
        }
    }
    Ok(())
}

fn decodable(row: &Row<'_>) -> bool {
    (0..7).all(|column| match row.get_ref(column) {
        Ok(ValueRef::Text(bytes)) => std::str::from_utf8(bytes).is_ok(),
        Ok(_) => true,
        Err(_) => false,
    })
}

/// One row as a `form` value, or `None` when its columns have the wrong shape.
fn decode(row: &Row<'_>, form: Form) -> Option<PageEntry> {
    let text = |column: usize| match row.get_ref(column) {
        Ok(ValueRef::Text(bytes)) => std::str::from_utf8(bytes).ok().map(str::to_owned),
        _ => None,
    };
    let count = |column: usize| match row.get_ref(column) {
        Ok(ValueRef::Integer(value)) if value >= 0 => Some(value),
        _ => None,
    };
    let blob = |column: usize| match row.get_ref(column) {
        Ok(ValueRef::Blob(bytes)) => Some(bytes),
        _ => None,
    };
    match form {
        Form::Text => text(1).map(PageEntry::Text),
        Form::Count => Some(PageEntry::Count {
            chars: count(2)?,
            words: count(3)?,
        }),
        Form::Words => {
            let words = text(4)?;
            let (rects, lines) = (blob(5)?, blob(6)?);
            if !rects.len().is_multiple_of(8) || !lines.len().is_multiple_of(4) {
                return None;
            }
            let rects: Vec<f64> = rects
                .as_chunks::<8>()
                .0
                .iter()
                .map(|bytes| f64::from_ne_bytes(*bytes))
                .collect();
            let lines: Vec<i32> = lines
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| i32::from_ne_bytes(*bytes))
                .collect();
            let word_count = if words.is_empty() {
                0
            } else {
                words.matches('\n').count() + 1
            };
            let consistent = rects.len().is_multiple_of(4)
                && lines.len().is_multiple_of(2)
                && word_count == rects.len() / 4
                && rects.len() / 4 == lines.len() / 2;
            consistent.then_some(PageEntry::Words(WordColumns {
                text: words,
                rects,
                lines,
            }))
        }
    }
}

fn unique(indices: &[usize]) -> Vec<usize> {
    let mut seen = HashSet::with_capacity(indices.len());
    indices
        .iter()
        .copied()
        .filter(|index| seen.insert(*index))
        .collect()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// A page task whose values are the cache's own entries, so cached and computed pages
/// are interchangeable.
pub trait CachedTask: PageTask<Value = PageEntry> {
    /// The open document's page count.
    fn page_count(&self, doc: &Self::Doc) -> usize;
}

/// `_cache_page_values(src, task, arg, form, no_cache, page_spec)`: the page count and the
/// `form` entry of every page `page_spec` selects (every page when absent), in spec order.
///
/// Stored entries are read back; the rest are computed through the page pool and written
/// through as they finish. `no_cache`, an unreadable file, or a disabled cache computes
/// every page instead. A stored page count the open document contradicts discards the
/// document's rows first.
pub fn cached_page_entries<T: CachedTask>(
    task: &T,
    source: &Path,
    form: Form,
    page_spec: Option<&str>,
    no_cache: bool,
    ctx: &Ctx,
) -> Result<(usize, Vec<PageEntry>), GoatError> {
    let tuning = ctx.pool()?;
    let page_spec = page_spec.filter(|spec| !spec.is_empty());
    let select = |page_count: usize| match page_spec {
        Some(spec) => parse_pages(spec, page_count),
        None => Ok((0..page_count).collect()),
    };
    if no_cache {
        return live_entries(task, &tuning, select);
    }
    let key = document_key(source);
    let cache = Cache::open(ctx.cache_path(), ctx.cache_cap_bytes());
    let Some(key) = key.filter(|_| cache.enabled()) else {
        return live_entries(task, &tuning, select);
    };

    let mut stored = cache.document(&key);
    let mut selected = None;
    let mut values = HashMap::new();
    let mut missing = Vec::new();
    if let Some(document) = stored {
        let chosen = select(document.page_count)?;
        values = cache.lookup(&key, form, &chosen);
        missing = unique(&chosen)
            .into_iter()
            .filter(|index| !values.contains_key(index))
            .collect();
        if missing.is_empty() {
            let entries = pick(values, &chosen)?;
            return Ok((document.page_count, entries));
        }
        selected = Some(chosen);
    }

    let mut doc = task.open()?;
    let page_count = task.page_count(&doc);
    if stored.is_some_and(|document| document.page_count != page_count) {
        cache.discard_document(&key);
        stored = None;
        selected = None;
        values.clear();
    }
    let selected = match (page_spec, selected) {
        (None, Some(selected)) => selected,
        _ => select(page_count)?,
    };
    if stored.is_none() {
        missing = unique(&selected);
    }
    let store = CacheStore {
        path: cache.path(),
        cap_bytes: cache.cap_bytes(),
        key: &key,
        source,
        page_count,
        form,
    };
    let computed = map_with_store(task, &mut doc, &missing, &tuning, &store, &cache)?;
    values.extend(missing.into_iter().zip(computed));
    Ok((page_count, pick(values, &selected)?))
}

/// `_live_page_values`: every selected page computed, the cache untouched.
fn live_entries<T: CachedTask>(
    task: &T,
    tuning: &PoolTuning,
    select: impl Fn(usize) -> Result<Vec<usize>, GoatError>,
) -> Result<(usize, Vec<PageEntry>), GoatError> {
    let mut doc = task.open()?;
    let page_count = task.page_count(&doc);
    let selected = select(page_count)?;
    let entries = crate::pool::map_pages(task, &mut doc, &selected, tuning)?;
    Ok((page_count, entries))
}

/// `[values[index] for index in selected]`, moving each value out at its last use.
fn pick(
    mut values: HashMap<usize, PageEntry>,
    selected: &[usize],
) -> Result<Vec<PageEntry>, GoatError> {
    let mut uses: HashMap<usize, usize> = HashMap::new();
    for index in selected {
        *uses.entry(*index).or_default() += 1;
    }
    selected
        .iter()
        .map(|index| {
            let left = uses.entry(*index).or_default();
            *left = left.saturating_sub(1);
            let entry = if *left == 0 {
                values.remove(index)
            } else {
                values.get(index).cloned()
            };
            entry.ok_or_else(|| GoatError::exception("KeyError", index.to_string()))
        })
        .collect()
}

/// Writes computed pages through to the cache; each pool worker opens its own connection.
struct CacheStore<'a> {
    path: &'a Path,
    cap_bytes: u64,
    key: &'a DocumentKey,
    source: &'a Path,
    page_count: usize,
    form: Form,
}

impl Store<PageEntry> for CacheStore<'_> {
    type Handle = Cache;

    fn open(&self) -> Cache {
        Cache::open(self.path, self.cap_bytes)
    }

    fn save(&self, cache: &Cache, indices: &[usize], values: &[PageEntry]) {
        cache.write(
            self.key,
            self.source,
            self.page_count,
            self.form,
            indices.iter().copied().zip(values),
        );
    }
}
