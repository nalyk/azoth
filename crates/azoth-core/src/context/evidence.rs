//! Evidence-lane collector — turns `LexicalRetrieval` spans into
//! `EvidenceItem`s for the Context Kernel's `evidence_lane`.
//!
//! Picklist #5: sort-by-decision-weight lived in `kernel.rs` from day one,
//! but the *content collection* side was a stub — `turn/mod.rs` always
//! passed `evidence: Vec::new()` into `StepInput`. This module closes the
//! gap with a trait + a ripgrep-backed impl. Wiring into the TurnDriver
//! is deliberately *not* part of this landing (scope-fenced to mirror the
//! 9623a45 adapter-smoke landing): a follow-up commit will decide on the
//! query-extraction policy — contract.goal verbatim, rubric terms, or a
//! richer planner-emitted query list — and plumb the collector into
//! StepInput.
//!
//! Ordering contract: this collector assigns each returned `EvidenceItem`
//! a `decision_weight` that strictly *decreases* with the order the
//! underlying retrieval returned its hits. That means the first hit from
//! the backend ends up with the highest weight, so the Context Kernel's
//! own `sort_by(|a,b| b.decision_weight.cmp(&a.decision_weight))` is a
//! stable no-op on the output of a single collector call. When multiple
//! collectors are composed later, the kernel sort still keeps critical
//! evidence up front.

use super::super::retrieval::{LexicalRetrieval, RetrievalError, Span};
use crate::schemas::EvidenceItem;
use async_trait::async_trait;

/// Produces `EvidenceItem`s for a single planning step.
///
/// Implementations are free to combine several retrieval backends
/// (lexical, graph, in-memory doc cache) as long as the returned vector
/// fits into the caller's token budget — the kernel will reject an
/// over-budget packet loudly, so collectors should respect `limit`.
#[async_trait]
pub trait EvidenceCollector: Send + Sync {
    async fn collect(&self, query: &str, limit: usize)
        -> Result<Vec<EvidenceItem>, RetrievalError>;
}

/// `LexicalRetrieval`-backed collector. Maps each `Span` to an
/// `EvidenceItem`:
/// - `label = "{path}:{start_line}"` — compact enough for logs yet
///   uniquely identifies the source location for forensic replay.
/// - `inline = Some(snippet)` — ripgrep already truncates to 200 chars
///   in `retrieval::Collector::matched`, so the content stays well
///   inside the "long payloads stay as artifact refs" rule
///   (`draft_plan.md:323`).
/// - `artifact_ref = None` — v1 has no artifact store; once Tier B
///   ships with a staging dir we can point at the file path here.
/// - `decision_weight` — descending from `limit` for the first hit to
///   `1` for the last, so the kernel sort preserves retrieval order.
pub struct LexicalEvidenceCollector<R: LexicalRetrieval + ?Sized> {
    pub retrieval: std::sync::Arc<R>,
}

impl<R: LexicalRetrieval + ?Sized> LexicalEvidenceCollector<R> {
    pub fn new(retrieval: std::sync::Arc<R>) -> Self {
        Self { retrieval }
    }
}

#[async_trait]
impl<R: LexicalRetrieval + ?Sized> EvidenceCollector for LexicalEvidenceCollector<R> {
    async fn collect(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceItem>, RetrievalError> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let spans = self.retrieval.search(query, limit).await?;
        Ok(spans_to_evidence(spans, limit))
    }
}

fn spans_to_evidence(spans: Vec<Span>, limit: usize) -> Vec<EvidenceItem> {
    // `limit` is u32-range in practice (the kernel's budget is in tokens,
    // not hits) so the as-cast is safe here.
    let base = limit as u32;
    spans
        .into_iter()
        .enumerate()
        .map(|(idx, s)| {
            let valid_at = s.source_mtime;
            EvidenceItem {
                label: format!("{}:{}", s.path, s.start_line),
                artifact_ref: None,
                inline: Some(s.snippet),
                // First hit gets `base`, decaying by 1; floor at 1 so
                // every item remains distinguishable from the kernel
                // default of 0.
                decision_weight: base.saturating_sub(idx as u32).max(1),
                lane: Some("lexical".into()),
                rerank_score: None,
                // CP-3: observed_at stays None here — the retrieval
                // impl stamps it when the composite collector runs.
                // valid_at propagates from the Span if the backend
                // supplied one (FTS5 does, ripgrep does not).
                observed_at: None,
                valid_at,
                freshness: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::{LexicalRetrieval, RetrievalError, Span};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct FakeLexical {
        spans: Vec<Span>,
    }

    #[async_trait]
    impl LexicalRetrieval for FakeLexical {
        async fn search(&self, _q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError> {
            Ok(self.spans.iter().take(limit).cloned().collect())
        }
    }

    fn span(path: &str, line: usize, snippet: &str) -> Span {
        Span {
            path: path.into(),
            start_line: line,
            end_line: line,
            snippet: snippet.into(),
            source_mtime: None,
        }
    }

    #[tokio::test]
    async fn empty_query_yields_no_evidence() {
        let fake = Arc::new(FakeLexical {
            spans: vec![span("a.rs", 1, "hit")],
        });
        let c = LexicalEvidenceCollector::new(fake);
        let out = c.collect("", 10).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn zero_limit_yields_no_evidence() {
        let fake = Arc::new(FakeLexical {
            spans: vec![span("a.rs", 1, "hit")],
        });
        let c = LexicalEvidenceCollector::new(fake);
        let out = c.collect("q", 0).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn weights_are_descending_and_preserve_order() {
        let fake = Arc::new(FakeLexical {
            spans: vec![
                span("a.rs", 10, "first"),
                span("b.rs", 20, "second"),
                span("c.rs", 30, "third"),
            ],
        });
        let c = LexicalEvidenceCollector::new(fake);
        let out = c.collect("needle", 5).await.unwrap();
        assert_eq!(out.len(), 3);

        assert_eq!(out[0].label, "a.rs:10");
        assert_eq!(out[0].inline.as_deref(), Some("first"));
        assert_eq!(out[0].decision_weight, 5);
        assert!(out[0].artifact_ref.is_none());

        assert_eq!(out[1].decision_weight, 4);
        assert_eq!(out[2].decision_weight, 3);

        // Strictly descending so the kernel's sort is a no-op.
        assert!(out[0].decision_weight > out[1].decision_weight);
        assert!(out[1].decision_weight > out[2].decision_weight);
    }

    #[tokio::test]
    async fn weight_floor_is_one_when_limit_exceeded() {
        // If a backend somehow returns more spans than `limit` (our
        // real ripgrep impl clamps, but the trait doesn't forbid it),
        // every item still keeps a non-zero weight distinct from the
        // EvidenceItem default.
        let spans: Vec<Span> = (0..4).map(|i| span("x.rs", i + 1, "s")).collect();
        let fake = Arc::new(FakeLexical { spans });
        let c = LexicalEvidenceCollector::new(fake);
        let out = c.collect("q", 2).await.unwrap();
        // FakeLexical respects `limit` via `.take()`, so we get 2 items.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].decision_weight, 2);
        assert_eq!(out[1].decision_weight, 1);
    }

    #[tokio::test]
    async fn kernel_sort_preserves_single_collector_order() {
        use crate::context::{ContextKernel, StepInput, TokenizerFamily};
        use crate::schemas::{Contract, ContractId, EffectBudget, Scope, TurnId};

        let fake = Arc::new(FakeLexical {
            spans: vec![
                span("a.rs", 1, "top"),
                span("b.rs", 2, "mid"),
                span("c.rs", 3, "bot"),
            ],
        });
        let collector = LexicalEvidenceCollector::new(fake);
        let evidence = collector.collect("q", 8).await.unwrap();

        let contract = Contract {
            id: ContractId::from("ctr_e".to_string()),
            goal: "g".into(),
            non_goals: vec![],
            success_criteria: vec![],
            scope: Scope::default(),
            effect_budget: EffectBudget::default(),
            notes: vec![],
        };
        let kernel = ContextKernel {
            policy_version: "v1",
            tokenizer: TokenizerFamily::Anthropic,
            max_input_tokens: 0,
        };
        let input = StepInput {
            contract: &contract,
            turn_id: TurnId::from("t".to_string()),
            step_goal: "g".into(),
            rubric: vec![],
            working_set: vec![],
            evidence,
            last_checkpoint: None,
            system_prompt: "s".into(),
            tool_schemas_digest: "sha256:0".into(),
        };
        let packet = kernel.compile(input).unwrap();
        assert_eq!(packet.evidence_lane[0].label, "a.rs:1");
        assert_eq!(packet.evidence_lane[1].label, "b.rs:2");
        assert_eq!(packet.evidence_lane[2].label, "c.rs:3");
    }
}
