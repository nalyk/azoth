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
            | RetrievalQueried { turn_id, .. } => Some(turn_id),
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
