//! Localization@k — precision of the predicted file list against a
//! ground-truth file list per seed task.
//!
//! The metric answers: "of the top-k files the retrieval stack
//! surfaced, how many actually needed to be touched for the task to
//! land?" A score of 1.0 means the top-k were all relevant; 0.0 means
//! none of them were. The mean across a seed sweep is the single
//! number v2 ships against a threshold (plan §Verification gate 8:
//! `localization@5 ≥ 0.75`).
//!
//! ### Definition
//!
//! Given predicted list P (ordered, may contain duplicates), relevant
//! set R (unordered, deduped), and cut-off k:
//!
//! ```text
//! top_k(P)      = P[..min(k, |P|)]      (preserves input order)
//! matches       = |{ p ∈ top_k(P) : p ∈ R } — dedupe before count|
//! denom         = min(k, |top_k(P)|)    = min(k, |P|)
//! precision@k   = matches / denom       (0.0 when denom = 0)
//! ```
//!
//! The denom is `min(k, |P|)`, not `k` — otherwise a seed task where
//! retrieval returned fewer than `k` items would cap its score below
//! 1.0 on the definition alone, which would be a metric bug rather
//! than a retrieval shortfall. A task with zero predictions scores
//! 0.0, consistent with "the retriever produced nothing" being a
//! total miss.
//!
//! Duplicates within the top-k count once — a retriever emitting the
//! same file twice should not be rewarded for confirmation. The
//! implementation de-dupes after truncation, so order of first
//! appearance is preserved (matters when `k < |P|` and the dupe
//! straddles the boundary).
//!
//! ### Path canonicalisation
//!
//! Paths are compared after trimming trailing whitespace and
//! normalising `\\` to `/`. This keeps cross-platform JSONL replays
//! stable: the SPrint 3 co-edit graph already canonicalises paths
//! that way when writing the `co_edit_edges` rows, so aligning here
//! keeps precision@k measurements consistent with the underlying
//! retrieval wire format.

use serde::{Deserialize, Serialize};

/// One hand-labelled task in a seed sweep.
///
/// `id` is the wire label surfaced in `SessionEvent::EvalSampled.task_id`.
/// `prompt` documents what a user-facing task would look like — not
/// consumed by the metric itself. `relevant_files` is the human-
/// labelled ground truth. `predicted_files` is the retrieval output
/// the sweep scores; the v2 CLI populates this from a chosen
/// retrieval backend, but the seed file may also carry a
/// pre-computed snapshot for reproducible CI runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeedTask {
    pub id: String,
    pub prompt: String,
    pub relevant_files: Vec<String>,
    #[serde(default)]
    pub predicted_files: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

/// Per-task localization score, aligned positionally with a seed
/// `Vec<SeedTask>`. `matched` is the de-duped intersection count;
/// `predicted_considered` is the top-k window size actually scored
/// (may be less than k when the retriever returned fewer items).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskScore {
    pub task_id: String,
    pub precision_at_k: f64,
    pub k: u32,
    pub matched: u32,
    pub relevant_total: u32,
    pub predicted_considered: u32,
}

/// Pure metric: precision@k for a single (predicted, relevant) pair.
///
/// - `predicted`: ordered list, top-ranked first.
/// - `relevant`: ground-truth set; iteration order does not matter.
/// - `k`: cut-off (0 short-circuits to 0.0 to match the "no window"
///   edge case).
pub fn precision_at_k<S1, S2>(predicted: &[S1], relevant: &[S2], k: usize) -> f64
where
    S1: AsRef<str>,
    S2: AsRef<str>,
{
    if k == 0 || predicted.is_empty() {
        return 0.0;
    }
    let relevant: std::collections::HashSet<String> =
        relevant.iter().map(|s| canonicalise(s.as_ref())).collect();

    let mut considered = 0usize;
    let mut seen = std::collections::HashSet::with_capacity(k);
    let mut matched = 0usize;
    for p in predicted.iter().take(k).map(|s| canonicalise(s.as_ref())) {
        if !seen.insert(p.clone()) {
            continue;
        }
        considered += 1;
        if relevant.contains(&p) {
            matched += 1;
        }
    }

    if considered == 0 {
        0.0
    } else {
        matched as f64 / considered as f64
    }
}

/// Sweep a seed set, produce a per-task score vector. Callers
/// aggregate via `mean_precision` or build an `EvalReport` around
/// this. Pure function; no I/O, no side effects.
///
/// Implementation note: addresses PR #10 gemini-MED feedback — one
/// canonicalisation pass per task, no redundant HashSet rebuilds, and
/// `matched` is an integer counter rather than a float-reconstructed
/// derivation (`(precision * considered).round()`) that could drift
/// by one under f64 rounding.
pub fn score_tasks(tasks: &[SeedTask], k: u32) -> Vec<TaskScore> {
    let k_usize = k as usize;
    tasks
        .iter()
        .map(|t| {
            let relevant_set: std::collections::HashSet<String> =
                t.relevant_files.iter().map(|p| canonicalise(p)).collect();
            let relevant_total = relevant_set.len() as u32;

            let mut seen = std::collections::HashSet::with_capacity(k_usize);
            let mut considered: u32 = 0;
            let mut matched: u32 = 0;
            for p in t
                .predicted_files
                .iter()
                .take(k_usize)
                .map(|p| canonicalise(p))
            {
                if !seen.insert(p.clone()) {
                    continue;
                }
                considered += 1;
                if relevant_set.contains(&p) {
                    matched += 1;
                }
            }

            let precision_at_k = if considered == 0 {
                0.0
            } else {
                matched as f64 / considered as f64
            };

            TaskScore {
                task_id: t.id.clone(),
                precision_at_k,
                k,
                matched,
                relevant_total,
                predicted_considered: considered,
            }
        })
        .collect()
}

/// Mean precision across a scored task vector. Returns `None` when
/// the vector is empty — honest "no data" signal, not a misleading
/// `Some(0.0)`.
pub fn mean_precision(scores: &[TaskScore]) -> Option<f64> {
    if scores.is_empty() {
        return None;
    }
    let sum: f64 = scores.iter().map(|s| s.precision_at_k).sum();
    Some(sum / scores.len() as f64)
}

fn canonicalise(path: &str) -> String {
    path.trim().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_relevant_in_top_k_scores_one() {
        let predicted = ["src/a.rs", "src/b.rs", "src/c.rs"];
        let relevant = ["src/a.rs", "src/b.rs", "src/c.rs"];
        assert_eq!(precision_at_k(&predicted, &relevant, 3), 1.0);
    }

    #[test]
    fn none_relevant_scores_zero() {
        let predicted = ["src/a.rs", "src/b.rs"];
        let relevant = ["src/z.rs"];
        assert_eq!(precision_at_k(&predicted, &relevant, 5), 0.0);
    }

    #[test]
    fn partial_match_uses_min_k_predicted_as_denominator() {
        // 1 of 2 retrieved matches → 0.5, not 1/5.
        let predicted = ["src/a.rs", "src/z.rs"];
        let relevant = ["src/a.rs"];
        assert!((precision_at_k(&predicted, &relevant, 5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn duplicates_in_top_k_count_once() {
        // `src/a.rs` repeats at rank 1+2; denom should be 2 (a and b),
        // not 3.
        let predicted = ["src/a.rs", "src/a.rs", "src/b.rs"];
        let relevant = ["src/a.rs"];
        assert!((precision_at_k(&predicted, &relevant, 3) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn windows_top_k_correctly() {
        // Truncate to k=2; only a and b considered, both relevant.
        let predicted = ["src/a.rs", "src/b.rs", "src/z.rs"];
        let relevant = ["src/a.rs", "src/b.rs"];
        assert!((precision_at_k(&predicted, &relevant, 2) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn zero_k_or_empty_predicted_returns_zero() {
        let predicted = ["src/a.rs"];
        let relevant = ["src/a.rs"];
        assert_eq!(precision_at_k(&predicted, &relevant, 0), 0.0);
        let empty: [&str; 0] = [];
        assert_eq!(precision_at_k(&empty, &relevant, 5), 0.0);
    }

    #[test]
    fn canonicalises_paths_for_cross_platform_stability() {
        let predicted = ["src\\a.rs", " src/b.rs "];
        let relevant = ["src/a.rs", "src/b.rs"];
        assert!((precision_at_k(&predicted, &relevant, 5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn score_tasks_round_trips_and_mean_precision_aggregates() {
        let tasks = vec![
            SeedTask {
                id: "t1".into(),
                prompt: "fix a".into(),
                relevant_files: vec!["src/a.rs".into()],
                predicted_files: vec!["src/a.rs".into(), "src/z.rs".into()],
                notes: String::new(),
            },
            SeedTask {
                id: "t2".into(),
                prompt: "fix b".into(),
                relevant_files: vec!["src/b.rs".into()],
                predicted_files: vec!["src/b.rs".into()],
                notes: String::new(),
            },
        ];
        let scores = score_tasks(&tasks, 5);
        assert_eq!(scores.len(), 2);
        assert!((scores[0].precision_at_k - 0.5).abs() < 1e-9);
        assert_eq!(scores[0].predicted_considered, 2);
        assert_eq!(scores[0].matched, 1);
        assert!((scores[1].precision_at_k - 1.0).abs() < 1e-9);
        let mean = mean_precision(&scores).unwrap();
        assert!((mean - 0.75).abs() < 1e-9);
    }

    #[test]
    fn mean_precision_none_on_empty_vector() {
        let scores: Vec<TaskScore> = Vec::new();
        assert!(mean_precision(&scores).is_none());
    }
}
