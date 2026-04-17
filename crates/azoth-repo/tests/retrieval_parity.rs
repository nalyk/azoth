//! Sprint 1 verification gate: for a seeded corpus of 20 identifier
//! queries, `FtsLexicalRetrieval` must return a superset of the files
//! `RipgrepLexicalRetrieval` returns. This proves the FTS backend has
//! at least the same recall as the ripgrep baseline over single-token
//! identifier queries — which is the traffic shape Sprint 5 eval and
//! Sprint 7 default-flip will validate at scale.
//!
//! We compare by **file set**, not span count: ripgrep emits one span
//! per matching line while FTS emits one snippet per document, so a
//! literal span-count comparison would punish FTS artificially without
//! any recall signal.

use azoth_core::retrieval::{LexicalRetrieval, RipgrepLexicalRetrieval};
use azoth_repo::{FtsLexicalRetrieval, RepoIndexer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Corpus: one file per identifier. Each file's content includes the
/// identifier plus a small amount of surrounding context so Porter
/// stemming / unicode61 tokenisation has something to chew on. Two
/// files also carry a shared "shared_token" so multi-file queries have
/// something to find.
fn seed_corpus(root: &Path) -> Vec<String> {
    // 20 identifiers — mix of snake_case, camelCase, short, long, doc words.
    let ids = vec![
        "alpha_parser",
        "betaEncoder",
        "gammaDispatcher",
        "delta_bucket",
        "epsilonGate",
        "zetaWriter",
        "eta_sink",
        "thetaCollector",
        "iotaReader",
        "kappaMigrator",
        "lambdaValidator",
        "muWalker",
        "nuRouter",
        "xi_registry",
        "omicronPolicy",
        "piChecker",
        "rhoAuthor",
        "sigmaGuard",
        "tauMapper",
        "upsilon_queue",
    ];

    // `.ignore` rather than `.gitignore` — tempdir is not a git work tree.
    std::fs::write(root.join(".ignore"), "skip.log\n").unwrap();

    for (i, id) in ids.iter().enumerate() {
        let filename = format!("m{:02}.rs", i);
        let body =
            format!("// module {i}\nfn {id}() {{ shared_token(); }}\nfn helper_{i}() {{}}\n");
        std::fs::write(root.join(filename), body).unwrap();
    }

    // A junk file excluded by the walker — must appear in neither
    // backend's result set.
    std::fs::write(root.join("skip.log"), "alpha_parser betaEncoder\n").unwrap();

    ids.into_iter().map(String::from).collect()
}

fn file_set(hits: &[azoth_core::retrieval::Span]) -> HashSet<String> {
    hits.iter()
        .map(|s| {
            let p = PathBuf::from(&s.path);
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(s.path.clone())
        })
        .collect()
}

#[tokio::test]
async fn fts_is_superset_of_ripgrep_for_20_identifier_queries() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let ids = seed_corpus(&repo);

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let stats = idx.reindex_incremental().await.unwrap();
    assert_eq!(
        stats.inserted as usize,
        ids.len(),
        "indexer must capture every seeded .rs file ({} expected, got {})",
        ids.len(),
        stats.inserted
    );

    let fts = FtsLexicalRetrieval::with_connection(idx.connection());
    let rg = RipgrepLexicalRetrieval { root: repo.clone() };

    let limit = ids.len() + 5;
    for id in &ids {
        let rg_files = file_set(&rg.search(id, limit).await.unwrap());
        let fts_files = file_set(&fts.search(id, limit).await.unwrap());
        assert!(
            fts_files.is_superset(&rg_files),
            "query={id:?} — FTS lost recall\n  ripgrep={rg_files:?}\n  fts    ={fts_files:?}"
        );
        assert!(
            !rg_files.contains("skip.log"),
            "precondition: ripgrep must have honored .ignore too"
        );
        assert!(
            !fts_files.contains("skip.log"),
            ".ignore filtering must carry through to the indexed set"
        );
    }
}

#[tokio::test]
async fn fts_finds_shared_token_across_many_files() {
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let ids = seed_corpus(&repo);

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    let fts = FtsLexicalRetrieval::with_connection(idx.connection());

    // Every seeded file contains `shared_token()`.
    let hits = fts.search("shared_token", ids.len() + 5).await.unwrap();
    let files = file_set(&hits);
    assert_eq!(
        files.len(),
        ids.len(),
        "FTS must find the shared token in every seeded file, got {files:?}"
    );
}

#[tokio::test]
async fn snippets_are_byte_stable_across_reindex() {
    // Risk #1 in the v2 plan: FTS5 snippet() non-determinism would
    // destroy Anthropic prompt-cache hit rate because the kernel hashes
    // the evidence lane. This test reindexes twice (same content, same
    // mtime) and asserts identical snippets. If this ever fails,
    // `FtsLexicalRetrieval::normalize_snippet` needs a harder scrub.
    let td = TempDir::new().unwrap();
    let db = td.path().join("mirror.sqlite");
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_corpus(&repo);

    let idx = RepoIndexer::open(&db, &repo).unwrap();
    let _ = idx.reindex_incremental().await.unwrap();
    let fts = FtsLexicalRetrieval::with_connection(idx.connection());
    let first: Vec<(String, String)> = fts
        .search("shared_token", 25)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.path, s.snippet))
        .collect();

    // Second reindex against unchanged files — mtime-gated, so no row
    // churn; FTS index untouched.
    let stats = idx.reindex_incremental().await.unwrap();
    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.updated, 0);

    let second: Vec<(String, String)> = fts
        .search("shared_token", 25)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.path, s.snippet))
        .collect();

    assert_eq!(
        first, second,
        "snippet or ordering drift across reindexes breaks cache keys"
    );
}
