//! Repo indexer — walks `root` via `ignore::WalkBuilder` (scope-parity
//! with `RipgrepLexicalRetrieval`) and upserts UTF-8 file contents into
//! the `documents` SQLite table. Triggers defined in migration
//! `m0002_fts_schema` keep `documents_fts` in sync automatically.
//!
//! ## Incremental model (four phases)
//!
//! 1. **Walk** — metadata-only pass. Collects `(path, abs, mtime_ns)`
//!    for every file inside `max_file_bytes`; files over the size
//!    limit or with unreadable metadata are counted into
//!    `skipped_binary_or_large` and deliberately NOT tracked, so
//!    previously-indexed files that have since grown or turned into
//!    binaries will be purged in phase 4.
//! 2. **Triage** — single read-only pass over `documents.mtime` to
//!    split candidates into `unchanged` vs. `needs-read`. Content
//!    is not touched yet, so memory stays bounded at metadata size.
//! 3. **Read** — UTF-8 `read_to_string` for only the
//!    insert/update set. A file whose content fails UTF-8 between
//!    walk and read is treated as if it had vanished.
//! 4. **Write+purge (one tx)** — stage every confirmed-present path
//!    into a TEMP `_seen_paths` table, apply all writes, then
//!    `DELETE FROM documents WHERE path NOT IN _seen_paths`. No
//!    Rust-side path-set needed; SQLite evaluates the anti-join.
//!
//! ## mtime precision
//!
//! `mtime` is stored as **nanoseconds** since `UNIX_EPOCH` (i64,
//! clamped at `i64::MAX` — good until year ~2262). This preserves
//! subsec precision so within-second edits are detected as changes.
//! Filesystems that only expose second-resolution times land at
//! `secs * 10^9` with no harm done.
//!
//! ## Size/binary policy
//!
//! - Files larger than `max_file_bytes` (default 1 MiB) are skipped
//!   in the walk, DO NOT enter the seen-set, and therefore are
//!   purged from any prior index row — stale content never lingers.
//! - Files that can't be read as UTF-8 are handled symmetrically.
//!
//! ## Connection ownership
//!
//! The indexer opens its OWN `rusqlite::Connection` to the shared
//! mirror DB file (`.azoth/state.sqlite` by convention). WAL mode is
//! enabled at open — set once, persisted on the file — so the
//! mirror's own connection and this one coexist. Migrations are
//! re-run defensively on open; the migrator is idempotent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use azoth_core::event_store::migrations;
use azoth_core::event_store::sqlite::MirrorError;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use thiserror::Error;

use crate::code_graph::{Language, ParserKey};

/// Default ceiling on file size. Larger files get skipped — matches the
/// Sprint 1 "cheap indexer" policy; Sprint 2 tree-sitter extraction may
/// raise this for specific languages.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migrator: {0}")]
    Migrator(#[from] MirrorError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("join: {0}")]
    Join(String),
    #[error("{0}")]
    Other(String),
}

/// Summary of a reindex pass — exposed for tests and future eval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Files walked (before any skip).
    pub walked: u32,
    /// Files inserted (new rows).
    pub inserted: u32,
    /// Files whose mtime matched storage and were skipped.
    pub skipped_unchanged: u32,
    /// Files updated because mtime differed.
    pub updated: u32,
    /// Rows deleted because the underlying file disappeared.
    pub deleted: u32,
    /// Files skipped because they exceed `max_file_bytes` or failed a
    /// UTF-8 read. Counted together because the distinction is not
    /// interesting at the eval-plane layer.
    pub skipped_binary_or_large: u32,
    /// Rust symbols extracted and persisted this pass (Sprint 2).
    /// Counts every row written by `replace_symbols_for_path`, across
    /// every Rust file in the insert/update set. Non-Rust files and
    /// files that skipped Phase 3 do not contribute.
    pub symbols_extracted: u32,
}

/// Holds a shared SQLite connection and the repo root. Both
/// `RepoIndexer` (writer) and `FtsLexicalRetrieval` (reader) can hold
/// `Arc`-clones of the same inner `Mutex<Connection>` to avoid opening
/// two file handles against the same mirror.
pub struct RepoIndexer {
    conn: Arc<Mutex<Connection>>,
    root: PathBuf,
    max_file_bytes: u64,
}

impl RepoIndexer {
    /// Open the mirror DB at `db_path` and associate it with repo `root`.
    /// Runs the migrator so the FTS schema is guaranteed present.
    pub fn open<P: AsRef<Path>, R: Into<PathBuf>>(
        db_path: P,
        root: R,
    ) -> Result<Self, IndexerError> {
        let mut conn = Connection::open(db_path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            root: root.into(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        })
    }

    /// Wrap an already-open connection. Useful in tests that want an
    /// in-memory DB, or in higher layers that share one connection
    /// between indexer and retrieval.
    pub fn with_connection(conn: Arc<Mutex<Connection>>, root: PathBuf) -> Self {
        Self {
            conn,
            root,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }

    /// Expose the underlying connection so a retrieval impl can share it.
    pub fn connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    /// Override the file-size ceiling.
    pub fn set_max_file_bytes(&mut self, bytes: u64) {
        self.max_file_bytes = bytes;
    }

    /// Incremental reindex: upsert changed files, delete rows for files
    /// that no longer exist. Returns stats.
    pub async fn reindex_incremental(&self) -> Result<IndexStats, IndexerError> {
        let conn = Arc::clone(&self.conn);
        let root = self.root.clone();
        let max_bytes = self.max_file_bytes;
        tokio::task::spawn_blocking(move || reindex_blocking(conn, &root, max_bytes))
            .await
            .map_err(|e| IndexerError::Join(e.to_string()))?
    }
}

fn reindex_blocking(
    conn: Arc<Mutex<Connection>>,
    root: &Path,
    max_bytes: u64,
) -> Result<IndexStats, IndexerError> {
    use ignore::WalkBuilder;

    let mut stats = IndexStats::default();
    if !root.exists() {
        return Ok(stats);
    }

    // Phase 1 (no lock, no reads): walk the repo and collect (path, abs,
    // mtime_ns) for every file that currently passes the size/metadata
    // gate. Files over `max_bytes` or with unreadable metadata are
    // counted into `skipped_binary_or_large` and intentionally NOT
    // tracked here — if such a file used to be indexed, the SQL purge
    // at Phase 4 will delete its stale row (Codex P1 on L257: skipped
    // files must not protect old rows from deletion).
    struct Candidate {
        path: String,
        abs: PathBuf,
        mtime_ns: i64,
    }
    let mut candidates: Vec<Candidate> = Vec::new();

    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .parents(true)
        .build();

    for dent in walker {
        let Ok(dent) = dent else {
            continue;
        };
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        stats.walked = stats.walked.saturating_add(1);

        let abs = dent.path().to_path_buf();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => abs.clone(),
        };
        let path_str = rel.to_string_lossy().into_owned();

        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => {
                stats.skipped_binary_or_large = stats.skipped_binary_or_large.saturating_add(1);
                continue;
            }
        };
        if meta.len() > max_bytes {
            stats.skipped_binary_or_large = stats.skipped_binary_or_large.saturating_add(1);
            continue;
        }

        // Nanosecond precision so within-second edits (fast save/
        // reindex loops) are not silently missed. u128 → i64 is safe
        // until ~year 2262; clamp defensively rather than panic.
        // (Codex P1 on L194.)
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(0);

        candidates.push(Candidate {
            path: path_str,
            abs,
            mtime_ns,
        });
    }

    // Phase 2 (short lock, no tx): triage candidates against the stored
    // mtime. Reads nothing from disk beyond metadata, so the memory
    // footprint stays O(number-of-candidates-with-small-fields). File
    // content is read only for rows that actually need an INSERT/
    // UPDATE (gemini HIGH on L212).
    enum Op {
        Insert,
        Update,
    }
    struct PlannedWrite {
        path: String,
        abs: PathBuf,
        mtime_ns: i64,
        op: Op,
    }
    let mut to_write: Vec<PlannedWrite> = Vec::new();
    let mut unchanged_paths: Vec<String> = Vec::new();
    {
        let guard = conn
            .lock()
            .map_err(|e| IndexerError::Other(format!("conn mutex poisoned: {e}")))?;
        let mut stmt = guard.prepare("SELECT mtime FROM documents WHERE path = ?1")?;
        for cand in &candidates {
            let existing: Option<i64> = stmt
                .query_row(params![cand.path], |r| r.get(0))
                .optional()?;
            match existing {
                Some(old) if old == cand.mtime_ns => {
                    unchanged_paths.push(cand.path.clone());
                }
                Some(_) => to_write.push(PlannedWrite {
                    path: cand.path.clone(),
                    abs: cand.abs.clone(),
                    mtime_ns: cand.mtime_ns,
                    op: Op::Update,
                }),
                None => to_write.push(PlannedWrite {
                    path: cand.path.clone(),
                    abs: cand.abs.clone(),
                    mtime_ns: cand.mtime_ns,
                    op: Op::Insert,
                }),
            }
        }
    }

    // Phase 3 (no lock, no tx): read file contents only for the
    // insert/update set. A file whose content fails UTF-8 reads
    // between walk and read is treated as if it vanished — its row
    // (if any) will be purged by Phase 4.
    struct WriteWithContent {
        path: String,
        mtime_ns: i64,
        language: Option<&'static str>,
        content: String,
        op: Op,
    }
    let mut writes: Vec<WriteWithContent> = Vec::with_capacity(to_write.len());
    for pw in to_write {
        match std::fs::read_to_string(&pw.abs) {
            Ok(content) => {
                let language = detect_language(Path::new(&pw.path));
                writes.push(WriteWithContent {
                    path: pw.path,
                    mtime_ns: pw.mtime_ns,
                    language,
                    content,
                    op: pw.op,
                });
            }
            Err(_) => {
                stats.skipped_binary_or_large = stats.skipped_binary_or_large.saturating_add(1);
            }
        }
    }

    // Phase 4 (one tx): populate a TEMP table with every path the
    // indexer confirmed as currently present (either unchanged or
    // freshly written), apply all writes, then purge any row in
    // `documents` whose path is NOT in the seen set.
    //
    // The SQL DELETE avoids loading all documents.path rows into a
    // Rust HashSet (gemini MEDIUM on L272) and scales better on large
    // repos because SQLite evaluates the NOT IN via the PRIMARY KEY
    // index on _seen_paths.
    let mut guard = conn
        .lock()
        .map_err(|e| IndexerError::Other(format!("conn mutex poisoned: {e}")))?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;

    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _seen_paths (path TEXT PRIMARY KEY);
         DELETE FROM _seen_paths;",
    )?;

    {
        let mut ins = tx.prepare("INSERT INTO _seen_paths (path) VALUES (?1)")?;
        for p in &unchanged_paths {
            ins.execute(params![p])?;
        }
        for w in &writes {
            ins.execute(params![w.path])?;
        }
    }

    // Prepare the symbol writer's DELETE + INSERT once for the whole
    // Phase-4 transaction (PR #6 gemini-code-assist MED — prior code
    // re-prepared both per file, burning ~1 sqlite3_prepare_v2 per
    // writer call × files-in-pass). Parsers are lazily built on first
    // hit per `ParserKey` and reused for every subsequent file that
    // hashes to the same key, so we pay `Parser::new` + `set_language`
    // at most once per parser flavor per pass. For PR-A only Rust is
    // wired (one `ParserKey` slot); PRs 2.1-B/C/D extend the dispatcher
    // in `code_graph` and the map populates itself automatically.
    // TypeScript is path-sensitive — `.ts` and `.tsx` hash to distinct
    // `ParserKey` variants (codex P2 on PR #19 + `code_graph::parser_key`).
    let mut symbol_writer = crate::code_graph::SymbolWriter::new(&tx)?;
    let mut parsers: HashMap<ParserKey, tree_sitter::Parser> = HashMap::new();

    let mut doc_insert = tx.prepare(
        "INSERT INTO documents (path, mtime, language, content) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut doc_update =
        tx.prepare("UPDATE documents SET mtime = ?2, language = ?3, content = ?4 WHERE path = ?1")?;

    for w in &writes {
        match w.op {
            Op::Insert => {
                doc_insert.execute(params![w.path, w.mtime_ns, w.language, w.content])?;
                stats.inserted = stats.inserted.saturating_add(1);
            }
            Op::Update => {
                doc_update.execute(params![w.path, w.mtime_ns, w.language, w.content])?;
                stats.updated = stats.updated.saturating_add(1);
            }
        }

        // Sprint 2 / v2.1-A — tree-sitter symbol extraction routed
        // through the central `code_graph` dispatcher. Only languages
        // recognised by `Language::from_wire` enter the extractor;
        // non-grammar tags (markdown, toml, javascript, json, yaml,
        // shell) fall through untouched. Inside the dispatcher,
        // languages without a wired grammar return
        // `ExtractError::UnsupportedLanguage` — `extract_and_store`
        // treats that as a benign skip (no warn log) until PRs
        // 2.1-B/C/D land. Extraction failures never abort the reindex;
        // the file simply lacks symbols this pass.
        if let Some(lang) = w.language.and_then(Language::from_wire) {
            stats.symbols_extracted = stats.symbols_extracted.saturating_add(extract_and_store(
                &w.path,
                &w.content,
                lang,
                &mut parsers,
                &mut symbol_writer,
            )?);
        }
    }

    // Codex P2 (PR #6 #4) — backfill Rust docs that have no matching
    // symbol rows. Two real scenarios this catches:
    //
    //   1. Schema v2 → v3 upgrade: `documents` was already populated
    //      by a prior Sprint 1 binary; `symbols` is freshly created
    //      empty by m0003. Every .rs file has an unchanged mtime so
    //      the per-file loop above never touches it — without this
    //      backfill, `by_name` / `enclosing` would return nothing
    //      until each file is manually edited.
    //
    //   2. Out-of-band desync (manual `DELETE FROM symbols`,
    //      partial extractor failures in a prior pass): the NOT-IN
    //      anti-join quietly heals those paths.
    //
    // The subquery is cheap: `symbols_by_path_line_idx` covers the
    // leading `path` column so the scan is an index lookup per row.
    // On a well-synced DB the outer query returns zero rows.
    //
    // **Known perf leak (gemini MED on PR #19 f41217f, accepted
    // advisory, deferred).** This predicate conflates "never
    // indexed for symbols" with "indexed but produced zero symbols":
    // a Rust file that parses successfully to `vec![]` (comment-only,
    // empty, or stub `mod.rs` re-exports) writes zero `symbols` rows
    // on Phase-4, then re-qualifies for this backfill on every
    // subsequent reindex pass until its content changes. I'm leaving
    // it unfixed in 2.1-A for three reasons:
    //
    //   - **Scope.** PR 2.1-A is "SymbolKind extension + language
    //     dispatcher." Schema changes belong in their own PR behind
    //     a migration step.
    //   - **Cost.** Each wasted re-extract is one `SELECT content`
    //     + one tree-sitter parse + one no-op `DELETE FROM symbols
    //     WHERE path=?`. On typical repos the empty-symbol
    //     population is small (a handful of re-export `mod.rs`
    //     files, test stubs) so per-pass waste is single-digit ms.
    //     The cost scales with that population, not with repo size.
    //   - **Origin.** I did not introduce this predicate — it shipped
    //     in PR #6's backfill (codex P2 #4) — but I'm touching this
    //     function this round, so the defer is mine to own.
    //
    // **Reopen condition.** Fix when (a) PR 2.1-B/C/D lands a grammar
    // whose typical files often produce zero symbols (Go's
    // `doc.go`, TypeScript `.d.ts`, Python `__init__.py`) so the
    // empty-symbol population grows, OR (b) user-visible reindex
    // latency on a real repo surfaces this as a hot path.
    //
    // **Fix shape when reopened.** Add `symbols_extracted_at INTEGER
    // NULL` to `documents` via a new migration (`m00NN_symbols_meta.rs`).
    // `SymbolWriter::replace` stamps `symbols_extracted_at = ?`
    // atomically with its DELETE+INSERT. This predicate becomes
    // `WHERE symbols_extracted_at IS NULL` — once a path is
    // extracted (even to zero symbols), it's no longer re-fetched
    // until its content (and thus `documents.content`) changes,
    // which already forces a Phase-4 Update that re-stamps the
    // column. Makes backfill idempotent without changing the
    // healing semantics for scenarios (1) and (2) above.
    // PR 2.1-B widened this predicate from `language = 'rust'` to the
    // full `all_extractor_wired()` set. PRs 2.1-C/D simply append
    // their grammar to the slice — no SQL edit, no new site to
    // remember. The placeholder-building idiom mirrors the Phase-5
    // reconcile block below; keeping the two call sites shaped the
    // same way means a future SSOT refactor (e.g. materialising the
    // wired set as a small SQLite view) replaces both in one pass.
    let backfill: Vec<(String, String, String)> = {
        let wired = crate::code_graph::Language::all_extractor_wired();
        let placeholders = std::iter::repeat("?")
            .take(wired.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT path, content, language FROM documents
             WHERE language IN ({placeholders})
               AND path NOT IN (SELECT DISTINCT path FROM symbols)"
        );
        let wire_params: Vec<&'static str> = wired.iter().map(|l| l.as_str()).collect();
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(wire_params.iter().copied()),
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (path, content, lang_tag) in &backfill {
        // Dispatch language is derived from the row's `language`
        // column (the Phase-3 writer's source-of-truth), not
        // hardcoded — gemini MED on PR #19 dbb5cdc. Previously
        // `Language::Rust` was pinned here, which meant PRs 2.1-B/C/D
        // had to remember to edit **two** places (the `WHERE`
        // predicate AND this call) in lock-step. Since PR 2.1-B the
        // `WHERE` predicate is built from `all_extractor_wired()` so
        // adding a grammar is a single-line append to that slice.
        // The co-evolution rule still applies: widen the slice only
        // in the same PR that lands the grammar in `parser_for` +
        // `extract_for`. Widening ahead of grammars re-qualifies
        // every matching doc on every reindex pass into
        // `extract_and_store`, which the `UnsupportedLanguage` arm
        // will convert to `Ok(0)` — a per-reindex perf leak scaling
        // with the size of the widened-but-unwired population.
        // Documented for future Claude per gemini MED on PR #19
        // 2b9d064.
        //
        // `from_wire` returning `None` means a row slipped the
        // predicate (schema drift, manual INSERT, migration desync);
        // warn so the anomaly surfaces in ops rather than
        // disappearing into a silent `continue` — gemini MED on PR
        // #19 b1ddfeb. The Phase-4 loop at ~:390 uses the same
        // `from_wire` without a warn because it runs over EVERY
        // walked row; non-grammar tags (markdown, toml, …) hit None
        // as the expected common case there. The asymmetry is
        // intentional: warn where None is anomalous (WHERE-filtered),
        // stay silent where None is expected (unfiltered walk).
        let Some(lang) = Language::from_wire(lang_tag) else {
            tracing::warn!(
                path = %path,
                lang_tag = %lang_tag,
                "backfill: skipping document with unknown language tag (schema drift?)"
            );
            continue;
        };
        stats.symbols_extracted = stats.symbols_extracted.saturating_add(extract_and_store(
            path,
            content,
            lang,
            &mut parsers,
            &mut symbol_writer,
        )?);
    }

    // Drop the prepared statements before the final tx-level
    // operations so the tx can borrow freely for the bulk DELETEs
    // below (rusqlite ties Statement lifetimes to &Transaction).
    drop(symbol_writer);
    drop(doc_insert);
    drop(doc_update);

    stats.skipped_unchanged = unchanged_paths.len() as u32;

    // Purge documents for files that vanished from the walk.
    let deleted_docs = tx.execute(
        "DELETE FROM documents WHERE path NOT IN (SELECT path FROM _seen_paths)",
        [],
    )? as u32;
    stats.deleted = deleted_docs;

    // Sprint 2: mirror the documents purge for the symbols table. A
    // file that vanishes from the walk (deleted OR grew-past-cap OR
    // renamed) must not leave stale symbol rows behind. The anti-join
    // against `_seen_paths` handles all three cases uniformly.
    tx.execute(
        "DELETE FROM symbols WHERE path NOT IN (SELECT path FROM _seen_paths)",
        [],
    )?;

    // Phase 5 reconciliation — gemini MEDs on PR #19 8fc89d5 lines
    // 389 and 459. The stale-path purge above handles paths that
    // vanished from the walk; it does NOT handle paths that are
    // still walked but whose `(documents.language, symbols.language)`
    // pair is no longer valid under this binary. Two real classes
    // of drift survive into here:
    //
    //   1. **Language transition on mtime-unchanged path.** The
    //      walker gates Phase-4 on mtime, so a path whose
    //      `documents.language` flipped from a grammar-wired tag
    //      ('rust') to a non-grammar tag ('markdown', or NULL) —
    //      detector drift across binary versions, admin edit of
    //      the mirror, schema migration corner — never re-enters
    //      `extract_and_store`, and its stale symbol rows live
    //      forever. Rounds 6/7's UnsupportedLanguage purge only
    //      fires when Phase-4 calls extract_and_store; unchanged
    //      files bypass it.
    //
    //   2. **Downgrade from a future extractor-wired binary on
    //      mtime-unchanged path.** A hypothetical future binary
    //      writes `symbols.language='typescript'` rows (or any tag
    //      not yet in `all_extractor_wired()` at current-binary
    //      authorship). User downgrades; the variant is enumerated
    //      in `Language` but not extractor-wired. An mtime-unchanged
    //      .ts file never triggers Phase-4; the backfill's
    //      `language IN (wired)` predicate skips it because the
    //      symbol-row language is not in the wired slice. Stale
    //      symbols survive until the file is edited.
    //      (Historical note: pre-PR-B this example used Python, which
    //      is now wired. PRs C/D will rotate to Go and then delete
    //      this enumeration when no pending grammars remain.)
    //
    // Both classes collapse into one predicate when we scope
    // valid symbol rows to the extractor-wired set AND require a
    // matching documents row: a symbol row is valid iff
    //
    //   (a) `symbols.language` is in `Language::all_extractor_wired()`
    //       — catches class (2), and
    //   (b) a `documents` row exists at the same `path` with the
    //       same `language` — catches class (1).
    //
    // The bulk DELETE below fires after the walk-based purges so
    // it only scans what those didn't already remove. On a
    // fresh-consistent DB it purges zero rows; the cost is one
    // index-assisted pass over `symbols` via `documents.path` PK.
    //
    // Reconciliation is intentionally a separate statement from the
    // stale-path DELETE above: the semantics differ (walk-presence
    // vs language-consistency), and keeping them split makes the
    // reason each row was purged traceable through logs if we ever
    // add per-phase counters.
    let wired = crate::code_graph::Language::all_extractor_wired();
    let placeholders = std::iter::repeat("?")
        .take(wired.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "DELETE FROM symbols
         WHERE language NOT IN ({placeholders})
            OR NOT EXISTS (
                SELECT 1 FROM documents d
                WHERE d.path = symbols.path
                  AND d.language = symbols.language
            )"
    );
    let wire_params: Vec<&'static str> = wired.iter().map(|l| l.as_str()).collect();
    let reconciled = tx.execute(
        &sql,
        rusqlite::params_from_iter(wire_params.iter().copied()),
    )?;
    if reconciled > 0 {
        tracing::info!(
            reconciled_rows = reconciled,
            "indexer: reconcile pass purged stale symbol rows (language transition or downgrade)"
        );
    }

    tx.commit()?;
    Ok(stats)
}

/// Run the tree-sitter extractor for one file and persist the result
/// via the shared `SymbolWriter`. Used by both the per-file write
/// loop and the post-loop backfill pass — the two call sites share
/// identical extract-then-write-or-purge semantics.
///
/// Parser instances are cached per language in the caller-owned
/// `parsers` map so a single `Parser::new + set_language` per
/// language threads through the entire reindex pass. On parser init
/// failure (ABI mismatch) or extraction failure we skip and return 0;
/// the caller continues with its next file without the error
/// cascading.
///
/// **v2.1-A dispatcher routing (gemini MED-3 on 830eaa5)**: grammar
/// selection goes through `code_graph::parser_for` and
/// `code_graph::extract_for`. Languages without a wired grammar
/// surface as `ExtractError::UnsupportedLanguage` — treated as a
/// benign skip (no log) because this is the expected state until PRs
/// 2.1-B/C/D land. The noisier `ExtractError::Language` (ABI
/// mismatch) and `ExtractError::Parse` still warn.
///
/// **Codex P2 #5 (PR #6) + gemini HIGH on 2b9d064 + gemini MED on
/// 0cff561**: the uniform invariant this function upholds is *"when
/// this function returns `Ok(0)` for `path`, the DB contains zero
/// symbol rows for `path` with `language = lang_tag`"*. Every early
/// `return Ok(0)` branch must therefore call
/// `SymbolWriter::replace(path, lang_tag, &[])` (a no-op on a
/// greenfield DB; a purge on a DB seeded by a prior binary). The four
/// branches and why each purges:
///
/// - `parser_for(..) → Err(ExtractError::Language|Parse)` (ABI
///   mismatch or parser init failure): the path *may* already have
///   rows from a prior successful pass on a compatible grammar;
///   purging prevents stale retrieval until the ABI is restored.
/// - `parser_for(..) → Err(UnsupportedLanguage)`: covers the
///   downgrade scenario — a future binary (e.g. 2.1-B with Python
///   wired) may have written rows for this path; re-indexing on a
///   binary where the variant exists but the grammar is not wired
///   must clear them. `replace` is cheap on paths with no rows, so
///   the no-op greenfield case pays <1ms per Python/TS/Go file.
/// - `extract_for(..) → Err(ExtractError::Parse)` (syntax errors,
///   runtime parser failure): same promise as the ABI path.
/// - `extract_for(..) → Err(UnsupportedLanguage)`: only reachable if
///   a future PR wires `parser_for` for a language but not
///   `extract_for`. Defensive symmetry with the init site keeps the
///   invariant holding even if the two dispatcher arms desync.
///
/// The early-return arms intentionally do not log a warning for
/// `UnsupportedLanguage`: one log per Python/TS/Go file per reindex
/// pass would flood stderr until PRs 2.1-B/C/D land. The purge is
/// silent; the noisier `Err(Language|Parse)` arms still warn.
fn extract_and_store(
    path: &str,
    content: &str,
    lang: Language,
    parsers: &mut HashMap<ParserKey, tree_sitter::Parser>,
    symbol_writer: &mut crate::code_graph::SymbolWriter<'_>,
) -> Result<u32, IndexerError> {
    let lang_tag = lang.as_str();

    // `parser_key` may fail if the `(language, path)` pair from the
    // DB is internally inconsistent (data-invariant violation — see
    // `code_graph::parser_key` doc). Treat Err the same way as every
    // other non-success branch in this function: log loudly, purge
    // this path's symbol rows, and return `Ok(0)`. The reindex
    // continues for every OTHER file in the pass, which is the
    // point of flipping from `unreachable!()` to `Result` on the
    // 4th gemini raise in PR #19 — a corrupt row should not kill
    // the session's whole reindex (CLAUDE.md: "SQLite mirror is a
    // rebuildable secondary index"). Uniform invariant: when this
    // function returns `Ok(0)`, the DB holds zero symbol rows for
    // `path`.
    let key = match crate::code_graph::parser_key(lang, Path::new(path)) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(
                path = %path,
                language = %lang_tag,
                error = %e,
                "parser_key rejected (language, path) pair as data-invariant violation; \
                 purging stale rows and skipping this file"
            );
            symbol_writer.replace(path, lang_tag, &[])?;
            return Ok(0);
        }
    };

    // Full `entry(key)` match (gemini MED on PR #19) — the cache is
    // keyed by `ParserKey` so `.ts` and `.tsx` files keep distinct
    // parsers once PR 2.1-C lands.
    let parser = match parsers.entry(key) {
        std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
        std::collections::hash_map::Entry::Vacant(slot) => {
            match crate::code_graph::parser_for(lang, Path::new(path)) {
                Ok(p) => slot.insert(p),
                Err(crate::code_graph::ExtractError::UnsupportedLanguage(_)) => {
                    // Expected state for non-Rust languages until PRs
                    // 2.1-B/C/D land: silent (no warn — would flood
                    // stderr on every Python/TS/Go file) but NOT empty
                    // — purge any rows a prior binary (e.g. 2.1-B with
                    // Python wired) may have written for this path.
                    // See function doc (gemini MED on PR #19 0cff561)
                    // for the uniform invariant motivating this.
                    symbol_writer.replace(path, lang_tag, &[])?;
                    return Ok(0);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path,
                        language = %lang_tag,
                        error = %e,
                        "tree-sitter parser init failed; purging stale rows for this path"
                    );
                    // Sibling to the extractor-failure branch below
                    // (gemini HIGH on PR #19 2b9d064). Both paths end
                    // in `Ok(0)` with a warn; both must uphold the
                    // "symbols missing until next pass" promise, else
                    // pre-edit rows survive as silent retrieval rot
                    // until the file is re-touched. Round-2 introduced
                    // the init-path warn but missed the purge — caught
                    // at the sibling site via gemini's cross-branch
                    // diff audit.
                    symbol_writer.replace(path, lang_tag, &[])?;
                    return Ok(0);
                }
            }
        }
    };

    match crate::code_graph::extract_for(lang, parser, content) {
        Ok(syms) => Ok(symbol_writer.replace(path, lang_tag, &syms)?),
        Err(crate::code_graph::ExtractError::UnsupportedLanguage(_)) => {
            // Dispatcher says no grammar wired; equivalent behaviour
            // to the parser_for branch above. Reachable only if a
            // future grammar lands parser_for but not extract_for
            // (defensive against desync between the two arms). Purge
            // for defensive symmetry — if a prior binary ever wrote
            // rows for this path and the current binary rejects them
            // via extract_for, the uniform "Ok(0) ⇒ zero rows"
            // invariant must still hold. See function doc.
            symbol_writer.replace(path, lang_tag, &[])?;
            Ok(0)
        }
        Err(e) => {
            tracing::warn!(
                path = %path,
                language = %lang_tag,
                error = %e,
                "tree-sitter extractor failed; purging stale rows for this path"
            );
            symbol_writer.replace(path, lang_tag, &[])?;
            Ok(0)
        }
    }
}

/// Language tag for `documents.language`. v2.1 routes grammar-wired
/// languages through `code_graph::detect_language` so there is one
/// source of truth for the four grammars; non-grammar tags (markdown,
/// toml, javascript, json, yaml, shell) are still recognised here to
/// keep `documents.language` byte-stable across v2.0 → v2.1.
fn detect_language(path: &Path) -> Option<&'static str> {
    if let Some(lang) = crate::code_graph::detect_language(path) {
        return Some(lang.as_str());
    }
    let ext = path.extension().and_then(|s| s.to_str())?;
    match ext {
        "md" => Some("markdown"),
        "toml" => Some("toml"),
        "js" | "jsx" => Some("javascript"),
        "json" => Some("json"),
        "yml" | "yaml" => Some("yaml"),
        "sh" | "bash" => Some("shell"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seed(root: &Path) {
        // Use `.ignore` rather than `.gitignore` because the tempdir is
        // not inside a git work tree (see memory note: "ignore crate
        // .gitignore vs .ignore").
        std::fs::write(root.join(".ignore"), "ignored.log\n").unwrap();
        std::fs::write(root.join("alpha.rs"), "fn alpha() { hello(); }\n").unwrap();
        std::fs::write(root.join("beta.md"), "# Title\n\nbody needle here\n").unwrap();
        std::fs::write(root.join("ignored.log"), "secret needle\n").unwrap();
    }

    #[tokio::test]
    async fn incremental_index_inserts_on_first_pass() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        seed(&repo);

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let stats = idx.reindex_incremental().await.unwrap();
        assert_eq!(stats.inserted, 2, "alpha.rs + beta.md");
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.deleted, 0);
        assert_eq!(
            stats.skipped_unchanged, 0,
            "fresh DB => nothing to skip yet"
        );
    }

    #[tokio::test]
    async fn second_pass_skips_unchanged_files() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        seed(&repo);

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let _ = idx.reindex_incremental().await.unwrap();
        let stats = idx.reindex_incremental().await.unwrap();
        assert_eq!(stats.inserted, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.skipped_unchanged, 2, "mtime-gate: both unchanged");
    }

    #[tokio::test]
    async fn removed_file_is_purged() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        seed(&repo);

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let _ = idx.reindex_incremental().await.unwrap();
        std::fs::remove_file(repo.join("beta.md")).unwrap();
        let stats = idx.reindex_incremental().await.unwrap();
        assert_eq!(stats.deleted, 1, "beta.md row must be purged");
    }

    #[tokio::test]
    async fn large_file_is_skipped() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // 2 KiB file + a 4-byte one; with max_file_bytes=64, only the small one indexes.
        std::fs::write(repo.join("big.txt"), "x".repeat(2048)).unwrap();
        std::fs::write(repo.join("small.md"), "ok\n").unwrap();

        let mut idx = RepoIndexer::open(&db, &repo).unwrap();
        idx.set_max_file_bytes(64);
        let stats = idx.reindex_incremental().await.unwrap();
        assert_eq!(stats.inserted, 1, "only small.md fits under 64 bytes");
        assert!(stats.skipped_binary_or_large >= 1);
    }

    #[tokio::test]
    async fn previously_indexed_file_that_grows_past_cap_is_purged() {
        // Regression gate for codex P1 on L257: a file that used to be
        // indexed and is now over the size cap must NOT linger in the
        // index returning stale content. The skipped-file path is
        // explicitly NOT added to the seen set, so the SQL purge
        // catches it.
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let small = repo.join("grows.md");
        std::fs::write(&small, "tiny body\n").unwrap();

        let mut idx = RepoIndexer::open(&db, &repo).unwrap();
        idx.set_max_file_bytes(1024);
        let first = idx.reindex_incremental().await.unwrap();
        assert_eq!(first.inserted, 1);

        // File grows past a newly-lowered cap (simulates both "file
        // got bigger" and "admin lowered ceiling").
        std::fs::write(&small, "x".repeat(4096)).unwrap();
        idx.set_max_file_bytes(128);
        let second = idx.reindex_incremental().await.unwrap();

        assert_eq!(
            second.deleted, 1,
            "row for now-oversize file must be purged, not kept stale: {second:?}"
        );
        assert!(second.skipped_binary_or_large >= 1);

        // And the DB must no longer carry that path.
        let guard = idx.conn.lock().unwrap();
        let n: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'grows.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn subsec_mtime_difference_triggers_update() {
        // Regression gate for codex P1 on L194: the old indexer
        // truncated mtimes to whole seconds, so a second edit within
        // the same second (common in test harnesses and fast save
        // loops) could be missed. The ns-precision mtime must catch
        // it. We drive the difference via `filetime` — not available
        // as a dep — so use File::set_modified (stable) to pin the
        // two snapshots at distinct ns-precision SystemTime values.
        use std::fs::OpenOptions;
        use std::time::{Duration, SystemTime};

        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let path = repo.join("hot.rs");
        std::fs::write(&path, "fn v1() {}\n").unwrap();

        // Pin mtime to a known value with ns precision.
        let base = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(base)
            .unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let first = idx.reindex_incremental().await.unwrap();
        assert_eq!(first.inserted, 1);

        // Rewrite with new content; pin mtime within the SAME second
        // but a different ns offset. Seconds-truncating code would
        // miss this; ns-precision must catch it.
        std::fs::write(&path, "fn v2() { different(); }\n").unwrap();
        let second_mtime = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 987_654_321);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(second_mtime)
            .unwrap();

        let second = idx.reindex_incremental().await.unwrap();
        assert_eq!(
            second.updated, 1,
            "within-second ns-precision edit must trigger an update: {second:?}"
        );
        assert_eq!(second.skipped_unchanged, 0);
    }

    #[tokio::test]
    async fn walks_honor_dot_ignore() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        seed(&repo);

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let _ = idx.reindex_incremental().await.unwrap();

        let guard = idx.conn.lock().unwrap();
        let n: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'ignored.log'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "ignored.log must be filtered by .ignore");
    }

    /// v2.1-A gemini MED-3 origin: the indexer routes symbol
    /// extraction through `code_graph::extract_for`/`parser_for`, so
    /// languages recognised by `Language::from_wire` but not yet
    /// extractor-wired flow through the dispatcher silently — no
    /// panic, no symbols, document row preserved. PR 2.1-B widened
    /// the wired set to include Python, so this test now uses
    /// TypeScript as the "admitted but pending" sentinel. PR 2.1-C
    /// flips this to Go; PR 2.1-D deletes the test (no pending
    /// grammars left).
    #[tokio::test]
    async fn pending_grammar_file_indexed_without_symbols() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("alpha.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(repo.join("script.ts"), "function beta() { return 1; }\n").unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let stats = idx.reindex_incremental().await.unwrap();
        assert_eq!(stats.inserted, 2, "both files inserted into documents");
        assert_eq!(
            stats.symbols_extracted, 1,
            "only Rust contributes symbols; TypeScript dispatcher arm returns UnsupportedLanguage"
        );

        let guard = idx.conn.lock().unwrap();
        let ts_doc_lang: String = guard
            .query_row(
                "SELECT language FROM documents WHERE path = 'script.ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ts_doc_lang, "typescript");

        let ts_symbol_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'script.ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ts_symbol_rows, 0,
            "TypeScript grammar not wired yet — no symbol rows expected"
        );

        let rs_symbol_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'alpha.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            rs_symbol_rows >= 1,
            "Rust extractor still produces at least one symbol for alpha.rs"
        );
    }

    /// v2.1-A PR #19 round-9 (gemini MED 4th raise; reversal of my
    /// round-6/round-8 rejections). `parser_key` now returns
    /// `Err(LanguagePathMismatch)` instead of panicking when the
    /// `(Language, path)` pair violates the data invariant. The
    /// indexer's call site must translate this to log + purge +
    /// `Ok(0)` — same uniform invariant as round-7's
    /// UnsupportedLanguage purge. This test exercises that translation
    /// directly: set up a minimal SymbolWriter context, call
    /// `extract_and_store` with a corrupt `(TypeScript, "foo.go")`
    /// pair, assert the function returns Ok(0) (no panic, reindex
    /// pass survives) AND that pre-existing symbol rows for the
    /// corrupt path are purged.
    ///
    /// Why a direct `extract_and_store` call instead of an end-to-end
    /// reindex: no code path from `reindex_incremental` reaches the
    /// LanguagePathMismatch arm because `detect_language` is
    /// deterministic on path extension (Phase-4 walker can't emit a
    /// `(TypeScript, foo.go)` pair) and the backfill's WHERE clause
    /// only selects `all_extractor_wired()` members — TypeScript
    /// rows are not selected until PR 2.1-C widens that slice. The
    /// Err arm becomes production-reachable in 2.1-C; until then,
    /// it's defensive + test-covered.
    #[tokio::test]
    async fn parser_key_mismatch_logs_purges_and_returns_ok_zero() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // One normal Rust file so the reindex produces a real
        // symbol row — lets us assert "other files still indexed"
        // after the corrupt call exercises the new error path.
        std::fs::write(repo.join("alpha.rs"), "fn alpha() {}\n").unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let _ = idx.reindex_incremental().await.unwrap();

        // Pre-seed a symbol row for the path we're about to hand
        // through the corrupt-pair call — simulates a prior-version
        // binary that indexed this file and left rows behind. The
        // round-9 purge must wipe it.
        let corrupt_path = "src/corrupt.go";
        {
            let guard = idx.conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO documents (path, mtime, language, content)
                     VALUES (?1, 0, 'typescript', 'whatever')",
                    [corrupt_path],
                )
                .unwrap();
            guard
                .execute(
                    "INSERT INTO symbols (name, kind, path, start_line, end_line, parent_id, language, digest)
                     VALUES ('stale', 'function', ?1, 1, 2, NULL, 'typescript', 'stale-digest')",
                    [corrupt_path],
                )
                .unwrap();
            let n: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM symbols WHERE path = ?1",
                    [corrupt_path],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "pre-seed sanity: one stale row in place");
        }

        // Open a transaction over the same connection and invoke
        // `extract_and_store` directly with the corrupt pair. This is
        // exactly what the 2.1-C backfill will do when a row with
        // language='typescript' + path='*.go' enters the loop.
        {
            let mut guard = idx.conn.lock().unwrap();
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let mut writer = crate::code_graph::SymbolWriter::new(&tx).unwrap();
            let mut parsers: HashMap<ParserKey, tree_sitter::Parser> = HashMap::new();
            let out = extract_and_store(
                corrupt_path,
                "whatever",
                Language::TypeScript,
                &mut parsers,
                &mut writer,
            )
            .expect("corrupt pair must not panic the reindex; it must log + purge + Ok(0)");
            assert_eq!(
                out, 0,
                "Err(LanguagePathMismatch) branch returns zero symbols written"
            );
            drop(writer);
            tx.commit().unwrap();
        }

        // Post-conditions: (a) corrupt path's symbol rows purged,
        // (b) the unrelated alpha.rs row is still present (the
        // reindex pass survived the corrupt pair instead of
        // panicking and aborting).
        let guard = idx.conn.lock().unwrap();
        let corrupt_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = ?1",
                [corrupt_path],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            corrupt_rows, 0,
            "LanguagePathMismatch must purge stale rows, same uniform invariant as UnsupportedLanguage"
        );
        let alpha_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'alpha.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            alpha_rows >= 1,
            "unrelated files indexed before the corrupt call remain — reindex didn't die"
        );
    }

    /// v2.1-A PR #19 round-7 gemini MED on `0cff561`: the
    /// `UnsupportedLanguage` branches in `extract_and_store` (parser-init
    /// site + extractor site) must purge any existing symbol rows for
    /// the re-indexed path, not silently `return Ok(0)`. Motivating
    /// scenario: a future binary (one that wires a grammar this binary
    /// lacks) writes symbol rows; the user downgrades back to a binary
    /// where the enum variant exists but the grammar is not wired. On
    /// re-edit, `Language::from_wire(tag)` → `Some(_)` →
    /// `extract_and_store` → `parser_for(_)` returns
    /// `UnsupportedLanguage`. Without the purge, the stale rows
    /// persist and corrupt retrieval until the file is re-edited on a
    /// binary that wires the grammar again.
    ///
    /// PR 2.1-B flipped this sentinel from Python (now wired) to
    /// TypeScript (pending until PR-C). Class lesson: structural
    /// bugs repeat on every axis of the same sibling relation (memory:
    /// `feedback_audit_sibling_sites_on_class_bugs.md`); round-6
    /// covered the `Err(_)` axis and missed the symmetric
    /// `UnsupportedLanguage` axis at both parser-init + extractor
    /// sites.
    #[tokio::test]
    async fn unsupported_language_purges_stale_symbol_rows_on_downgrade() {
        use std::fs::OpenOptions;
        use std::time::{Duration, SystemTime};

        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ts = repo.join("script.ts");
        std::fs::write(&ts, "function foo() { return 1; }\n").unwrap();

        // Pin mtime at a known ns-precision value so we can force an
        // `Op::Update` on the second pass without relying on wall-clock
        // drift.
        let t0 = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 111_111_111);
        OpenOptions::new()
            .write(true)
            .open(&ts)
            .unwrap()
            .set_modified(t0)
            .unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let first = idx.reindex_incremental().await.unwrap();
        assert_eq!(first.inserted, 1);
        assert_eq!(
            first.symbols_extracted, 0,
            "TypeScript grammar not wired → no symbols on fresh index"
        );

        // Simulate a prior-version binary (a hypothetical 2.1-C with
        // TypeScript wired) having written a symbol row for this path.
        {
            let guard = idx.conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO symbols (name, kind, path, start_line, end_line, parent_id, language, digest)
                     VALUES ('foo', 'function', 'script.ts', 1, 2, NULL, 'typescript', 'stale-digest')",
                    [],
                )
                .unwrap();
            let n: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM symbols WHERE path = 'script.ts'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "pre-seeded stale symbol row must be present");
        }

        // Edit the file + bump mtime so Phase-4 sees Op::Update and
        // hands the path to `extract_and_store`. Without the
        // UnsupportedLanguage purge, the stale symbol row survives
        // because `return Ok(0)` skipped `SymbolWriter::replace`.
        std::fs::write(&ts, "function bar() { return 2; }\n").unwrap();
        let t1 = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 999_999_999);
        OpenOptions::new()
            .write(true)
            .open(&ts)
            .unwrap()
            .set_modified(t1)
            .unwrap();

        let second = idx.reindex_incremental().await.unwrap();
        assert_eq!(second.updated, 1, "second pass must re-touch the file");
        assert_eq!(
            second.symbols_extracted, 0,
            "TypeScript still unwired; no new symbols this pass"
        );

        let guard = idx.conn.lock().unwrap();
        let stale_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'script.ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stale_rows, 0,
            "UnsupportedLanguage must purge stale rows on re-index, not leak them"
        );
    }

    /// Round-10 gemini MED #1 on 8fc89d5 (line 389). Covers the
    /// **language-transition on mtime-unchanged path** class bug:
    ///
    ///   foo.rs is indexed as Rust → symbol rows exist.
    ///   Something flips documents.language to 'markdown' without
    ///   touching the file's mtime — a future binary's detector
    ///   drift, an admin editing the mirror, or a cross-version
    ///   re-walk that reclassifies the path.
    ///   The walker sees mtime-unchanged → Op::Skip → Phase-4 never
    ///   calls extract_and_store → the UnsupportedLanguage purge
    ///   path (which is how round-6/7 fixed the *changed-file*
    ///   downgrade case) is bypassed entirely.
    ///
    /// Before the Phase-5 reconciliation: stale Rust symbol rows
    /// outlive the language transition indefinitely — retrieval
    /// surfaces a file that no longer looks like code.
    ///
    /// After the reconciliation: the predicate "symbols.language
    /// must be extractor-wired AND match a documents row with the
    /// same path+language" fails for the transitioned path, so its
    /// symbol rows are purged in bulk.
    #[tokio::test]
    async fn reconcile_purges_symbols_when_language_transitions_on_unchanged_path() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("foo.rs"), "fn alpha() {}\n").unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let first = idx.reindex_incremental().await.unwrap();
        assert!(
            first.symbols_extracted > 0,
            "first pass must produce Rust symbol rows to stage the transition"
        );

        // Flip documents.language to 'markdown' without touching the
        // file — mirrors the detector-drift / admin-edit / cross-
        // version re-walk scenario. The stored mtime stays the same,
        // so the next walk's mtime gate will mark foo.rs as
        // unchanged and Phase-4 will skip it entirely.
        {
            let guard = idx.conn.lock().unwrap();
            guard
                .execute(
                    "UPDATE documents SET language = 'markdown' WHERE path = 'foo.rs'",
                    [],
                )
                .unwrap();
            let pre: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM symbols WHERE path = 'foo.rs'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                pre > 0,
                "stale Rust symbol rows must exist before reconcile runs"
            );
        }

        let second = idx.reindex_incremental().await.unwrap();
        assert_eq!(
            second.updated, 0,
            "mtime-unchanged path must not re-enter Phase-4 write loop"
        );
        assert_eq!(second.inserted, 0, "no new inserts — same file, same mtime");

        let guard = idx.conn.lock().unwrap();
        let post: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'foo.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            post, 0,
            "reconcile must purge Rust symbol rows after documents.language flips to a non-grammar tag"
        );
    }

    /// Round-10 gemini MED #2 on 8fc89d5 (line 459). Covers the
    /// **downgrade with mtime-unchanged path** class bug:
    ///
    ///   A hypothetical future binary indexes foo.ts into
    ///   symbols.language='typescript'. User downgrades to a binary
    ///   where TypeScript is enumerated but not extractor-wired.
    ///   Reindex: walker sees foo.ts unchanged → Op::Skip → Phase-4
    ///   never touches it. Backfill's WHERE clause is bound to
    ///   `all_extractor_wired()` → skips foo.ts too. The changed-file
    ///   UnsupportedLanguage purge (rounds 6/7) never fires.
    ///
    /// The stale TypeScript rows survive forever until someone edits
    /// the file. Phase-5 reconciliation catches this by scoping
    /// valid symbols to the extractor-wired set — not the broader
    /// `Language::from_wire` set.
    ///
    /// PR 2.1-B flipped this sentinel from Python (now wired) to
    /// TypeScript (pending until PR-C). Simulated by inserting a
    /// future-binary-shaped row directly; the injection step is the
    /// same pattern used by
    /// `unsupported_language_purges_stale_symbol_rows_on_downgrade`
    /// above.
    #[tokio::test]
    async fn reconcile_purges_symbols_whose_language_is_not_extractor_wired() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("foo.ts"), "function alpha() { return 1; }\n").unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let first = idx.reindex_incremental().await.unwrap();
        assert_eq!(
            first.symbols_extracted, 0,
            "TypeScript grammar not wired → zero symbols on fresh index"
        );

        // Inject a future-binary-shaped stale row on the mtime-unchanged
        // path. Phase-4 will skip this file next pass; only Phase-5
        // reconciliation can catch the staleness.
        {
            let guard = idx.conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO symbols (name, kind, path, start_line, end_line, parent_id, language, digest)
                     VALUES ('alpha', 'function', 'foo.ts', 0, 1, NULL, 'typescript', 'stale-downgrade')",
                    [],
                )
                .unwrap();
            let pre: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM symbols WHERE path = 'foo.ts'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(pre, 1, "injected downgrade artifact must be present");
        }

        let second = idx.reindex_incremental().await.unwrap();
        assert_eq!(
            second.updated, 0,
            "foo.ts mtime-unchanged — Phase-4 must skip, reconcile is the only purge path"
        );

        let guard = idx.conn.lock().unwrap();
        let post: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'foo.ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            post, 0,
            "reconcile must purge symbol rows whose language is not extractor-wired in this binary"
        );
    }
}
