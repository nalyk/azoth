//! SQLite mirror of committed/aborted turns.
//!
//! JSONL remains the authoritative event log (CRIT-1). This mirror is a
//! rebuildable secondary index so `/status`, history, and future query APIs
//! don't have to re-read every session file.
//!
//! **HARD invariant** (`docs/draft_plan.md` ~line 308): the mirror only
//! observes *definite* outcome events — `TurnCommitted`, `TurnAborted`,
//! and (Sprint 5) `ImpactComputed`. `TurnInterrupted` and dangling
//! turns live in JSONL only. `RunStarted` is observed so the mirror
//! can tag rows with the current run; all other variants are silently
//! ignored.
//!
//! Sprint 5 projection: `ImpactComputed` events fan out one
//! `test_impact` row per selected test, carrying the per-test
//! `rationale[i]` as `selected_because`, the per-test `confidence[i]`,
//! and the event-level `ran_at` timestamp. Rebuild via
//! `rebuild_from` is idempotent — `ON CONFLICT(turn_id, test_id) DO
//! UPDATE` refreshes in place so multiple selectors targeting the
//! same test under one turn converge on the last-seen row.
//!
//! Schema evolution is handled by the hand-rolled migrator at
//! `event_store::migrations` — an ordered list of `MigrationStep` fns
//! guarded by `PRAGMA user_version`, applied inside a single transaction
//! so a half-migrated DB is never persisted.

use crate::event_store::jsonl::{JsonlReader, ProjectionError};
use crate::event_store::migrations;
use crate::schemas::{AbortReason, CommitOutcome, RunId, SessionEvent, TurnId, Usage};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MirrorError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("projection: {0}")]
    Projection(#[from] ProjectionError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown schema version {current}; this binary knows up to {known}")]
    UnknownSchema { current: u32, known: u32 },
}

/// Append-only SQLite index of terminal turn outcomes.
pub struct SqliteMirror {
    conn: Connection,
    path: PathBuf,
    current_run: Option<RunId>,
}

impl SqliteMirror {
    /// Open (or create) the mirror database at `path`. Creates the v1
    /// schema if the file is new; otherwise trusts the existing schema as
    /// long as `user_version` matches.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, MirrorError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn,
            path,
            current_run: None,
        })
    }

    /// In-memory variant for tests. Not persisted.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, MirrorError> {
        let mut conn = Connection::open_in_memory()?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
            current_run: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply one session event. No-op for every variant except
    /// `RunStarted` (sets current run), `TurnCommitted`, and `TurnAborted`.
    pub fn apply(&mut self, event: &SessionEvent) -> Result<(), MirrorError> {
        match event {
            SessionEvent::RunStarted { run_id, .. } => {
                self.current_run = Some(run_id.clone());
            }
            SessionEvent::TurnCommitted {
                turn_id,
                outcome,
                usage,
                ..
            } => {
                let run_id = self.run_id_for_upsert();
                self.upsert_turn(
                    &run_id,
                    turn_id,
                    commit_outcome_label(*outcome),
                    None,
                    usage,
                )?;
            }
            SessionEvent::TurnAborted {
                turn_id,
                reason,
                detail,
                usage,
            } => {
                let run_id = self.run_id_for_upsert();
                let label = abort_reason_label(*reason);
                self.upsert_turn(&run_id, turn_id, label, detail.as_deref(), usage)?;
            }
            SessionEvent::ImpactComputed {
                turn_id,
                ran_at,
                selected_tests,
                rationale,
                confidence,
                ..
            } => {
                // PR #9 codex P1: migration m0005 adds `test_impact`
                // but no projector populates it. Fan out one row per
                // selected test; rationale[i] and confidence[i] are
                // positionally aligned with selected_tests[i] per
                // the schema invariant enforced by
                // TestPlan::is_well_formed.
                self.upsert_impact(turn_id, ran_at, selected_tests, rationale, confidence)?;
            }
            SessionEvent::EvalSampled {
                turn_id,
                metric,
                value,
                k,
                sampled_at,
                task_id,
            } => {
                // Sprint 6: eval_runs mirror. Identical fallback
                // discipline to m0005's ran_at: a pre-review emitter
                // that omits `sampled_at` still lands under a sentinel
                // so the NOT NULL column is honoured.
                let run_id = self.run_id_for_upsert();
                self.upsert_eval(&run_id, turn_id, metric, *value, *k, task_id, sampled_at)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn upsert_impact(
        &self,
        turn_id: &TurnId,
        ran_at: &str,
        selected_tests: &[String],
        rationale: &[String],
        confidence: &[f32],
    ) -> Result<(), MirrorError> {
        if selected_tests.is_empty() {
            return Ok(());
        }
        // `ran_at` is NOT NULL in m0005. A pre-review event that
        // happens to be replayed without `ran_at` falls back to an
        // explicit sentinel so the row still lands; real emitters
        // populate it via `now_iso()` in `TurnDriver`.
        let ts = if ran_at.is_empty() {
            "1970-01-01T00:00:00Z"
        } else {
            ran_at
        };
        let mut stmt = self.conn.prepare(
            r#"
            INSERT INTO test_impact (
                turn_id, test_id, status, confidence, selected_because, ran_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(turn_id, test_id) DO UPDATE SET
                status = excluded.status,
                confidence = excluded.confidence,
                selected_because = excluded.selected_because,
                ran_at = excluded.ran_at
            "#,
        )?;
        for (i, test_id) in selected_tests.iter().enumerate() {
            // Positional lookup — missing rationale/confidence fall
            // back to neutral defaults so the row still lands.
            let why = rationale.get(i).cloned().unwrap_or_else(|| "".to_string());
            let score = confidence.get(i).copied().unwrap_or(0.0) as f64;
            stmt.execute(params![
                turn_id.as_str(),
                test_id,
                "planned", // v2 plan-only; TestRunner in v2.1 will flip to passed/failed
                score,
                why,
                ts,
            ])?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_eval(
        &self,
        run_id: &str,
        turn_id: &TurnId,
        metric: &str,
        value: f64,
        k: u32,
        task_id: &str,
        sampled_at: &str,
    ) -> Result<(), MirrorError> {
        // `sampled_at` is NOT NULL in m0006; fall back to the same
        // epoch sentinel m0005 uses for pre-review emitters.
        let ts = if sampled_at.is_empty() {
            "1970-01-01T00:00:00Z"
        } else {
            sampled_at
        };
        self.conn.execute(
            r#"
            INSERT INTO eval_runs (
                run_id, turn_id, metric, value, k, task_id, sampled_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(run_id, turn_id, metric, task_id) DO UPDATE SET
                value = excluded.value,
                k = excluded.k,
                sampled_at = excluded.sampled_at
            "#,
            params![
                run_id,
                turn_id.as_str(),
                metric,
                value,
                k as i64,
                task_id,
                ts,
            ],
        )?;
        Ok(())
    }

    fn run_id_for_upsert(&self) -> String {
        // Terminal events always follow a RunStarted in a well-formed
        // session; if the mirror was attached mid-run (e.g. rebuild from
        // JSONL seeded on a non-RunStarted scan), fall back to empty —
        // the row is still useful and queryable.
        self.current_run
            .as_ref()
            .map(|r| r.as_str().to_string())
            .unwrap_or_default()
    }

    fn upsert_turn(
        &self,
        run_id: &str,
        turn_id: &TurnId,
        outcome: &str,
        detail: Option<&str>,
        usage: &Usage,
    ) -> Result<(), MirrorError> {
        self.conn.execute(
            r#"
            INSERT INTO turns (
                run_id, turn_id, outcome, detail,
                input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(turn_id) DO UPDATE SET
                run_id = excluded.run_id,
                outcome = excluded.outcome,
                detail = excluded.detail,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_creation_tokens = excluded.cache_creation_tokens
            "#,
            params![
                run_id,
                turn_id.as_str(),
                outcome,
                detail,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
            ],
        )?;
        Ok(())
    }

    /// Drop every row and re-apply the forensic projection of `reader`.
    /// Used on startup when the mirror is missing or out of sync.
    pub fn rebuild_from(&mut self, reader: &JsonlReader) -> Result<(), MirrorError> {
        self.conn.execute("DELETE FROM turns", [])?;
        // Sprint 5: truncate the mirror's forensic indexes too so
        // rebuild is a true zero-state refresh. `apply` will re-
        // insert rows from every ImpactComputed event in the
        // forensic projection.
        self.conn.execute("DELETE FROM test_impact", [])?;
        // Sprint 6: same truncate-then-reapply discipline for the
        // eval_runs mirror.
        self.conn.execute("DELETE FROM eval_runs", [])?;
        self.current_run = None;
        // Forensic projection includes aborted turns; replayable would
        // drop them, and those are exactly the terminal-negative rows we
        // need to keep. `apply` itself filters by variant.
        for f in reader.forensic()? {
            self.apply(&f.event)?;
        }
        Ok(())
    }

    /// Count rows in the `test_impact` mirror — exposed for tests
    /// and future query APIs.
    pub fn test_impact_row_count(&self) -> Result<i64, MirrorError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM test_impact", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Count rows in the `eval_runs` mirror — exposed for tests and
    /// the `azoth eval run` reporter.
    pub fn eval_row_count(&self) -> Result<i64, MirrorError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM eval_runs", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Count of rows — exposed for tests and `/status` v2.
    pub fn turn_count(&self) -> Result<i64, MirrorError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))?;
        Ok(n)
    }
}

fn commit_outcome_label(o: CommitOutcome) -> &'static str {
    match o {
        CommitOutcome::Success => "committed",
        CommitOutcome::PartialSuccess => "committed_partial",
    }
}

fn abort_reason_label(r: AbortReason) -> &'static str {
    match r {
        AbortReason::UserCancel => "aborted_user_cancel",
        AbortReason::AdapterError => "aborted_adapter_error",
        AbortReason::ValidatorFail => "aborted_validator_fail",
        AbortReason::ApprovalDenied => "aborted_approval_denied",
        AbortReason::TokenBudget => "aborted_token_budget",
        AbortReason::RuntimeError => "aborted_runtime_error",
        AbortReason::Crash => "aborted_crash",
        AbortReason::ModelTruncated => "aborted_model_truncated",
        AbortReason::ContextOverflow => "aborted_context_overflow",
        AbortReason::SandboxDenied => "aborted_sandbox_denied",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_store::jsonl::JsonlWriter;
    use crate::schemas::{ContractId, TurnId};
    use tempfile::tempdir;

    fn ts() -> String {
        "2026-04-15T12:00:00Z".to_string()
    }

    fn run_started(run: &str) -> SessionEvent {
        SessionEvent::RunStarted {
            run_id: RunId::from(run.to_string()),
            contract_id: ContractId::from("ctr".to_string()),
            timestamp: ts(),
        }
    }

    fn turn_started(run: &str, tid: &str) -> SessionEvent {
        SessionEvent::TurnStarted {
            turn_id: TurnId::from(tid.to_string()),
            run_id: RunId::from(run.to_string()),
            parent_turn: None,
            timestamp: ts(),
        }
    }

    fn committed(tid: &str, input: u32, output: u32) -> SessionEvent {
        SessionEvent::TurnCommitted {
            turn_id: TurnId::from(tid.to_string()),
            outcome: CommitOutcome::Success,
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
            user_input: None,
            final_assistant: None,
        }
    }

    fn aborted(tid: &str, reason: AbortReason) -> SessionEvent {
        SessionEvent::TurnAborted {
            turn_id: TurnId::from(tid.to_string()),
            reason,
            detail: Some("forced".to_string()),
            usage: Usage::default(),
        }
    }

    fn interrupted(tid: &str) -> SessionEvent {
        SessionEvent::TurnInterrupted {
            turn_id: TurnId::from(tid.to_string()),
            reason: AbortReason::UserCancel,
            partial_usage: Default::default(),
        }
    }

    fn impact_computed(
        tid: &str,
        selector: &str,
        tests: &[&str],
        rationale: &[&str],
        confidence: &[f32],
        ran_at: &str,
    ) -> SessionEvent {
        SessionEvent::ImpactComputed {
            turn_id: TurnId::from(tid.to_string()),
            selector: selector.into(),
            selector_version: 1,
            ran_at: ran_at.into(),
            changed_files: vec!["src/foo.rs".into()],
            selected_tests: tests.iter().map(|s| s.to_string()).collect(),
            rationale: rationale.iter().map(|s| s.to_string()).collect(),
            confidence: confidence.to_vec(),
        }
    }

    fn eval_sampled(
        tid: &str,
        metric: &str,
        value: f64,
        k: u32,
        sampled_at: &str,
        task_id: &str,
    ) -> SessionEvent {
        SessionEvent::EvalSampled {
            turn_id: TurnId::from(tid.to_string()),
            metric: metric.to_string(),
            value,
            k,
            sampled_at: sampled_at.to_string(),
            task_id: task_id.to_string(),
        }
    }

    #[test]
    fn mirror_projects_eval_sampled_into_eval_runs() {
        // Sprint 6: seed-sweep rows (non-empty task_id) and turn-
        // embedded rows (empty task_id) both land, and per-
        // (run, turn, metric, task_id) upsert is idempotent.
        let mut m = SqliteMirror::open_in_memory().unwrap();
        m.apply(&run_started("run_eval")).unwrap();
        m.apply(&turn_started("run_eval", "t_eval")).unwrap();
        m.apply(&eval_sampled(
            "t_eval",
            "localization_precision_at_k",
            0.8,
            5,
            "2026-04-17T15:00:00Z",
            "loc01",
        ))
        .unwrap();
        m.apply(&eval_sampled(
            "t_eval",
            "localization_precision_at_k",
            0.6,
            5,
            "2026-04-17T15:00:00Z",
            "loc02",
        ))
        .unwrap();
        m.apply(&eval_sampled(
            "t_eval",
            "regression_rate",
            0.0,
            0,
            "2026-04-17T15:00:01Z",
            "", // turn-embedded signal
        ))
        .unwrap();

        assert_eq!(m.eval_row_count().unwrap(), 3);

        // Replay of the same (run, turn, metric, task_id) tuple must
        // update-in-place, not double-count — mirrors the
        // ImpactComputed upsert guarantee.
        m.apply(&eval_sampled(
            "t_eval",
            "localization_precision_at_k",
            1.0,
            5,
            "2026-04-17T16:00:00Z",
            "loc01",
        ))
        .unwrap();
        assert_eq!(m.eval_row_count().unwrap(), 3);

        let (run_id, val, k, ts): (String, f64, i64, String) = m
            .conn
            .query_row(
                "SELECT run_id, value, k, sampled_at FROM eval_runs \
                 WHERE turn_id = ?1 AND metric = ?2 AND task_id = ?3",
                params!["t_eval", "localization_precision_at_k", "loc01"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(run_id, "run_eval");
        assert!((val - 1.0).abs() < 1e-9);
        assert_eq!(k, 5);
        assert_eq!(ts, "2026-04-17T16:00:00Z");
    }

    #[test]
    fn mirror_eval_runs_fallback_sampled_at_on_empty() {
        // Pre-review fixtures may omit `sampled_at`. m0006 defines the
        // column as NOT NULL — the projector backfills with the same
        // epoch sentinel m0005's `upsert_impact` uses.
        let mut m = SqliteMirror::open_in_memory().unwrap();
        m.apply(&run_started("run_eval_b")).unwrap();
        m.apply(&eval_sampled(
            "t_b",
            "localization_precision_at_k",
            0.5,
            5,
            "",
            "x",
        ))
        .unwrap();
        let ts: String = m
            .conn
            .query_row(
                "SELECT sampled_at FROM eval_runs WHERE turn_id = ?1",
                params!["t_b"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ts, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn mirror_rebuild_wipes_eval_runs_before_reapply() {
        // Sprint 6 parity with m0005: rebuild_from truncates eval_runs
        // so back-to-back rebuilds converge instead of accumulating.
        let dir = tempdir().unwrap();
        let session_path = dir.path().join("s.jsonl");
        let mut w = JsonlWriter::open(&session_path).unwrap();
        w.append(&run_started("run_eval_c")).unwrap();
        w.append(&turn_started("run_eval_c", "t_c")).unwrap();
        w.append(&eval_sampled(
            "t_c",
            "localization_precision_at_k",
            0.75,
            5,
            "2026-04-17T17:00:00Z",
            "seed01",
        ))
        .unwrap();
        w.append(&committed("t_c", 1, 1)).unwrap();
        drop(w);

        let mut m = SqliteMirror::open(dir.path().join("mirror.sqlite")).unwrap();
        let reader = JsonlReader::open(&session_path);
        m.rebuild_from(&reader).unwrap();
        assert_eq!(m.eval_row_count().unwrap(), 1);
        m.rebuild_from(&reader).unwrap();
        assert_eq!(m.eval_row_count().unwrap(), 1);
    }

    #[test]
    fn mirror_projects_impact_computed_into_test_impact() {
        // PR #9 codex P1: migration m0005 created the table but the
        // mirror had no arm to populate it. Fan out one row per test
        // in the selected_tests vec, with rationale[i] /
        // confidence[i] positionally aligned.
        let mut m = SqliteMirror::open_in_memory().unwrap();
        m.apply(&run_started("run_A")).unwrap();
        m.apply(&turn_started("run_A", "t_1")).unwrap();
        m.apply(&impact_computed(
            "t_1",
            "impact:cargo_test",
            &["crate::foo::tests::a", "crate::bar::tests::b"],
            &["direct", "co-edit"],
            &[1.0, 0.6],
            "2026-04-17T14:00:00Z",
        ))
        .unwrap();
        m.apply(&committed("t_1", 5, 5)).unwrap();

        assert_eq!(m.test_impact_row_count().unwrap(), 2);

        // Verify the per-test columns land correctly.
        let row: (String, String, f64, String, String) = m
            .conn
            .query_row(
                "SELECT test_id, status, confidence, selected_because, ran_at \
                 FROM test_impact WHERE turn_id = ?1 ORDER BY test_id",
                params!["t_1"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row.0, "crate::bar::tests::b");
        assert_eq!(row.1, "planned");
        assert!((row.2 - 0.6).abs() < 0.001);
        assert_eq!(row.3, "co-edit");
        assert_eq!(row.4, "2026-04-17T14:00:00Z");
    }

    #[test]
    fn mirror_impact_projection_is_idempotent_on_conflict() {
        // Replaying the same turn's ImpactComputed twice (e.g. via
        // rebuild_from) must converge on the latest values —
        // ON CONFLICT(turn_id, test_id) DO UPDATE. Guards against
        // the stale-row trap the v1 ensure_schema docs flagged.
        let mut m = SqliteMirror::open_in_memory().unwrap();
        m.apply(&run_started("run_A")).unwrap();
        m.apply(&impact_computed(
            "t_1",
            "impact:cargo_test",
            &["crate::foo::tests::a"],
            &["old rationale"],
            &[0.3],
            "2026-04-17T12:00:00Z",
        ))
        .unwrap();
        m.apply(&impact_computed(
            "t_1",
            "impact:cargo_test",
            &["crate::foo::tests::a"],
            &["new rationale"],
            &[1.0],
            "2026-04-17T13:00:00Z",
        ))
        .unwrap();

        assert_eq!(m.test_impact_row_count().unwrap(), 1);
        let (why, score, ran_at): (String, f64, String) = m
            .conn
            .query_row(
                "SELECT selected_because, confidence, ran_at FROM test_impact WHERE turn_id = ?1",
                params!["t_1"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(why, "new rationale");
        assert!((score - 1.0).abs() < 0.001);
        assert_eq!(ran_at, "2026-04-17T13:00:00Z");
    }

    #[test]
    fn mirror_rebuild_wipes_test_impact_before_reapply() {
        // Ensures rebuild_from does not accumulate stale rows across
        // back-to-back rebuilds. Sprint 5's `DELETE FROM test_impact`
        // in rebuild_from is load-bearing.
        let dir = tempdir().unwrap();
        let session_path = dir.path().join("s.jsonl");
        let mut w = JsonlWriter::open(&session_path).unwrap();
        w.append(&run_started("run_A")).unwrap();
        w.append(&turn_started("run_A", "t_1")).unwrap();
        w.append(&impact_computed(
            "t_1",
            "impact:cargo_test",
            &["crate::foo::tests::a"],
            &["direct"],
            &[1.0],
            "2026-04-17T12:00:00Z",
        ))
        .unwrap();
        w.append(&committed("t_1", 1, 1)).unwrap();
        drop(w);

        let mut m = SqliteMirror::open(dir.path().join("mirror.sqlite")).unwrap();
        let reader = JsonlReader::open(&session_path);
        m.rebuild_from(&reader).unwrap();
        assert_eq!(m.test_impact_row_count().unwrap(), 1);
        // Rebuild again — must not double-count.
        m.rebuild_from(&reader).unwrap();
        assert_eq!(m.test_impact_row_count().unwrap(), 1);
    }

    #[test]
    fn mirror_tracks_committed_and_aborted_but_drops_interrupted() {
        let mut m = SqliteMirror::open_in_memory().unwrap();

        m.apply(&run_started("run_A")).unwrap();
        m.apply(&turn_started("run_A", "t_1")).unwrap();
        m.apply(&committed("t_1", 10, 20)).unwrap();

        m.apply(&turn_started("run_A", "t_2")).unwrap();
        m.apply(&aborted("t_2", AbortReason::ValidatorFail))
            .unwrap();

        m.apply(&turn_started("run_A", "t_3")).unwrap();
        m.apply(&interrupted("t_3")).unwrap();

        assert_eq!(m.turn_count().unwrap(), 2);

        let row: (String, String, String, i64, i64) = m
            .conn
            .query_row(
                "SELECT run_id, turn_id, outcome, input_tokens, output_tokens
                 FROM turns WHERE turn_id = ?1",
                params!["t_1"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row.0, "run_A");
        assert_eq!(row.1, "t_1");
        assert_eq!(row.2, "committed");
        assert_eq!(row.3, 10);
        assert_eq!(row.4, 20);

        let abort_outcome: String = m
            .conn
            .query_row(
                "SELECT outcome FROM turns WHERE turn_id = ?1",
                params!["t_2"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(abort_outcome, "aborted_validator_fail");

        // t_3 must be absent.
        let t3_exists: i64 = m
            .conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE turn_id = ?1",
                params!["t_3"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t3_exists, 0);
    }

    #[test]
    fn mirror_rebuilds_from_jsonl() {
        let dir = tempdir().unwrap();
        let session_path = dir.path().join("session.jsonl");

        let mut w = JsonlWriter::open(&session_path).unwrap();
        w.append(&run_started("run_B")).unwrap();
        w.append(&turn_started("run_B", "t_a")).unwrap();
        w.append(&committed("t_a", 1, 2)).unwrap();
        w.append(&turn_started("run_B", "t_b")).unwrap();
        w.append(&aborted("t_b", AbortReason::ApprovalDenied))
            .unwrap();
        drop(w);

        let mut m = SqliteMirror::open_in_memory().unwrap();
        let reader = JsonlReader::open(&session_path);
        m.rebuild_from(&reader).unwrap();
        assert_eq!(m.turn_count().unwrap(), 2);

        let outcomes: Vec<(String, String)> = {
            let mut stmt = m
                .conn
                .prepare("SELECT turn_id, outcome FROM turns ORDER BY turn_id")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].0, "t_a");
        assert_eq!(outcomes[0].1, "committed");
        assert_eq!(outcomes[1].0, "t_b");
        assert_eq!(outcomes[1].1, "aborted_approval_denied");
    }

    #[test]
    fn mirror_opens_and_reopens_on_disk() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(".azoth").join("state.sqlite");

        {
            let mut m = SqliteMirror::open(&db_path).unwrap();
            m.apply(&run_started("run_C")).unwrap();
            m.apply(&committed("t_x", 5, 6)).unwrap();
            assert_eq!(m.turn_count().unwrap(), 1);
        }

        // Reopen preserves the row and schema.
        let m2 = SqliteMirror::open(&db_path).unwrap();
        assert_eq!(m2.turn_count().unwrap(), 1);
        let v: i32 = m2
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            v, 6,
            "Sprint 6 ships m0006 (eval_runs) on top of m0005 (test_impact) / m0004 (co_edit_edges) / m0003 (symbols) / m0002 (FTS) / m0001 (turns)"
        );
    }
}
