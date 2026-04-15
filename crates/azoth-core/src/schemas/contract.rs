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
