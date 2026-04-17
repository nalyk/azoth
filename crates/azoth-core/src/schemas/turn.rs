//! Turn-scoped state types — the ContextPacket, Checkpoint, and associated
//! requests/responses that cross the TurnDriver ↔ adapter ↔ tool seams.

use super::{
    CheckpointId, ContentBlock, ContextPacketId, ContractId, Message, ToolDefinition, TurnId,
    Usage, UsageDelta,
};
use serde::{Deserialize, Serialize};

/// Five-lane Context Kernel output. Packing rules live in `context::kernel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPacket {
    pub id: ContextPacketId,
    pub contract_id: ContractId,
    pub turn_id: TurnId,
    pub digest: String, // sha256 hex of the serialized packet

    pub constitution_lane: ConstitutionLane,
    pub working_set_lane: Vec<WorkingSetItem>,
    pub evidence_lane: Vec<EvidenceItem>,
    pub checkpoint_lane: Option<CheckpointSummary>,
    pub exit_criteria_lane: ExitCriteria,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstitutionLane {
    pub contract_digest: String,
    pub tool_schemas_digest: String,
    pub policy_version: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingSetItem {
    pub label: String,
    pub summary: String,
}

/// Evidence item consumed by the Context Kernel's `evidence_lane`.
///
/// v2 Sprint 4 adds two additive optional fields — `lane` and
/// `rerank_score` — so composite collectors can tag each item with
/// provenance (which backend produced it) and a post-rerank score for
/// forensic replay. Both carry `#[serde(default)]` so v1.5 JSONL
/// sessions deserialise unchanged (risk ledger #1 — schema stability).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub label: String,
    pub artifact_ref: Option<String>,
    pub inline: Option<String>,
    pub decision_weight: u32,
    /// Provenance tag: which collector lane produced this item.
    /// Conventional values: `"graph"`, `"symbol"`, `"lexical"`,
    /// `"fts"`. `None` on pre-v2 items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Statistical reranker score (e.g. RRF). `None` when the
    /// identity reranker is used or on pre-v2 items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub id: CheckpointId,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitCriteria {
    pub step_goal: String,
    pub rubric: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub turn_id: TurnId,
    pub summary: String,
    #[serde(default)]
    pub evidence_artifacts: Vec<String>,
}

/// Cache breakpoint advice for the adapter layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHints {
    /// Place an Anthropic `cache_control: ephemeral` breakpoint at the end of
    /// the constitution lane. OpenAI adapter ignores this.
    pub constitution_boundary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    #[serde(default)]
    pub cache_hints: CacheHints,
    pub metadata: RequestMetadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub run_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    ContentFilter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterErrorCode {
    RateLimited,
    AuthFailed,
    InvalidRequest,
    ContextTooLong,
    ContentFilter,
    Timeout,
    Network,
    Unknown,
}

/// Streaming event yielded by a provider adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart,
    ContentBlockStart {
        index: usize,
        block: ContentBlockStub,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    InputJsonDelta {
        index: usize,
        partial_json: String,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        usage_delta: UsageDelta,
    },
    MessageStop,
    Error {
        code: AdapterErrorCode,
        message: String,
        retryable: bool,
    },
}

/// Skeleton of a content block announced at stream start — only the type
/// discriminator and minimal identifiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStub {
    Text,
    ToolUse { id: super::ToolUseId, name: String },
    Thinking,
}
