//! Contract — the explicit success criteria / scope / effect-budget object
//! attached to every non-trivial run.

use super::ContractId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    pub id: ContractId,
    pub goal: String,
    #[serde(default)]
    pub non_goals: Vec<String>,
    pub success_criteria: Vec<String>,
    pub scope: Scope,
    pub effect_budget: EffectBudget,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub include_paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Chronon CP-2: wall-clock budget for the entire session, in whole
    /// seconds. When set, the TurnDriver races the adapter stream against
    /// this deadline and aborts with `TurnAborted { reason:
    /// TimeExceeded }` on overrun. `None` = no wall-clock enforcement,
    /// identical to pre-CP-2 behaviour.
    ///
    /// Stored as u64 seconds (not `Duration`) so the JSONL wire shape is
    /// `"max_wall_secs": 900` — human-readable and replay-stable.
    #[serde(default)]
    pub max_wall_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EffectBudget {
    #[serde(default)]
    pub max_apply_local: u32,
    #[serde(default)]
    pub max_apply_repo: u32,
    #[serde(default)]
    pub max_network_reads: u32,
}

/// β: additive delta to an `EffectBudget`, carried on a `SessionEvent::
/// ContractAmended` when the user grants a mid-run budget extension.
///
/// Per-class additivity preserves the invariant-4 property that
/// `EffectCounter` tallies stay meaningful after an amend — the counter
/// bumps each tool call against a ceiling that only ever grows.
///
/// Multiplier cap (research §10.4, plan §β): a single grant's delta is
/// clamped by `contract::apply_amend_clamped` to `min(delta, current)`
/// per class — i.e. the ceiling at most doubles per amend. Without this
/// brake, a socially-engineered user could raise the ceiling unboundedly
/// across repeated prompts in one turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EffectBudgetDelta {
    #[serde(default)]
    pub apply_local: u32,
    #[serde(default)]
    pub apply_repo: u32,
    #[serde(default)]
    pub network_reads: u32,
}
