//! Sprint 0 gate test: a v1.5 mirror DB opens clean under the v2
//! migrator, every committed row survives, and the schema version stays
//! at whatever ladder the current binary knows about.
//!
//! The whole point of the hand-rolled migrator is that existing on-disk
//! state is never discarded when a new binary boots. If this test goes
//! red, a later sprint broke forward-compat and v1.5 users would lose
//! their turn history on upgrade.

use azoth_core::event_store::SqliteMirror;
use azoth_core::schemas::{
    AbortReason, CommitOutcome, ContractId, RunId, SessionEvent, TurnId, Usage,
};
use rusqlite::Connection;

fn ts() -> String {
    "2026-04-17T00:00:00Z".to_string()
}

fn run_started(run: &str) -> SessionEvent {
    SessionEvent::RunStarted {
        run_id: RunId::from(run.to_string()),
        contract_id: ContractId::from("ctr".to_string()),
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
        detail: Some("forensic".to_string()),
        usage: Usage::default(),
    }
}

/// The v1.5 `ensure_schema` body — inlined here so this test asserts
/// forward-compat against the _shipped_ v1 shape, not whatever the
/// current migrator produces. Any later divergence between this and
/// `m0001_initial::up` is the bug the test is designed to catch.
fn seed_v1_5_mirror(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS turns (
            run_id       TEXT NOT NULL,
            turn_id      TEXT NOT NULL PRIMARY KEY,
            outcome      TEXT NOT NULL,
            detail       TEXT,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS turns_by_run ON turns(run_id);
        "#,
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 1_i32).unwrap();
}

#[test]
fn v1_5_mirror_opens_under_v2_migrator_with_rows_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".azoth").join("state.sqlite");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // 1. Seed a DB byte-identical to what v1.5 would have written.
    seed_v1_5_mirror(&path);

    // 2. Populate it via the shipping writer path — those rows must
    //    survive the v2 open. We use the mirror's own API so this leg
    //    doesn't couple to private internals.
    {
        let mut m = SqliteMirror::open(&path).unwrap();
        m.apply(&run_started("run_v15")).unwrap();
        m.apply(&committed("t_legacy_1", 11, 22)).unwrap();
        m.apply(&aborted("t_legacy_2", AbortReason::ValidatorFail))
            .unwrap();
        assert_eq!(m.turn_count().unwrap(), 2);
    }

    // 3. Reopen with the current binary — this is the "v2 boot" moment
    //    the plan cares about. Rows from (2) must still be there, and
    //    the schema version must equal the ladder length the migrator
    //    knows.
    let m2 = SqliteMirror::open(&path).unwrap();
    assert_eq!(
        m2.turn_count().unwrap(),
        2,
        "committed + aborted rows must survive forward migration"
    );
    drop(m2);

    let conn = Connection::open(&path).unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert!(
        version >= 1,
        "user_version regressed below v1.5 baseline: got {version}"
    );

    // 4. Every column named in the shipped v1 schema is still reachable
    //    by name. New columns added by later migrations are allowed —
    //    the check asserts a subset, not equality.
    let mut cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(turns)").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    cols.sort();
    for required in [
        "cache_creation_tokens",
        "cache_read_tokens",
        "detail",
        "input_tokens",
        "outcome",
        "output_tokens",
        "run_id",
        "turn_id",
    ] {
        assert!(
            cols.iter().any(|c| c == required),
            "v1 column `{required}` missing after forward migration; cols={cols:?}"
        );
    }

    // 5. The `turns_by_run` index — the `/status` query path — must
    //    survive too.
    let idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='turns_by_run'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "turns_by_run index must survive forward migration");
}

#[test]
fn v1_5_partial_init_is_healed_on_v2_boot() {
    // Regression guard for PR #4 Codex P2: v1.5 `ensure_schema` used
    //   CREATE TABLE IF NOT EXISTS turns (...);
    //   CREATE INDEX IF NOT EXISTS turns_by_run ON turns(run_id);
    //   PRAGMA user_version = 1;
    // inside a non-transactional `execute_batch`. Each statement is
    // atomic on its own; the batch is not. If v1.5 crashed between the
    // CREATE TABLE and the CREATE INDEX, the next boot's `if current == 0`
    // branch would re-run the batch and the `IF NOT EXISTS` clauses
    // would heal the missing index.
    //
    // The v2 migrator must preserve that self-heal semantic. An earlier
    // draft of m0001 short-circuited on table existence, which regressed
    // this case: the index stayed missing AND user_version got bumped to
    // 1, permanently freezing the broken state.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");

    // Seed the v1.5 partial-init state: table present, index MISSING,
    // user_version still at 0 (the post-CREATE-TABLE, pre-PRAGMA moment).
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE turns (
                run_id                TEXT NOT NULL,
                turn_id               TEXT NOT NULL PRIMARY KEY,
                outcome               TEXT NOT NULL,
                detail                TEXT,
                input_tokens          INTEGER NOT NULL DEFAULT 0,
                output_tokens         INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .unwrap();
        // user_version deliberately left at 0.

        let idx_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='turns_by_run'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_before, 0, "seed precondition: index must be absent");
    }

    // Boot under the v2 migrator. Codex's regression scenario.
    {
        let _m = SqliteMirror::open(&path).unwrap();
    }

    // Post-boot: index MUST exist, user_version MUST have advanced to 1.
    let conn = Connection::open(&path).unwrap();
    let idx_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='turns_by_run'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        idx_after, 1,
        "v2 migrator must heal missing turns_by_run index (PR #4 Codex P2)"
    );

    let v: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        v, 5,
        "user_version advances past every migration on self-heal (m0001 turns + m0002 fts + m0003 symbols + m0004 co_edit + m0005 test_impact)"
    );
}

#[test]
fn fresh_db_lands_at_same_schema_as_v1_5_seed() {
    // Zero-state convergence: an empty file must end up at the same
    // user_version as a pre-populated v1.5 DB after the migrator runs.
    // Guarantees the migrator's two entry points don't drift.
    let dir = tempfile::tempdir().unwrap();
    let fresh_path = dir.path().join("fresh.sqlite");
    let seeded_path = dir.path().join("seeded.sqlite");

    seed_v1_5_mirror(&seeded_path);

    // Running the migrator on both via `SqliteMirror::open`.
    let _fresh = SqliteMirror::open(&fresh_path).unwrap();
    let _seeded = SqliteMirror::open(&seeded_path).unwrap();
    drop(_fresh);
    drop(_seeded);

    let fresh_conn = Connection::open(&fresh_path).unwrap();
    let seeded_conn = Connection::open(&seeded_path).unwrap();
    let fresh_v: i32 = fresh_conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    let seeded_v: i32 = seeded_conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        fresh_v, seeded_v,
        "fresh DB and v1.5 DB diverged on schema version after migrator run"
    );
}
