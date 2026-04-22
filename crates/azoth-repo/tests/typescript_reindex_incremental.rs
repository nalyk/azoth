//! PR 2.1-C — end-to-end indexer tests for TypeScript.
//!
//! Sibling to `python_reindex_incremental` + `rust_reindex_incremental`
//! coverage: proves that `.ts` AND `.tsx` files flow through
//! `RepoIndexer::reindex_incremental` (mtime gating, malformed-input
//! tolerance, no-panic discipline, Phase-5 reconcile preserves wired
//! rows). The `.ts`/`.tsx` split exercises the path-aware parser cache
//! keyed by `ParserKey::TypeScriptTs` / `TypeScriptTsx` — a single
//! indexer instance must keep two distinct parsers live without
//! contaminating trees.

use azoth_repo::indexer::RepoIndexer;
use tempfile::TempDir;

#[tokio::test]
async fn typescript_file_extraction_is_mtime_gated() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("mod.ts"),
        "export function alpha(): number { return 1; }\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let s1 = idx.reindex_incremental().await.unwrap();
    assert_eq!(s1.inserted, 1, "file inserted on first pass");
    assert!(
        s1.symbols_extracted >= 1,
        "TypeScript grammar wired — at least `alpha` must be extracted (got {})",
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
async fn tsx_file_routes_through_tsx_parser() {
    // `.tsx` paths must hash to `ParserKey::TypeScriptTsx` and land
    // on the TSX grammar. A `.ts` grammar would error on JSX syntax;
    // this test's success is end-to-end evidence the indexer picks
    // the right flavour.
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("component.tsx"),
        "export function Greeting({ name }: { name: string }) { return <div>{name}</div>; }\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let stats = idx.reindex_incremental().await.unwrap();
    assert_eq!(stats.inserted, 1);
    assert!(
        stats.symbols_extracted >= 1,
        "TSX grammar must extract at least `Greeting` (got {})",
        stats.symbols_extracted,
    );
}

#[tokio::test]
async fn mixed_ts_and_tsx_files_coexist_in_one_pass() {
    // Codex P2 on PR #19 motivated the `ParserKey` split from
    // `Language`: if the cache keyed on Language alone, the first
    // TypeScript file in the pass would pin the parser flavour and
    // a later file of the other flavour would either error out
    // (grammar mismatch on JSX) or silently drop symbols.
    // End-to-end evidence the split works: index one of each and
    // verify both produce symbols in the same pass.
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("plain.ts"),
        "export function alpha(): number { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("widget.tsx"),
        "export function Widget() { return <span>hi</span>; }\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let stats = idx.reindex_incremental().await.unwrap();
    assert_eq!(stats.inserted, 2, "both files indexed");
    assert!(
        stats.symbols_extracted >= 2,
        "both flavours must produce symbols in one pass (got {})",
        stats.symbols_extracted,
    );
}

#[tokio::test]
async fn malformed_typescript_doesnt_abort_reindex() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("bad.ts"),
        "function ok() {}\n~~garbage~~\nfunction also() {}\n",
    )
    .unwrap();
    std::fs::write(repo.join("good.ts"), "interface Shape { area(): number }\n").unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let s = idx.reindex_incremental().await.unwrap();
    assert_eq!(s.inserted, 2, "both typescript files indexed");
    assert!(
        s.symbols_extracted >= 2,
        "at least `ok` + `Shape` extracted (got {})",
        s.symbols_extracted,
    );
}

#[tokio::test]
async fn typescript_symbols_survive_reconcile() {
    // Regression guard mirroring `python_symbols_survive_reconcile`.
    // If a future refactor accidentally drops TypeScript from
    // `all_extractor_wired()`, Phase-5 reconcile would purge legit
    // TS rows on every reindex pass. This test catches that before
    // retrieval rots.
    use rusqlite::Connection;

    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("mod.ts"),
        "export function alpha(): number { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("widget.tsx"),
        "export function Widget() { return <span>hi</span>; }\n",
    )
    .unwrap();

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    drop(idx);

    let probe = Connection::open(&db).unwrap();
    let ts_rows: i64 = probe
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE language = 'typescript'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        ts_rows >= 2,
        "TypeScript symbols must survive Phase-5 reconcile across both \
         `.ts` and `.tsx` (got {ts_rows})",
    );
}
