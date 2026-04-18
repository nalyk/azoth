//! Composite evidence collector — fans out to multiple per-lane
//! collectors, tags each item with its lane of origin, rerank-scores
//! the flat result, and applies a per-lane-floor token budget before
//! handing the survivors back to the kernel.
//!
//! ## Composition order (v2 plan §Sprint 4)
//!
//! `graph → symbol → lexical → fts → rerank`
//!
//! Each lane slot is *optional* so deployments can leave one out
//! without reshaping the collector. Graph in particular starts
//! unwired in Sprint 4 (query-extraction from the contract goal is
//! not yet defined for graph retrieval) but the slot exists so when
//! v2.1 wires it in the composite shape stays fixed.
//!
//! ## Lane tagging
//!
//! Composite *always* overwrites `EvidenceItem.lane` with the slot
//! name (`"graph"`, `"symbol"`, `"lexical"`, `"fts"`). That lets a
//! single sub-collector type (`LexicalEvidenceCollector`) back two
//! different lanes (ripgrep as `lexical`, FTS5 as `fts`) — the
//! distinction is a deployment concern, not the sub-collector's.
//!
//! ## Reranker signature contract
//!
//! The reranker returns a parallel `Vec<f32>` aligned with the input
//! slice. Composite copies each score onto `item.rerank_score` *in
//! place*, then dedupes cross-lane duplicates by `label` — the
//! retained item's `rerank_score` is the *sum* of each lane's
//! contribution. This reproduces the canonical RRF definition
//! `Σ_{L ∈ lanes(d)} 1/(k + rank_L(d))`: a document that shows up in
//! two lanes gets a fused score higher than either lane alone, which
//! is exactly the cross-lane-agreement signal RRF is designed to
//! surface. Forensic replay keeps the fused score that drove the
//! ordering.
//!
//! ## Budget semantics
//!
//! After rerank-based sort, `TokenBudget::apply` admits items. The
//! per-lane floor guarantees no lane starves even when a single lane
//! dominates the top rerank ranks (risk ledger #4). The final list
//! is returned *in rerank-sorted order* — the kernel's own final
//! `sort_by(decision_weight)` then rescues critical-first ordering,
//! but composite callers who want to preserve rerank order can
//! bypass that by emitting an ordered `decision_weight` derived from
//! rank position (see `CompositeEvidenceCollector::collect` doc).
//!
//! ## FTS snippet byte-stability
//!
//! Composite never touches `item.inline`. `azoth-repo`'s
//! `normalize_snippet` already collapses whitespace + strips
//! highlight markers before items reach this layer, so cache-prefix
//! stability (risk ledger #1) is preserved end-to-end.

use super::budget::{Slot, TokenBudget};
use super::evidence::EvidenceCollector;
use super::reranker::{RerankError, Reranker};
use crate::retrieval::RetrievalError;
use crate::schemas::EvidenceItem;
use async_trait::async_trait;
use std::sync::Arc;

/// Four lanes, all optional so deployments can leave slots unwired.
pub struct CompositeEvidenceCollector {
    pub graph: Option<Arc<dyn EvidenceCollector>>,
    pub symbol: Option<Arc<dyn EvidenceCollector>>,
    pub lexical: Option<Arc<dyn EvidenceCollector>>,
    pub fts: Option<Arc<dyn EvidenceCollector>>,
    pub reranker: Arc<dyn Reranker>,
    pub budget: TokenBudget,
    /// Max items to request from each lane sub-collector.
    pub per_lane_limit: usize,
}

impl CompositeEvidenceCollector {
    /// Ship-default builder: no lanes wired, identity reranker, v2
    /// default budget, 8 items per lane.
    pub fn empty(reranker: Arc<dyn Reranker>) -> Self {
        Self {
            graph: None,
            symbol: None,
            lexical: None,
            fts: None,
            reranker,
            budget: TokenBudget::v2_default(),
            per_lane_limit: 8,
        }
    }
}

#[async_trait]
impl EvidenceCollector for CompositeEvidenceCollector {
    async fn collect(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceItem>, RetrievalError> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        // Collect in the plan's prescribed order. Any sub-collector
        // failure is logged-but-survived: the composite must stay
        // useful under partial backend outage (Tier B resilience).
        let mut combined: Vec<EvidenceItem> = Vec::new();

        for (lane, slot) in [
            ("graph", self.graph.as_ref()),
            ("symbol", self.symbol.as_ref()),
            ("lexical", self.lexical.as_ref()),
            ("fts", self.fts.as_ref()),
        ] {
            if let Some(coll) = slot {
                match coll.collect(query, self.per_lane_limit).await {
                    Ok(items) => {
                        for mut it in items {
                            it.lane = Some(lane.to_string());
                            combined.push(it);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            lane,
                            error = %e,
                            "composite: sub-collector failed, continuing with remaining lanes"
                        );
                    }
                }
            }
        }

        if combined.is_empty() {
            return Ok(Vec::new());
        }

        // Rerank — map score errors into RetrievalError::Other so the
        // trait surface stays narrow.
        let scores = self
            .reranker
            .score(query, &combined)
            .await
            .map_err(|e: RerankError| RetrievalError::Other(e.to_string()))?;
        if scores.len() != combined.len() {
            return Err(RetrievalError::Other(format!(
                "reranker returned {} scores for {} items",
                scores.len(),
                combined.len()
            )));
        }
        for (it, s) in combined.iter_mut().zip(scores.iter().copied()) {
            it.rerank_score = Some(s);
        }

        // Canonical RRF fusion — dedupe by `label`, summing the
        // per-lane rerank contributions into a single item.
        //
        // Standard RRF is `Σ_{L ∈ lanes(d)} 1/(k + rank_L(d))`. Our
        // `Reranker::score` returns the *per-item* contribution
        // `1/(k + rank_L(item))`; composite is where the cross-lane
        // sum happens. Without this step, a document that surfaces in
        // both the lexical and FTS lanes would double-spend the token
        // budget (PR #8 review P2) and the expected "cross-lane
        // agreement boosts relevance" signal would never materialise.
        //
        // Determinism: we keep the *first* occurrence's metadata
        // (walking the lane-ordered `combined` list). That's stable
        // across runs because the outer collection loop is ordered
        // and each sub-collector is deterministic.
        let mut by_label: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut fused: Vec<EvidenceItem> = Vec::with_capacity(combined.len());
        for item in combined.drain(..) {
            if let Some(&idx) = by_label.get(&item.label) {
                let add = item.rerank_score.unwrap_or(0.0);
                let existing = &mut fused[idx];
                let cur = existing.rerank_score.unwrap_or(0.0);
                existing.rerank_score = Some(cur + add);
            } else {
                by_label.insert(item.label.clone(), fused.len());
                fused.push(item);
            }
        }
        combined = fused;

        // Sort by rerank_score desc, tie-broken by `label` ascending.
        //
        // Rust's `sort_by` is stable, so ties would normally fall back
        // on the `combined` insertion order — but that order is the
        // concatenation of per-lane sub-collector outputs, and the
        // ripgrep lane's hit sequence is filesystem-walk order (not
        // deterministic across FS state or reindexes). Without an
        // explicit tie-breaker, two items with equal RRF scores could
        // swap positions between runs on the *same* logical input,
        // perturbing the packet digest and collapsing Anthropic
        // prompt-cache hits (risk ledger #1 — cache-prefix stability).
        // PR #8 review (Gemini HIGH) called this out; fix is the
        // `.then_with(label cmp)` below. After dedupe, every item has
        // a unique label, so the tie-break is well-defined.
        combined.sort_by(|a, b| {
            b.rerank_score
                .unwrap_or(0.0)
                .partial_cmp(&a.rerank_score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.label.cmp(&b.label))
        });

        // Token budget with per-lane floor. Each slot's token cost is
        // an approximation — `label + inline` byte length divided by
        // four. The kernel will run its own full tokenizer on the
        // compiled packet; composite only needs a coarse filter.
        let slots: Vec<Slot> = combined
            .iter()
            .map(|it| Slot {
                lane: it.lane.clone().unwrap_or_else(|| "legacy".into()),
                tokens: approx_tokens(it),
            })
            .collect();
        let kept_idx = self.budget.apply(&slots);
        let mut kept: Vec<EvidenceItem> = Vec::with_capacity(kept_idx.len());
        for (idx, it) in combined.into_iter().enumerate() {
            if kept_idx.binary_search(&idx).is_ok() {
                kept.push(it);
            }
        }

        // Cap at caller's `limit` — honours the `StepInput.evidence`
        // size contract even if the budget had room for more.
        if kept.len() > limit {
            kept.truncate(limit);
        }

        // Overwrite decision_weight with a descending rank so the
        // kernel's own `sort_by(decision_weight desc)` preserves
        // rerank-sorted order end-to-end. First item gets `kept.len()`,
        // last gets 1.
        let total = kept.len() as u32;
        for (idx, it) in kept.iter_mut().enumerate() {
            it.decision_weight = total.saturating_sub(idx as u32).max(1);
        }

        Ok(kept)
    }
}

fn approx_tokens(item: &EvidenceItem) -> u32 {
    let label_len = item.label.len();
    let inline_len = item.inline.as_deref().map(str::len).unwrap_or(0);
    let artifact_len = item.artifact_ref.as_deref().map(str::len).unwrap_or(0);
    // Four chars per token — matches tokenizer::count_tokens heuristic.
    (((label_len + inline_len + artifact_len) as u32) / 4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::reranker::{IdentityReranker, ReciprocalRankFusion};

    struct StaticCollector(Vec<EvidenceItem>);

    #[async_trait]
    impl EvidenceCollector for StaticCollector {
        async fn collect(
            &self,
            _query: &str,
            _limit: usize,
        ) -> Result<Vec<EvidenceItem>, RetrievalError> {
            Ok(self.0.clone())
        }
    }

    fn item(label: &str, w: u32) -> EvidenceItem {
        EvidenceItem {
            label: label.into(),
            artifact_ref: None,
            inline: Some("x".repeat(40)), // 10 tokens
            decision_weight: w,
            lane: None,
            rerank_score: None,
        }
    }

    #[tokio::test]
    async fn empty_query_yields_empty() {
        let c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        let out = c.collect("", 10).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn zero_limit_yields_empty() {
        let c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        let out = c.collect("q", 0).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn no_lanes_wired_yields_empty() {
        let c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        let out = c.collect("q", 10).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn each_lane_tagged_by_slot_name_not_sub_collector() {
        // Pre-tag one item with a *wrong* lane — composite must
        // overwrite with the slot name.
        let mut pre_tagged = item("evil", 5);
        pre_tagged.lane = Some("graph".into()); // wrong on purpose
        let lexical = Arc::new(StaticCollector(vec![pre_tagged]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        c.lexical = Some(lexical);
        let out = c.collect("q", 10).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lane.as_deref(), Some("lexical"));
    }

    #[tokio::test]
    async fn rerank_score_copied_onto_each_item() {
        let lex = Arc::new(StaticCollector(vec![item("a", 10), item("b", 5)]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        c.lexical = Some(lex);
        let out = c.collect("q", 10).await.unwrap();
        assert!(out[0].rerank_score.is_some());
        assert!(out[1].rerank_score.is_some());
        // Identity score = decision_weight; first survives with
        // highest rank.
        assert!(out[0].rerank_score.unwrap() >= out[1].rerank_score.unwrap());
    }

    #[tokio::test]
    async fn rrf_prefers_lane_top_items_then_mixes() {
        // Two lanes, each with three items. RRF should pick top-of-
        // each-lane before lower-ranked ones.
        let lex = Arc::new(StaticCollector(vec![
            item("lex_top", 10),
            item("lex_mid", 5),
            item("lex_low", 1),
        ]));
        let sym = Arc::new(StaticCollector(vec![
            item("sym_top", 10),
            item("sym_mid", 5),
            item("sym_low", 1),
        ]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(ReciprocalRankFusion::default()));
        c.lexical = Some(lex);
        c.symbol = Some(sym);
        let out = c.collect("q", 10).await.unwrap();
        // Both top items have RRF rank 1 (tied), so both land in the
        // first two positions.
        let top_two: Vec<&str> = out.iter().take(2).map(|i| i.label.as_str()).collect();
        assert!(top_two.contains(&"lex_top"));
        assert!(top_two.contains(&"sym_top"));
    }

    #[tokio::test]
    async fn decision_weight_overwritten_to_preserve_sort_through_kernel() {
        // If composite returns items with stale weights, the kernel's
        // own sort would reshuffle. We overwrite with descending rank
        // so the kernel sort is a no-op on composite output.
        let lex = Arc::new(StaticCollector(vec![
            item("a", 1),   // worst original weight…
            item("b", 100), // …best original weight…
            item("c", 50),
        ]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        c.lexical = Some(lex);
        let out = c.collect("q", 10).await.unwrap();
        // Identity reranker uses decision_weight, so b sorts first,
        // c next, a last. Composite *then* rewrites weights.
        assert_eq!(out[0].label, "b");
        assert_eq!(out[1].label, "c");
        assert_eq!(out[2].label, "a");
        assert!(out[0].decision_weight > out[1].decision_weight);
        assert!(out[1].decision_weight > out[2].decision_weight);
    }

    #[tokio::test]
    async fn limit_truncates_budget_surplus() {
        let lex = Arc::new(StaticCollector(
            (0..20).map(|i| item(&format!("l{i}"), 100 - i)).collect(),
        ));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        c.lexical = Some(lex);
        let out = c.collect("q", 5).await.unwrap();
        assert_eq!(out.len(), 5);
    }

    #[tokio::test]
    async fn inline_snippet_preserved_byte_for_byte() {
        // Risk ledger #1: composite must not corrupt snippet bytes.
        let snippet = "fn foo() { bar(); }";
        let mut it = item("src/a.rs:1", 10);
        it.inline = Some(snippet.into());
        let lex = Arc::new(StaticCollector(vec![it]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        c.lexical = Some(lex);
        let out = c.collect("q", 10).await.unwrap();
        assert_eq!(out[0].inline.as_deref(), Some(snippet));
    }

    #[tokio::test]
    async fn cross_lane_duplicates_fuse_rrf_scores_and_dedupe() {
        // PR #8 review P2: same `label` from two lanes must fuse into
        // a single item whose rerank_score is the sum of per-lane RRF
        // contributions — the canonical RRF definition. Before the
        // fix, duplicates survived as separate items and double-spent
        // the token budget.
        let shared = "src/foo.rs:42";
        let lex = Arc::new(StaticCollector(vec![item(shared, 10)]));
        let fts = Arc::new(StaticCollector(vec![item(shared, 9)]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(ReciprocalRankFusion::default()));
        c.lexical = Some(lex);
        c.fts = Some(fts);
        let out = c.collect("q", 10).await.unwrap();
        // Single surviving item (deduped).
        assert_eq!(out.len(), 1, "cross-lane duplicates must dedupe");
        assert_eq!(out[0].label, shared);
        // Fused score ≈ 1/61 (from lexical rank 1) + 1/61 (from fts rank 1).
        let expected = 2.0f32 * (1.0 / 61.0);
        let got = out[0].rerank_score.unwrap();
        assert!(
            (got - expected).abs() < 1e-6,
            "fused RRF score should sum lane contributions: got {got}, expected {expected}"
        );
    }

    #[tokio::test]
    async fn fused_doc_ranks_above_single_lane_doc() {
        // A doc that surfaces in two lanes should outrank a doc that
        // surfaces in only one lane with equal per-lane rank — that's
        // the cross-lane-agreement signal RRF is designed to surface.
        let shared = "src/hot.rs:1";
        let only_fts = "src/cold.rs:1";
        let lex = Arc::new(StaticCollector(vec![item(shared, 10)]));
        let fts = Arc::new(StaticCollector(vec![
            item(shared, 10),  // same doc, rank 1 in fts
            item(only_fts, 9), // different doc, rank 2 in fts
        ]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(ReciprocalRankFusion::default()));
        c.lexical = Some(lex);
        c.fts = Some(fts);
        let out = c.collect("q", 10).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].label, shared,
            "the fused doc must sort ahead of the single-lane doc"
        );
        assert_eq!(out[1].label, only_fts);
    }

    #[tokio::test]
    async fn tied_rerank_scores_break_by_label_alphabetically() {
        // PR #8 Gemini review: cache-prefix stability requires the
        // sort be deterministic even when scores tie. Two items with
        // the same rank (hence the same RRF contribution) must come
        // out in a stable order regardless of upstream insertion
        // order. Label-alpha tie-break pins it.
        //
        // `zed.rs:1` has weight 10 (rank 1) in lexical, `apple.rs:1`
        // has weight 10 (rank 1) in fts — same RRF score. Label
        // tie-breaker must put apple first.
        let lex = Arc::new(StaticCollector(vec![item("zed.rs:1", 10)]));
        let fts = Arc::new(StaticCollector(vec![item("apple.rs:1", 10)]));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(ReciprocalRankFusion::default()));
        c.lexical = Some(lex);
        c.fts = Some(fts);
        let out = c.collect("q", 10).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].label, "apple.rs:1",
            "label tie-break must be ascending"
        );
        assert_eq!(out[1].label, "zed.rs:1");
    }

    #[tokio::test]
    async fn graph_lane_items_are_tagged_and_reach_final_output() {
        use super::super::graph_evidence::GraphEvidenceCollector;
        use crate::retrieval::{Edge, GraphRetrieval, NodeRef, RetrievalError};
        use async_trait::async_trait;

        // Minimal fake graph: seed "src/foo.rs" → "src/bar.rs".
        // Composite with ONLY the graph slot wired: proves the lane
        // tag propagates through RRF + dedup + sort + budget. Risk
        // ledger #3: an unwired graph lane is a silent
        // three-lanes-instead-of-four bug.
        struct GraphOnly;
        #[async_trait]
        impl GraphRetrieval for GraphOnly {
            async fn neighbors(
                &self,
                node: NodeRef,
                _depth: usize,
                _limit: usize,
            ) -> Result<Vec<(NodeRef, Edge)>, RetrievalError> {
                if node.0 == "path:src/foo.rs" {
                    Ok(vec![(
                        NodeRef("path:src/bar.rs".into()),
                        Edge {
                            kind: "co_edit".into(),
                            weight: 0.5,
                        },
                    )])
                } else {
                    Ok(Vec::new())
                }
            }
        }
        let graph_retr: Arc<dyn GraphRetrieval> = Arc::new(GraphOnly);
        let graph_lane: Arc<dyn EvidenceCollector> =
            Arc::new(GraphEvidenceCollector::new(graph_retr));
        let mut c = CompositeEvidenceCollector::empty(Arc::new(ReciprocalRankFusion::default()));
        c.graph = Some(graph_lane);
        let out = c.collect("touch src/foo.rs", 10).await.unwrap();
        assert_eq!(out.len(), 1, "expected one graph neighbour surviving");
        assert_eq!(out[0].label, "src/bar.rs");
        assert_eq!(
            out[0].lane.as_deref(),
            Some("graph"),
            "composite must tag graph-slot items with lane='graph'"
        );
    }

    #[tokio::test]
    async fn sub_collector_failure_does_not_kill_composite() {
        struct FailingCollector;
        #[async_trait]
        impl EvidenceCollector for FailingCollector {
            async fn collect(
                &self,
                _q: &str,
                _l: usize,
            ) -> Result<Vec<EvidenceItem>, RetrievalError> {
                Err(RetrievalError::Other("boom".into()))
            }
        }
        let mut c = CompositeEvidenceCollector::empty(Arc::new(IdentityReranker));
        c.symbol = Some(Arc::new(FailingCollector));
        c.lexical = Some(Arc::new(StaticCollector(vec![item("survive", 10)])));
        let out = c.collect("q", 10).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "survive");
    }
}
