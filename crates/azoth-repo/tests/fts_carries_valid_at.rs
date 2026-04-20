//! Chronon CP-3 — FTS retrieval surfaces `source_mtime` on every Span.
//!
//! Indexes a tiny repo, runs an FTS query, asserts the returned span
//! carries a non-None `source_mtime` derived from the document's
//! mtime column. The number itself must be a plausible Unix epoch
//! second (greater than 1_600_000_000, i.e. post-2020). This is the
//! seam CP-3 evidence bitemporality depends on.

use azoth_core::retrieval::{LexicalRetrieval, Span};
use azoth_repo::fts::FtsLexicalRetrieval;
use azoth_repo::indexer::RepoIndexer;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread")]
async fn fts_search_populates_source_mtime_from_documents_table() {
    let dir = tempdir().unwrap();
    let repo = dir.path().to_path_buf();

    // Seed a single file with a searchable term.
    let foo = repo.join("foo.rs");
    std::fs::write(&foo, "fn chronon_plane_marker() {}\n").unwrap();

    let state = repo.join(".azoth/state.sqlite");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();

    let indexer = RepoIndexer::open(&state, &repo).expect("indexer opens");
    indexer.reindex_incremental().await.expect("indexer runs");

    let retrieval = FtsLexicalRetrieval::open(&state).expect("fts opens");
    let hits: Vec<Span> = retrieval
        .search("chronon_plane_marker", 5)
        .await
        .expect("search ok");
    assert!(!hits.is_empty(), "fts found the seeded marker");

    let mtime = hits[0]
        .source_mtime
        .expect("CP-3: FTS hit must surface source_mtime");
    // Post-2020 sanity check — indexing just happened, so this
    // must be "recent" in any reasonable sense.
    assert!(
        mtime > 1_600_000_000,
        "source_mtime should be a plausible epoch second, got {mtime}"
    );
}
