//! JSONL session event records — the turn-scoped, append-only protocol that
//! every replay and projection reads. See `event_store::jsonl`.

use super::{
    ApprovalId, ArtifactId, CallGroupId, CapabilityTokenId, CheckpointId, ContentBlock,
    ContextPacketId, Contract, ContractId, EffectClass, EffectRecord, RunId, SandboxTier,
    ToolUseId, TurnId, Usage, UsageDelta,
};
use serde::{Deserialize, Serialize};

/// Reasons a turn can fail to commit. `turn_interrupted` is distinct from
/// `turn_aborted`: interrupted = the turn never completed (cancel, crash);
/// aborted = the turn ran to a definite negative outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    UserCancel,
    AdapterError,
    ValidatorFail,
    ApprovalDenied,
    TokenBudget,
    RuntimeError,
    Crash,
}

/// Union of every line that can appear in a session's JSONL log.
///
/// The `type` discriminator matches the wire shape documented in
/// `docs/draft_plan.md` section "Turn-scoped JSONL session log". Every variant
/// carries the turn_id it belongs to so projections can drop a turn whole
/// without reparsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    RunStarted {
        run_id: RunId,
        contract_id: ContractId,
        timestamp: String,
    },
    /// An accepted (lint-clean) contract snapshot, persisted so a resuming
    /// session can rehydrate the full object — not just its id. Multiple
    /// `ContractAccepted` events may appear over a session's lifetime; the
    /// reader treats the last one as authoritative.
    ContractAccepted {
        contract: Contract,
        timestamp: String,
    },
    TurnStarted {
        turn_id: TurnId,
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_turn: Option<TurnId>,
        timestamp: String,
    },
    ContextPacket {
        turn_id: TurnId,
        packet_id: ContextPacketId,
        packet_digest: String,
    },
    ModelRequest {
        turn_id: TurnId,
        request_digest: String,
        profile_id: String,
    },
    ContentBlock {
        turn_id: TurnId,
        index: usize,
        block: ContentBlock,
    },
    EffectRecord {
        turn_id: TurnId,
        effect: EffectRecord,
    },
    ToolResult {
        turn_id: TurnId,
        tool_use_id: ToolUseId,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_artifact: Option<ArtifactId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_group: Option<CallGroupId>,
    },
    ValidatorResult {
        turn_id: TurnId,
        validator: String,
        status: ValidatorStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ApprovalRequest {
        turn_id: TurnId,
        approval_id: ApprovalId,
        effect_class: EffectClass,
        tool_name: String,
        summary: String,
    },
    ApprovalGranted {
        turn_id: TurnId,
        approval_id: ApprovalId,
        token: CapabilityTokenId,
        scope: ApprovalScope,
    },
    ApprovalDenied {
        turn_id: TurnId,
        approval_id: ApprovalId,
    },
    SandboxEntered {
        turn_id: TurnId,
        tier: SandboxTier,
    },
    Checkpoint {
        turn_id: TurnId,
        checkpoint_id: CheckpointId,
    },
    TurnCommitted {
        turn_id: TurnId,
        outcome: CommitOutcome,
        usage: Usage,
        /// User message that triggered this turn, captured at turn-start.
        /// Enables JSONL-only replay of the cross-turn history without
        /// treating intermediate `ContentBlock` events as user-visible text.
        /// `None` for turns written by pre-v1.5 driver versions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_input: Option<Vec<ContentBlock>>,
        /// Final assistant content (the `EndTurn`/`StopSequence` response,
        /// with no unpaired `ToolUse` blocks). This is what the caller folds
        /// back into the next turn's history for cross-turn memory; persisting
        /// it lets a restarted worker rebuild that same history from JSONL.
        /// `None` for turns written by pre-v1.5 driver versions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_assistant: Option<Vec<ContentBlock>>,
    },
    TurnAborted {
        turn_id: TurnId,
        reason: AbortReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default)]
        usage: Usage,
    },
    TurnInterrupted {
        turn_id: TurnId,
        reason: AbortReason,
        #[serde(default)]
        partial_usage: UsageDelta,
    },
    /// A lexical/fts retrieval call completed. Emitted from retrieval
    /// call sites so the eval plane (Sprint 6) can measure precision@k
    /// and compare backends without re-running queries. `backend` names
    /// the concrete impl (`ripgrep`, `fts`, or future `composite`).
    /// All numeric fields carry `#[serde(default)]` so older binaries
    /// can tolerate forward-compat additions.
    RetrievalQueried {
        turn_id: TurnId,
        backend: String,
        query: String,
        #[serde(default)]
        result_count: u32,
        #[serde(default)]
        latency_ms: u64,
    },
    /// A symbol-index query completed (Sprint 2). `matched` carries the
    /// session-ephemeral `SymbolId`s returned for the query — never
    /// treated as a durable reference by any replay consumer because
    /// IDs regenerate every reindex pass (invariant #1). `backend`
    /// names the concrete impl (`sqlite` today; future `composite` in
    /// Sprint 4). All non-essential fields carry `#[serde(default)]`
    /// for forward-compat.
    SymbolResolved {
        turn_id: TurnId,
        backend: String,
        query: String,
        #[serde(default)]
        matched: Vec<i64>,
    },
    /// A `TestPlan` was computed by an `ImpactValidator` at the turn's
    /// validate phase (Sprint 5). `changed_files` is the diff input
    /// the selector operated on; `selected_tests` lists the plan's
    /// ordered `TestId` values (plain strings on the wire). `selector`
    /// names the concrete impl (`cargo_test`, future `pytest`,
    /// `jest`, `go_test`), and `selector_version` bumps on heuristic
    /// changes so replay can detect plan drift without re-running.
    ///
    /// `rationale` and `confidence` mirror the selector's per-test
    /// provenance from `TestPlan`, positionally aligned with
    /// `selected_tests`. They let the SQLite `test_impact` mirror
    /// populate its `selected_because` / `confidence` columns without
    /// re-running the selector, and preserve forensic detail
    /// (PR #9 codex P2: plan payload would otherwise be dropped).
    ///
    /// `ran_at` is the ISO-8601 UTC timestamp captured when the
    /// validator emitted the plan. Required because the `test_impact`
    /// table defines `ran_at` as `NOT NULL` (m0005) — without it the
    /// mirror cannot insert a consistent row (PR #9 gemini HIGH).
    ///
    /// All non-essential fields carry `#[serde(default)]` for
    /// forward-compat: older binaries tolerate future extensions, and
    /// the vec fields use `skip_serializing_if = Vec::is_empty` so
    /// pre-Sprint-5 JSONL byte shape stays cache-prefix-stable.
    ImpactComputed {
        turn_id: TurnId,
        #[serde(default)]
        selector: String,
        #[serde(default)]
        selector_version: u32,
        /// ISO-8601 UTC wall-clock at emit time. Empty only for
        /// forward-compat fixtures that predate this field; real
        /// writers MUST populate it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ran_at: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changed_files: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        selected_tests: Vec<String>,
        /// Positionally aligned with `selected_tests`: `rationale[i]`
        /// explains why `selected_tests[i]` was picked. Empty when
        /// the selector returned no rationale — older writers, or
        /// a future minimal writer.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rationale: Vec<String>,
        /// Positionally aligned with `selected_tests`. Empty when the
        /// selector produced no confidence scores.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        confidence: Vec<f32>,
    },
}

impl SessionEvent {
    /// The turn_id this event is scoped to, if any. `RunStarted` has none.
    pub fn turn_id(&self) -> Option<&TurnId> {
        use SessionEvent::*;
        match self {
            RunStarted { .. } | ContractAccepted { .. } => None,
            TurnStarted { turn_id, .. }
            | ContextPacket { turn_id, .. }
            | ModelRequest { turn_id, .. }
            | ContentBlock { turn_id, .. }
            | EffectRecord { turn_id, .. }
            | ToolResult { turn_id, .. }
            | ValidatorResult { turn_id, .. }
            | ApprovalRequest { turn_id, .. }
            | ApprovalGranted { turn_id, .. }
            | ApprovalDenied { turn_id, .. }
            | SandboxEntered { turn_id, .. }
            | Checkpoint { turn_id, .. }
            | TurnCommitted { turn_id, .. }
            | TurnAborted { turn_id, .. }
            | TurnInterrupted { turn_id, .. }
            | RetrievalQueried { turn_id, .. }
            | SymbolResolved { turn_id, .. }
            | ImpactComputed { turn_id, .. } => Some(turn_id),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionEvent::TurnCommitted { .. }
                | SessionEvent::TurnAborted { .. }
                | SessionEvent::TurnInterrupted { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorStatus {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitOutcome {
    Success,
    PartialSuccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalScope {
    Once,
    Session,
    ScopedPaths { paths: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrieval_queried_round_trips() {
        let ev = SessionEvent::RetrievalQueried {
            turn_id: TurnId::from("t_9".to_string()),
            backend: "fts".to_string(),
            query: "TurnDriver".to_string(),
            result_count: 7,
            latency_ms: 42,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"retrieval_queried""#), "{s}");
        let back: SessionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        assert_eq!(
            back.turn_id().map(|t| t.as_str()),
            Some("t_9"),
            "new variant must be covered by turn_id() match"
        );
    }

    #[test]
    fn symbol_resolved_round_trips_and_defaults_matched() {
        // With matched populated.
        let ev = SessionEvent::SymbolResolved {
            turn_id: TurnId::from("t_5".to_string()),
            backend: "sqlite".to_string(),
            query: "TurnDriver".to_string(),
            matched: vec![1, 2, 3],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"symbol_resolved""#), "{s}");
        let back: SessionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.turn_id().map(|t| t.as_str()), Some("t_5"));

        // Forward-compat: missing `matched` defaults to empty vec.
        let wire = r#"{
            "type":"symbol_resolved",
            "turn_id":"t_6",
            "backend":"sqlite",
            "query":"foo"
        }"#;
        let back2: SessionEvent = serde_json::from_str(wire).unwrap();
        match back2 {
            SessionEvent::SymbolResolved { matched, .. } => assert!(matched.is_empty()),
            other => panic!("expected SymbolResolved, got {other:?}"),
        }
    }

    #[test]
    fn impact_computed_round_trips_and_defaults_on_forward_compat() {
        let ev = SessionEvent::ImpactComputed {
            turn_id: TurnId::from("t_77".to_string()),
            selector: "cargo_test".to_string(),
            selector_version: 1,
            ran_at: "2026-04-17T13:45:00Z".to_string(),
            changed_files: vec!["src/foo.rs".into()],
            selected_tests: vec!["azoth_core::foo::tests::bar".into()],
            rationale: vec!["direct filename match".into()],
            confidence: vec![1.0],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"impact_computed""#), "{s}");
        assert!(s.contains(r#""ran_at":"2026-04-17T13:45:00Z""#), "{s}");
        assert!(s.contains(r#""rationale""#), "{s}");
        let back: SessionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.turn_id().map(|t| t.as_str()), Some("t_77"));

        // Forward-compat: a minimal writer (e.g. the pre-Sprint-5
        // PR-#9-snapshot of this event, which had no ran_at /
        // rationale / confidence) must still deserialise under the
        // current schema.
        let wire = r#"{
            "type":"impact_computed",
            "turn_id":"t_78"
        }"#;
        let back2: SessionEvent = serde_json::from_str(wire).unwrap();
        match back2 {
            SessionEvent::ImpactComputed {
                selector,
                selector_version,
                ran_at,
                changed_files,
                selected_tests,
                rationale,
                confidence,
                ..
            } => {
                assert!(selector.is_empty());
                assert_eq!(selector_version, 0);
                assert!(ran_at.is_empty());
                assert!(changed_files.is_empty());
                assert!(selected_tests.is_empty());
                assert!(rationale.is_empty());
                assert!(confidence.is_empty());
            }
            other => panic!("expected ImpactComputed, got {other:?}"),
        }
    }

    #[test]
    fn impact_computed_empty_vecs_omit_from_wire_for_cache_stability() {
        // When a selector emits a plan with no rationale or
        // confidence (e.g. NullImpactSelector, or a custom selector
        // that doesn't score), the wire shape must NOT include empty
        // `rationale: []` / `confidence: []` arrays — leaving them in
        // would shift the byte prefix and break Anthropic prompt-
        // cache hit rate on any event-replay context. See memory:
        // pattern_serde_skip_serializing_if_for_cache_stability.
        let ev = SessionEvent::ImpactComputed {
            turn_id: TurnId::from("t_90".to_string()),
            selector: "null".into(),
            selector_version: 0,
            ran_at: "2026-04-17T00:00:00Z".into(),
            changed_files: vec!["src/a.rs".into()],
            selected_tests: Vec::new(),
            rationale: Vec::new(),
            confidence: Vec::new(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("\"rationale\""), "wire leaked rationale: {s}");
        assert!(!s.contains("\"confidence\""), "wire leaked confidence: {s}");
        assert!(
            !s.contains("\"selected_tests\""),
            "wire leaked selected_tests: {s}"
        );
    }

    #[test]
    fn retrieval_queried_tolerates_missing_optional_numeric_fields() {
        // Forward-compat guard: the v2 Sprint 1 plan marks numeric
        // fields `#[serde(default)]` so a future binary writing without
        // them still deserialises here.
        let wire = r#"{
            "type":"retrieval_queried",
            "turn_id":"t_x",
            "backend":"ripgrep",
            "query":"needle"
        }"#;
        let ev: SessionEvent = serde_json::from_str(wire).unwrap();
        match ev {
            SessionEvent::RetrievalQueried {
                result_count,
                latency_ms,
                ..
            } => {
                assert_eq!(result_count, 0);
                assert_eq!(latency_ms, 0);
            }
            other => panic!("expected RetrievalQueried, got {other:?}"),
        }
    }
}
