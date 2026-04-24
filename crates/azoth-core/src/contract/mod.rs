//! Contract lifecycle: draft → lint → accept → amend (β).
//!
//! `accept_and_persist` writes a `ContractAccepted` event to the JSONL log so a
//! resuming session can rehydrate the full contract (not just its id).
//!
//! β adds:
//! - `apply_amend_clamped` / `apply_amend_clamped_against_base` — enforce the
//!   ≤2× multiplier cap.
//! - `apply_amends` — replay fold from the JSONL `ContractAmended` stream.

use crate::event_store::JsonlWriter;
use crate::schemas::{
    Contract, ContractId, EffectBudget, EffectBudgetDelta, EffectClass, Scope, SessionEvent,
};
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
            // CP-2: wall-clock budget opt-in; draft contracts leave it
            // unset so default behaviour stays identical to pre-CP-2.
            max_wall_secs: None,
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

/// β: apply a budget delta to `contract.effect_budget` in place, clamping
/// each class to ≤2× the pre-amend ceiling. Returns the actually-applied
/// delta (the clamped values) so the caller can persist exactly what was
/// committed to the contract, not the proposed amount that might be larger.
///
/// The clamp is per-class and independent: a single `EffectBudgetDelta`
/// carrying `{ apply_local: 60, apply_repo: 3, network_reads: 0 }` against
/// `{ max_apply_local: 20, max_apply_repo: 5, max_network_reads: 0 }`
/// applies as `{ apply_local: 20, apply_repo: 3, network_reads: 0 }` —
/// i.e. apply_local hits the 2× ceiling while apply_repo passes through
/// under the cap.
///
/// Using `saturating_add` on the final write makes an adversarial delta
/// equal to `u32::MAX` still produce a defined ceiling (u32::MAX) rather
/// than overflow.
pub fn apply_amend_clamped(
    contract: &mut Contract,
    proposed: &EffectBudgetDelta,
) -> EffectBudgetDelta {
    let clamped = EffectBudgetDelta {
        apply_local: proposed
            .apply_local
            .min(contract.effect_budget.max_apply_local),
        apply_repo: proposed
            .apply_repo
            .min(contract.effect_budget.max_apply_repo),
        network_reads: proposed
            .network_reads
            .min(contract.effect_budget.max_network_reads),
    };
    contract.effect_budget.max_apply_local = contract
        .effect_budget
        .max_apply_local
        .saturating_add(clamped.apply_local);
    contract.effect_budget.max_apply_repo = contract
        .effect_budget
        .max_apply_repo
        .saturating_add(clamped.apply_repo);
    contract.effect_budget.max_network_reads = contract
        .effect_budget
        .max_network_reads
        .saturating_add(clamped.network_reads);
    clamped
}

/// β: driver-side variant of the clamp for use when the `Contract` is held
/// behind a shared reference and cannot be mutated in place. Clamps
/// against `max_base + bonus` (the effective ceiling that overflowed),
/// returns the applied delta; the caller is responsible for folding the
/// returned delta into its own ceiling-bonus counters.
///
/// `class` narrows the clamp to one class — callers that offered a
/// single-class extension get a single-class delta back, preventing
/// accidental multi-class amend from a narrowly-scoped approval.
pub fn apply_amend_clamped_against_base(
    max_base: u32,
    bonus: u32,
    proposed: &EffectBudgetDelta,
    class: EffectClass,
) -> EffectBudgetDelta {
    // Effective ceiling at the moment of overflow. A delta ≤ this value
    // keeps the new ceiling ≤2× the effective ceiling.
    let cap = max_base.saturating_add(bonus);
    match class {
        EffectClass::ApplyLocal => EffectBudgetDelta {
            apply_local: proposed.apply_local.min(cap),
            apply_repo: 0,
            network_reads: 0,
        },
        EffectClass::ApplyRepo => EffectBudgetDelta {
            apply_local: 0,
            apply_repo: proposed.apply_repo.min(cap),
            network_reads: 0,
        },
        // Other classes do not flow through the amend path in β.
        _ => EffectBudgetDelta::default(),
    }
}

/// β: replay helper. Fold every `EffectBudgetDelta` onto `contract`,
/// saturating each class at u32::MAX. Does NOT enforce the 2× cap —
/// that happens at grant time via `apply_amend_clamped`; JSONL only
/// carries already-clamped deltas. Use on resume to rebuild the
/// effective ceiling.
pub fn apply_amends(contract: &mut Contract, amends: &[EffectBudgetDelta]) {
    for d in amends {
        contract.effect_budget.max_apply_local = contract
            .effect_budget
            .max_apply_local
            .saturating_add(d.apply_local);
        contract.effect_budget.max_apply_repo = contract
            .effect_budget
            .max_apply_repo
            .saturating_add(d.apply_repo);
        contract.effect_budget.max_network_reads = contract
            .effect_budget
            .max_network_reads
            .saturating_add(d.network_reads);
    }
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
