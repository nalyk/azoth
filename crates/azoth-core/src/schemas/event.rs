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
            | TurnInterrupted { turn_id, .. } => Some(turn_id),
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
