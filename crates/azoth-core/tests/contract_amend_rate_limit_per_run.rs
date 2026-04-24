//! β: the ≤6-per-run brake. `amends_this_run` is never reset — even
//! across turns, the 7th grant attempt declines. Per-run takes
//! precedence over per-turn in the error hint ordering so the user
//! sees the harder limit first.

use azoth_core::authority::{
    ApprovalPolicyV1, AuthorityDecision, AuthorityEngine, CapabilityStore, MAX_AMENDS_PER_RUN,
};
use azoth_core::schemas::EffectCounter;

#[test]
fn amends_this_run_at_limit_declines_regardless_of_turn_counter() {
    let caps = CapabilityStore::new();
    let engine = AuthorityEngine::new(&caps, ApprovalPolicyV1);
    let counter = EffectCounter {
        // amends_this_turn = 0 — turn just started fresh.
        amends_this_turn: 0,
        amends_this_run: MAX_AMENDS_PER_RUN,
        ..Default::default()
    };
    match engine.authorize_budget_extension("apply_repo", 5, &counter) {
        AuthorityDecision::NotAvailable { hint } => {
            assert_eq!(hint, "amend rate limit exceeded: max 6 per run");
        }
        other => panic!("expected NotAvailable, got {other:?}"),
    }
}

#[test]
fn per_run_brake_takes_precedence_over_per_turn_in_error_ordering() {
    // Both limits are violated — the engine must surface the per-run
    // one. Reason: the per-turn brake clears on the next turn; the
    // per-run brake does not. Telling the user "max 2 per turn" when
    // they'd actually be blocked by the per-run cap would send them
    // down a false recovery path.
    let caps = CapabilityStore::new();
    let engine = AuthorityEngine::new(&caps, ApprovalPolicyV1);
    let counter = EffectCounter {
        amends_this_turn: 10,
        amends_this_run: 10,
        ..Default::default()
    };
    match engine.authorize_budget_extension("apply_local", 20, &counter) {
        AuthorityDecision::NotAvailable { hint } => {
            assert!(
                hint.contains("per run"),
                "per-run brake should win precedence, got: {hint}"
            );
        }
        other => panic!("expected NotAvailable, got {other:?}"),
    }
}
