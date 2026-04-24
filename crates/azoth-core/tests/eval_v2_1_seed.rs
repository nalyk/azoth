//! v2.1 seed gate — asserts the expanded 50-task seed (20 v2 + 10 Python,
//! 10 TypeScript, 10 Go) sits at or above `localization@5 ≥ 0.45` in seed mode.
//!
//! This pins the hand-labelled seed itself, not the live-retrieval pipeline —
//! seed mode scores each task's pre-populated `predicted_files` against
//! `relevant_files`, matching the contract that `azoth eval run` uses when no
//! `--live-retrieval` flag is set. Live-retrieval dogfood lives in
//! `docs/dogfood/v2.1/` as qualitative writeups (the metric there is different
//! enough that a single threshold does not make sense for both paths).
//!
//! Loose-but-honest shape: if the seed grows beyond 50 or the floor drifts, a
//! deliberate edit to this test is required — no silent drift. If a future
//! round wants to tune the per-task `predicted_files`, this test catches any
//! tuning that pushes the mean below the plan's ship gate.

use azoth_core::eval::{mean_precision, score_tasks, SeedTask};

#[test]
fn v2_1_seed_loads_ships_50_tasks_and_meets_localization_at_5_floor() {
    // Walk up two levels from the crate's CARGO_MANIFEST_DIR to reach the
    // workspace root. Sibling `tests/eval_localization.rs::seed_file_loads_and_scores`
    // uses the identical pattern. Gemini's PR #28 MED asked about a
    // workspace-relative path helper — no such helper exists in
    // `azoth-core` today, and extracting one for two call sites is below
    // the `pattern_extract_helper_over_inline_reviewer_patch.md` bar
    // (n ≥ 3 sibling sites). If PR-K or a later sprint adds a third
    // site, extract then. Descriptive `expect` messages make CI failures
    // actionable if the workspace layout moves.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let seed_path = manifest_dir
        .parent()
        .expect("CARGO_MANIFEST_DIR (crates/azoth-core) must have a parent (crates/)")
        .parent()
        .expect("crates/ must have a parent (workspace root)")
        .join("docs/eval/v2.1_seed_tasks.json");
    let bytes = std::fs::read(&seed_path)
        .unwrap_or_else(|e| panic!("failed to read v2.1 seed at {}: {e}", seed_path.display()));
    let tasks: Vec<SeedTask> = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse v2.1 seed as Vec<SeedTask>: {e}"));
    assert_eq!(
        tasks.len(),
        50,
        "v2.1 seed ships exactly 50 hand-labelled tasks (20 v2 + 10 Py + 10 TS + 10 Go)"
    );

    // Every task must have a non-empty relevant_files — a zero-ground-truth
    // task contributes noise, not signal. Task IDs must be unique: duplicates
    // would make the SQLite mirror's composite PK (run_id, turn_id, metric,
    // task_id) collide, silently overwriting per-task rows when the eval CLI
    // writes the synthetic session JSONL (see `crates/azoth/src/eval.rs`
    // around the `write_eval_session` path). Gemini PR #28 flagged this.
    let mut seen_ids = std::collections::HashSet::with_capacity(tasks.len());
    for t in &tasks {
        assert!(
            seen_ids.insert(t.id.as_str()),
            "duplicate task id {} in seed — breaks eval_runs composite PK",
            t.id
        );
        assert!(
            !t.relevant_files.is_empty(),
            "seed task {} has no relevant_files",
            t.id
        );
        assert!(
            !t.predicted_files.is_empty(),
            "seed task {} has no predicted_files",
            t.id
        );
    }

    // Per-language coverage (ids are the contract): 20 v2 rows use `locNN`,
    // then 10 each `py_NNN` / `ts_NNN` / `go_NNN`. Prefix + length + numeric
    // suffix guards against drift (e.g. a future `location_*` or `py_alpha`
    // id inflating a bucket silently). Gemini PR #28 R4 flagged the loose
    // `starts_with` check — regex dep would be overkill for 4 call sites.
    let count_pattern = |prefix: &str, digits: usize| {
        tasks
            .iter()
            .filter(|t| {
                // `strip_prefix` returns `Option<&str>` and handles UTF-8
                // boundaries safely — unlike raw `t.id[prefix.len()..]` which
                // panics if `prefix.len()` lands mid-codepoint. Current
                // prefixes are ASCII so the panic is unreachable, but the
                // idiom keeps the check robust against future non-ASCII ids.
                t.id.strip_prefix(prefix).is_some_and(|rest| {
                    rest.len() == digits && rest.chars().all(|c| c.is_ascii_digit())
                })
            })
            .count()
    };
    assert_eq!(count_pattern("loc", 2), 20, "20 original v2 tasks (locNN)");
    assert_eq!(count_pattern("py_", 3), 10, "10 Python tasks (py_NNN)");
    assert_eq!(count_pattern("ts_", 3), 10, "10 TypeScript tasks (ts_NNN)");
    assert_eq!(count_pattern("go_", 3), 10, "10 Go tasks (go_NNN)");

    let scores = score_tasks(&tasks, 5);
    let mean = mean_precision(&scores).expect(
        "mean_precision returned None — scores vector is empty, which contradicts the tasks.len() == 50 assertion above",
    );
    assert!(
        mean >= 0.45,
        "v2.1 plan §J gate: localization@5 {mean:.4} below 0.45 floor"
    );
    // Upper sanity bound — a trivially perfect seed would score 1.0 and mean
    // nothing. We ship at ~0.48 today; anything above 0.9 is almost certainly
    // a seed-authoring bug (predicted == relevant for every task).
    assert!(
        mean <= 0.9,
        "v2.1 seed scores suspiciously perfect at {mean:.4} — check that predicted_files are realistic supersets of relevant_files"
    );
}
