//! Retrieval traits. Split from day one (HIGH-2): lexical ships in v1,
//! graph is a trait-only stub so v2 can land without touching signatures.

use async_trait::async_trait;
use thiserror::Error;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub kind: String,
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

/// Naive LexicalRetrieval impl — used until a real ripgrep+FTS5 backend
/// lands. Reuses the `repo.search` walker for symmetry.
pub struct NaiveLexicalRetrieval {
    pub root: std::path::PathBuf,
}

#[async_trait]
impl LexicalRetrieval for NaiveLexicalRetrieval {
    async fn search(&self, q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError> {
        let root = self.root.clone();
        let q = q.to_string();
        tokio::task::spawn_blocking(move || naive_scan(&root, &q, limit))
            .await
            .map_err(|e| RetrievalError::Other(e.to_string()))?
    }
}

fn naive_scan(root: &std::path::Path, q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError> {
    let mut hits = Vec::new();
    walk(root, q, limit, &mut hits)?;
    Ok(hits)
}

fn walk(
    dir: &std::path::Path,
    q: &str,
    limit: usize,
    out: &mut Vec<Span>,
) -> Result<(), RetrievalError> {
    if out.len() >= limit || !dir.exists() {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            walk(&path, q, limit, out)?;
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            if !line.contains(q) {
                continue;
            }
            out.push(Span {
                path: path.display().to_string(),
                start_line: idx + 1,
                end_line: idx + 1,
                snippet: line.trim().chars().take(200).collect(),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}
