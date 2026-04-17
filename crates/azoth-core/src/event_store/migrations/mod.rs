//! Hand-rolled SQLite migrator for the turn mirror.
//!
//! Chosen over `refinery` to avoid ~30 transitive deps for ~120 lines of
//! logic. Migrations are ordered `MigrationStep` fns in `all_steps()`;
//! each step receives a `&Transaction` and is applied inside one
//! `BEGIN IMMEDIATE ... COMMIT`. The `PRAGMA user_version` guards
//! re-runs.
//!
//! Replaces the hard-fail `ensure_schema` that shipped in v1.5 — the
//! same guard still rejects future schema versions the binary doesn't
//! know how to downgrade from.
//!
//! Idempotence rule: every migration must tolerate being a no-op
//! against a DB that already contains the objects it would create.
//! `m0001_initial` uses `CREATE ... IF NOT EXISTS` per-object so fresh
//! DBs (`user_version = 0`, no tables), complete v1.5 DBs
//! (`user_version = 1`, tables present), and partially-initialised v1.5
//! DBs (table present, index missing, `user_version = 0`) all converge
//! on the same steady state.
//!
//! ## Transactionality notes
//!
//! - `BEGIN IMMEDIATE` (not the rusqlite `transaction()` default of
//!   DEFERRED) acquires the RESERVED lock up front, so a concurrent
//!   writer cannot race between our `user_version` read and the first
//!   migration DDL. v1 is single-process, but the daemon-mode v3
//!   scope anchor already points here; no reason to set a loose
//!   precedent.
//! - `PRAGMA user_version = N` **does** participate in the surrounding
//!   transaction on our rusqlite 0.32 / bundled SQLite build — verified
//!   empirically: value reverted on `ROLLBACK`, landed on `COMMIT`. The
//!   `pragma_update`-after-`commit()` pattern that
//!   gemini-code-assist[bot] suggested on PR #4 would actually
//!   introduce a crash window (DDL applied, version not updated, next
//!   boot re-runs migration — only safe because m0001 is idempotent).
//!   Keeping it inside the transaction is the honest single-fsync
//!   atomic commit.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::event_store::sqlite::MirrorError;

mod m0001_initial;
mod m0002_fts_schema;
mod m0003_symbols;
mod m0004_co_edit;

type MigrationStep = fn(&Transaction) -> Result<(), MirrorError>;

fn all_steps() -> &'static [MigrationStep] {
    &[
        m0001_initial::up,
        m0002_fts_schema::up,
        m0003_symbols::up,
        m0004_co_edit::up,
    ]
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

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for step in steps.iter().skip(current as usize) {
        step(&tx)?;
    }
    // PRAGMA user_version participates in the surrounding transaction
    // (see module-level note); keeping it inside the tx gives single-
    // fsync atomicity. On any step failure the COMMIT never runs and
    // the version stays at `current`, so a retry starts from the same
    // point — which is safe because every step is itself idempotent.
    tx.execute_batch(&format!("PRAGMA user_version = {latest};"))?;
    tx.commit()?;
    Ok(latest)
}
