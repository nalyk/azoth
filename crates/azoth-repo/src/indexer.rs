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
    let backfill: Vec<(String, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT path, content, language FROM documents
             WHERE language = 'rust'
               AND path NOT IN (SELECT DISTINCT path FROM symbols)",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (path, content, lang_tag) in &backfill {
        // Dispatch language is derived from the row's `language`
        // column (the Phase-3 writer's source-of-truth), not
        // hardcoded — gemini MED on PR #19 dbb5cdc. Previously
        // `Language::Rust` was pinned here, which meant PRs 2.1-B/C/D
        // had to remember to edit **two** places (the `WHERE`
        // predicate AND this call) in lock-step. Now widening the
        // predicate is sufficient; dispatch follows automatically —
        // BUT only widen the `WHERE` predicate in the SAME PR that
        // lands the grammar in `parser_for` + `extract_for`. Widening
        // ahead of grammars is a per-reindex perf leak: every
        // Python/TS/Go doc missing from `symbols` gets fetched +
        // handed to `extract_and_store` only to return `Ok(0)` via
        // the `UnsupportedLanguage` skip. The backfill query hits
        // every grammarless doc on every reindex pass until a
        // grammar lands — documented for future Claude per gemini
        // MED on PR #19 2b9d064.
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
    let key = crate::code_graph::parser_key(lang, Path::new(path));

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

    /// v2.1-A gemini MED-3: the indexer routes symbol extraction
    /// through `code_graph::extract_for`/`parser_for`, so non-Rust
    /// languages recognised by `Language::from_wire` (Python, TS, Go)
    /// flow through the dispatcher but silently produce no symbols
    /// until PRs 2.1-B/C/D wire their grammars. This test locks the
    /// contract: a `.py` file is indexed as a document with
    /// `language='python'`, but the `symbols` table carries no rows
    /// for that path.
    #[tokio::test]
    async fn pending_grammar_file_indexed_without_symbols() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("alpha.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(repo.join("script.py"), "def beta():\n    pass\n").unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let stats = idx.reindex_incremental().await.unwrap();
        assert_eq!(stats.inserted, 2, "both files inserted into documents");
        assert_eq!(
            stats.symbols_extracted, 1,
            "only Rust contributes symbols; Python dispatcher arm returns UnsupportedLanguage"
        );

        let guard = idx.conn.lock().unwrap();
        let py_doc_lang: String = guard
            .query_row(
                "SELECT language FROM documents WHERE path = 'script.py'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(py_doc_lang, "python");

        let py_symbol_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'script.py'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            py_symbol_rows, 0,
            "Python grammar not wired yet — no symbol rows expected"
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

    /// v2.1-A PR #19 round-7 gemini MED on `0cff561`: the
    /// `UnsupportedLanguage` branches in `extract_and_store` (parser-init
    /// site + extractor site) must purge any existing symbol rows for
    /// the re-indexed path, not silently `return Ok(0)`. Motivating
    /// scenario: a future binary (e.g. 2.1-B) wires Python, writes
    /// Python symbol rows; the user downgrades back to 2.1-A where the
    /// Python enum variant exists but the grammar is not wired. On
    /// re-edit, `Language::from_wire("python")` → `Some(Python)` →
    /// `extract_and_store` → `parser_for(Python)` returns
    /// `UnsupportedLanguage`. Without the purge, the stale v2.1-B
    /// symbol rows persist in the 2.1-A DB and corrupt retrieval until
    /// the file is re-edited on a binary that wires Python again.
    ///
    /// Class lesson (see memory:
    /// `feedback_audit_sibling_sites_on_class_bugs.md`): round-6 fixed
    /// the same purge gap on the parser-init `Err(_)` branch by
    /// sibling-pointing at the extractor-failure `Err(_)` branch; the
    /// audit covered the `Err(_)` axis but missed the symmetric
    /// `UnsupportedLanguage` axis at both sites. Structural bugs repeat
    /// on every axis of the same sibling relation, not just the one
    /// you happened to inspect.
    #[tokio::test]
    async fn unsupported_language_purges_stale_symbol_rows_on_downgrade() {
        use std::fs::OpenOptions;
        use std::time::{Duration, SystemTime};

        let td = TempDir::new().unwrap();
        let db = td.path().join("mirror.sqlite");
        let repo = td.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let py = repo.join("script.py");
        std::fs::write(&py, "def foo():\n    pass\n").unwrap();

        // Pin mtime at a known ns-precision value so we can force an
        // `Op::Update` on the second pass without relying on wall-clock
        // drift.
        let t0 = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 111_111_111);
        OpenOptions::new()
            .write(true)
            .open(&py)
            .unwrap()
            .set_modified(t0)
            .unwrap();

        let idx = RepoIndexer::open(&db, &repo).unwrap();
        let first = idx.reindex_incremental().await.unwrap();
        assert_eq!(first.inserted, 1);
        assert_eq!(
            first.symbols_extracted, 0,
            "2.1-A: Python grammar not wired → no symbols on fresh index"
        );

        // Simulate a prior-version binary (e.g. hypothetical 2.1-B with
        // Python wired) having written a symbol row for this path.
        {
            let guard = idx.conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO symbols (name, kind, path, start_line, end_line, parent_id, language, digest)
                     VALUES ('foo', 'function', 'script.py', 1, 2, NULL, 'python', 'stale-digest')",
                    [],
                )
                .unwrap();
            let n: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM symbols WHERE path = 'script.py'",
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
        std::fs::write(&py, "def bar():\n    pass\n").unwrap();
        let t1 = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 999_999_999);
        OpenOptions::new()
            .write(true)
            .open(&py)
            .unwrap()
            .set_modified(t1)
            .unwrap();

        let second = idx.reindex_incremental().await.unwrap();
        assert_eq!(second.updated, 1, "second pass must re-touch the file");
        assert_eq!(
            second.symbols_extracted, 0,
            "Python still unwired; no new symbols this pass"
        );

        let guard = idx.conn.lock().unwrap();
        let stale_rows: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE path = 'script.py'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stale_rows, 0,
            "UnsupportedLanguage must purge stale rows on re-index, not leak them"
        );
    }
}
