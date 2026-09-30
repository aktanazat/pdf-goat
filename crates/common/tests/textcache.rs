//! The text cache through its public API; corruption is injected with raw SQL.

use std::path::{Path, PathBuf};

use goat_common::textcache::{Cache, DocumentKey, Form, PageEntry, WordColumns};
use rusqlite::Connection;
use tempfile::TempDir;

const ROOMY: u64 = 1 << 20;

fn cache_file() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("temp dir");
    let path = dir.path().join("cache.sqlite");
    (dir, path)
}

fn key(digest: u8) -> DocumentKey {
    DocumentKey {
        size_bytes: 1000,
        mtime_ns: 7,
        digest: vec![digest; 64],
    }
}

fn texts(count: usize) -> Vec<PageEntry> {
    (0..count)
        .map(|index| PageEntry::Text(format!("page {index} text")))
        .collect()
}

fn prime(cache: &Cache, key: &DocumentKey, form: Form, entries: &[PageEntry], page_count: usize) {
    cache.write(
        key,
        Path::new("/doc.pdf"),
        page_count,
        form,
        entries.iter().enumerate(),
    );
}

fn sql(path: &Path, statement: &str) {
    Connection::open(path)
        .expect("open")
        .execute_batch(statement)
        .expect("corrupt");
}

fn count(path: &Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("open");
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

#[test]
fn a_changed_digest_misses_even_with_the_same_size_and_time() {
    let (_dir, path) = cache_file();
    let cache = Cache::open(&path, ROOMY);
    prime(&cache, &key(1), Form::Text, &texts(3), 3);
    let hits = cache.lookup(&key(1), Form::Text, &[0, 1, 2]);
    assert_eq!(hits.get(&1), texts(3).get(1));
    assert_eq!(hits.len(), 3);
    assert_eq!(cache.document(&key(2)), None);
    assert!(cache.lookup(&key(2), Form::Text, &[0, 1, 2]).is_empty());
}

#[test]
fn a_document_its_rows_contradict_is_dropped_and_can_be_primed_again() {
    let corruptions = [
        "UPDATE documents SET page_count = 1",
        "UPDATE documents SET page_count = size_bytes + 1",
        "UPDATE pages SET page_index = 9999 WHERE form = 'text' AND page_index = 2",
    ];
    for corruption in corruptions {
        let (_dir, path) = cache_file();
        let cache = Cache::open(&path, ROOMY);
        prime(&cache, &key(1), Form::Text, &texts(3), 3);
        sql(&path, corruption);
        assert_eq!(cache.document(&key(1)), None, "{corruption}");
        assert!(
            cache.lookup(&key(1), Form::Text, &[0, 1, 2]).is_empty(),
            "{corruption}"
        );

        let sentinel = PageEntry::Text("sentinel".into());
        prime(
            &cache,
            &key(1),
            Form::Text,
            std::slice::from_ref(&sentinel),
            3,
        );
        assert_eq!(
            cache.lookup(&key(1), Form::Text, &[0]).get(&0),
            Some(&sentinel),
            "{corruption}"
        );
    }
}

#[test]
fn an_unreadable_row_is_skipped_and_the_others_served() {
    let words = PageEntry::Words(WordColumns {
        text: "alpha\nbeta".into(),
        rects: vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
        lines: vec![0, 0, 0, 1],
    });
    let cases = [
        (
            Form::Text,
            texts(3),
            "UPDATE pages SET text_value = x'00' WHERE page_index = 1",
        ),
        (
            Form::Count,
            vec![PageEntry::Count { chars: 5, words: 1 }; 3],
            "UPDATE pages SET char_count = -1 WHERE page_index = 1",
        ),
        (
            Form::Words,
            vec![words; 3],
            "UPDATE pages SET word_text = word_text || char(10) || 'orphanmarker' WHERE page_index = 1",
        ),
    ];
    for (form, entries, corruption) in cases {
        let (_dir, path) = cache_file();
        let cache = Cache::open(&path, ROOMY);
        prime(&cache, &key(1), form, &entries, 3);
        sql(&path, corruption);
        let hits = cache.lookup(&key(1), form, &[0, 1, 2]);
        let mut served: Vec<usize> = hits.keys().copied().collect();
        served.sort_unstable();
        assert_eq!(served, [0, 2], "{corruption}");
        assert_eq!(hits.get(&2), entries.get(2), "{corruption}");
    }
}

#[test]
fn a_zero_budget_serves_nothing_from_a_primed_file() {
    let (_dir, path) = cache_file();
    prime(
        &Cache::open(&path, ROOMY),
        &key(1),
        Form::Text,
        &texts(2),
        2,
    );
    let disabled = Cache::open(&path, 0);
    assert!(!disabled.enabled());
    assert_eq!(disabled.document(&key(1)), None);
    assert!(disabled.lookup(&key(1), Form::Text, &[0, 1]).is_empty());
}

#[test]
fn a_budget_smaller_than_one_document_stores_nothing() {
    let (_dir, path) = cache_file();
    let cache = Cache::open(&path, 1);
    prime(
        &cache,
        &key(1),
        Form::Text,
        &vec![PageEntry::Text(String::new()); 4],
        4,
    );
    assert_eq!((count(&path, "documents"), count(&path, "pages")), (0, 0));
}

#[test]
fn a_write_past_the_budget_evicts_the_older_document() {
    let (_dir, path) = cache_file();
    prime(
        &Cache::open(&path, ROOMY),
        &key(1),
        Form::Text,
        &texts(2),
        2,
    );
    let first_bytes: i64 = Connection::open(&path)
        .expect("open")
        .query_row("SELECT row_bytes FROM documents", [], |row| row.get(0))
        .expect("row bytes");
    let cap = first_bytes + (first_bytes / 2).max(1);
    let cache = Cache::open(&path, u64::try_from(cap).expect("positive"));
    prime(&cache, &key(2), Form::Text, &texts(2), 2);

    assert_eq!(cache.document(&key(1)), None);
    assert_eq!(
        cache.document(&key(2)).map(|document| document.page_count),
        Some(2)
    );
    assert_eq!(count(&path, "pages"), 2);
    let stored: i64 = Connection::open(&path)
        .expect("open")
        .query_row("SELECT sum(row_bytes) FROM documents", [], |row| row.get(0))
        .expect("row bytes");
    assert!(stored <= cap, "{stored} > {cap}");
}
