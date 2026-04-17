//! m0003: tree-sitter symbol index.
//!
//! Adds a single table, `symbols`, co-located in the mirror DB next to
//! `documents` (m0002). The writer (`azoth_repo::code_graph::SqliteSymbolIndex`)
//! replaces the symbol rows for a given `path` whenever Sprint 1's
//! 4-phase indexer writes or updates the corresponding `documents` row.
//!
//! ## Schema decisions
//!
//! - `id INTEGER PRIMARY KEY AUTOINCREMENT` — ephemeral row id. Never
//!   baked into JSONL events as a durable key. `AUTOINCREMENT` (not just
//!   `INTEGER PRIMARY KEY`) guarantees monotonically increasing ids even
//!   across row deletes so a single session cannot collide ids between
//!   successive reindex passes on the same path.
//! - `parent_id INTEGER NULL REFERENCES symbols(id) ON DELETE CASCADE`
//!   encodes the enclosing symbol (method → impl, variant → enum). The
//!   ON DELETE CASCADE means purging a parent row automatically drops
//!   its children in one `DELETE FROM symbols WHERE path = ?` — the
//!   indexer relies on this rather than maintaining a separate tree.
//! - `digest TEXT` stores a short hash of the extracted body at write
//!   time. Stored for debugging / forensic diffs, NOT used for gating
//!   (mtime on `documents` is the primary invalidation; see
//!   `SqliteSymbolIndex` module docs for the full rule).
//! - `path TEXT NOT NULL` — not a FOREIGN KEY to `documents(path)` on
//!   purpose. The two tables are populated transactionally from the
//!   indexer's Phase-4 tx; adding an FK here would require pragma
//!   `foreign_keys = ON` to mean anything at all (SQLite default is OFF)
//!   and would force delete-ordering without adding a real invariant.
//!   We rely on the indexer's write discipline instead.
//!
//! ## Indexes
//!
//! - `symbols_by_name_idx(name)` — covers `by_name(name)`.
//! - `symbols_by_path_line_idx(path, start_line, end_line)` — covers
//!   `enclosing(path, line)` via a range probe.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS symbols (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT    NOT NULL,
            kind        TEXT    NOT NULL,
            path        TEXT    NOT NULL,
            start_line  INTEGER NOT NULL,
            end_line    INTEGER NOT NULL,
            parent_id   INTEGER NULL REFERENCES symbols(id) ON DELETE CASCADE,
            language    TEXT    NOT NULL,
            digest      TEXT
        );

        CREATE INDEX IF NOT EXISTS symbols_by_name_idx
            ON symbols(name);

        CREATE INDEX IF NOT EXISTS symbols_by_path_line_idx
            ON symbols(path, start_line, end_line);
        "#,
    )?;
    Ok(())
}
