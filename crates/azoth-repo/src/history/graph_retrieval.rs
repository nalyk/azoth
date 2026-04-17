//! `CoEditGraphRetrieval` — reads `co_edit_edges` to answer
//! `GraphRetrieval::neighbors` queries, displacing `NullGraphRetrieval`
//! as the real default wherever azoth-repo is available.
//!
//! ## `NodeRef` convention
//!
//! Per plan §Sprint 3, a path node is encoded as `"path:<rel_path>"`.
//! `PATH_PREFIX` exists as the single source of truth so callers
//! (the future composite collector in Sprint 4) do not have to
//! rebuild the string literal. Non-path node refs currently return
//! zero neighbors — the graph only knows about files today.
//!
//! ## Depth semantics
//!
//! The trait's `depth: usize` is interpreted as "BFS radius", so
//! `depth = 1` returns immediate co-edit neighbors, `depth = 2`
//! also returns their neighbors, and so on. Each returned neighbor
//! carries the **highest co-edit weight on any path** back to the
//! seed node, so multi-hop results rank sensibly (closer, heavier
//! paths dominate). `limit` bounds the result set, applied after
//! the full BFS has completed so the strongest neighbors survive.
//!
//! ## Edge weight & direction
//!
//! `co_edit_edges` is canonicalised `(a < b)`. A neighbor query
//! for file `P` therefore unions rows where `P = path_a` (returning
//! `path_b`) with rows where `P = path_b` (returning `path_a`). The
//! co-edit relation is symmetric, so `kind = "co_edit"` and the
//! weight is whatever the builder accumulated.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use azoth_core::retrieval::{Edge, GraphRetrieval, NodeRef, RetrievalError};
use rusqlite::{params, Connection};

/// Prefix used to encode a path node. Exposed so the composite
/// evidence collector (Sprint 4) can build query refs without
/// duplicating the literal.
pub const PATH_PREFIX: &str = "path:";

/// Convenience constructor for callers that want to hand a relative
/// path into `neighbors` without stringifying the prefix inline.
pub fn path_node<S: AsRef<str>>(rel_path: S) -> NodeRef {
    NodeRef(format!("{PATH_PREFIX}{}", rel_path.as_ref()))
}

pub struct CoEditGraphRetrieval {
    conn: Arc<Mutex<Connection>>,
}

impl CoEditGraphRetrieval {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl GraphRetrieval for CoEditGraphRetrieval {
    async fn neighbors(
        &self,
        node: NodeRef,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<(NodeRef, Edge)>, RetrievalError> {
        if depth == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        let Some(seed_path) = node.0.strip_prefix(PATH_PREFIX).map(str::to_owned) else {
            // Non-path node — the v2 graph only knows files.
            return Ok(Vec::new());
        };
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || bfs_neighbors(&conn, seed_path, depth, limit))
            .await
            .map_err(|e| RetrievalError::Other(format!("join: {e}")))?
    }
}

fn bfs_neighbors(
    conn: &Arc<Mutex<Connection>>,
    seed: String,
    depth: usize,
    limit: usize,
) -> Result<Vec<(NodeRef, Edge)>, RetrievalError> {
    let guard = conn
        .lock()
        .map_err(|e| RetrievalError::Other(format!("conn mutex poisoned: {e}")))?;

    // best[path] = greatest weight on any visited path back to seed.
    let mut best: HashMap<String, f32> = HashMap::new();
    let mut frontier: Vec<String> = vec![seed.clone()];
    best.insert(seed, f32::INFINITY); // seed itself is not a neighbor

    for _ in 0..depth {
        if frontier.is_empty() {
            break;
        }
        let mut next: Vec<String> = Vec::new();
        for anchor in frontier.drain(..) {
            for (neighbor, weight) in query_one_hop(&guard, &anchor)? {
                let entry = best.entry(neighbor.clone()).or_insert(f32::NEG_INFINITY);
                let improved = weight > *entry;
                if improved {
                    *entry = weight;
                    next.push(neighbor);
                }
            }
        }
        frontier = next;
    }

    // Drop the seed from the result set. `f32::INFINITY` is the
    // sentinel we seeded the map with; any legitimate edge weight
    // is finite.
    let mut ranked: Vec<(String, f32)> = best.into_iter().filter(|(_, w)| w.is_finite()).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);

    Ok(ranked
        .into_iter()
        .map(|(p, w)| {
            (
                path_node(&p),
                Edge {
                    kind: "co_edit".to_owned(),
                    weight: w,
                },
            )
        })
        .collect())
}

fn query_one_hop(conn: &Connection, anchor: &str) -> Result<Vec<(String, f32)>, RetrievalError> {
    let mut stmt = conn
        .prepare(
            "SELECT path_b AS neighbor, weight FROM co_edit_edges WHERE path_a = ?1 \
             UNION ALL \
             SELECT path_a AS neighbor, weight FROM co_edit_edges WHERE path_b = ?1",
        )
        .map_err(|e| RetrievalError::Other(format!("prepare neighbor: {e}")))?;
    let rows = stmt
        .query_map(params![anchor], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)? as f32))
        })
        .map_err(|e| RetrievalError::Other(format!("query neighbor: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| RetrievalError::Other(format!("row neighbor: {e}")))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefix_round_trips() {
        let n = path_node("src/foo.rs");
        assert_eq!(n.0, "path:src/foo.rs");
        assert_eq!(n.0.strip_prefix(PATH_PREFIX), Some("src/foo.rs"));
    }
}
