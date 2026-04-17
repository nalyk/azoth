//! m0006: eval-plane measurements (Sprint 6, localization@k).
//!
//! Adds one table, `eval_runs`, mirroring the wire shape of
//! `SessionEvent::EvalSampled`. A row lands per (run_id, turn_id,
//! metric, task_id) tuple; seed-task sweeps write one row per task,
//! turn-embedded signals write one row with `task_id = ''`.
//!
//! ## Schema decisions
//!
//! - **Composite PK `(run_id, turn_id, metric, task_id)`**. A turn
//!   may emit multiple metrics (precision@5, regression_rate, …), and
//!   a seed-task sweep writes many rows per synthetic turn — the PK
//!   reflects both scopes. `task_id` defaults to `''` so turn-embedded
//!   emitters never collide under a single metric per turn.
//! - **`value REAL NOT NULL`**. The v2 metrics are all `[0.0, 1.0]`
//!   rate-style scalars, but `REAL` leaves room for counts or latencies
//!   in v2.1 without a schema bump.
//! - **`k INTEGER NOT NULL DEFAULT 0`**. The cut-off for precision@k /
//!   recall@k. `0` signals a k-independent scalar metric.
//! - **`sampled_at TEXT NOT NULL`**. ISO-8601 UTC at emit time.
//!   Allows time-ordered replay without reparsing JSONL. A pre-review
//!   event missing the field is recorded under a `'1970-01-01T00:00:00Z'`
//!   sentinel, identical to the m0005 `ran_at` fallback.
//! - **No foreign key into the `turns` table.** `eval_runs` is a
//!   forensic index, not a source of truth (CRIT-1: JSONL is
//!   authoritative). An `EvalSampled` event may be emitted against a
//!   synthetic turn_id (seed sweeps) that never commits to `turns`.
//!
//! ## Indexes
//!
//! A secondary index on `metric` exists so `/status` and future
//! dashboards can rank metrics cheaply across the whole history; the
//! composite PK's leftmost prefix already indexes `run_id`.
//!
//! ## Idempotence
//!
//! `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS` — same
//! convention as m0001..m0005. Running m0006 twice converges.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS eval_runs (
            run_id      TEXT    NOT NULL,
            turn_id     TEXT    NOT NULL,
            metric      TEXT    NOT NULL,
            value       REAL    NOT NULL,
            k           INTEGER NOT NULL DEFAULT 0,
            task_id     TEXT    NOT NULL DEFAULT '',
            sampled_at  TEXT    NOT NULL,
            PRIMARY KEY (run_id, turn_id, metric, task_id)
        );

        CREATE INDEX IF NOT EXISTS eval_runs_by_metric_idx
            ON eval_runs(metric);
        "#,
    )?;
    Ok(())
}
