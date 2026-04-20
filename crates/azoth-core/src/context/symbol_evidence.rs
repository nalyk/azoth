//! Symbol-index evidence collector (Sprint 2).
//!
//! Sibling of `LexicalEvidenceCollector`, wired to a `SymbolRetrieval`
//! backend instead of a `LexicalRetrieval` one. Follows the Sprint 1
//! scope fence verbatim: **constructible, testable, but NOT default-
//! wired** into `TurnDriver`. Sprint 4 will compose this into a
//! `CompositeEvidenceCollector`; flipping the default is the same
//! sprint's concern. Today this module just provides the class so
//! higher layers can opt in.
//!
//! ## Mapping
//!
//! Each `Symbol` becomes an `EvidenceItem`:
//! - `label = "symbol {name} ({kind})"` — matches the v2 plan's
//!   "lane:symbol {name}" intent but defers the explicit `lane` field
//!   to Sprint 4's EvidenceItem extension.
//! - `artifact_ref = Some("{path}#L{start_line}")` — the only place we
//!   persist the file location; the TUI can render this as a jump
//!   target without re-querying the index.
//! - `inline = None` — symbols are pointers, not content. Sprint 4's
//!   composite collector will add a separate pass for inline bodies.
//! - `decision_weight` — descending from `limit` to 1, same shape as
//!   `LexicalEvidenceCollector`, so the kernel's stable sort preserves
//!   retrieval order across any composition.

use std::sync::Arc;

use async_trait::async_trait;

use super::evidence::EvidenceCollector;
use crate::retrieval::{RetrievalError, Symbol, SymbolRetrieval};
use crate::schemas::EvidenceItem;

pub struct SymbolEvidenceCollector {
    pub retrieval: Arc<dyn SymbolRetrieval>,
}

impl SymbolEvidenceCollector {
    pub fn new(retrieval: Arc<dyn SymbolRetrieval>) -> Self {
        Self { retrieval }
    }
}

#[async_trait]
impl EvidenceCollector for SymbolEvidenceCollector {
    async fn collect(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceItem>, RetrievalError> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let symbols = self.retrieval.by_name(query, limit).await?;
        Ok(symbols_to_evidence(symbols, limit))
    }
}

fn symbols_to_evidence(symbols: Vec<Symbol>, limit: usize) -> Vec<EvidenceItem> {
    let base = limit as u32;
    symbols
        .into_iter()
        .enumerate()
        .map(|(idx, s)| {
            let valid_at = s.source_mtime;
            EvidenceItem {
                label: format!("symbol {} ({})", s.name, s.kind.as_str()),
                artifact_ref: Some(format!("{}#L{}", s.path, s.start_line)),
                inline: None,
                decision_weight: base.saturating_sub(idx as u32).max(1),
                lane: Some("symbol".into()),
                rerank_score: None,
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
    use crate::retrieval::{Symbol, SymbolId, SymbolKind};

    struct FakeSyms {
        hits: Vec<Symbol>,
    }

    #[async_trait]
    impl SymbolRetrieval for FakeSyms {
        async fn by_name(&self, _name: &str, limit: usize) -> Result<Vec<Symbol>, RetrievalError> {
            Ok(self.hits.iter().take(limit).cloned().collect())
        }
        async fn enclosing(
            &self,
            _path: &str,
            _line: u32,
        ) -> Result<Option<Symbol>, RetrievalError> {
            Ok(None)
        }
    }

    fn sym(name: &str, kind: SymbolKind, path: &str, line: u32) -> Symbol {
        Symbol {
            id: SymbolId(1),
            name: name.into(),
            kind,
            path: path.into(),
            start_line: line,
            end_line: line,
            parent_id: None,
            language: "rust".into(),
            source_mtime: None,
        }
    }

    #[tokio::test]
    async fn empty_query_or_zero_limit_yields_nothing() {
        let fake = Arc::new(FakeSyms {
            hits: vec![sym("Foo", SymbolKind::Struct, "a.rs", 1)],
        });
        let c = SymbolEvidenceCollector::new(fake.clone());
        assert!(c.collect("", 5).await.unwrap().is_empty());
        assert!(c.collect("Foo", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn maps_symbols_to_evidence_with_descending_weights() {
        let fake = Arc::new(FakeSyms {
            hits: vec![
                sym("Foo", SymbolKind::Struct, "src/a.rs", 10),
                sym("Foo", SymbolKind::Impl, "src/a.rs", 40),
            ],
        });
        let c = SymbolEvidenceCollector::new(fake);
        let out = c.collect("Foo", 5).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "symbol Foo (struct)");
        assert_eq!(out[0].artifact_ref.as_deref(), Some("src/a.rs#L10"));
        assert!(out[0].inline.is_none());
        assert_eq!(out[0].decision_weight, 5);
        assert_eq!(out[1].decision_weight, 4);
        assert!(out[0].decision_weight > out[1].decision_weight);
    }

    #[tokio::test]
    async fn weight_floor_is_one_under_large_index() {
        // 6 hits against a limit of 3. FakeSyms takes `limit`, so we
        // exercise the non-clamped path via the 6-wide fixture and a
        // limit of 6 to land two items deeper than `base`.
        let hits: Vec<Symbol> = (1..=6)
            .map(|i| sym(&format!("S{i}"), SymbolKind::Struct, "x.rs", i))
            .collect();
        let fake = Arc::new(FakeSyms { hits });
        let c = SymbolEvidenceCollector::new(fake);
        let out = c.collect("S", 6).await.unwrap();
        // Weights run 6,5,4,3,2,1 — every one non-zero and distinct.
        let weights: Vec<u32> = out.iter().map(|e| e.decision_weight).collect();
        assert_eq!(weights, vec![6, 5, 4, 3, 2, 1]);
    }
}
