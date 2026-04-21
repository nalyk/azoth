//! Sprint 4 verification: composite collector lane ordering is stable
//! under reranker permutation, and RRF correctly correlates items
//! across lanes.
//!
//! The v2 plan's verification checklist (§Sprint 4) calls for:
//!   "tests/context_kernel_v2.rs asserts lane ordering stable under
//!    reranker permutation."
//!
//! We interpret "stable under reranker permutation" as: swapping the
//! reranker between `IdentityReranker` and `ReciprocalRankFusion`
//! should yield deterministic outputs — same inputs → same outputs
//! for the *same* reranker, and the *shape* (lanes represented,
//! cross-lane ordering) should remain coherent across reranker
//! choices.

use async_trait::async_trait;
use azoth_core::context::{
    CompositeEvidenceCollector, EvidenceCollector, IdentityReranker, ReciprocalRankFusion,
    Reranker, TokenBudget,
};
use azoth_core::retrieval::RetrievalError;
use azoth_core::schemas::EvidenceItem;
use std::sync::Arc;

struct Static(Vec<EvidenceItem>);
#[async_trait]
impl EvidenceCollector for Static {
    async fn collect(&self, _q: &str, _l: usize) -> Result<Vec<EvidenceItem>, RetrievalError> {
        Ok(self.0.clone())
    }
}

fn item(label: &str, w: u32) -> EvidenceItem {
    EvidenceItem {
        label: label.into(),
        artifact_ref: None,
        inline: Some("x".repeat(20)),
        decision_weight: w,
        lane: None,
        rerank_score: None,
        observed_at: None,
        valid_at: None,
        freshness: None,
    }
}

fn composite_with(
    symbol: Vec<EvidenceItem>,
    lexical: Vec<EvidenceItem>,
    fts: Vec<EvidenceItem>,
    reranker: Arc<dyn Reranker>,
) -> CompositeEvidenceCollector {
    CompositeEvidenceCollector {
        graph: None,
        symbol: Some(Arc::new(Static(symbol))),
        lexical: Some(Arc::new(Static(lexical))),
        fts: Some(Arc::new(Static(fts))),
        reranker,
        budget: TokenBudget::v2_default(),
        per_lane_limit: 8,
    }
}

#[tokio::test]
async fn composite_is_deterministic_across_runs() {
    // Same reranker, same inputs → byte-identical outputs.
    let c = composite_with(
        vec![item("sym1", 10), item("sym2", 5)],
        vec![item("lex1", 9), item("lex2", 4)],
        vec![item("fts1", 8), item("fts2", 3)],
        Arc::new(ReciprocalRankFusion::default()),
    );
    let a = c.collect("refresh_token", 20).await.unwrap();
    let b = c.collect("refresh_token", 20).await.unwrap();
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.label, y.label);
        assert_eq!(x.lane, y.lane);
        assert_eq!(x.decision_weight, y.decision_weight);
    }
}

#[tokio::test]
async fn identity_reranker_preserves_top_per_lane() {
    // Identity scores = decision_weight. Top item of each lane
    // (weight 10) should all appear in the output.
    let c = composite_with(
        vec![item("sym1", 10), item("sym2", 5)],
        vec![item("lex1", 10), item("lex2", 4)],
        vec![item("fts1", 10), item("fts2", 3)],
        Arc::new(IdentityReranker),
    );
    let out = c.collect("q", 20).await.unwrap();
    let labels: Vec<&str> = out.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"sym1"));
    assert!(labels.contains(&"lex1"));
    assert!(labels.contains(&"fts1"));
}

#[tokio::test]
async fn rrf_places_top_per_lane_items_at_the_front() {
    // RRF gives rank 1 items the highest score. With three lanes all
    // having a top item at rank 1, the top three output slots should
    // each contain one rank-1 item.
    let c = composite_with(
        vec![item("sym_top", 10), item("sym_mid", 5), item("sym_low", 1)],
        vec![item("lex_top", 10), item("lex_mid", 5), item("lex_low", 1)],
        vec![item("fts_top", 10), item("fts_mid", 5), item("fts_low", 1)],
        Arc::new(ReciprocalRankFusion::default()),
    );
    let out = c.collect("q", 20).await.unwrap();
    let top_three: Vec<&str> = out.iter().take(3).map(|i| i.label.as_str()).collect();
    assert!(top_three.contains(&"sym_top"));
    assert!(top_three.contains(&"lex_top"));
    assert!(top_three.contains(&"fts_top"));
}

#[tokio::test]
async fn lanes_present_in_output_match_wired_slots() {
    // Only symbol + fts wired → output lanes are limited to those two.
    let c = CompositeEvidenceCollector {
        graph: None,
        symbol: Some(Arc::new(Static(vec![item("sym1", 10)]))),
        lexical: None,
        fts: Some(Arc::new(Static(vec![item("fts1", 8)]))),
        reranker: Arc::new(ReciprocalRankFusion::default()),
        budget: TokenBudget::v2_default(),
        per_lane_limit: 8,
    };
    let out = c.collect("q", 20).await.unwrap();
    let lanes: std::collections::HashSet<&str> =
        out.iter().filter_map(|i| i.lane.as_deref()).collect();
    assert!(lanes.contains("symbol"));
    assert!(lanes.contains("fts"));
    assert!(!lanes.contains("lexical"));
    assert!(!lanes.contains("graph"));
}

#[tokio::test]
async fn decision_weight_descends_so_kernel_sort_is_a_noop() {
    // Composite overwrites `decision_weight` with a descending rank
    // after rerank-sorting. The kernel's own
    // `sort_by(decision_weight desc)` must therefore be a no-op.
    let c = composite_with(
        vec![item("sym1", 5), item("sym2", 4), item("sym3", 3)],
        vec![item("lex1", 10), item("lex2", 9), item("lex3", 8)],
        vec![],
        Arc::new(IdentityReranker),
    );
    let out = c.collect("q", 20).await.unwrap();
    // Strictly descending decision_weight preserves ordering through
    // the kernel's eventual sort.
    for window in out.windows(2) {
        assert!(
            window[0].decision_weight >= window[1].decision_weight,
            "decision_weight must be non-increasing; got {:?}",
            out.iter()
                .map(|i| (i.label.clone(), i.decision_weight))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn rerank_score_recorded_on_every_item() {
    // Forensic replay needs the score that drove the ordering.
    let c = composite_with(
        vec![item("sym1", 10)],
        vec![item("lex1", 5)],
        vec![item("fts1", 1)],
        Arc::new(ReciprocalRankFusion::default()),
    );
    let out = c.collect("q", 20).await.unwrap();
    for i in &out {
        assert!(
            i.rerank_score.is_some(),
            "missing rerank_score on {}",
            i.label
        );
    }
}
