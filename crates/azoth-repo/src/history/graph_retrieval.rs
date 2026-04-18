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
//! also returns their neighbors, and so on.
//!
//! Each returned neighbor carries the **widest-path strength** on
//! any path back to the seed: the maximum over all seed → node
//! paths of the *minimum* edge weight on the path. This is the
//! classic widest-path (bottleneck) metric — a long chain of tiny
//! weights can't smuggle itself onto the top of the result list by
//! riding a single strong last hop. Rationale: co-edit weight is a
//! correlation strength, so the meaningful strength of a multi-hop
//! path is bounded by its weakest link.
//!
//! `limit` bounds the result set, applied after the full BFS has
//! completed so the strongest neighbors survive.
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

    /// Open a dedicated reader Connection on `db_path` with WAL mode
    /// enabled and migrations applied (idempotent). Mirrors
    /// `FtsLexicalRetrieval::open` and `SqliteSymbolIndex::open` so
    /// each composite-lane backend can own its own Connection — the
    /// Mutex then only serialises calls within a single backend,
    /// leaving the shared WAL to multiplex reads across backends.
    /// PR #11 review feedback.
    pub fn open<P: AsRef<std::path::Path>>(db_path: P) -> Result<Self, crate::IndexerError> {
        let mut conn = Connection::open(db_path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        azoth_core::event_store::migrations::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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

    // PR #7 review (gemini HIGH): hoist the `prepare` out of the
    // per-anchor loop. Inside BFS with depth>1, a frontier of N
    // nodes would otherwise re-parse the same SQL N times per hop;
    // one prepared statement reused for every `query_rows` call
    // turns that into N bind-and-step cycles over the same bytecode.
    let mut stmt = guard
        .prepare(
            "SELECT path_b AS neighbor, weight FROM co_edit_edges WHERE path_a = ?1 \
             UNION ALL \
             SELECT path_a AS neighbor, weight FROM co_edit_edges WHERE path_b = ?1",
        )
        .map_err(|e| RetrievalError::Other(format!("prepare neighbor: {e}")))?;

    // best[v] = widest-path strength from seed to v (max over
    // paths of min edge weight). `INFINITY` on the seed makes the
    // first hop's `min(best[anchor], edge_weight)` collapse to
    // `edge_weight`, so depth-1 results match direct-edge weights
    // exactly (unchanged from the pre-PR-#7-review behavior).
    let mut best: HashMap<String, f32> = HashMap::new();
    let mut frontier: Vec<String> = vec![seed.clone()];
    best.insert(seed, f32::INFINITY); // seed itself is filtered from results below

    for _ in 0..depth {
        if frontier.is_empty() {
            break;
        }
        let mut next: Vec<String> = Vec::new();
        for anchor in frontier.drain(..) {
            let anchor_strength = *best.get(&anchor).unwrap_or(&f32::NEG_INFINITY);
            for (neighbor, edge_weight) in query_one_hop(&mut stmt, &anchor)? {
                // Widest-path update: the path's strength equals
                // its weakest link. A long chain cannot surface on
                // top of a direct edge just because its final hop
                // happens to be heavy (PR #7 review, codex P1).
                let path_strength = anchor_strength.min(edge_weight);
                let entry = best.entry(neighbor.clone()).or_insert(f32::NEG_INFINITY);
                if path_strength > *entry {
                    *entry = path_strength;
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

/// Execute the hoisted prepared statement against one anchor path
/// and collect the `(neighbor, weight)` rows. See `bfs_neighbors`
/// for why the `Statement` is owned at the caller level.
fn query_one_hop(
    stmt: &mut rusqlite::Statement<'_>,
    anchor: &str,
) -> Result<Vec<(String, f32)>, RetrievalError> {
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
