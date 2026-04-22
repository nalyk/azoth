//! SQLite-backed `SymbolRetrieval` implementation.
//!
//! The writer half (`replace_symbols_for_path`) is called from
//! `RepoIndexer`'s Phase-4 transaction whenever a Rust `documents` row
//! is inserted or updated; the reader half (`SqliteSymbolIndex`) backs
//! `azoth_core::retrieval::SymbolRetrieval` so higher layers can query
//! the extracted graph.
//!
//! ## Invalidation rule (resolved from the v2 plan's §Sprint 2
//! ambiguity on digest vs. mtime)
//!
//! The primary invalidation gate is Sprint 1's mtime-gated documents
//! pipeline. Phase 4 calls `replace_symbols_for_path` whenever a
//! document row is inserted or updated; the writer first deletes every
//! existing row for that path (ON DELETE CASCADE cleans up child rows
//! automatically) and re-inserts. `digest` is stored for debugging
//! ("did the extracted body actually change?"), not as a gate.
//!
//! This piggy-back design means a file that hasn't been touched since
//! last pass (mtime unchanged) triggers exactly zero symbol churn —
//! both the `documents` content AND the `symbols` rows are stable.

use std::sync::{Arc, Mutex};

use azoth_core::retrieval::{RetrievalError, Symbol, SymbolId, SymbolKind, SymbolRetrieval};
use rusqlite::{params, Connection, OptionalExtension, Statement, Transaction};

use crate::indexer::IndexerError;

use super::ExtractedSymbol;

/// Reader-facing handle. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct SqliteSymbolIndex {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSymbolIndex {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Open a dedicated reader Connection on `db_path` with WAL mode
    /// enabled and migrations applied (idempotent). Mirrors
    /// `FtsLexicalRetrieval::open` so each composite-lane backend can
    /// own its own Connection — the Mutex then only serialises calls
    /// within a single backend, leaving the shared WAL to multiplex
    /// reads across backends. PR #11 review feedback.
    pub fn open<P: AsRef<std::path::Path>>(db_path: P) -> Result<Self, IndexerError> {
        let mut conn = Connection::open(db_path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        azoth_core::event_store::migrations::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait::async_trait]
impl SymbolRetrieval for SqliteSymbolIndex {
    async fn by_name(&self, name: &str, limit: usize) -> Result<Vec<Symbol>, RetrievalError> {
        if name.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = Arc::clone(&self.conn);
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| RetrievalError::Other(format!("conn mutex poisoned: {e}")))?;
            query_by_name(&guard, &name, limit)
        })
        .await
        .map_err(|e| RetrievalError::Other(format!("join: {e}")))?
    }

    async fn enclosing(&self, path: &str, line: u32) -> Result<Option<Symbol>, RetrievalError> {
        let conn = Arc::clone(&self.conn);
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| RetrievalError::Other(format!("conn mutex poisoned: {e}")))?;
            query_enclosing(&guard, &path, line)
        })
        .await
        .map_err(|e| RetrievalError::Other(format!("join: {e}")))?
    }
}

fn query_by_name(
    conn: &Connection,
    name: &str,
    limit: usize,
) -> Result<Vec<Symbol>, RetrievalError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, kind, path, start_line, end_line, parent_id, language
             FROM symbols
             WHERE name = ?1
             ORDER BY path, start_line
             LIMIT ?2",
        )
        .map_err(|e| RetrievalError::Other(format!("prepare by_name: {e}")))?;
    let rows = stmt
        .query_map(params![name, limit as i64], row_to_symbol)
        .map_err(|e| RetrievalError::Other(format!("query by_name: {e}")))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| RetrievalError::Other(format!("row by_name: {e}")))?);
    }
    Ok(out)
}

fn query_enclosing(
    conn: &Connection,
    path: &str,
    line: u32,
) -> Result<Option<Symbol>, RetrievalError> {
    // Smallest enclosing range wins: when a method's range is fully
    // inside its impl's range, the method is the enclosing symbol.
    conn.query_row(
        "SELECT id, name, kind, path, start_line, end_line, parent_id, language
         FROM symbols
         WHERE path = ?1 AND start_line <= ?2 AND end_line >= ?2
         ORDER BY (end_line - start_line) ASC
         LIMIT 1",
        params![path, line as i64],
        row_to_symbol,
    )
    .optional()
    .map_err(|e| RetrievalError::Other(format!("query enclosing: {e}")))
}

fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<Symbol> {
    let kind_str: String = row.get(2)?;
    let kind = SymbolKind::from_wire(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown symbol kind: {kind_str}"),
            )),
        )
    })?;
    let parent_id: Option<i64> = row.get(6)?;
    Ok(Symbol {
        id: SymbolId(row.get(0)?),
        name: row.get(1)?,
        kind,
        path: row.get(3)?,
        start_line: row.get::<_, i64>(4)? as u32,
        end_line: row.get::<_, i64>(5)? as u32,
        parent_id: parent_id.map(SymbolId),
        language: row.get(7)?,
        // CP-3: leaving source_mtime None here keeps the existing
        // symbol-query hot path untouched. The FTS lane already
        // plumbs valid_at via its documents JOIN; symbols can be
        // enriched in a follow-up with `JOIN documents ON path`.
        source_mtime: None,
    })
}

/// Transaction-scoped writer that owns the prepared DELETE and INSERT
/// statements so the Phase-4 loop prepares once per reindex pass, not
/// once per file (PR #6 gemini-code-assist MED). Construct with
/// [`SymbolWriter::new`] inside a Phase-4 transaction, then call
/// [`SymbolWriter::replace`] per affected path.
pub struct SymbolWriter<'tx> {
    delete: Statement<'tx>,
    insert: Statement<'tx>,
}

impl<'tx> SymbolWriter<'tx> {
    pub fn new(tx: &'tx Transaction<'_>) -> Result<Self, IndexerError> {
        let delete = tx.prepare("DELETE FROM symbols WHERE path = ?1")?;
        let insert = tx.prepare(
            "INSERT INTO symbols
             (name, kind, path, start_line, end_line, parent_id, language, digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        Ok(Self { delete, insert })
    }

    /// Drop every symbol row for `path` and re-insert the newly
    /// extracted set. Called inside the owning Phase-4 transaction so
    /// the documents write and symbol refresh commit atomically
    /// together. Returns the number of rows inserted.
    ///
    /// Statements stay compiled across calls — the single biggest
    /// cost (`sqlite3_prepare_v2`) runs once in `SymbolWriter::new`.
    pub fn replace(
        &mut self,
        path: &str,
        language: &str,
        extracted: &[ExtractedSymbol],
    ) -> Result<u32, IndexerError> {
        // ON DELETE CASCADE on parent_id fires *only* if foreign_keys
        // is enabled for this connection. Our Phase-4 writer doesn't
        // enable it, so we drop rows top-down and let the flat
        // `path = ?` match pick up every child too.
        self.delete.execute(params![path])?;

        if extracted.is_empty() {
            return Ok(0);
        }

        // Map the extractor's positional parent_idx → freshly-
        // assigned rowid. Parents come before children in the
        // extraction order (walker pushes self before recursing) so
        // this is a single pass.
        let mut rowids: Vec<i64> = Vec::with_capacity(extracted.len());
        let mut inserted: u32 = 0;
        for (i, sym) in extracted.iter().enumerate() {
            let parent_rowid: Option<i64> = match sym.parent_idx {
                Some(idx) => rowids.get(idx).copied().map(Some).unwrap_or_else(|| {
                    tracing::debug!(
                        symbol_idx = i,
                        bad_parent_idx = idx,
                        "extractor emitted out-of-order parent_idx; dropping link"
                    );
                    None
                }),
                None => None,
            };
            // `Statement::insert` executes the INSERT and returns the
            // new rowid in one call — avoids plumbing a `&Transaction`
            // just to reach `last_insert_rowid()` on the connection.
            let rowid = self.insert.insert(params![
                sym.name,
                sym.kind.as_str(),
                path,
                sym.start_line as i64,
                sym.end_line as i64,
                parent_rowid,
                language,
                sym.digest,
            ])?;
            rowids.push(rowid);
            inserted += 1;
        }
        Ok(inserted)
    }
}

/// Back-compat wrapper for one-shot call sites (unit tests). Prepares
/// a fresh `SymbolWriter` and applies it — one path, one tx. Not the
/// hot path; the indexer's Phase-4 loop uses the writer directly so
/// prepare cost amortises across every file.
pub fn replace_symbols_for_path(
    tx: &Transaction<'_>,
    path: &str,
    language: &str,
    extracted: &[ExtractedSymbol],
) -> Result<u32, IndexerError> {
    SymbolWriter::new(tx)?.replace(path, language, extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::event_store::migrations;
    use azoth_core::retrieval::SymbolKind;
    use rusqlite::TransactionBehavior;

    fn open_mem() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::run(&mut conn).unwrap();
        conn
    }

    /// Spin up a current-thread tokio runtime per call — the index's
    /// reader API uses `spawn_blocking`, which needs a tokio context.
    /// `futures::executor::block_on` would deadlock on that.
    fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn ext(
        name: &str,
        kind: SymbolKind,
        start: u32,
        end: u32,
        parent: Option<usize>,
    ) -> ExtractedSymbol {
        ExtractedSymbol {
            name: name.into(),
            kind,
            start_line: start,
            end_line: end,
            parent_idx: parent,
            digest: format!("{:016x}", (start as u64) << 8 | end as u64),
        }
    }

    #[test]
    fn write_then_read_by_name() {
        let mut conn = open_mem();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let n = replace_symbols_for_path(
            &tx,
            "src/lib.rs",
            "rust",
            &[
                ext("Foo", SymbolKind::Struct, 1, 3, None),
                ext("Foo", SymbolKind::Impl, 5, 10, None),
                ext("bar", SymbolKind::Function, 6, 8, Some(1)),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(n, 3);

        let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(conn)));
        let hits = tokio_test_block_on(idx.by_name("Foo", 10)).unwrap();
        assert_eq!(hits.len(), 2, "struct Foo + impl Foo both named Foo");
        let bar = tokio_test_block_on(idx.by_name("bar", 10)).unwrap();
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].kind, SymbolKind::Function);
        assert!(bar[0].parent_id.is_some(), "method must link to its impl");
    }

    #[test]
    fn enclosing_picks_smallest_range() {
        let mut conn = open_mem();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        replace_symbols_for_path(
            &tx,
            "src/lib.rs",
            "rust",
            &[
                ext("Outer", SymbolKind::Impl, 1, 20, None),
                ext("inner", SymbolKind::Function, 5, 8, Some(0)),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(conn)));
        let hit = tokio_test_block_on(idx.enclosing("src/lib.rs", 6))
            .unwrap()
            .expect("inner match");
        assert_eq!(hit.name, "inner");

        let outer = tokio_test_block_on(idx.enclosing("src/lib.rs", 2))
            .unwrap()
            .expect("outer match");
        assert_eq!(outer.name, "Outer");
    }

    #[test]
    fn replace_purges_old_rows_for_path() {
        let mut conn = open_mem();
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            replace_symbols_for_path(
                &tx,
                "src/a.rs",
                "rust",
                &[ext("old", SymbolKind::Function, 1, 2, None)],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            replace_symbols_for_path(
                &tx,
                "src/a.rs",
                "rust",
                &[ext("new", SymbolKind::Function, 1, 2, None)],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(conn)));
        assert!(tokio_test_block_on(idx.by_name("old", 10))
            .unwrap()
            .is_empty());
        assert_eq!(
            tokio_test_block_on(idx.by_name("new", 10)).unwrap().len(),
            1,
        );
    }

    #[test]
    fn empty_extracted_still_purges() {
        let mut conn = open_mem();
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            replace_symbols_for_path(
                &tx,
                "src/a.rs",
                "rust",
                &[ext("gone", SymbolKind::Function, 1, 2, None)],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let n = replace_symbols_for_path(&tx, "src/a.rs", "rust", &[]).unwrap();
            assert_eq!(n, 0);
            tx.commit().unwrap();
        }
        let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(conn)));
        assert!(tokio_test_block_on(idx.by_name("gone", 10))
            .unwrap()
            .is_empty());
    }
}
