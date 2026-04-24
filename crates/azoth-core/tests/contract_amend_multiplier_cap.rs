//! β: `contract::apply_amend_clamped` enforces the ≤2× multiplier cap
//! regardless of the proposed delta. A caller proposing `3× current`
//! gets back a delta equal to `current`; the contract's ceiling ends
//! at `2× current`, not `4× current`.
//!
//! Covers plan §β key design decision 3 and the regression the test
//! `contract_amend_multiplier_cap` was written for.

use azoth_core::contract::{apply_amend_clamped, apply_amend_clamped_against_base};
use azoth_core::schemas::{
    Contract, ContractId, EffectBudget, EffectBudgetDelta, EffectClass, Scope,
};

fn contract_20() -> Contract {
    Contract {
        id: ContractId::from("ctr_cap".to_string()),
        goal: "cap test".into(),
        non_goals: Vec::new(),
        success_criteria: vec!["cap holds".into()],
        scope: Scope {
            include_paths: vec![".".into()],
            exclude_paths: Vec::new(),
            max_turns: Some(4),
            max_wall_secs: None,
        },
        effect_budget: EffectBudget {
            max_apply_local: 20,
            max_apply_repo: 5,
            max_network_reads: 0,
        },
        notes: vec!["accepted".into()],
    }
}

#[test]
fn three_times_current_clamps_to_current_and_doubles_ceiling() {
    let mut c = contract_20();
    let proposed = EffectBudgetDelta {
        apply_local: 60, // 3× current
        apply_repo: 0,
        network_reads: 0,
    };
    let applied = apply_amend_clamped(&mut c, &proposed);
    assert_eq!(
        applied.apply_local, 20,
        "delta must be clamped to current, not proposed"
    );
    assert_eq!(
        c.effect_budget.max_apply_local, 40,
        "ceiling doubles — never more"
    );
}

#[test]
fn exactly_current_passes_through_unclamped() {
    let mut c = contract_20();
    let proposed = EffectBudgetDelta {
        apply_local: 20, // == current
        apply_repo: 0,
        network_reads: 0,
    };
    let applied = apply_amend_clamped(&mut c, &proposed);
    assert_eq!(applied.apply_local, 20);
    assert_eq!(c.effect_budget.max_apply_local, 40);
}

#[test]
fn under_current_passes_through_unclamped() {
    let mut c = contract_20();
    let proposed = EffectBudgetDelta {
        apply_local: 7,
        apply_repo: 2,
        network_reads: 0,
    };
    let applied = apply_amend_clamped(&mut c, &proposed);
    assert_eq!(applied.apply_local, 7);
    assert_eq!(applied.apply_repo, 2);
    assert_eq!(c.effect_budget.max_apply_local, 27);
    assert_eq!(c.effect_budget.max_apply_repo, 7);
}

#[test]
fn per_class_clamp_is_independent() {
    // One class proposed above its 2× limit, another well under its
    // limit — the in-limit class must pass through at full value.
    let mut c = contract_20();
    let proposed = EffectBudgetDelta {
        apply_local: 100, // 5× current — will clamp
        apply_repo: 3,    // under 2× — passes through
        network_reads: 0,
    };
    let applied = apply_amend_clamped(&mut c, &proposed);
    assert_eq!(applied.apply_local, 20);
    assert_eq!(applied.apply_repo, 3);
    assert_eq!(c.effect_budget.max_apply_local, 40);
    assert_eq!(c.effect_budget.max_apply_repo, 8);
}

#[test]
fn against_base_variant_clamps_against_effective_ceiling() {
    // The driver-side helper clamps against `base + bonus` (the
    // effective ceiling that overflowed), not the base alone. A prior
    // amend raises the bar for the next amend.
    let proposed = EffectBudgetDelta {
        apply_local: 80,
        apply_repo: 0,
        network_reads: 0,
    };
    // base 20 + bonus 20 → effective 40 → cap 40 → delta at most 40.
    let applied = apply_amend_clamped_against_base(20, 20, &proposed, EffectClass::ApplyLocal);
    assert_eq!(applied.apply_local, 40);
    assert_eq!(applied.apply_repo, 0, "narrow class — other classes zero");
}

#[test]
fn zero_current_stays_zero_after_clamp() {
    // 2× 0 is 0 — a ceiling of 0 cannot be extended via this path.
    // Edge case: the caller MUST set max_network_reads > 0 in the
    // contract before any amend can raise it.
    let mut c = contract_20();
    let proposed = EffectBudgetDelta {
        apply_local: 0,
        apply_repo: 0,
        network_reads: 10,
    };
    let applied = apply_amend_clamped(&mut c, &proposed);
    assert_eq!(applied.network_reads, 0);
    assert_eq!(c.effect_budget.max_network_reads, 0);
}
