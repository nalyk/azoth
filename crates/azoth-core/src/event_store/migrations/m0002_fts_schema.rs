//! m0002: FTS5 document index + external-content sync triggers.
//!
//! Adds two objects to the mirror DB:
//! - `documents(path PK, mtime, language, content)` — the source of truth
//!   for indexed repo content. `RepoIndexer` upserts rows here with
//!   mtime-gating so re-indexing is incremental.
//! - `documents_fts USING fts5(path, content, ...)` — external-content
//!   FTS5 virtual table synchronised with `documents` via triggers so
//!   FTS stays consistent with any write to the content table.
//!
//! Tokenizer: `porter unicode61`. Porter stemming is benign for
//! identifier-heavy code and keeps English docstrings queryable by
//! stem. `unicode61` splits on non-alphanumeric, so `camelCase` is one
//! token and `snake_case` is two — the parity test at
//! `crates/azoth-core/tests/retrieval_parity.rs` validates that the set
//! of files FTS finds for identifier queries is a superset of the files
//! the ripgrep lexical retrieval finds.
//!
//! ## Trigger shape
//!
//! External-content FTS5 needs three triggers (AFTER INSERT / AFTER
//! DELETE / AFTER UPDATE) per the SQLite FTS5 docs §4.4.3. The DELETE
//! sentinel uses `INSERT INTO documents_fts(documents_fts, rowid, ...)
//! VALUES('delete', ...)` — that's the FTS5 delete-row form, not
//! ordinary SQL DELETE.
//!
//! Idempotence: every DDL uses `IF NOT EXISTS` (or `CREATE TRIGGER IF
//! NOT EXISTS`). Matches m0001's per-object idempotency so a partially
//! applied migration still converges to the steady state on retry.

use rusqlite::Transaction;

use crate::event_store::sqlite::MirrorError;

pub fn up(tx: &Transaction) -> Result<(), MirrorError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS documents (
            path      TEXT PRIMARY KEY,
            mtime     INTEGER NOT NULL,
            language  TEXT,
            content   TEXT NOT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
            path,
            content,
            tokenize='porter unicode61',
            content='documents',
            content_rowid='rowid'
        );

        CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN
            INSERT INTO documents_fts(rowid, path, content)
            VALUES (new.rowid, new.path, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
            INSERT INTO documents_fts(documents_fts, rowid, path, content)
            VALUES ('delete', old.rowid, old.path, old.content);
        END;

        CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN
            INSERT INTO documents_fts(documents_fts, rowid, path, content)
            VALUES ('delete', old.rowid, old.path, old.content);
            INSERT INTO documents_fts(rowid, path, content)
            VALUES (new.rowid, new.path, new.content);
        END;
        "#,
    )?;
    Ok(())
}
