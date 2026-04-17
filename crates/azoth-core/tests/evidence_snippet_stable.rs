//! Sprint 4 verification: composite collector preserves snippet
//! byte-stability end-to-end (risk ledger #1).
//!
//! The cache-prefix-stable ordering contract (`draft_plan.md` §"Context
//! Kernel v0") keys off the serialized packet bytes. If composite
//! mutated `EvidenceItem.inline` between invocations — whitespace
//! wobble, highlight injection, any byte-level drift — Anthropic
//! prompt cache hits would collapse.
//!
//! The lower-level FTS5 stability test lives in
//! `azoth-repo/src/fts.rs` (`snippet_is_byte_stable_across_requery`);
//! this test pins the composite-layer guarantee: whatever bytes a
//! sub-collector produces, composite returns them untouched.

use async_trait::async_trait;
use azoth_core::context::{
    CompositeEvidenceCollector, EvidenceCollector, IdentityReranker, TokenBudget,
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

fn with_snippet(label: &str, snippet: &str) -> EvidenceItem {
    EvidenceItem {
        label: label.into(),
        artifact_ref: None,
        inline: Some(snippet.into()),
        decision_weight: 10,
        lane: None,
        rerank_score: None,
    }
}

#[tokio::test]
async fn composite_preserves_inline_snippet_bytes() {
    // Deliberately unusual bytes: leading/trailing whitespace,
    // repeated runs, unicode — composite must hand them back
    // untouched.
    let snippets = vec![
        "  fn foo() { bar(); }  ",
        "spaces\tbetween\ntabs",
        "unicode ❤ café naïve",
        "",
        "x",
    ];
    for s in snippets {
        let c = CompositeEvidenceCollector {
            graph: None,
            symbol: None,
            lexical: Some(Arc::new(Static(vec![with_snippet("a.rs:1", s)]))),
            fts: None,
            reranker: Arc::new(IdentityReranker),
            budget: TokenBudget::v2_default(),
            per_lane_limit: 8,
        };
        let out = c.collect("q", 10).await.unwrap();
        assert_eq!(
            out[0].inline.as_deref(),
            Some(s),
            "composite must preserve inline bytes unchanged"
        );
    }
}

#[tokio::test]
async fn composite_output_stable_across_repeated_calls() {
    // Two calls with identical inputs must yield byte-identical JSON
    // — the bytes the kernel hashes for the cache key.
    let items = vec![
        with_snippet("a.rs:1", "alpha"),
        with_snippet("b.rs:2", "bravo"),
        with_snippet("c.rs:3", "charlie"),
    ];
    let c = CompositeEvidenceCollector {
        graph: None,
        symbol: None,
        lexical: Some(Arc::new(Static(items))),
        fts: None,
        reranker: Arc::new(IdentityReranker),
        budget: TokenBudget::v2_default(),
        per_lane_limit: 8,
    };
    let first = c.collect("q", 10).await.unwrap();
    let second = c.collect("q", 10).await.unwrap();
    let first_json = serde_json::to_string(&first).unwrap();
    let second_json = serde_json::to_string(&second).unwrap();
    assert_eq!(
        first_json, second_json,
        "composite output must be byte-stable across calls (cache-prefix risk #1)"
    );
}
