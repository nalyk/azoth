//! Sprint 6 verification: `tests/eval_regression.rs` — regression
//! rate from two adjacent validator snapshots.

use azoth_core::eval::regression_rate;
use azoth_core::schemas::ValidatorStatus;

fn pass(n: &str) -> (String, ValidatorStatus) {
    (n.to_string(), ValidatorStatus::Pass)
}
fn fail(n: &str) -> (String, ValidatorStatus) {
    (n.to_string(), ValidatorStatus::Fail)
}

#[test]
fn regression_rate_flags_new_fails_only() {
    // Prior: v1, v2, v3 all pass.
    // Current: v1 pass, v2 fail (regressed), v3 pass.
    // Baseline (prior-Pass ∩ overlap) = 3; regressed = 1 → 1/3.
    let prior = [pass("v1"), pass("v2"), pass("v3")];
    let current = [pass("v1"), fail("v2"), pass("v3")];
    let r = regression_rate(&prior, &current);
    assert!((r - 1.0 / 3.0).abs() < 1e-9, "got {r}");
}

#[test]
fn regression_rate_ignores_persistent_fails() {
    // v1 was Fail → not in baseline. v2 flipped Pass → Fail → 1/1 = 1.0.
    let prior = [fail("v1"), pass("v2")];
    let current = [fail("v1"), fail("v2")];
    let r = regression_rate(&prior, &current);
    assert!((r - 1.0).abs() < 1e-9, "got {r}");
}

#[test]
fn regression_rate_zero_on_all_pass() {
    let prior = [pass("v1"), pass("v2")];
    let current = [pass("v1"), pass("v2")];
    assert_eq!(regression_rate(&prior, &current), 0.0);
}

#[test]
fn regression_rate_zero_on_empty_baseline() {
    // No validator in `current` was Pass in `prior`; nothing to regress.
    let prior = [fail("v1")];
    let current = [pass("v1")];
    assert_eq!(regression_rate(&prior, &current), 0.0);
}
