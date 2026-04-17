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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use azoth_core::event_store::migrations;
use azoth_core::event_store::sqlite::MirrorError;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use thiserror::Error;

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
    // writer call × files-in-pass). The Rust parser is lazily built
    // on first Rust hit and reused for every subsequent .rs file so
    // we pay `Parser::new` + `set_language` at most once per pass.
    let mut symbol_writer = crate::code_graph::SymbolWriter::new(&tx)?;
    let mut rust_parser: Option<tree_sitter::Parser> = None;

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

        // Sprint 2 — tree-sitter symbol extraction, Rust only. The
        // language detector from `detect_language` names the grammar
        // ("rust"); every other language skips the extractor until
        // v2.1 lands the Python/TS/Go grammars. Extraction failures
        // (bad bytes, parser error) never abort the reindex; the file
        // simply lacks symbols this pass and gets another shot next
        // pass.
        if w.language == Some("rust") {
            let parser = match rust_parser.as_mut() {
                Some(p) => p,
                None => {
                    match crate::code_graph::rust_parser() {
                        Ok(p) => {
                            rust_parser = Some(p);
                            rust_parser.as_mut().expect("just set above")
                        }
                        Err(e) => {
                            // set_language failing is catastrophic
                            // (grammar ABI mismatch) — skip every
                            // Rust file this pass rather than
                            // thrashing through retries.
                            tracing::warn!(
                                error = %e,
                                "tree-sitter rust parser init failed; skipping all .rs files this pass"
                            );
                            continue;
                        }
                    }
                }
            };
            match crate::code_graph::extract_rust(parser, &w.content) {
                Ok(syms) => {
                    let n = symbol_writer.replace(&w.path, "rust", &syms)?;
                    stats.symbols_extracted = stats.symbols_extracted.saturating_add(n);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %w.path,
                        error = %e,
                        "tree-sitter rust extractor failed; symbols for this path will be missing until next pass"
                    );
                }
            }
        }
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

fn detect_language(path: &Path) -> Option<&'static str> {
    let ext = path.extension().and_then(|s| s.to_str())?;
    match ext {
        "rs" => Some("rust"),
        "md" => Some("markdown"),
        "toml" => Some("toml"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" => Some("javascript"),
        "go" => Some("go"),
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
}
