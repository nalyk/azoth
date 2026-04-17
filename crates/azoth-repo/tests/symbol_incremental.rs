//! Sprint 2 — symbol extraction respects Sprint 1's mtime gating.
//!
//! An unchanged file must not trigger re-extraction on a second
//! `reindex_incremental` pass; when it IS edited, the Phase-4 writer
//! drops the old symbol rows and inserts fresh ones so by_name returns
//! the post-edit identifiers, not the pre-edit ones.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use azoth_core::retrieval::SymbolRetrieval;
use azoth_repo::{RepoIndexer, SqliteSymbolIndex};
use tempfile::TempDir;

fn write_rust(root: &Path, rel: &str, body: &str) {
    std::fs::write(root.join(rel), body).unwrap();
}

#[tokio::test]
async fn mtime_unchanged_file_skips_symbol_reextraction() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    write_rust(&repo, "a.rs", "pub fn alpha() {}\n");
    let db = td.path().join("state.sqlite");

    let indexer = RepoIndexer::open(&db, &repo).unwrap();

    let first = indexer.reindex_incremental().await.unwrap();
    assert_eq!(first.inserted, 1);
    assert!(
        first.symbols_extracted >= 1,
        "first pass must extract symbols"
    );

    // Second pass with no filesystem change. mtime gate must short-
    // circuit to zero writes — documents unchanged AND zero symbols
    // re-extracted.
    let second = indexer.reindex_incremental().await.unwrap();
    assert_eq!(second.inserted, 0);
    assert_eq!(second.updated, 0);
    assert_eq!(
        second.skipped_unchanged, 1,
        "mtime-gate must short-circuit unchanged .rs file"
    );
    assert_eq!(
        second.symbols_extracted, 0,
        "unchanged file must not re-extract symbols: {second:?}"
    );
}

#[tokio::test]
async fn edit_replaces_prior_symbols_for_same_path() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let file = repo.join("a.rs");
    write_rust(&repo, "a.rs", "pub fn first_name() {}\n");
    let db = td.path().join("state.sqlite");

    let indexer = RepoIndexer::open(&db, &repo).unwrap();
    indexer.reindex_incremental().await.unwrap();

    // Rename the symbol and pin mtime to a clearly-later value so the
    // mtime-gate fires even on filesystems with coarse second
    // resolution.
    std::fs::write(&file, "pub fn second_name() {}\n").unwrap();
    let new_mtime = SystemTime::now()
        .checked_add(Duration::from_secs(3600))
        .unwrap();
    OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(new_mtime)
        .unwrap();

    let after = indexer.reindex_incremental().await.unwrap();
    assert_eq!(after.updated, 1);
    assert!(after.symbols_extracted >= 1);

    let sym_conn = rusqlite::Connection::open(&db).unwrap();
    let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(sym_conn)));

    assert!(
        idx.by_name("first_name", 5).await.unwrap().is_empty(),
        "pre-edit name must be purged when file is rewritten"
    );
    assert_eq!(
        idx.by_name("second_name", 5).await.unwrap().len(),
        1,
        "post-edit name must be the new row"
    );
}

#[tokio::test]
async fn deleted_file_purges_its_symbol_rows() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    write_rust(&repo, "doomed.rs", "pub fn goner() {}\n");
    let db = td.path().join("state.sqlite");

    let indexer = RepoIndexer::open(&db, &repo).unwrap();
    indexer.reindex_incremental().await.unwrap();

    // Drop the only file and reindex — the Phase-4 anti-join purge
    // for symbols must catch it.
    std::fs::remove_file(repo.join("doomed.rs")).unwrap();
    let stats = indexer.reindex_incremental().await.unwrap();
    assert_eq!(stats.deleted, 1);

    let sym_conn = rusqlite::Connection::open(&db).unwrap();
    let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(sym_conn)));
    assert!(
        idx.by_name("goner", 5).await.unwrap().is_empty(),
        "symbols for deleted files must not linger"
    );
}
