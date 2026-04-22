//! PR 2.1-D — end-to-end indexer tests for Go.
//!
//! Sibling to `rust_reindex_incremental` / `python_reindex_incremental`
//! / `typescript_reindex_incremental`: proves `.go` files flow through
//! `RepoIndexer::reindex_incremental` identically (mtime gating,
//! malformed-input tolerance, no-panic discipline, Phase-5 reconcile
//! survival now that `all_extractor_wired()` contains `Language::Go`).

use azoth_repo::indexer::RepoIndexer;
use tempfile::TempDir;

#[tokio::test]
async fn go_file_extraction_is_mtime_gated() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("main.go"),
        "package main\nfunc alpha() int { return 1 }\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let s1 = idx.reindex_incremental().await.unwrap();
    assert_eq!(s1.inserted, 1, "file inserted on first pass");
    assert!(
        s1.symbols_extracted >= 2,
        "Go grammar wired — at least `main` package + `alpha` function extracted (got {})",
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
async fn malformed_go_doesnt_abort_reindex() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("bad.go"),
        "package main\nfunc ok() {}\n~~garbage~~\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("good.go"),
        "package main\ntype W struct{}\nfunc (w W) M() {}\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let s = idx.reindex_incremental().await.unwrap();
    assert_eq!(s.inserted, 2, "both go files indexed");
    assert!(
        s.symbols_extracted >= 3,
        "at least `main` package + `W` + `M` extracted across the two files (got {})",
        s.symbols_extracted,
    );
}

#[tokio::test]
async fn go_symbols_survive_reconcile() {
    // Regression guard: the Phase-5 reconcile pass (added in PR-A
    // round-10) purges rows whose language is not in
    // `all_extractor_wired()`. PR-D widened that set to include Go,
    // so Go rows must NOT be purged by reconcile even when the file
    // is mtime-unchanged across passes. If someone accidentally
    // removes Go from `all_extractor_wired()` in a future refactor,
    // this test catches it before retrieval rots.
    //
    // Same rusqlite-probe pattern as the Python / TS siblings —
    // `RepoIndexer::conn` is private, and the CLAUDE.md invariant
    // (§Architecture Constraints) prescribes a separate Connection
    // per subsystem anyway.
    use rusqlite::Connection;

    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("mod.go"),
        "package main\nfunc alpha() int { return 1 }\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    drop(idx);

    let probe = Connection::open(&db).unwrap();
    let go_rows: i64 = probe
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE path = 'mod.go' AND language = 'go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        go_rows >= 1,
        "Go symbols must survive Phase-5 reconcile (got {go_rows})",
    );
}
