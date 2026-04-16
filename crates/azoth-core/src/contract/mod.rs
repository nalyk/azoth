//! Contract lifecycle: draft → lint → accept. Amend deferred.
//!
//! `accept_and_persist` writes a `ContractAccepted` event to the JSONL log so a
//! resuming session can rehydrate the full contract (not just its id).

use crate::event_store::JsonlWriter;
use crate::schemas::{Contract, ContractId, EffectBudget, Scope, SessionEvent};
use std::collections::HashSet;
use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("lint failed: {0}")]
    Lint(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
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

/// Lint rules (v1). Every failure produces a single, actionable message.
pub fn lint(contract: &Contract) -> Result<(), ContractError> {
    let bail = |msg: &str| Err(ContractError::Lint(msg.to_string()));

    if contract.goal.trim().is_empty() {
        return bail("goal is empty");
    }
    if contract.success_criteria.is_empty() {
        return bail("contract must have at least one success criterion");
    }
    if contract
        .success_criteria
        .iter()
        .any(|c| c.trim().is_empty())
    {
        return bail("success criteria must not contain blank entries");
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for c in &contract.success_criteria {
        if !seen.insert(c.trim()) {
            return Err(ContractError::Lint(format!(
                "duplicate success criterion: {:?}",
                c.trim()
            )));
        }
    }
    if contract.scope.include_paths.is_empty() {
        return bail("scope.include_paths must name at least one path");
    }
    if let Some(0) = contract.scope.max_turns {
        return bail("scope.max_turns must be > 0 when set");
    }
    let includes: HashSet<&str> = contract
        .scope
        .include_paths
        .iter()
        .map(|s| s.as_str())
        .collect();
    for ex in &contract.scope.exclude_paths {
        if includes.contains(ex.as_str()) {
            return Err(ContractError::Lint(format!(
                "scope.exclude_paths overlaps include_paths: {ex:?}"
            )));
        }
    }
    for ng in &contract.non_goals {
        if ng.trim().is_empty() {
            return bail("non_goals must not contain blank entries");
        }
    }
    Ok(())
}

pub fn accept(mut contract: Contract) -> Result<Contract, ContractError> {
    lint(&contract)?;
    if !contract.notes.iter().any(|n| n == "accepted") {
        contract.notes.push("accepted".into());
    }
    Ok(contract)
}

/// Lint, accept, and append a `ContractAccepted` event to the session log.
/// Returns the accepted contract on success. On lint failure, nothing is
/// written.
pub fn accept_and_persist(
    writer: &mut JsonlWriter,
    contract: Contract,
    timestamp: impl Into<String>,
) -> Result<Contract, ContractError> {
    let accepted = accept(contract)?;
    writer.append(&SessionEvent::ContractAccepted {
        contract: accepted.clone(),
        timestamp: timestamp.into(),
    })?;
    Ok(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Contract {
        let mut c = draft("fix token refresh bug");
        c.success_criteria.push("auth tests pass".into());
        c
    }

    #[test]
    fn lint_accepts_well_formed_contract() {
        lint(&good()).unwrap();
    }

    #[test]
    fn lint_rejects_empty_goal() {
        let mut c = good();
        c.goal = "   ".into();
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn lint_rejects_missing_success_criteria() {
        let mut c = good();
        c.success_criteria.clear();
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn lint_rejects_blank_success_criterion() {
        let mut c = good();
        c.success_criteria.push("   ".into());
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn lint_rejects_duplicate_success_criteria() {
        let mut c = good();
        c.success_criteria.push("auth tests pass".into());
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn lint_rejects_empty_include_paths() {
        let mut c = good();
        c.scope.include_paths.clear();
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn lint_rejects_zero_max_turns() {
        let mut c = good();
        c.scope.max_turns = Some(0);
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn lint_rejects_include_exclude_overlap() {
        let mut c = good();
        c.scope.include_paths = vec!["src".into()];
        c.scope.exclude_paths = vec!["src".into()];
        assert!(matches!(lint(&c), Err(ContractError::Lint(_))));
    }

    #[test]
    fn accept_is_idempotent_in_notes() {
        let c = accept(good()).unwrap();
        let c = accept(c).unwrap();
        let count = c.notes.iter().filter(|n| n.as_str() == "accepted").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn accept_and_persist_round_trips_via_reader() {
        use crate::event_store::{JsonlReader, JsonlWriter};
        use crate::schemas::{ContractId, RunId};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut w = JsonlWriter::open(&path).unwrap();
        w.append(&SessionEvent::RunStarted {
            run_id: RunId::from("run_ctr".to_string()),
            contract_id: ContractId::from("placeholder".to_string()),
            timestamp: "2026-04-15T12:00:00Z".into(),
        })
        .unwrap();

        let contract = accept_and_persist(&mut w, good(), "2026-04-15T12:00:01Z").unwrap();

        let r = JsonlReader::open(&path);
        let rehydrated = r.last_accepted_contract().unwrap().expect("contract");
        assert_eq!(rehydrated, contract);

        let replay = r.replayable().unwrap();
        assert!(replay
            .iter()
            .any(|e| matches!(e.0, SessionEvent::ContractAccepted { .. })));
    }
}
