//! m0007: bitemporal index on the turn mirror (Chronon CP-5).
//!
//! Adds `at TEXT` to the `turns` table plus `turns_by_at` on it. Backs
//! the forensic as-of projection: `SELECT turn_id FROM turns WHERE at
//! <= ?` answers "what did the mirror look like at time T?" without a
//! full-scan over the JSONL log. JSONL stays authoritative (CRIT-1) —
//! the SQL column is a rebuildable secondary projection.
//!
//! `at` is nullable. Pre-CP-1 turns committed/aborted without a wall-
//! clock marker land with `at = NULL`; the index simply excludes them
//! from range queries. This matches the JSONL semantics where pre-CP-1
//! terminal events omit `at` via `skip_serializing_if = Option::is_none`.
//!
//! ## Idempotence
//!
//! Unlike the rest of the schema, `ALTER TABLE ... ADD COLUMN` has no
//! `IF NOT EXISTS` clause in SQLite. We probe `pragma_table_info` and
//! branch. Running m0007 twice against a post-m0007 DB converges; the
//! index itself is guarded with `IF NOT EXISTS`.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    let has_at: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('turns') WHERE name = 'at'",
        [],
        |r| r.get(0),
    )?;
    if has_at == 0 {
        tx.execute_batch("ALTER TABLE turns ADD COLUMN at TEXT;")?;
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS turns_by_at ON turns(at);")?;
    Ok(())
}
