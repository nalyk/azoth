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
    /// The contract's side-effect / turn-count budget was exhausted.
    /// Deterministic stop requested by Azoth's own policy, not the
    /// provider. Pre-Sprint-7.5 this also fired when the model's
    /// `StopReason::MaxTokens` came back — that case now uses
    /// `ModelTruncated` instead.
    TokenBudget,
    RuntimeError,
    Crash,
    /// Sprint 7.5 (2026-04-18): the model's `StopReason::MaxTokens`
    /// path — provider-side output truncation mid-stream. Distinct
    /// from `TokenBudget` because the remediation differs
    /// (continue via `/continue` or raise max_tokens vs. amend the
    /// contract's side-effect budget).
    ModelTruncated,
    /// Sprint 7.5 (2026-04-18): TurnDriver pre-flight estimate of
    /// the outgoing `ModelTurnRequest` would exceed the active
    /// profile's `max_context_tokens`. Aborted before any network
    /// call; no `model_request` is emitted. Remediation: truncate
    /// history, switch profile, or raise the profile cap.
    ContextOverflow,
    /// Sprint 7.5 (2026-04-18): the sandbox layer refused to
    /// prepare for this effect class — either the class is not
    /// available in v2 (Tier C/D) or a runtime dependency is
    /// missing. The turn never dispatched the tool.
    SandboxDenied,
    /// Chronon CP-2: the contract's `scope.max_wall_secs` budget was
    /// exhausted mid-turn. Deterministic stop requested by the
    /// TurnDriver's wall-clock race, distinct from `TokenBudget`
    /// (turn-count exhaustion) and `ModelTruncated` (provider stop
    /// reason). Detail field carries the budget and actual spent
    /// seconds so operators can choose between "raise budget" and
    /// "split work."
    TimeExceeded,
    /// Chronon CP-2: an in-flight turn stopped emitting heartbeats
    /// for longer than the configured stall threshold. Resume-time
    /// reclassification of a dangling turn whose last event trail
    /// includes a heartbeat older than the threshold; for fresh
    /// sessions with no heartbeats this still reads as `Crash`.
    Stalled,
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
        /// Chronon CP-1: wall-clock (RFC3339 UTC) of the terminal
        /// transition. `None` on pre-CP-1 sessions so replay stays clean.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
    },
    TurnAborted {
        turn_id: TurnId,
        reason: AbortReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default)]
        usage: Usage,
        /// Chronon CP-1: wall-clock of the abort. `None` on pre-CP-1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
    },
    TurnInterrupted {
        turn_id: TurnId,
        reason: AbortReason,
        #[serde(default)]
        partial_usage: UsageDelta,
        /// Chronon CP-1: wall-clock of the interrupt. `None` on pre-CP-1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
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
    /// An eval-plane measurement (Sprint 6). Emitted by
    /// `azoth eval run` for each seed task and — in v2.1 — by the
    /// `TurnDriver` itself at commit time for in-flow retrieval-
    /// quality signals (invariant 6: every subsystem is eval-able).
    ///
    /// `metric` names the metric (`localization_precision_at_k`,
    /// `regression_rate`, future additions). `value` is the measured
    /// scalar in `[0.0, 1.0]` for rate-style metrics. `k` is the cut-
    /// off used by precision@k / recall@k metrics; `0` when the
    /// metric is scalar and k-independent.
    ///
    /// `task_id` identifies a seed-task scope. Empty for
    /// turn-embedded signals where the per-turn `turn_id` is the only
    /// needed scope.
    ///
    /// All non-essential fields carry `#[serde(default)]` so older
    /// binaries tolerate forward-compat additions without treating
    /// the absent field as a parse error.
    EvalSampled {
        turn_id: TurnId,
        #[serde(default)]
        metric: String,
        #[serde(default)]
        value: f64,
        #[serde(default)]
        k: u32,
        /// ISO-8601 UTC wall-clock at emit time. Empty only for
        /// forward-compat fixtures that predate this field; real
        /// emitters populate it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        sampled_at: String,
        /// Seed-task scope label. Empty when the sample is turn-
        /// embedded rather than seed-driven.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        task_id: String,
    },
    /// Chronon CP-2 heartbeat. Emitted by the TurnDriver on a throttled
    /// cadence during an open turn so operators (and the forensic
    /// projection) can distinguish "turn is alive and working" from
    /// "turn hung / crashed / deadlocked." `progress` tallies what the
    /// driver has produced since the previous heartbeat; if all three
    /// counters are zero the driver skips the heartbeat entirely (no-op
    /// throttle). Stored wall-clock via `at`, so replay is coherent.
    TurnHeartbeat {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        at: String,
        progress: HeartbeatProgress,
    },
}

/// Progress tally carried on each heartbeat. All fields are cumulative
/// since the turn opened, not deltas since the last heartbeat — makes
/// stall-detection arithmetic trivial (compare to the previous
/// heartbeat's counters to see whether anything moved).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatProgress {
    #[serde(default)]
    pub content_blocks: u32,
    #[serde(default)]
    pub tool_calls: u32,
    #[serde(default)]
    pub tokens_out: u64,
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
            | ImpactComputed { turn_id, .. }
            | EvalSampled { turn_id, .. }
            | TurnHeartbeat { turn_id, .. } => Some(turn_id),
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
    fn eval_sampled_round_trips_and_omits_empty_optionals() {
        // Populated emit: all fields round-trip exactly.
        let ev = SessionEvent::EvalSampled {
            turn_id: TurnId::from("t_eval_1".to_string()),
            metric: "localization_precision_at_k".to_string(),
            value: 0.8,
            k: 5,
            sampled_at: "2026-04-17T15:00:00Z".to_string(),
            task_id: "loc01".to_string(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"eval_sampled""#), "{s}");
        assert!(s.contains(r#""task_id":"loc01""#), "{s}");
        let back: SessionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.turn_id().map(|t| t.as_str()), Some("t_eval_1"));

        // Turn-embedded variant: empty task_id must not leak onto the
        // wire — cache-prefix stability (see pattern_serde_skip_
        // serializing_if_for_cache_stability).
        let ev2 = SessionEvent::EvalSampled {
            turn_id: TurnId::from("t_eval_2".to_string()),
            metric: "regression_rate".into(),
            value: 0.0,
            k: 0,
            sampled_at: "2026-04-17T15:01:00Z".into(),
            task_id: String::new(),
        };
        let s2 = serde_json::to_string(&ev2).unwrap();
        assert!(!s2.contains("\"task_id\""), "wire leaked task_id: {s2}");

        // Forward-compat: a minimal writer (no sampled_at, no task_id)
        // must still deserialise.
        let wire = r#"{
            "type":"eval_sampled",
            "turn_id":"t_eval_3"
        }"#;
        let back3: SessionEvent = serde_json::from_str(wire).unwrap();
        match back3 {
            SessionEvent::EvalSampled {
                metric,
                value,
                k,
                sampled_at,
                task_id,
                ..
            } => {
                assert!(metric.is_empty());
                assert_eq!(value, 0.0);
                assert_eq!(k, 0);
                assert!(sampled_at.is_empty());
                assert!(task_id.is_empty());
            }
            other => panic!("expected EvalSampled, got {other:?}"),
        }
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
