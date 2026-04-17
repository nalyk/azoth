//! Regression rate — the fraction of validator outcomes that went
//! from pass → fail between two adjacent snapshots.
//!
//! A snapshot is a set of `(validator_name, status)` pairs captured
//! at some point in time: typically the `ValidatorResult` events of
//! two adjacent committed turns within one run, or of two runs of
//! the same seed task. The metric is scoped to validators present in
//! both snapshots; validators that only exist on one side are
//! dropped to avoid punishing test churn.
//!
//! ### Definition
//!
//! Given `prior` and `current` each a set of
//! `(name, status ∈ {Pass, Fail, Skip})`:
//!
//! ```text
//! overlap   = { n : ∃ (n, _) in prior ∧ ∃ (n, _) in current }
//! baseline  = | { n ∈ overlap : prior[n] = Pass } |
//! regressed = | { n ∈ overlap : prior[n] = Pass ∧ current[n] = Fail } |
//! rate      = regressed / baseline   (0.0 when baseline = 0)
//! ```
//!
//! Why `baseline = prior-Pass` instead of `|overlap|`: a validator
//! that was *Fail* in prior and stays *Fail* in current is not a
//! regression — treating the denominator as the whole overlap would
//! let the rate shrink as flaky tests accumulate, which is the exact
//! pathology this metric is meant to surface.
//!
//! `Skip` outcomes are never regressions (a skip means "not
//! evaluated" — lack of signal, not a failure). If a validator flips
//! from Pass → Skip we do not flag it; the prior Pass just doesn't
//! contribute to the new snapshot. If it flips from Skip → Fail
//! there is no prior-Pass baseline to regress against, so the
//! metric ignores it. Dashboards wanting to track skip churn should
//! add their own metric rather than conflating it into regression
//! rate.
//!
//! ### Duplicates
//!
//! If the same validator name appears twice on one side (rare, but
//! allowed — a turn may run the same validator twice under different
//! configurations that share a name), the last-seen pair wins.
//! Callers who care about that distinction should disambiguate the
//! name before handing the snapshot in.

use std::collections::HashMap;

use crate::schemas::ValidatorStatus;

/// A validator outcome pair. Construct from `SessionEvent::ValidatorResult`
/// events at the caller — keeping this module schema-agnostic past
/// `ValidatorStatus` lets it reuse across test harnesses that don't
/// ship the full event type.
pub type Outcome = (String, ValidatorStatus);

/// Compute the regression rate between two snapshots. See module docs
/// for the exact definition. Returns `0.0` when no validator in
/// `current` was `Pass` in `prior` — an honest "nothing at risk, so
/// nothing regressed" rather than `None`.
pub fn regression_rate(prior: &[Outcome], current: &[Outcome]) -> f64 {
    let prior: HashMap<&str, ValidatorStatus> =
        prior.iter().map(|(n, s)| (n.as_str(), *s)).collect();
    let current: HashMap<&str, ValidatorStatus> =
        current.iter().map(|(n, s)| (n.as_str(), *s)).collect();

    let mut baseline = 0usize;
    let mut regressed = 0usize;
    for (name, prior_status) in &prior {
        if !matches!(prior_status, ValidatorStatus::Pass) {
            continue;
        }
        let Some(curr_status) = current.get(name) else {
            continue;
        };
        // PR #10 codex P2: prior=Pass AND current=Skip is "no signal"
        // per the module-level doc, so it must NOT contribute to the
        // denominator. Leaving it in diluted the rate — e.g. one
        // Pass→Skip + one Pass→Fail reported 0.5 instead of the
        // intended 1.0, masking real regressions behind flaky/skipped
        // validators. The baseline counts *evaluable* at-risk
        // validators, not every prior Pass that kept a name slot
        // under any status on the new side.
        if matches!(curr_status, ValidatorStatus::Skip) {
            continue;
        }
        baseline += 1;
        if matches!(curr_status, ValidatorStatus::Fail) {
            regressed += 1;
        }
    }

    if baseline == 0 {
        0.0
    } else {
        regressed as f64 / baseline as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(n: &str) -> Outcome {
        (n.to_string(), ValidatorStatus::Pass)
    }
    fn fail(n: &str) -> Outcome {
        (n.to_string(), ValidatorStatus::Fail)
    }
    fn skip(n: &str) -> Outcome {
        (n.to_string(), ValidatorStatus::Skip)
    }

    #[test]
    fn no_regression_when_both_pass() {
        let prior = [pass("v1"), pass("v2")];
        let current = [pass("v1"), pass("v2")];
        assert_eq!(regression_rate(&prior, &current), 0.0);
    }

    #[test]
    fn one_of_two_regressed_is_one_half() {
        let prior = [pass("v1"), pass("v2")];
        let current = [fail("v1"), pass("v2")];
        assert!((regression_rate(&prior, &current) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn persistent_fail_is_not_a_regression() {
        // v1 was Fail in prior and stays Fail: baseline excludes it,
        // so it doesn't count as regressed.
        let prior = [fail("v1"), pass("v2")];
        let current = [fail("v1"), fail("v2")];
        // Only v2 is in the baseline (prior-Pass ∩ overlap).
        // It regressed → 1/1.
        assert!((regression_rate(&prior, &current) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn validator_only_in_one_snapshot_is_dropped() {
        // v2 only in prior → drops out. v3 only in current → drops out.
        // Baseline = {v1}, regressed = {v1} → 1.0.
        let prior = [pass("v1"), pass("v2")];
        let current = [fail("v1"), pass("v3")];
        assert!((regression_rate(&prior, &current) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn skip_does_not_contribute_to_regression() {
        // Pass → Skip: not a regression (no signal).
        let prior = [pass("v1"), pass("v2")];
        let current = [skip("v1"), pass("v2")];
        // Skip on the current side is "no signal" → v1 drops out of
        // the baseline entirely. Only v2 (pass → pass) remains, and
        // it did not flip to Fail → 0/1 = 0.0.
        assert_eq!(regression_rate(&prior, &current), 0.0);
    }

    /// PR #10 codex P2 regression guard: a mixed snapshot where one
    /// validator flips Pass → Skip and another flips Pass → Fail
    /// must report the full regression rate (1.0) — the skip is "no
    /// signal" and must not dilute the denominator.
    #[test]
    fn mixed_skip_and_fail_does_not_dilute_rate() {
        let prior = [pass("v1"), pass("v2")];
        let current = [skip("v1"), fail("v2")];
        // v1 pass → skip: excluded from baseline.
        // v2 pass → fail: baseline=1, regressed=1 → 1.0.
        let r = regression_rate(&prior, &current);
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {r}");
    }

    #[test]
    fn zero_baseline_yields_zero() {
        // Prior had no Pass outcomes — nothing to regress.
        let prior = [fail("v1")];
        let current = [pass("v1")];
        assert_eq!(regression_rate(&prior, &current), 0.0);

        // Or: nothing overlaps.
        let prior = [pass("a")];
        let current = [pass("b")];
        assert_eq!(regression_rate(&prior, &current), 0.0);
    }

    #[test]
    fn empty_snapshots_are_zero() {
        let empty: [Outcome; 0] = [];
        assert_eq!(regression_rate(&empty, &empty), 0.0);
    }
}
