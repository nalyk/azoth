//! PR 2.1-B — end-to-end indexer tests for Python.
//!
//! Sibling to `rust_reindex_incremental` coverage: proves that Python
//! files flow through `RepoIndexer::reindex_incremental` identically
//! (mtime gating, malformed-input tolerance, no-panic discipline).

use azoth_repo::indexer::RepoIndexer;
use tempfile::TempDir;

#[tokio::test]
async fn python_file_extraction_is_mtime_gated() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("mod.py"), "def alpha():\n    return 1\n").unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let s1 = idx.reindex_incremental().await.unwrap();
    assert_eq!(s1.inserted, 1, "file inserted on first pass");
    assert!(
        s1.symbols_extracted >= 1,
        "Python grammar wired — at least `alpha` must be extracted (got {})",
        s1.symbols_extracted,
    );

    // Second pass with no disk change — extract_and_store must not
    // run; `symbols_extracted` on this pass stays zero.
    let s2 = idx.reindex_incremental().await.unwrap();
    assert_eq!(s2.skipped_unchanged, 1, "mtime-unchanged file was skipped");
    assert_eq!(
        s2.symbols_extracted, 0,
        "mtime-gated file must not re-enter extract_and_store",
    );
}

#[tokio::test]
async fn malformed_python_doesnt_abort_reindex() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("bad.py"), "def ok():\n    pass\n\n~~garbage~~\n").unwrap();
    std::fs::write(repo.join("good.py"), "class C:\n    pass\n").unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let s = idx.reindex_incremental().await.unwrap();
    assert_eq!(s.inserted, 2, "both python files indexed");
    assert!(
        s.symbols_extracted >= 2,
        "at least `ok` + `C` extracted (got {})",
        s.symbols_extracted,
    );
}

#[tokio::test]
async fn python_symbols_survive_reconcile() {
    // Regression guard: the Phase-5 reconcile pass (added in PR-A
    // round-10) purges rows whose language is not in
    // `all_extractor_wired()`. PR-B widened that set to include
    // Python, so Python rows must NOT be purged by reconcile even
    // when the file is mtime-unchanged across passes. If someone
    // accidentally removes Python from `all_extractor_wired()` in a
    // future refactor, this test catches it before retrieval rots.
    //
    // We assert via a separate rusqlite Connection to the mirror DB
    // — `RepoIndexer`'s `conn` field is private, and opening a
    // second Connection is the same pattern the CLAUDE.md invariant
    // (§Architecture Constraints: each subsystem opens its own
    // Connection) already prescribes.
    use rusqlite::Connection;

    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("mod.py"), "def alpha():\n    return 1\n").unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    drop(idx);

    let probe = Connection::open(&db).unwrap();
    let py_rows: i64 = probe
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE path = 'mod.py' AND language = 'python'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        py_rows >= 1,
        "Python symbols must survive Phase-5 reconcile (got {py_rows})",
    );
}
