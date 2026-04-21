//! Reranker abstraction — re-scoring evidence items after multi-lane
//! collection. v2 Sprint 4 ships two statistical impls; the
//! cross-encoder `BgeReranker` is trait-registered but
//! `unimplemented!()` until v2.5 where inference lands.
//!
//! Why a trait here and not just a free fn: the composite collector in
//! `context::composite` takes an `Arc<dyn Reranker>` so downstream
//! deployments (daemon mode, SDK) can swap in their own scoring
//! strategy without touching the kernel. The kernel signature itself
//! is unchanged — reranking happens upstream of `compile()`.
//!
//! ## Reciprocal Rank Fusion (RRF)
//!
//! Standard RRF is:
//!
//! ```text
//! RRF(d) = Σ_{L ∈ lanes containing d}  1 / (k + rank_L(d))
//! ```
//!
//! where `rank_L(d)` is the 1-based rank of document `d` within lane
//! `L`, and `k` dampens the top-rank boost (classic choice is 60, per
//! Cormack et al. 2009).
//!
//! Our `EvidenceItem` list arrives flat — each item already carries a
//! `lane` tag (or `None`, treated as a synthetic `"legacy"` lane) and a
//! `decision_weight` assigned by the upstream collector. We derive the
//! per-lane rank from the weight (descending → rank 1, 2, 3 …) and
//! return a parallel `Vec<f32>` of per-item RRF scores. Composite
//! collector then dedupes by `label`, keeping the highest-scoring item
//! (cross-lane agreement surfaces duplicates with elevated scores,
//! which is exactly the RRF signal).
//!
//! ## IdentityReranker
//!
//! Returns `decision_weight as f32` for each item. The composite
//! collector downstream still sorts by decision_weight after this, so
//! Identity is equivalent to "do not rerank" — useful as a default for
//! the `retrieval.mode = legacy` path and as a test double.

use crate::schemas::EvidenceItem;
use async_trait::async_trait;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RerankError {
    #[error("reranker inference not implemented (ship in v2.5)")]
    NotImplemented,
    #[error("rerank failed: {0}")]
    Other(String),
}

/// Scoring strategy applied to the flat combined output of the
/// composite collector. Returns a parallel `Vec<f32>` of length
/// `items.len()`. Higher is better.
#[async_trait]
pub trait Reranker: Send + Sync {
    async fn score(&self, query: &str, items: &[EvidenceItem]) -> Result<Vec<f32>, RerankError>;
}

/// Pass-through reranker. Returns `item.decision_weight as f32` for
/// each item. Semantically a no-op: sorting by this score preserves
/// the existing weight-based ordering.
pub struct IdentityReranker;

#[async_trait]
impl Reranker for IdentityReranker {
    async fn score(&self, _query: &str, items: &[EvidenceItem]) -> Result<Vec<f32>, RerankError> {
        Ok(items.iter().map(|it| it.decision_weight as f32).collect())
    }
}

/// Reciprocal Rank Fusion. See module docs for the formula.
///
/// Items are grouped by `lane` (None mapped to `"legacy"`). Within each
/// lane, rank is derived from `decision_weight` descending — ties
/// broken by input order for determinism. The returned score for item
/// `i` is `1 / (k + rank_of(i))` — the *classic per-item RRF*
/// contribution. The composite collector downstream sums contributions
/// when it dedupes by `label`, which reproduces the canonical RRF
/// definition across lanes.
pub struct ReciprocalRankFusion {
    pub k: f32,
}

impl Default for ReciprocalRankFusion {
    fn default() -> Self {
        Self { k: 60.0 }
    }
}

#[async_trait]
impl Reranker for ReciprocalRankFusion {
    async fn score(&self, _query: &str, items: &[EvidenceItem]) -> Result<Vec<f32>, RerankError> {
        // Per-lane rank map: lane → Vec<(original_index, decision_weight)>
        let mut by_lane: HashMap<&str, Vec<(usize, u32)>> = HashMap::new();
        for (idx, it) in items.iter().enumerate() {
            let lane = it.lane.as_deref().unwrap_or("legacy");
            by_lane
                .entry(lane)
                .or_default()
                .push((idx, it.decision_weight));
        }

        // For each lane, sort descending by weight; ties broken by the
        // original index so deterministic across runs.
        let mut rank_of_idx = vec![0u32; items.len()];
        for lane_items in by_lane.values_mut() {
            lane_items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (rank, (idx, _)) in lane_items.iter().enumerate() {
                // 1-based rank — standard RRF convention.
                rank_of_idx[*idx] = (rank + 1) as u32;
            }
        }

        Ok(rank_of_idx
            .iter()
            .map(|r| 1.0 / (self.k + (*r as f32)))
            .collect())
    }
}

/// Cross-encoder reranker — trait-registered but deferred to v2.5.
/// Ships now so the kernel wiring does not need surgery when the real
/// impl arrives.
pub struct BgeReranker;

#[async_trait]
impl Reranker for BgeReranker {
    async fn score(&self, _query: &str, _items: &[EvidenceItem]) -> Result<Vec<f32>, RerankError> {
        Err(RerankError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, w: u32, lane: Option<&str>) -> EvidenceItem {
        EvidenceItem {
            label: label.into(),
            artifact_ref: None,
            inline: None,
            decision_weight: w,
            lane: lane.map(|s| s.into()),
            rerank_score: None,
            observed_at: None,
            valid_at: None,
            freshness: None,
        }
    }

    #[tokio::test]
    async fn identity_returns_weights_as_scores() {
        let items = vec![
            item("a", 10, Some("lexical")),
            item("b", 5, Some("lexical")),
        ];
        let r = IdentityReranker;
        let out = r.score("q", &items).await.unwrap();
        assert_eq!(out, vec![10.0, 5.0]);
    }

    #[tokio::test]
    async fn rrf_ranks_top_item_highest_within_lane() {
        let items = vec![
            item("a", 10, Some("lexical")),
            item("b", 5, Some("lexical")),
            item("c", 1, Some("lexical")),
        ];
        let r = ReciprocalRankFusion::default();
        let out = r.score("q", &items).await.unwrap();
        // Rank 1: 1/(60+1); rank 2: 1/(60+2); rank 3: 1/(60+3).
        assert!(out[0] > out[1] && out[1] > out[2]);
        assert!((out[0] - 1.0 / 61.0).abs() < 1e-6);
        assert!((out[1] - 1.0 / 62.0).abs() < 1e-6);
        assert!((out[2] - 1.0 / 63.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn rrf_computes_rank_per_lane_independently() {
        // Two lanes. Each item's score depends on its rank within its
        // own lane, not the global list.
        let items = vec![
            item("lex1", 10, Some("lexical")), // lexical rank 1
            item("sym1", 1, Some("symbol")),   // symbol rank 1
            item("lex2", 9, Some("lexical")),  // lexical rank 2
            item("sym2", 0, Some("symbol")),   // symbol rank 2
        ];
        let r = ReciprocalRankFusion::default();
        let out = r.score("q", &items).await.unwrap();
        let r1 = 1.0f32 / 61.0;
        let r2 = 1.0f32 / 62.0;
        assert!((out[0] - r1).abs() < 1e-6);
        assert!((out[1] - r1).abs() < 1e-6);
        assert!((out[2] - r2).abs() < 1e-6);
        assert!((out[3] - r2).abs() < 1e-6);
    }

    #[tokio::test]
    async fn rrf_none_lane_mapped_to_legacy() {
        let items = vec![item("a", 10, None), item("b", 5, None)];
        let r = ReciprocalRankFusion::default();
        let out = r.score("q", &items).await.unwrap();
        assert!(out[0] > out[1]);
    }

    #[tokio::test]
    async fn rrf_ties_broken_by_input_order_deterministic() {
        let items = vec![
            item("a", 5, Some("lexical")),
            item("b", 5, Some("lexical")),
            item("c", 5, Some("lexical")),
        ];
        let r = ReciprocalRankFusion::default();
        let out1 = r.score("q", &items).await.unwrap();
        let out2 = r.score("q", &items).await.unwrap();
        assert_eq!(out1, out2);
        // Input order breaks ties → first item gets rank 1, best score.
        assert!(out1[0] > out1[1]);
        assert!(out1[1] > out1[2]);
    }

    #[tokio::test]
    async fn bge_reranker_not_implemented_in_v2() {
        let items = vec![item("a", 10, Some("lexical"))];
        let r = BgeReranker;
        let err = r.score("q", &items).await.unwrap_err();
        assert!(matches!(err, RerankError::NotImplemented));
    }
}
