//! m0001: initial turn-mirror schema.
//!
//! Creates the `turns` table + `turns_by_run` index that `SqliteMirror`
//! shipped with in v1.5. Per-object idempotent via `IF NOT EXISTS` on
//! each DDL — matches v1.5 `ensure_schema` byte-for-byte, so a partially
//! initialized v1.5 DB (table created, index creation interrupted) still
//! heals to a complete v1 schema on the next v2 boot.
//!
//! The plan (§Sprint 0) says "detect existing v1 `turns` table via
//! `SELECT name FROM sqlite_master` before `CREATE`"; that captures the
//! intent (idempotency) but the original implementation of it as a
//! table-existence short-circuit regressed v1.5's self-heal semantic.
//! Flagged on PR #4 by Codex (P2). `IF NOT EXISTS` is SQLite's
//! engine-level idempotency idiom and applies per-object.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS turns (
            run_id                TEXT NOT NULL,
            turn_id               TEXT NOT NULL PRIMARY KEY,
            outcome               TEXT NOT NULL,
            detail                TEXT,
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS turns_by_run ON turns(run_id);
        "#,
    )?;
    Ok(())
}
