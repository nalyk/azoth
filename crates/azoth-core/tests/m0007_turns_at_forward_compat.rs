//! Chronon CP-5 (m0007): `turns.at` + `turns_by_at` index.
//!
//! Three invariants this test locks in:
//!
//! 1. A fresh DB lands at `user_version = 7` with `at` column + index present.
//! 2. A v2.0.1-era DB (m0001..m0006, no `turns.at`) heals on the next boot —
//!    `ALTER TABLE turns ADD COLUMN at TEXT` runs exactly once, the index is
//!    created, existing rows stay put with `at = NULL`.
//! 3. Running the migrator twice against a post-m0007 DB is a no-op (the
//!    `pragma_table_info` probe skips the ALTER, and `CREATE INDEX IF NOT
//!    EXISTS` is idempotent). Boot-loops don't churn schema.

use azoth_core::event_store::SqliteMirror;
use rusqlite::{params, Connection};
use tempfile::tempdir;

fn column_exists(conn: &Connection, table: &str, col: &str) -> bool {
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2")
        .unwrap();
    let n: i64 = stmt.query_row(params![table, col], |r| r.get(0)).unwrap();
    n > 0
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
            params![name],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

#[test]
fn fresh_db_has_at_column_and_turns_by_at_index() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    // Opening a SqliteMirror runs the full migrator stack (m0001..m0007).
    let _ = SqliteMirror::open(&path).unwrap();

    let conn = Connection::open(&path).unwrap();
    assert!(column_exists(&conn, "turns", "at"));
    assert!(index_exists(&conn, "turns_by_at"));

    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 7);
}

#[test]
fn pre_m0007_db_heals_without_losing_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.sqlite");

    // Seed the m0006 steady state by hand: open and immediately rewind
    // user_version to 6, DROP the m0007 additions so the next open has to
    // re-run them. Mimics a v2.0.1 binary that shipped without m0007.
    {
        let _ = SqliteMirror::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        // Simulate the pre-m0007 shape.
        conn.execute_batch(
            r#"
            DROP INDEX IF EXISTS turns_by_at;
            -- SQLite <3.35 lacks DROP COLUMN; rebuild the table.
            CREATE TABLE turns_pre_m0007 (
                run_id                TEXT NOT NULL,
                turn_id               TEXT NOT NULL PRIMARY KEY,
                outcome               TEXT NOT NULL,
                detail                TEXT,
                input_tokens          INTEGER NOT NULL DEFAULT 0,
                output_tokens         INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO turns_pre_m0007
                SELECT run_id, turn_id, outcome, detail,
                       input_tokens, output_tokens,
                       cache_read_tokens, cache_creation_tokens
                FROM turns;
            DROP TABLE turns;
            ALTER TABLE turns_pre_m0007 RENAME TO turns;
            CREATE INDEX IF NOT EXISTS turns_by_run ON turns(run_id);
            "#,
        )
        .unwrap();
        // Insert a pre-CP-1-style row: no `at`, because the column doesn't
        // exist yet.
        conn.execute(
            r#"
            INSERT INTO turns (run_id, turn_id, outcome, detail,
                               input_tokens, output_tokens,
                               cache_read_tokens, cache_creation_tokens)
            VALUES ('run_legacy', 't_legacy', 'success', NULL, 0, 0, 0, 0)
            "#,
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 6_i32).unwrap();

        assert!(!column_exists(&conn, "turns", "at"));
        assert!(!index_exists(&conn, "turns_by_at"));
    }

    // Re-open → m0007 runs, heals both.
    let _ = SqliteMirror::open(&path).unwrap();

    let conn = Connection::open(&path).unwrap();
    assert!(column_exists(&conn, "turns", "at"));
    assert!(index_exists(&conn, "turns_by_at"));
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 7);

    // Legacy row preserved, `at` lands as NULL.
    let (outcome, at): (String, Option<String>) = conn
        .query_row(
            "SELECT outcome, at FROM turns WHERE turn_id = 't_legacy'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "success");
    assert_eq!(at, None);
}

#[test]
fn m0007_is_idempotent_on_a_post_m0007_db() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.sqlite");

    // First open: m0007 runs. Second open: must be a no-op, same schema.
    let _ = SqliteMirror::open(&path).unwrap();
    let _ = SqliteMirror::open(&path).unwrap();
    let _ = SqliteMirror::open(&path).unwrap();

    let conn = Connection::open(&path).unwrap();
    assert!(column_exists(&conn, "turns", "at"));
    assert!(index_exists(&conn, "turns_by_at"));

    // Column count must not have drifted (no accidental duplicate ALTER).
    let at_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('turns') WHERE name = 'at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(at_count, 1);
}
