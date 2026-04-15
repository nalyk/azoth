//! Contract lifecycle: draft → lint → accept. Amend deferred.

use crate::schemas::{Contract, ContractId, EffectBudget, Scope};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("lint failed: {0}")]
    Lint(String),
}

pub fn draft(goal: impl Into<String>) -> Contract {
    Contract {
        id: ContractId::new(),
        goal: goal.into(),
        non_goals: Vec::new(),
        success_criteria: Vec::new(),
        scope: Scope {
            include_paths: vec![".".into()],
            exclude_paths: Vec::new(),
            max_turns: Some(32),
        },
        effect_budget: EffectBudget {
            max_apply_local: 20,
            max_apply_repo: 5,
            max_network_reads: 0,
        },
        notes: Vec::new(),
    }
}

pub fn lint(contract: &Contract) -> Result<(), ContractError> {
    if contract.goal.trim().is_empty() {
        return Err(ContractError::Lint("goal is empty".into()));
    }
    if contract.success_criteria.is_empty() {
        return Err(ContractError::Lint(
            "contract must have at least one success criterion".into(),
        ));
    }
    Ok(())
}

pub fn accept(mut contract: Contract) -> Result<Contract, ContractError> {
    lint(&contract)?;
    contract.notes.push("accepted".into());
    Ok(contract)
}
