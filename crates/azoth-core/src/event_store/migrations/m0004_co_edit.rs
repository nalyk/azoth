//! m0004: co-edit graph edges.
//!
//! Adds one table, `co_edit_edges`, populated by
//! `azoth_repo::history::co_edit::build` from a `git log` walk. Rows are
//! canonicalised so `path_a < path_b` — the symmetry of co-edit
//! adjacency is collapsed into a single storage row, halving the row
//! count and making neighbor queries a simple `UNION` over both
//! orientations.
//!
//! ## Schema decisions
//!
//! - **Composite PK `(path_a, path_b)` + `CHECK (path_a < path_b)`**.
//!   The CHECK is the canonicalisation guard. An attempted insert that
//!   forgot to reorder the pair blows up in SQLite at write time rather
//!   than silently double-storing the edge.
//! - **`weight REAL NOT NULL`**. The accumulator in `co_edit::build` is
//!   a fractional sum (`1 / max(1, n-1)` per commit), so real precision
//!   is correct; storing as INTEGER would truncate to zero on the
//!   common 2-file-commit case.
//! - **`last_commit_sha TEXT NOT NULL`**. Records the newest commit
//!   that contributed to this pair, giving forensics ("when did this
//!   edge last accumulate weight?") without holding a per-commit log.
//! - **No foreign key to `documents(path)`**. Co-edit is a *history*
//!   graph — it legitimately names paths that have since been deleted
//!   or renamed. Indexer churn must not break graph rows.
//!
//! ## Indexes
//!
//! The PK on `(path_a, path_b)` already provides a BTree that answers
//! `WHERE path_a = ?` efficiently. The reverse direction
//! (`WHERE path_b = ?`) needs its own index, added explicitly below.
//! A neighbor query for file `P` selects rows where `path_a = P` OR
//! `path_b = P` and each side uses its own index.
//!
//! ## Idempotence
//!
//! `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS` match
//! the m0001..m0003 convention. Running m0004 twice against the same
//! DB converges to the same steady state.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS co_edit_edges (
            path_a          TEXT    NOT NULL,
            path_b          TEXT    NOT NULL,
            weight          REAL    NOT NULL,
            last_commit_sha TEXT    NOT NULL,
            PRIMARY KEY (path_a, path_b),
            CHECK (path_a < path_b)
        );

        CREATE INDEX IF NOT EXISTS co_edit_edges_by_b_idx
            ON co_edit_edges(path_b);
        "#,
    )?;
    Ok(())
}
