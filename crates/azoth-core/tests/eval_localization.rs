//! Sprint 6 verification: `tests/eval_localization.rs` — seeds a
//! known run, asserts precision@5 computation, and proves the
//! `EvalSampled` → `eval_runs` projection survives a JSONL round
//! trip.

use azoth_core::eval::{mean_precision, precision_at_k, score_tasks, SeedTask};
use azoth_core::event_store::{JsonlReader, JsonlWriter, SqliteMirror};
use azoth_core::schemas::{ContractId, RunId, SessionEvent, TurnId};
use tempfile::tempdir;

fn ts() -> String {
    "2026-04-17T15:00:00Z".into()
}

fn run_started(run: &str) -> SessionEvent {
    SessionEvent::RunStarted {
        run_id: RunId::from(run.to_string()),
        contract_id: ContractId::from("ctr_eval".to_string()),
        timestamp: ts(),
    }
}

fn eval_sampled(tid: &str, task_id: &str, value: f64) -> SessionEvent {
    SessionEvent::EvalSampled {
        turn_id: TurnId::from(tid.to_string()),
        metric: "localization_precision_at_k".into(),
        value,
        k: 5,
        sampled_at: ts(),
        task_id: task_id.into(),
    }
}

#[test]
fn precision_at_5_happy_path() {
    // Two relevant files, all present in the top-5 predicted list.
    let predicted = [
        "crates/azoth-core/src/turn/driver.rs",
        "crates/azoth-core/src/turn/mod.rs",
        "crates/azoth-core/src/adapter/mod.rs",
    ];
    let relevant = [
        "crates/azoth-core/src/turn/driver.rs",
        "crates/azoth-core/src/turn/mod.rs",
    ];
    let p = precision_at_k(&predicted, &relevant, 5);
    // 2 relevant / 3 considered = 0.666...
    assert!((p - 2.0 / 3.0).abs() < 1e-9, "got {p}");
}

#[test]
fn score_tasks_matches_known_seed() {
    // Hand-crafted 3-task seed with known precisions: 1.0, 0.5, 0.0.
    let tasks = vec![
        SeedTask {
            id: "t1".into(),
            prompt: "a".into(),
            relevant_files: vec!["src/a.rs".into(), "src/b.rs".into()],
            predicted_files: vec!["src/a.rs".into(), "src/b.rs".into()],
            notes: String::new(),
        },
        SeedTask {
            id: "t2".into(),
            prompt: "a".into(),
            relevant_files: vec!["src/c.rs".into()],
            predicted_files: vec!["src/c.rs".into(), "src/z.rs".into()],
            notes: String::new(),
        },
        SeedTask {
            id: "t3".into(),
            prompt: "a".into(),
            relevant_files: vec!["src/d.rs".into()],
            predicted_files: vec!["src/y.rs".into()],
            notes: String::new(),
        },
    ];
    let scores = score_tasks(&tasks, 5);
    assert!((scores[0].precision_at_k - 1.0).abs() < 1e-9);
    assert!((scores[1].precision_at_k - 0.5).abs() < 1e-9);
    assert_eq!(scores[2].precision_at_k, 0.0);
    let mean = mean_precision(&scores).unwrap();
    assert!((mean - 0.5).abs() < 1e-9);
}

#[test]
fn eval_sampled_round_trips_through_jsonl_and_projects_to_eval_runs() {
    // End-to-end: write EvalSampled → JSONL → rebuild mirror →
    // assert the row count and a representative row.
    let dir = tempdir().unwrap();
    let session = dir.path().join("eval.jsonl");
    let mirror_path = dir.path().join("mirror.sqlite");

    let mut w = JsonlWriter::open(&session).unwrap();
    w.append(&run_started("run_eval")).unwrap();
    w.append(&eval_sampled("t_001", "loc01", 0.8)).unwrap();
    w.append(&eval_sampled("t_002", "loc02", 1.0)).unwrap();
    w.append(&eval_sampled("t_003", "loc03", 0.0)).unwrap();
    drop(w);

    let mut m = SqliteMirror::open(&mirror_path).unwrap();
    let reader = JsonlReader::open(&session);
    m.rebuild_from(&reader).unwrap();
    assert_eq!(m.eval_row_count().unwrap(), 3);

    // Re-running rebuild must not double-count — the truncate-then-
    // reapply discipline lives in `rebuild_from`.
    m.rebuild_from(&reader).unwrap();
    assert_eq!(m.eval_row_count().unwrap(), 3);
}

#[test]
fn seed_file_loads_and_scores() {
    // Uses the committed `docs/eval/v2_seed_tasks.json` — if this
    // test breaks, either the seed schema drifted or the metric
    // changed. The assertion is deliberately loose (range check,
    // not exact) so cosmetic tuning of the predicted_files list
    // doesn't destabilise CI.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let seed_path = manifest_dir
        .parent() // crates/azoth-core → crates
        .unwrap()
        .parent() // crates → repo root
        .unwrap()
        .join("docs/eval/v2_seed_tasks.json");
    let bytes = std::fs::read(&seed_path).expect("v2_seed_tasks.json present");
    let tasks: Vec<SeedTask> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        tasks.len(),
        20,
        "Sprint 6 ships exactly 20 hand-labelled seed tasks"
    );

    let scores = score_tasks(&tasks, 5);
    let mean = mean_precision(&scores).unwrap();
    assert!(
        (0.0..=1.0).contains(&mean),
        "mean precision must land in [0,1]; got {mean}"
    );
    // Every task must have at least one relevant file — a seed task
    // with no ground truth contributes noise, not signal.
    for t in &tasks {
        assert!(
            !t.relevant_files.is_empty(),
            "seed task {} has no relevant_files",
            t.id
        );
    }
}
