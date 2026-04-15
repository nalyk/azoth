//! Effect classes, sandbox tiers, and the compile-time-exhaustive mapping
//! between them.

use super::{ArtifactId, EffectRecordId, ToolUseId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Observe,
    Stage,
    ApplyLocal,
    ApplyRepo,
    ApplyRemoteReversible,
    ApplyRemoteStateful,
    ApplyIrreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SandboxTier {
    A,
    B,
    C,
    D,
}

impl From<EffectClass> for SandboxTier {
    fn from(ec: EffectClass) -> Self {
        match ec {
            EffectClass::Observe => SandboxTier::A,
            EffectClass::Stage => SandboxTier::B,
            EffectClass::ApplyLocal => SandboxTier::B,
            EffectClass::ApplyRepo => SandboxTier::B,
            EffectClass::ApplyRemoteReversible => SandboxTier::C,
            EffectClass::ApplyRemoteStateful => SandboxTier::D,
            EffectClass::ApplyIrreversible => SandboxTier::D,
        }
    }
}

impl EffectClass {
    /// Tiers C and D are architectural hooks only in v1 — any effect landing
    /// on those tiers should be rejected with `EffectNotAvailable`.
    pub fn is_available_in_v1(self) -> bool {
        matches!(
            self,
            EffectClass::Observe | EffectClass::Stage | EffectClass::ApplyLocal | EffectClass::ApplyRepo
        )
    }
}

/// Per-run tally of effects consumed, indexed by effect class. Owned by the
/// TUI worker (or a test harness) across turns so the `TurnDriver` can short-
/// circuit a tool call when the contract's `EffectBudget` for that class
/// would be exceeded. Defaulted to zero at worker start; resume-time
/// recomputation from JSONL is a follow-up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectCounter {
    pub apply_local: u32,
    pub apply_repo: u32,
    pub network_reads: u32,
}

/// One recorded effect against the world. Emitted on every tool dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub id: EffectRecordId,
    pub tool_use_id: ToolUseId,
    pub class: EffectClass,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_artifact: Option<ArtifactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
