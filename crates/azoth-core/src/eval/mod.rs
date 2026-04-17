//! Eval plane (v2 Sprint 6) — localization@k + regression rate.
//!
//! Invariant 6 ("every subsystem is eval-able") shipped as a header in
//! v1.5 but had no teeth: retrieval was a black box that nobody could
//! score, and no projection of validator outcomes ever reached the
//! mirror. This module closes that gap. Functions here are pure and
//! library-shaped — they consume serde schema types and return
//! aggregate reports — so every caller (the `azoth eval run` CLI, the
//! TurnDriver's in-flow probes, future dashboards) agrees on
//! identical math.
//!
//! Scope fence (from `docs/v2_plan.md`):
//! - **In v2:** `localization_precision_at_k`, `regression_rate`.
//! - **Not in v2:** "mergeability proxy" — demoted to a research
//!   followup. No PR corpus exists to calibrate it; shipping a
//!   placeholder would bake false confidence.

pub mod localization;
pub mod regression;

pub use localization::{mean_precision, precision_at_k, score_tasks, SeedTask, TaskScore};
pub use regression::regression_rate;

use serde::{Deserialize, Serialize};

/// Aggregate report emitted by `azoth eval run` or any library caller
/// that sweeps a seed set. The scalar fields are the ship thresholds
/// the v2 gate measures against; raw per-task scores live in `tasks`
/// for forensic drill-down.
///
/// Optional-to-None fields signal "not measured on this run" rather
/// than "measured and zero" — calibration matters, `None` keeps the
/// dashboard honest when a sweep skips a metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReport {
    /// Mean precision@k across every task that contributed a
    /// `predicted_files` set. `None` when no task was scored.
    pub localization_precision_at_k: Option<f64>,
    /// Mean regression rate across adjacent validator snapshots.
    /// `None` when no snapshot pair was provided.
    pub regression_rate: Option<f64>,
    /// ISO-8601 UTC wall-clock at report time.
    pub sampled_at: String,
    /// The k used for precision@k. `0` when localization was not
    /// computed.
    pub k: u32,
    /// Number of seed tasks that contributed to the aggregate.
    pub tasks_scored: u32,
    /// Per-task scores, index-aligned with the seed input. Empty when
    /// localization was not computed.
    #[serde(default)]
    pub tasks: Vec<TaskScore>,
}

impl EvalReport {
    pub fn empty(sampled_at: impl Into<String>) -> Self {
        Self {
            localization_precision_at_k: None,
            regression_rate: None,
            sampled_at: sampled_at.into(),
            k: 0,
            tasks_scored: 0,
            tasks: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_report_round_trips() {
        let r = EvalReport {
            localization_precision_at_k: Some(0.75),
            regression_rate: Some(0.1),
            sampled_at: "2026-04-17T15:00:00Z".into(),
            k: 5,
            tasks_scored: 20,
            tasks: vec![TaskScore {
                task_id: "loc01".into(),
                precision_at_k: 0.8,
                k: 5,
                matched: 4,
                relevant_total: 5,
                predicted_considered: 5,
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: EvalReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}
