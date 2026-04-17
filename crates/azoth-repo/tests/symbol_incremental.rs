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
async fn backfill_populates_symbols_for_upgrade_from_v2_to_v3() {
    // Codex P2 #4 (PR #6) — when a v2 mirror is opened under a v2
    // binary shipping Sprint 2, the migrator creates the `symbols`
    // table but every existing `documents` row has a mtime matching
    // the on-disk file. Phase 2 triage classifies those files as
    // "unchanged", so the per-file extractor loop never touches them.
    // Without the backfill pass, `by_name` / `enclosing` would return
    // nothing until each file is manually edited.
    //
    // The test simulates that post-upgrade state by seeding a freshly-
    // migrated DB with a pre-populated documents row whose mtime
    // matches the on-disk file, then running reindex and asserting
    // symbols were extracted.
    use std::time::UNIX_EPOCH;
    let td = TempDir::new().unwrap();
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let rs = repo.join("legacy.rs");
    let content = "pub fn legacy_fn() {}\n";
    std::fs::write(&rs, content).unwrap();
    let mtime_ns = rs
        .metadata()
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .min(i64::MAX as u128) as i64;

    let db = td.path().join("state.sqlite");
    // Open once to run migrations (creates symbols table via m0003).
    let indexer = RepoIndexer::open(&db, &repo).unwrap();

    // Seed a documents row directly — simulates "v2 mirror opened by
    // v3 binary; symbols is empty, documents is pre-populated".
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO documents (path, mtime, language, content) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["legacy.rs", mtime_ns, "rust", content],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "seed precondition: symbols must start empty");
    }

    // Reindex. The file's mtime matches documents.mtime, so Phase 2
    // classifies it as unchanged; the per-file extractor loop never
    // touches it. The backfill must pick up the slack.
    let stats = indexer.reindex_incremental().await.unwrap();
    assert_eq!(
        stats.skipped_unchanged, 1,
        "pre-seeded doc row must register as mtime-unchanged: {stats:?}"
    );
    assert!(
        stats.symbols_extracted >= 1,
        "backfill must extract symbols for unchanged .rs files on first post-upgrade pass: {stats:?}"
    );

    let sym_conn = rusqlite::Connection::open(&db).unwrap();
    let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(sym_conn)));
    let hits = idx.by_name("legacy_fn", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "by_name must resolve the backfilled symbol");
    assert_eq!(hits[0].path, "legacy.rs");

    // Second pass must not re-extract (backfill idempotent — symbols
    // already present for the path).
    let stats2 = indexer.reindex_incremental().await.unwrap();
    assert_eq!(
        stats2.symbols_extracted, 0,
        "backfill must be idempotent: {stats2:?}"
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
