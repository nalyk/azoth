//! `FtsLexicalRetrieval` — SQLite FTS5 implementation of
//! `azoth_core::retrieval::LexicalRetrieval`. Drop-in sibling of
//! `RipgrepLexicalRetrieval` — Sprint 7 flips the default backend; Sprint 1
//! ships the engine behind the `AZOTH_LEXICAL_BACKEND=fts` opt-in.
//!
//! ## Query semantics
//!
//! The raw query string is wrapped into an FTS5 **phrase** (`"..."`) with
//! inner quotes doubled, so user input is treated as a literal token-seq
//! rather than interpreted as an FTS5 expression. This keeps semantics
//! predictable (matches `RipgrepLexicalRetrieval::search`'s `fixed_strings`
//! mode) and defangs tainted input — an attacker who controls `q` cannot
//! inject FTS5 operators like `MATCH`/`NEAR`/`OR`.
//!
//! ## Snippet normalisation (risk #1 — cache-prefix stability)
//!
//! `ContextKernel::compile()` hashes the evidence lane for Anthropic
//! prompt-cache keying. If snippets drift byte-for-byte between reindexes
//! (whitespace wobble, highlight markers from default `snippet()`), the
//! cache hit rate collapses. We therefore:
//!   1. Pass empty open/close markers to `snippet()` so no `<b>…</b>` or
//!      other highlight artefacts leak in.
//!   2. Run the snippet through `normalize_snippet` — collapse all
//!      whitespace runs to single spaces and trim. Result is byte-stable
//!      across reindexes of the same content.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use azoth_core::event_store::migrations;
use azoth_core::retrieval::{LexicalRetrieval, RetrievalError, Span};
use rusqlite::{params, Connection};

use crate::indexer::IndexerError;

/// FTS5-backed lexical retrieval. See module docs for query semantics
/// and cache-stability guarantees.
pub struct FtsLexicalRetrieval {
    conn: Arc<Mutex<Connection>>,
    /// Size (in bytes) of the context window around the matching token
    /// that FTS5 `snippet()` emits. Chosen as a balance between useful
    /// preview and packet-token budget.
    snippet_tokens: u32,
}

impl FtsLexicalRetrieval {
    /// Open the mirror DB at `db_path` and run migrations. Use this in
    /// a single-component setup where the retrieval owns its own
    /// connection.
    pub fn open<P: AsRef<Path>>(db_path: P) -> Result<Self, IndexerError> {
        let mut conn = Connection::open(db_path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            snippet_tokens: 16,
        })
    }

    /// Wrap an already-open connection — typically the one owned by
    /// `RepoIndexer` so both components share a single file handle.
    pub fn with_connection(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            snippet_tokens: 16,
        }
    }

    pub fn set_snippet_tokens(&mut self, n: u32) {
        self.snippet_tokens = n;
    }
}

#[async_trait]
impl LexicalRetrieval for FtsLexicalRetrieval {
    async fn search(&self, q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError> {
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = Arc::clone(&self.conn);
        let q_owned = q.to_string();
        let snippet_tokens = self.snippet_tokens;
        tokio::task::spawn_blocking(move || {
            fts_search_blocking(&conn, &q_owned, limit, snippet_tokens)
        })
        .await
        .map_err(|e| RetrievalError::Other(format!("join: {e}")))?
    }
}

fn fts_search_blocking(
    conn: &Arc<Mutex<Connection>>,
    q: &str,
    limit: usize,
    snippet_tokens: u32,
) -> Result<Vec<Span>, RetrievalError> {
    let guard = conn
        .lock()
        .map_err(|e| RetrievalError::Other(format!("conn mutex poisoned: {e}")))?;

    let phrase = fts5_phrase(q);

    // snippet(tbl, col_index, open, close, ellipsis, ntoken).
    // col_index=1 corresponds to `content` (0-indexed after rowid).
    // CP-3: JOIN against `documents.mtime_nanos` so every returned
    // Span carries an `source_mtime` (seconds since epoch). Falls
    // back to None (via NULL → Option) when the content-sync trigger
    // lags behind the FTS view.
    let sql = format!(
        "SELECT documents_fts.path, \
                snippet(documents_fts, 1, '', '', '…', {snippet_tokens}), \
                documents.mtime \
         FROM documents_fts \
         LEFT JOIN documents ON documents.path = documents_fts.path \
         WHERE documents_fts MATCH ?1 \
         ORDER BY rank \
         LIMIT ?2"
    );

    let mut stmt = guard
        .prepare(&sql)
        .map_err(|e| RetrievalError::Other(format!("prepare: {e}")))?;
    let rows = stmt
        .query_map(params![phrase, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(|e| RetrievalError::Other(format!("query: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        let (path, snippet, mtime_nanos) =
            row.map_err(|e| RetrievalError::Other(format!("row: {e}")))?;
        // `documents.mtime` is nanoseconds since epoch (see indexer).
        // Seconds is plenty of precision for freshness-decay math;
        // downcast when present.
        let source_mtime = mtime_nanos
            .filter(|ns| *ns > 0)
            .map(|ns| (ns / 1_000_000_000) as u64);
        out.push(Span {
            path,
            // FTS5 operates at document granularity — line numbers would
            // require a second pass over the raw content. Sprint 2
            // symbol extraction supplies line-precise spans; for Sprint
            // 1 we leave zero as the "FTS-doesn't-know" sentinel.
            start_line: 0,
            end_line: 0,
            snippet: normalize_snippet(&snippet),
            source_mtime,
        });
    }
    Ok(out)
}

/// Wrap `q` as an FTS5 phrase and escape embedded double quotes by
/// doubling. FTS5 phrase syntax: `"a b c"` matches `a` then `b` then
/// `c`. This keeps semantics literal and prevents operator injection.
fn fts5_phrase(q: &str) -> String {
    let mut s = String::with_capacity(q.len() + 2);
    s.push('"');
    for ch in q.chars() {
        if ch == '"' {
            s.push('"');
            s.push('"');
        } else {
            s.push(ch);
        }
    }
    s.push('"');
    s
}

/// Collapse whitespace runs to single spaces and trim. See module docs
/// §"Snippet normalisation" for why this is load-bearing.
fn normalize_snippet(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::RepoIndexer;
    use tempfile::TempDir;

    async fn seeded_retrieval() -> (TempDir, FtsLexicalRetrieval) {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".ignore"), "skip.log\n").unwrap();
        std::fs::write(
            repo.join("alpha.rs"),
            "fn compile(query: &str) {}\nfn dispatch(x: u32) {}\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("beta.md"),
            "# Doc\n\ncompile queries into packets\n",
        )
        .unwrap();
        std::fs::write(repo.join("skip.log"), "compile here too\n").unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let _ = idx.reindex_incremental().await.unwrap();
        let fts = FtsLexicalRetrieval::with_connection(idx.connection());
        (td, fts)
    }

    #[tokio::test]
    async fn fts_finds_token_across_both_files() {
        let (_td, fts) = seeded_retrieval().await;
        let hits = fts.search("compile", 10).await.unwrap();
        let paths: Vec<&str> = hits.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"alpha.rs"), "{paths:?}");
        assert!(paths.contains(&"beta.md"), "{paths:?}");
    }

    #[tokio::test]
    async fn fts_respects_dot_ignore_via_indexer_scope() {
        let (_td, fts) = seeded_retrieval().await;
        let hits = fts.search("compile", 10).await.unwrap();
        assert!(
            !hits.iter().any(|s| s.path == "skip.log"),
            "skip.log was walker-filtered, must not appear: {hits:?}"
        );
    }

    #[tokio::test]
    async fn fts_limit_bounds_result_count() {
        let (_td, fts) = seeded_retrieval().await;
        let hits = fts.search("compile", 1).await.unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn fts_empty_query_returns_empty() {
        let (_td, fts) = seeded_retrieval().await;
        let hits = fts.search("", 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn fts_injection_attempt_is_literal() {
        // If we didn't phrase-wrap, the FTS5 column filter `content:`
        // syntax would succeed and this query would look different.
        // Under phrase-wrapping it's just a literal token stream that
        // won't match the seeded content.
        let (_td, fts) = seeded_retrieval().await;
        let hits = fts.search("content: compile OR *", 10).await.unwrap();
        // No document contains this literal phrase → zero hits, no error.
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[tokio::test]
    async fn snippet_is_byte_stable_across_requery() {
        let (_td, fts) = seeded_retrieval().await;
        let a = fts.search("compile", 5).await.unwrap();
        let b = fts.search("compile", 5).await.unwrap();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.path, y.path);
            assert_eq!(x.snippet, y.snippet, "snippet drift breaks cache key");
        }
    }

    #[test]
    fn normalize_snippet_collapses_runs() {
        assert_eq!(normalize_snippet("  a\n\n b\tc  "), "a b c");
        assert_eq!(normalize_snippet(""), "");
    }

    #[test]
    fn fts5_phrase_escapes_quotes() {
        assert_eq!(fts5_phrase("foo"), "\"foo\"");
        assert_eq!(fts5_phrase("a\"b"), "\"a\"\"b\"");
    }
}
