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
            EffectClass::Observe
                | EffectClass::Stage
                | EffectClass::ApplyLocal
                | EffectClass::ApplyRepo
        )
    }
}

/// Per-run tally of effects consumed, indexed by effect class. Owned by the
/// TUI worker (or a test harness) across turns so the `TurnDriver` can short-
/// circuit a tool call when the contract's `EffectBudget` for that class
/// would be exceeded. On resume the worker recomputes this from the
/// replayable JSONL projection (see
/// [`JsonlReader::committed_run_progress`]); fresh sessions start at zero.
///
/// β: the three `*_ceiling_bonus` fields accumulate every granted
/// `ContractAmended` delta, so the driver's budget check reads the
/// effective ceiling as `contract.effect_budget.max_X + apply_X_ceiling_bonus`
/// without mutating the base `Contract` object through its shared reference.
/// JSONL replay rebuilds the bonuses the same way it rebuilds the
/// consumption tallies (see `committed_run_progress`) so resume observes
/// the same effective ceiling the live turn did.
///
/// `amends_this_turn` is reset to 0 each time `TurnDriver::drive_turn`
/// enters; `amends_this_run` is never reset (brake: ≤6 per run).
/// Kept `Copy` by staying all-u32 — so every existing test constructing
/// an `EffectCounter` by field literal keeps compiling via
/// `..Default::default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectCounter {
    pub apply_local: u32,
    pub apply_repo: u32,
    pub network_reads: u32,
    /// β: accumulated `apply_local` delta from granted amends, folded
    /// additively into the effective ceiling.
    pub apply_local_ceiling_bonus: u32,
    /// β: accumulated `apply_repo` delta from granted amends.
    pub apply_repo_ceiling_bonus: u32,
    /// β: accumulated `network_reads` delta from granted amends.
    pub network_reads_ceiling_bonus: u32,
    /// β: amend grants observed in the currently-open turn. Reset to 0
    /// at drive_turn entry. Drives the ≤2-per-turn brake in
    /// `AuthorityEngine::authorize_budget_extension`.
    pub amends_this_turn: u32,
    /// β: amend grants observed over the whole run. Never reset.
    /// Drives the ≤6-per-run brake.
    pub amends_this_run: u32,
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
