//! `GraphEvidenceCollector` — wires a `GraphRetrieval` (e.g. the v2
//! `CoEditGraphRetrieval`) into the composite's `graph` lane, giving
//! the retrieval plane its fourth rail alongside lexical, FTS, and
//! symbol.
//!
//! ## Seed extraction
//!
//! The collector needs paths to query the graph with. The plan-level
//! v2.5 answer is a symbol-resolver-driven seeder tied to the
//! contract goal, but v2 ships the dumb-but-deterministic version:
//! regex over the query string for `*.rs`, `*.md`, `*.toml`,
//! `*.json`, `*.yaml` fragments plus explicit `crates/<path>` /
//! `src/<path>` walks. Good enough to surface meaningful neighbours
//! when the prompt references a file path or module, which is the
//! common case in contract goals and user turns.
//!
//! ## Node-ID convention
//!
//! v2's `CoEditGraphRetrieval` stores path-shaped nodes as
//! `"path:{rel_path}"` (see `azoth-repo/src/history/graph_retrieval.rs`
//! and its `PATH_PREFIX` constant). The collector assumes this
//! contract: it prepends `"path:"` when seeding and strips it when
//! reading the neighbour node back into an `EvidenceItem.label`.
//! Graph retrievals that use a different prefix scheme should ship
//! their own collector rather than reuse this one.
//!
//! ## Output shape
//!
//! Each surviving neighbour is emitted as
//! `EvidenceItem { label: "{neighbour_path}", artifact_ref: Some("{neighbour_path}"),
//! decision_weight: (edge.weight * 100.0).round() as u32, lane: Some("graph") }`.
//!
//! The label is the bare path — lined up with the shape PR A's path
//! extractor already falls through on, so the live-retrieval eval
//! picks the graph lane up for free once the composite is wired.

use crate::context::evidence::EvidenceCollector;
use crate::retrieval::{GraphRetrieval, NodeRef, RetrievalError};
use crate::schemas::EvidenceItem;
use async_trait::async_trait;
use std::sync::Arc;

/// Same constant as `azoth-repo::history::PATH_PREFIX`. Duplicated
/// here so `azoth-core` stays dep-thin (invariant: core has no
/// heavy indexer deps).
const PATH_PREFIX: &str = "path:";

pub struct GraphEvidenceCollector {
    retrieval: Arc<dyn GraphRetrieval>,
    /// Max neighbours pulled per seed path. Guard against pathological
    /// "everything co-edits with everything" patterns on squashed
    /// repos (see `docs/v2_plan.md` risk ledger #3).
    per_seed_cap: usize,
}

impl GraphEvidenceCollector {
    pub fn new(retrieval: Arc<dyn GraphRetrieval>) -> Self {
        Self {
            retrieval,
            per_seed_cap: 8,
        }
    }

    pub fn with_per_seed_cap(mut self, cap: usize) -> Self {
        self.per_seed_cap = cap;
        self
    }
}

#[async_trait]
impl EvidenceCollector for GraphEvidenceCollector {
    async fn collect(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceItem>, RetrievalError> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let seeds = extract_seed_paths(query);
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let per_seed = std::cmp::min(self.per_seed_cap, limit.div_ceil(seeds.len().max(1)).max(1));

        let mut out: Vec<EvidenceItem> = Vec::new();
        let mut seen_labels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for seed in &seeds {
            let node = NodeRef(format!("{PATH_PREFIX}{seed}"));
            match self.retrieval.neighbors(node, 1, per_seed).await {
                Ok(neighbours) => {
                    for (neighbour, edge) in neighbours {
                        if out.len() >= limit {
                            break;
                        }
                        let Some(path) = neighbour.0.strip_prefix(PATH_PREFIX) else {
                            continue;
                        };
                        if path.is_empty() {
                            continue;
                        }
                        // Skip self-loops: a graph that reports
                        // `seed → seed` would double-count a path
                        // the user already named in the prompt.
                        if path == seed {
                            continue;
                        }
                        if !seen_labels.insert(path.to_string()) {
                            continue;
                        }
                        // edge.weight is in [0.0, 1.0] for co-edit
                        // (normalised by `1 / max(1, |commit| - 1)`).
                        // Multiply by 100 to land decision_weight in
                        // a scale compatible with lexical/FTS weights.
                        let decision_weight = (edge.weight.max(0.0) * 100.0).round() as u32;
                        out.push(EvidenceItem {
                            label: path.to_string(),
                            artifact_ref: Some(path.to_string()),
                            inline: None,
                            decision_weight: decision_weight.max(1),
                            lane: Some("graph".into()),
                            rerank_score: None,
                        });
                    }
                }
                Err(_) => continue,
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

/// Pull file-path-shaped fragments out of `query`. Deliberately
/// simple: tokenise on whitespace, keep tokens that either end in
/// a known source-file extension or start with a well-known
/// top-level directory (`crates/`, `src/`, `docs/`, `tests/`).
/// Trims trailing punctuation so "fix `src/foo.rs`." still yields
/// `src/foo.rs`.
///
/// Returns a de-duplicated list preserving first-seen order. Upper
/// bound of 16 to stop a pathological prompt (e.g. a stack trace
/// pasted in full) from driving 100+ graph queries per turn.
pub fn extract_seed_paths(query: &str) -> Vec<String> {
    const EXTENSIONS: &[&str] = &[
        ".rs", ".md", ".toml", ".json", ".yaml", ".yml", ".sh", ".py", ".ts", ".js",
    ];
    const PREFIXES: &[&str] = &[
        "crates/",
        "src/",
        "docs/",
        "tests/",
        "examples/",
        "schemas/",
    ];
    const MAX_SEEDS: usize = 16;

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();

    for raw in query.split_whitespace() {
        // Strip leading non-alnum + trailing anything-that-isn't-
        // alnum-or-slash. Keeps `.` mid-token (part of the
        // extension) while dropping trailing sentence punctuation
        // like a full-stop or comma.
        let leading_trimmed =
            raw.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.');
        let trimmed = leading_trimmed.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '/');
        if trimmed.is_empty() {
            continue;
        }
        // Codex round-5 P2: tokens like `src/foo.rs:42` or
        // `src/foo.rs:42:7` (rustc/grep/compile-output
        // convention) survived the trim above — `:` is
        // non-alphanumeric but neither `/` nor `.`, and `42` is
        // alphanumeric, so rstrip stopped at the digit. Graph
        // nodes are keyed on bare paths (`path:src/foo.rs`), so
        // the `:line(:col)?` suffix caused every such token to
        // miss the graph lane silently. Strip up to two trailing
        // `:number` groups before the extension/prefix check.
        let trimmed = strip_line_col_suffix(trimmed);
        let looks_like_path = EXTENSIONS.iter().any(|ext| trimmed.ends_with(ext))
            || PREFIXES.iter().any(|p| trimmed.starts_with(p));
        if !looks_like_path {
            continue;
        }
        // Filter out pure noise tokens: a bare extension (".rs")
        // or a leading-only prefix ("crates/"). Earlier versions
        // used `trimmed.len() <= 5`, which also skipped legitimate
        // short filenames like `a.rs` (gemini MEDIUM on PR #14).
        // Exact-equality against the known-noise vocabulary is the
        // precise filter.
        if EXTENSIONS.contains(&trimmed) || PREFIXES.contains(&trimmed) {
            continue;
        }
        if !seen.insert(trimmed.to_string()) {
            continue;
        }
        out.push(trimmed.to_string());
        if out.len() >= MAX_SEEDS {
            break;
        }
    }
    out
}

/// Strip up to two trailing `:<digits>` groups from a token.
/// Converts `src/foo.rs:42` → `src/foo.rs` and
/// `src/foo.rs:42:7` → `src/foo.rs`, leaving `src/foo.rs` alone
/// and refusing to strip `:cfg(test)` etc. (only digit suffixes
/// are stripped). Keeps the colon-separated-anything form
/// (e.g. LSP-style `file://path`) intact — those don't end in
/// `:\d+`.
fn strip_line_col_suffix(s: &str) -> &str {
    let mut out = s;
    for _ in 0..2 {
        let Some((prefix, suffix)) = out.rsplit_once(':') else {
            break;
        };
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        out = prefix;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::{Edge, NullGraphRetrieval};

    #[test]
    fn seed_extraction_catches_rust_files_with_trailing_punctuation() {
        let q = "please fix `crates/azoth-core/src/turn/mod.rs`, also src/foo.rs.";
        let seeds = extract_seed_paths(q);
        assert_eq!(
            seeds,
            vec![
                "crates/azoth-core/src/turn/mod.rs".to_string(),
                "src/foo.rs".to_string(),
            ]
        );
    }

    #[test]
    fn seed_extraction_catches_prefixed_paths_without_extension() {
        let q = "walk crates/azoth-repo/src/history/ for the graph code";
        let seeds = extract_seed_paths(q);
        assert!(
            seeds
                .iter()
                .any(|s| s == "crates/azoth-repo/src/history"
                    || s == "crates/azoth-repo/src/history/"),
            "expected a crates/... fragment, got {seeds:?}"
        );
    }

    #[test]
    fn seed_extraction_accepts_short_valid_filenames() {
        // PR #14 gemini MEDIUM regression guard: the previous
        // `trimmed.len() <= 5` filter dropped short legitimate
        // filenames (e.g. `a.rs`, `lib.rs`) along with actual
        // noise (`.rs`, `crates/`). Exact-equality against the
        // noise vocabulary fixes it.
        let q = "fix a.rs and b.md, also crates/x.toml";
        let seeds = extract_seed_paths(q);
        assert!(
            seeds.iter().any(|s| s == "a.rs"),
            "short filename `a.rs` must survive the noise filter; got {seeds:?}"
        );
        assert!(
            seeds.iter().any(|s| s == "b.md"),
            "short filename `b.md` must survive the noise filter; got {seeds:?}"
        );
        assert!(
            !seeds.iter().any(|s| s == ".rs" || s == "crates/"),
            "bare extension/prefix must still be filtered; got {seeds:?}"
        );
    }

    #[test]
    fn seed_extraction_ignores_non_path_tokens() {
        let q = "the turn driver uses a biased select";
        let seeds = extract_seed_paths(q);
        assert!(seeds.is_empty(), "no path-ish tokens, got {seeds:?}");
    }

    /// Codex round-5 P2 regression guard: compiler/grep-style
    /// `path:line` and `path:line:col` tokens must normalise to
    /// the bare path before NodeRef construction — graph nodes
    /// are keyed `path:src/foo.rs`, not `path:src/foo.rs:42`.
    #[test]
    fn seed_extraction_strips_line_col_suffix() {
        let q = "stack trace at src/foo.rs:42 and crates/azoth-core/src/mod.rs:120:8";
        let seeds = extract_seed_paths(q);
        assert!(
            seeds.iter().any(|s| s == "src/foo.rs"),
            "src/foo.rs:42 must normalise to src/foo.rs; got {seeds:?}"
        );
        assert!(
            seeds.iter().any(|s| s == "crates/azoth-core/src/mod.rs"),
            "crates/.../mod.rs:120:8 must normalise to bare path; got {seeds:?}"
        );
        // And we must NOT have kept the line-suffixed form.
        assert!(
            !seeds.iter().any(|s| s.contains(':')),
            "no seed should retain a colon after normalisation; got {seeds:?}"
        );
    }

    #[test]
    fn strip_line_col_suffix_edge_cases() {
        assert_eq!(strip_line_col_suffix("src/foo.rs"), "src/foo.rs");
        assert_eq!(strip_line_col_suffix("src/foo.rs:42"), "src/foo.rs");
        assert_eq!(strip_line_col_suffix("src/foo.rs:42:7"), "src/foo.rs");
        // Only digit suffixes — don't eat non-digit colon parts.
        assert_eq!(
            strip_line_col_suffix("src/cfg(test):bar"),
            "src/cfg(test):bar"
        );
        // At most two strip rounds — `a:1:2:3` → `a:1`.
        assert_eq!(strip_line_col_suffix("a:1:2:3"), "a:1");
        // Empty segments: `foo:` → `foo:` (empty suffix fails
        // the `all-digits` check).
        assert_eq!(strip_line_col_suffix("foo:"), "foo:");
    }

    #[test]
    fn seed_extraction_caps_at_max() {
        let many = (0..32)
            .map(|i| format!("src/file_{i}.rs"))
            .collect::<Vec<_>>()
            .join(" ");
        let seeds = extract_seed_paths(&many);
        assert!(seeds.len() <= 16, "MAX_SEEDS must cap, got {}", seeds.len());
    }

    struct FakeGraph {
        edges: Vec<(NodeRef, Vec<(NodeRef, Edge)>)>,
    }

    #[async_trait]
    impl GraphRetrieval for FakeGraph {
        async fn neighbors(
            &self,
            node: NodeRef,
            _depth: usize,
            limit: usize,
        ) -> Result<Vec<(NodeRef, Edge)>, RetrievalError> {
            for (src, dst) in &self.edges {
                if src.0 == node.0 {
                    let mut v = dst.clone();
                    v.truncate(limit);
                    return Ok(v);
                }
            }
            Ok(Vec::new())
        }
    }

    fn edge(kind: &str, weight: f32) -> Edge {
        Edge {
            kind: kind.into(),
            weight,
        }
    }

    #[tokio::test]
    async fn collect_returns_neighbours_for_seed_path() {
        let fake = Arc::new(FakeGraph {
            edges: vec![(
                NodeRef("path:src/foo.rs".into()),
                vec![
                    (NodeRef("path:src/bar.rs".into()), edge("co_edit", 0.5)),
                    (NodeRef("path:src/baz.rs".into()), edge("co_edit", 0.25)),
                ],
            )],
        });
        let c = GraphEvidenceCollector::new(fake);
        let out = c.collect("please edit src/foo.rs", 10).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "src/bar.rs");
        assert_eq!(out[0].lane.as_deref(), Some("graph"));
        assert_eq!(out[0].decision_weight, 50); // 0.5 * 100
        assert_eq!(out[1].label, "src/baz.rs");
        assert_eq!(out[1].decision_weight, 25);
    }

    #[tokio::test]
    async fn collect_dedupes_neighbours_across_seeds() {
        let fake = Arc::new(FakeGraph {
            edges: vec![
                (
                    NodeRef("path:src/foo.rs".into()),
                    vec![(NodeRef("path:src/shared.rs".into()), edge("co_edit", 0.4))],
                ),
                (
                    NodeRef("path:src/bar.rs".into()),
                    vec![(NodeRef("path:src/shared.rs".into()), edge("co_edit", 0.3))],
                ),
            ],
        });
        let c = GraphEvidenceCollector::new(fake);
        let out = c
            .collect("touch src/foo.rs and src/bar.rs", 10)
            .await
            .unwrap();
        let shared_hits = out.iter().filter(|i| i.label == "src/shared.rs").count();
        assert_eq!(shared_hits, 1, "duplicate neighbour must collapse");
    }

    #[tokio::test]
    async fn collect_skips_self_loops() {
        let fake = Arc::new(FakeGraph {
            edges: vec![(
                NodeRef("path:src/foo.rs".into()),
                vec![
                    (NodeRef("path:src/foo.rs".into()), edge("co_edit", 1.0)),
                    (NodeRef("path:src/bar.rs".into()), edge("co_edit", 0.5)),
                ],
            )],
        });
        let c = GraphEvidenceCollector::new(fake);
        let out = c.collect("src/foo.rs", 10).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "src/bar.rs");
    }

    #[tokio::test]
    async fn collect_respects_limit() {
        let dst = (0..20)
            .map(|i| (NodeRef(format!("path:src/out{i}.rs")), edge("co_edit", 0.5)))
            .collect();
        let fake = Arc::new(FakeGraph {
            edges: vec![(NodeRef("path:src/foo.rs".into()), dst)],
        });
        let c = GraphEvidenceCollector::new(fake);
        let out = c.collect("src/foo.rs", 5).await.unwrap();
        assert!(out.len() <= 5);
    }

    #[tokio::test]
    async fn collect_returns_empty_for_null_graph() {
        let c = GraphEvidenceCollector::new(Arc::new(NullGraphRetrieval));
        let out = c.collect("edit src/foo.rs please", 10).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn collect_returns_empty_on_empty_query() {
        let fake = Arc::new(FakeGraph {
            edges: vec![(
                NodeRef("path:src/foo.rs".into()),
                vec![(NodeRef("path:src/bar.rs".into()), edge("co_edit", 0.5))],
            )],
        });
        let c = GraphEvidenceCollector::new(fake);
        let out = c.collect("", 10).await.unwrap();
        assert!(out.is_empty());
        let out = c.collect("no paths mentioned here", 10).await.unwrap();
        assert!(out.is_empty());
    }
}
