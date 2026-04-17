//! SQLite mirror of committed/aborted turns.
//!
//! JSONL remains the authoritative event log (CRIT-1). This mirror is a
//! rebuildable secondary index so `/status`, history, and future query APIs
//! don't have to re-read every session file.
//!
//! **HARD invariant** (`docs/draft_plan.md` ~line 308): the mirror only
//! observes `TurnCommitted` and `TurnAborted` — both are *definite*
//! outcomes. `TurnInterrupted` and dangling turns live in JSONL only.
//! `RunStarted` is observed so the mirror can tag rows with the current
//! run; all other variants are silently ignored.
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
            _ => {}
        }
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
        self.current_run = None;
        // Forensic projection includes aborted turns; replayable would
        // drop them, and those are exactly the terminal-negative rows we
        // need to keep. `apply` itself filters by variant.
        for f in reader.forensic()? {
            self.apply(&f.event)?;
        }
        Ok(())
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
            v, 5,
            "Sprint 5 ships m0005 (test_impact) on top of m0004 (co_edit_edges) on top of m0003 (symbols) on top of m0002 (FTS) on top of m0001 (turns)"
        );
    }
}
