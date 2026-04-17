//! m0005: TDAD test impact selection.
//!
//! Adds one table, `test_impact`, keyed per (turn_id, test_id). An
//! `ImpactValidator` writes one row per test in the `TestPlan` it
//! produced at the validator phase. Rows preserve the selector's
//! rationale ("selected_because") so forensics can answer *why*
//! each test entered the plan without re-running the selector.
//!
//! ## Schema decisions
//!
//! - **Composite PK `(turn_id, test_id)`**. A single turn may invoke
//!   multiple `ImpactValidator`s against the same universe, but
//!   selector names are disambiguated downstream via `selected_because`
//!   text; enforcing uniqueness per (turn, test) prevents duplicate
//!   rows when two selectors both pick the same test.
//! - **`status TEXT NOT NULL`**. Plan-only in v2 (values: `planned`).
//!   v2.1 extends to `passed` / `failed` / `skipped` when a real
//!   `TestRunner` ships. Stored as free-form text so new variants do
//!   not require a migration.
//! - **`confidence REAL NOT NULL`**. Selector-provided score in
//!   `[0.0, 1.0]`; `CargoTestImpact` emits `1.0` for the direct-path
//!   heuristic, lower for co-edit-adjacent additions.
//! - **`selected_because TEXT NOT NULL`**. Human-readable rationale
//!   echoed from the selector (e.g. `"direct filename match on
//!   tests/foo_tests.rs"`, `"co-edit neighbour of src/bar.rs"`).
//! - **`ran_at TEXT NOT NULL`**. ISO-8601 UTC at selector call time.
//!   Allows time-ordered replay without reparsing JSONL.
//! - **No foreign key to any event table**. `test_impact` is an
//!   indexable mirror, not a source of truth. JSONL
//!   `SessionEvent::ImpactComputed` is authoritative (CRIT-1).
//!
//! ## Indexes
//!
//! A secondary index on `turn_id` already ships via the composite PK's
//! leftmost prefix. A `test_id`-only index is added so forensic
//! "which turns selected test X" queries stay cheap on dogfood runs.
//!
//! ## Idempotence
//!
//! `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS` match
//! the m0001..m0004 convention. Running m0005 twice against the same
//! DB converges to the same steady state.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS test_impact (
            turn_id          TEXT    NOT NULL,
            test_id          TEXT    NOT NULL,
            status           TEXT    NOT NULL,
            confidence       REAL    NOT NULL,
            selected_because TEXT    NOT NULL,
            ran_at           TEXT    NOT NULL,
            PRIMARY KEY (turn_id, test_id)
        );

        CREATE INDEX IF NOT EXISTS test_impact_by_test_idx
            ON test_impact(test_id);
        "#,
    )?;
    Ok(())
}
