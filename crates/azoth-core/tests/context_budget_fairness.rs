//! Sprint 4 verification: no lane starves under pathological weights.
//!
//! The v2 plan's risk ledger #4 calls out the failure mode:
//! "Token budget starvation under pathological weights. Composite
//!  collector runs greedy-by-weight post-rerank; one lane can crowd
//!  out others. Mitigation: `TokenBudget.per_lane_floor` guarantees
//!  minimum tokens per lane."
//!
//! This test stands in for the behaviour contract: given a skewed
//! weight distribution (lexical dominates), the composite collector
//! must still surface at least one item from each wired lane.

use async_trait::async_trait;
use azoth_core::context::{
    CompositeEvidenceCollector, EvidenceCollector, ReciprocalRankFusion, TokenBudget,
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

fn bulky_item(label: &str, w: u32) -> EvidenceItem {
    // Large inline payload so budget pressure matters.
    EvidenceItem {
        label: label.into(),
        artifact_ref: None,
        inline: Some("x".repeat(400)),
        decision_weight: w,
        lane: None,
        rerank_score: None,
    }
}

#[tokio::test]
async fn no_lane_starves_under_lexical_dominance() {
    // 20 fat lexical items with weights far exceeding graph/symbol.
    // Without per-lane floors, greedy-by-weight would eat the entire
    // budget on lexical.
    let lexical: Vec<EvidenceItem> = (0..20)
        .map(|i| bulky_item(&format!("lex_{i}"), 1000 - i))
        .collect();
    let symbol = vec![bulky_item("sym_1", 50), bulky_item("sym_2", 40)];
    let graph = vec![bulky_item("graph_1", 30), bulky_item("graph_2", 20)];
    let fts = vec![bulky_item("fts_1", 25), bulky_item("fts_2", 15)];

    let c = CompositeEvidenceCollector {
        graph: Some(Arc::new(Static(graph))),
        symbol: Some(Arc::new(Static(symbol))),
        lexical: Some(Arc::new(Static(lexical))),
        fts: Some(Arc::new(Static(fts))),
        reranker: Arc::new(ReciprocalRankFusion::default()),
        budget: TokenBudget::v2_default(),
        per_lane_limit: 10,
    };

    let out = c.collect("pathological", 100).await.unwrap();

    let lanes: std::collections::HashSet<String> =
        out.iter().filter_map(|i| i.lane.clone()).collect();

    assert!(
        lanes.contains("lexical"),
        "lexical should survive; got lanes {lanes:?}"
    );
    assert!(
        lanes.contains("symbol"),
        "symbol must not be starved; got lanes {lanes:?}"
    );
    assert!(
        lanes.contains("graph"),
        "graph must not be starved; got lanes {lanes:?}"
    );
    assert!(
        lanes.contains("fts"),
        "fts must not be starved; got lanes {lanes:?}"
    );
}

#[tokio::test]
async fn symbol_only_still_returns_items_under_floor() {
    // Only symbol wired. Floor semantics must still produce output.
    let c = CompositeEvidenceCollector {
        graph: None,
        symbol: Some(Arc::new(Static(vec![bulky_item("sym_only", 10)]))),
        lexical: None,
        fts: None,
        reranker: Arc::new(ReciprocalRankFusion::default()),
        budget: TokenBudget::v2_default(),
        per_lane_limit: 8,
    };
    let out = c.collect("q", 10).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].label, "sym_only");
    assert_eq!(out[0].lane.as_deref(), Some("symbol"));
}

#[tokio::test]
async fn tight_budget_still_reserves_floor_for_each_lane() {
    // Extreme squeeze: max_tokens intentionally low, but per-lane
    // floors force at least the top of each lane to survive.
    let lexical: Vec<EvidenceItem> = (0..10)
        .map(|i| bulky_item(&format!("lex_{i}"), 1000 - i))
        .collect();
    let symbol = vec![bulky_item("sym_top", 5)];
    let graph = vec![bulky_item("graph_top", 5)];

    let mut budget = TokenBudget::v2_default();
    budget.max_tokens = 800; // well below the 20 items × 100-tok cost
    let c = CompositeEvidenceCollector {
        graph: Some(Arc::new(Static(graph))),
        symbol: Some(Arc::new(Static(symbol))),
        lexical: Some(Arc::new(Static(lexical))),
        fts: None,
        reranker: Arc::new(ReciprocalRankFusion::default()),
        budget,
        per_lane_limit: 10,
    };
    let out = c.collect("q", 100).await.unwrap();
    let lanes: std::collections::HashSet<String> =
        out.iter().filter_map(|i| i.lane.clone()).collect();
    assert!(
        lanes.contains("lexical") && lanes.contains("symbol") && lanes.contains("graph"),
        "tight budget must reserve floor per lane; got {lanes:?}"
    );
}
