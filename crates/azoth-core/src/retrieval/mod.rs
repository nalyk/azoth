//! Retrieval traits. Split from day one (HIGH-2): lexical ships in v1,
//! graph is a trait-only stub so v2 can land without touching signatures.

pub mod config;
pub mod symbol;
pub use config::{CoEditConfig, LexicalBackend, RetrievalConfig, RetrievalMode};
pub use symbol::{NullSymbolRetrieval, Symbol, SymbolId, SymbolKind, SymbolRetrieval};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RetrievalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef(pub String);

/// Sprint 3 additive extension: edges gain a `weight`. Old JSONL
/// sessions (pre-v2) have no `weight` field and deserialize as
/// `1.0` via `#[serde(default)]`.
///
/// `Eq`/`Hash` were intentional before `weight` existed (edges went
/// through `HashSet` for dedupe). `f32` has no `Eq`/`Hash`, so both
/// derives are dropped — the Sprint-3 `CoEditGraphRetrieval` does
/// not need them and a future consumer that does can hash a
/// projection it owns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub kind: String,
    #[serde(default = "one_f32")]
    pub weight: f32,
}

/// Default for `Edge::weight` when loading pre-v2 JSONL that has
/// no `weight` field. Named so it shows up in backtraces as the
/// explicit "old sessions default" seam rather than an anonymous
/// closure.
fn one_f32() -> f32 {
    1.0
}

#[async_trait]
pub trait LexicalRetrieval: Send + Sync {
    async fn search(&self, q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError>;
}

#[async_trait]
pub trait GraphRetrieval: Send + Sync {
    async fn neighbors(
        &self,
        node: NodeRef,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<(NodeRef, Edge)>, RetrievalError>;
}

/// v1 placeholder. Returns nothing. Graph retrieval lands in v2 without
/// touching this signature.
pub struct NullGraphRetrieval;

#[async_trait]
impl GraphRetrieval for NullGraphRetrieval {
    async fn neighbors(
        &self,
        _node: NodeRef,
        _depth: usize,
        _limit: usize,
    ) -> Result<Vec<(NodeRef, Edge)>, RetrievalError> {
        Ok(Vec::new())
    }
}

/// Ripgrep-backed `LexicalRetrieval` — walks `root` via `ignore::WalkBuilder`
/// (honors `.gitignore`/`.ignore`/hidden-file conventions) and searches each
/// file with `grep-searcher` + `grep-regex` in `fixed_strings` mode so `q` is
/// treated as a literal. True SQLite FTS5 lands later with the mirror-DB;
/// this impl keeps the trait signature stable and swaps the engine.
pub struct RipgrepLexicalRetrieval {
    pub root: std::path::PathBuf,
}

#[async_trait]
impl LexicalRetrieval for RipgrepLexicalRetrieval {
    async fn search(&self, q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError> {
        let root = self.root.clone();
        let q = q.to_string();
        tokio::task::spawn_blocking(move || ripgrep_scan(&root, &q, limit))
            .await
            .map_err(|e| RetrievalError::Other(e.to_string()))?
    }
}

fn ripgrep_scan(
    root: &std::path::Path,
    q: &str,
    limit: usize,
) -> Result<Vec<Span>, RetrievalError> {
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::SearcherBuilder;
    use ignore::WalkBuilder;

    if q.is_empty() || limit == 0 || !root.exists() {
        return Ok(Vec::new());
    }

    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(true)
        .build(q)
        .map_err(|e| RetrievalError::Other(format!("matcher: {e}")))?;

    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut out: Vec<Span> = Vec::new();

    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .parents(true)
        .build();

    for dent in walker {
        if out.len() >= limit {
            break;
        }
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue,
        };
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path().to_path_buf();
        let path_str = path.display().to_string();

        let mut sink = Collector {
            path: path_str,
            out: &mut out,
            limit,
        };
        // Per-file errors (binary, unreadable) must not abort the whole scan.
        let _ = searcher.search_path(&matcher, &path, &mut sink);
    }

    Ok(out)
}

struct Collector<'a> {
    path: String,
    out: &'a mut Vec<Span>,
    limit: usize,
}

impl<'a> grep_searcher::Sink for Collector<'a> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        m: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        if self.out.len() >= self.limit {
            return Ok(false);
        }
        let line_num = m.line_number().unwrap_or(0) as usize;
        let text = std::str::from_utf8(m.bytes()).unwrap_or("");
        let snippet: String = text
            .trim_end_matches(['\n', '\r'])
            .trim()
            .chars()
            .take(200)
            .collect();
        self.out.push(Span {
            path: self.path.clone(),
            start_line: line_num,
            end_line: line_num,
            snippet,
        });
        Ok(self.out.len() < self.limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn edge_without_weight_field_deserialises_to_one() {
        // Sprint 3 additive: pre-v2 JSONL has `{"kind": "..."}` with
        // no `weight` field. Must still parse — Sprint 4's composite
        // collector is allowed to treat `1.0` as the pre-v2 default.
        let old: Edge = serde_json::from_str(r#"{"kind":"co_edit"}"#).unwrap();
        assert_eq!(old.kind, "co_edit");
        assert!((old.weight - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn edge_round_trips_with_weight() {
        let e = Edge {
            kind: "co_edit".into(),
            weight: 3.5,
        };
        let wire = serde_json::to_string(&e).unwrap();
        let back: Edge = serde_json::from_str(&wire).unwrap();
        assert_eq!(e, back);
    }

    fn seed_repo() -> (TempDir, std::path::PathBuf) {
        let td = TempDir::new().expect("tempdir");
        let root = td.path().to_path_buf();
        // `.ignore` is the non-git-specific file respected by `ignore::Walk`
        // even when the tempdir is not inside a git work tree. `.gitignore`
        // alone would be skipped here.
        std::fs::write(root.join(".ignore"), "ignored.log\n").unwrap();
        std::fs::write(
            root.join("foo.rs"),
            "fn main() {}\nlet needle = 1;\nprintln!(\"bye\");\n",
        )
        .unwrap();
        std::fs::write(root.join("bar.md"), "# title\n\nsome needle in markdown\n").unwrap();
        std::fs::write(root.join("ignored.log"), "needle in junk\n").unwrap();
        (td, root)
    }

    #[tokio::test]
    async fn ripgrep_retrieval_honors_gitignore() {
        let (_td, root) = seed_repo();
        let r = RipgrepLexicalRetrieval { root };
        let hits = r.search("needle", 10).await.expect("search");
        assert!(
            hits.iter().any(|s| s.path.ends_with("foo.rs")),
            "expected foo.rs hit, got {:?}",
            hits
        );
        assert!(
            hits.iter().any(|s| s.path.ends_with("bar.md")),
            "expected bar.md hit, got {:?}",
            hits
        );
        assert!(
            !hits.iter().any(|s| s.path.ends_with("ignored.log")),
            "ignored.log should be filtered by .ignore, got {:?}",
            hits
        );
        let rs = hits.iter().find(|s| s.path.ends_with("foo.rs")).unwrap();
        assert_eq!(rs.start_line, 2);
        assert_eq!(rs.snippet, "let needle = 1;");
    }

    #[tokio::test]
    async fn ripgrep_retrieval_respects_limit() {
        let (_td, root) = seed_repo();
        let r = RipgrepLexicalRetrieval { root };
        let hits = r.search("needle", 1).await.expect("search");
        assert_eq!(hits.len(), 1);
    }
}
