//! Verification item #3 (adapted): schema smoke for the SQLite turn mirror.
//!
//! `docs/draft_plan.md` §Verification item 3 names `refinery` migrations and
//! FTS5 content tables, but v1 of `event_store::sqlite` is intentionally
//! refinery-free (see `crates/azoth-core/src/event_store/sqlite.rs:13`): one
//! `CREATE TABLE IF NOT EXISTS` pass guarded by `PRAGMA user_version`.
//! This test exercises the shipped schema path end-to-end from an
//! integration-test crate (public API only) so #3 is closed in spirit
//! against the v1 surface. When refinery + FTS5 actually land, this file
//! gets replaced with the spec-literal migration test.

use azoth_core::event_store::{MirrorError, SqliteMirror};
use azoth_core::schemas::{
    AbortReason, CommitOutcome, ContractId, RunId, SessionEvent, TurnId, Usage,
};
use rusqlite::Connection;

fn ts() -> String {
    "2026-04-16T00:00:00Z".to_string()
}

fn run_started(run: &str) -> SessionEvent {
    SessionEvent::RunStarted {
        run_id: RunId::from(run.to_string()),
        contract_id: ContractId::from("ctr".to_string()),
        timestamp: ts(),
    }
}

fn committed(tid: &str) -> SessionEvent {
    SessionEvent::TurnCommitted {
        turn_id: TurnId::from(tid.to_string()),
        outcome: CommitOutcome::Success,
        usage: Usage {
            input_tokens: 3,
            output_tokens: 4,
            ..Default::default()
        },
        user_input: None,
        final_assistant: None,
    }
}

fn aborted(tid: &str) -> SessionEvent {
    SessionEvent::TurnAborted {
        turn_id: TurnId::from(tid.to_string()),
        reason: AbortReason::ValidatorFail,
        detail: Some("test".to_string()),
        usage: Usage::default(),
    }
}

#[test]
fn fresh_open_creates_v1_schema_and_accepts_terminal_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".azoth").join("state.sqlite");

    // First open: file does not exist → ensure_schema runs, user_version = 1.
    let mut m = SqliteMirror::open(&path).unwrap();
    assert_eq!(m.turn_count().unwrap(), 0);

    // Terminal-event apply path proves the `turns` table plus every column
    // named in the v1 CREATE TABLE actually exist, without any private
    // peeking at the connection.
    m.apply(&run_started("run_schema")).unwrap();
    m.apply(&committed("t_ok")).unwrap();
    m.apply(&aborted("t_bad")).unwrap();
    assert_eq!(m.turn_count().unwrap(), 2);

    drop(m);

    // Independent verification of the on-disk shape: user_version pragma
    // plus the exact column list from sqlite.rs.
    let conn = Connection::open(&path).unwrap();
    let user_version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(user_version, 1);

    let mut cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(turns)").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    cols.sort();
    assert_eq!(
        cols,
        vec![
            "cache_creation_tokens",
            "cache_read_tokens",
            "detail",
            "input_tokens",
            "outcome",
            "output_tokens",
            "run_id",
            "turn_id",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );

    // The run index must survive the first open — that's the query path
    // `/status` v2 will hit.
    let index_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='turns_by_run'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index_exists, 1);
}

#[test]
fn reopen_is_idempotent_and_preserves_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");

    {
        let mut m = SqliteMirror::open(&path).unwrap();
        m.apply(&run_started("run_reopen")).unwrap();
        m.apply(&committed("t_persist")).unwrap();
        assert_eq!(m.turn_count().unwrap(), 1);
    }

    // Reopen must NOT re-run `CREATE TABLE` in a way that nukes data;
    // the guard is the `user_version == 0` branch in ensure_schema.
    let m2 = SqliteMirror::open(&path).unwrap();
    assert_eq!(m2.turn_count().unwrap(), 1);

    // Third open: still fine.
    drop(m2);
    let m3 = SqliteMirror::open(&path).unwrap();
    assert_eq!(m3.turn_count().unwrap(), 1);
}

#[test]
fn open_rejects_future_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");

    // Seed a file the mirror created itself (so it looks legitimate), then
    // bump user_version past v1 to simulate a future release's schema.
    {
        let _ = SqliteMirror::open(&path).unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99_i32).unwrap();
    }

    match SqliteMirror::open(&path) {
        Ok(_) => panic!("expected open to fail on future schema version"),
        Err(MirrorError::UnknownSchema { current, known }) => {
            assert_eq!(current, 99);
            assert!(known < current, "known schema must trail future version");
        }
        Err(other) => panic!("expected UnknownSchema error, got {other:?}"),
    }
}
