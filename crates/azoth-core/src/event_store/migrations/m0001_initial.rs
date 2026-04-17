//! m0001: initial turn-mirror schema.
//!
//! Creates the `turns` table + `turns_by_run` index that `SqliteMirror`
//! shipped with in v1.5. Idempotent: skips the CREATE when the table
//! already exists, which lets v1.5 DBs (`user_version = 1` already) and
//! fresh DBs (`user_version = 0`) converge on the same migration ladder.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='turns')",
        [],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    tx.execute_batch(
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
        CREATE INDEX turns_by_run ON turns(run_id);
        "#,
    )?;
    Ok(())
}
