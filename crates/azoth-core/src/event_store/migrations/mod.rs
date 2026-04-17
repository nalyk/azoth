//! Hand-rolled SQLite migrator for the turn mirror.
//!
//! Chosen over `refinery` to avoid ~30 transitive deps for ~120 lines of
//! logic. Migrations are ordered `MigrationStep` fns in `all_steps()`; each
//! step receives a `&Transaction` and is applied inside a single
//! `BEGIN IMMEDIATE ... COMMIT`. The `PRAGMA user_version` guards
//! re-runs.
//!
//! Replaces the hard-fail `ensure_schema` that shipped in v1.5 — the same
//! guard still rejects future schema versions the binary doesn't know
//! how to downgrade from.
//!
//! Idempotence rule: every migration must tolerate being a no-op against a
//! DB that already contains the objects it would create. `m0001_initial`
//! converges both fresh DBs (`user_version = 0`, no tables) and v1.5 DBs
//! (`user_version = 1`, tables present) to the same steady state.

use rusqlite::{Connection, Transaction};

use crate::event_store::sqlite::MirrorError;

mod m0001_initial;

type MigrationStep = fn(&Transaction) -> Result<(), MirrorError>;

fn all_steps() -> &'static [MigrationStep] {
    &[m0001_initial::up]
}

/// Bring `conn` up to the latest schema version. Returns the new
/// `user_version`. Safe to call on every open — no-op when current.
///
/// Errors:
/// - `MirrorError::UnknownSchema` when the DB reports a `user_version`
///   higher than any migration this binary knows about.
/// - `MirrorError::Sqlite` on any DDL failure; the surrounding
///   transaction is rolled back so the DB is never left half-migrated.
pub fn run(conn: &mut Connection) -> Result<u32, MirrorError> {
    let steps = all_steps();
    let latest = steps.len() as u32;

    let current: u32 = {
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        v.max(0) as u32
    };

    if current > latest {
        return Err(MirrorError::UnknownSchema {
            current,
            known: latest,
        });
    }
    if current == latest {
        return Ok(current);
    }

    let tx = conn.transaction()?;
    for step in steps.iter().skip(current as usize) {
        step(&tx)?;
    }
    // PRAGMA user_version participates in the surrounding transaction:
    // if any step above failed the COMMIT never runs and the version
    // stays at `current`, so a retry starts from the same point.
    tx.execute_batch(&format!("PRAGMA user_version = {latest};"))?;
    tx.commit()?;
    Ok(latest)
}
