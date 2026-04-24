//! β: the ≤2-per-turn brake. Once `amends_this_turn >= 2`,
//! `AuthorityEngine::authorize_budget_extension` returns
//! `NotAvailable` with the exact hint string — the TurnDriver's
//! abort path reads this and surfaces it to the user.

use azoth_core::authority::{
    ApprovalPolicyV1, AuthorityDecision, AuthorityEngine, CapabilityStore, MAX_AMENDS_PER_TURN,
};
use azoth_core::schemas::EffectCounter;

#[test]
fn amends_this_turn_at_limit_declines_to_prompt() {
    let caps = CapabilityStore::new();
    let engine = AuthorityEngine::new(&caps, ApprovalPolicyV1);
    let counter = EffectCounter {
        amends_this_turn: MAX_AMENDS_PER_TURN,
        amends_this_run: MAX_AMENDS_PER_TURN, // well under the 6/run brake
        ..Default::default()
    };
    match engine.authorize_budget_extension("apply_local", 20, &counter) {
        AuthorityDecision::NotAvailable { hint } => {
            assert_eq!(hint, "amend rate limit exceeded: max 2 per turn");
        }
        other => panic!("expected NotAvailable, got {other:?}"),
    }
}

#[test]
fn zero_current_is_not_available_even_when_brakes_clear() {
    // R1 (PR #31 codex P1): a contract with `max_X = 0` cannot be
    // extended by an amend (2 × 0 = 0, clamp yields 0, grant would
    // be a no-op). The engine MUST decline the offer before asking
    // the user rather than prompting for a grant that bypasses the
    // budget once.
    let caps = CapabilityStore::new();
    let engine = AuthorityEngine::new(&caps, ApprovalPolicyV1);
    let counter = EffectCounter::default();
    match engine.authorize_budget_extension("network_reads", 0, &counter) {
        AuthorityDecision::NotAvailable { hint } => {
            assert!(
                hint.contains("zero ceiling"),
                "expected zero-ceiling hint, got: {hint}"
            );
        }
        other => panic!("expected NotAvailable for current=0, got {other:?}"),
    }
}

#[test]
fn amends_this_turn_below_limit_offers_extension() {
    let caps = CapabilityStore::new();
    let engine = AuthorityEngine::new(&caps, ApprovalPolicyV1);
    // Two grants in a run is fine; the per-turn brake only trips at 2.
    let counter = EffectCounter {
        amends_this_turn: 1,
        amends_this_run: 1,
        ..Default::default()
    };
    match engine.authorize_budget_extension("apply_local", 20, &counter) {
        AuthorityDecision::RequireBudgetExtension {
            label,
            current,
            proposed,
            ..
        } => {
            assert_eq!(label, "apply_local");
            assert_eq!(current, 20);
            assert_eq!(proposed, 40, "β proposes 2× current");
        }
        other => panic!("expected RequireBudgetExtension, got {other:?}"),
    }
}
