//! Repo indexer — walks `root` via `ignore::WalkBuilder` (scope-parity
//! with `RipgrepLexicalRetrieval`) and upserts UTF-8 file contents into
//! the `documents` SQLite table. Triggers defined in migration
//! `m0002_fts_schema` keep `documents_fts` in sync automatically.
//!
//! Incremental: each row carries an `mtime` column. `reindex_incremental`
//! skips files whose on-disk mtime equals the stored mtime — matches the
//! plan §Sprint 1 "mtime-gated upsert" requirement. Files that used to be
//! indexed but are now gone are deleted at the end of the pass (so
//! renamed or removed files don't linger in the index).
//!
//! Size/binary policy:
//! - Files larger than `max_file_bytes` (default 1 MiB) are skipped.
//! - Files that can't be read as UTF-8 (binaries, images, etc.) are
//!   skipped. The read error is not surfaced — binary files are a
//!   routine case, not an error.
//!
//! Connection ownership: the indexer opens its OWN `rusqlite::Connection`
//! to the shared mirror DB file (`.azoth/state.sqlite` by convention).
//! WAL mode is enabled at open — set once, persisted on the file — so
//! the mirror's own connection and this one coexist. Migrations are
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

    // Collect file data (path, mtime, content) outside the DB lock so
    // the SQLite write transaction is short.
    struct Item {
        path: String,
        mtime: i64,
        language: Option<&'static str>,
        content: String,
    }
    let mut items: Vec<Item> = Vec::new();
    let mut walked_paths: Vec<String> = Vec::new();

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

        let abs = dent.path();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => abs,
        };
        let path_str = rel.to_string_lossy().into_owned();
        walked_paths.push(path_str.clone());

        let meta = match std::fs::metadata(abs) {
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

        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let content = match std::fs::read_to_string(abs) {
            Ok(s) => s,
            Err(_) => {
                // Binary / non-UTF-8 / unreadable — silently skip.
                stats.skipped_binary_or_large = stats.skipped_binary_or_large.saturating_add(1);
                continue;
            }
        };

        items.push(Item {
            path: path_str,
            mtime,
            language: detect_language(rel),
            content,
        });
    }

    let mut guard = conn
        .lock()
        .map_err(|e| IndexerError::Other(format!("conn mutex poisoned: {e}")))?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;

    for item in &items {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT mtime FROM documents WHERE path = ?1",
                params![item.path],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(old) if old == item.mtime => {
                stats.skipped_unchanged = stats.skipped_unchanged.saturating_add(1);
            }
            Some(_) => {
                tx.execute(
                    "UPDATE documents
                       SET mtime = ?2, language = ?3, content = ?4
                     WHERE path = ?1",
                    params![item.path, item.mtime, item.language, item.content],
                )?;
                stats.updated = stats.updated.saturating_add(1);
            }
            None => {
                tx.execute(
                    "INSERT INTO documents (path, mtime, language, content)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![item.path, item.mtime, item.language, item.content],
                )?;
                stats.inserted = stats.inserted.saturating_add(1);
            }
        }
    }

    // Purge rows for files that disappeared from the walk. `walked_paths`
    // contains every file we considered (including those we skipped as
    // binary/large) so we don't falsely purge large-but-present files
    // from a previous smaller-threshold pass.
    let purge_count = {
        let present: std::collections::HashSet<&str> =
            walked_paths.iter().map(|s| s.as_str()).collect();
        let mut stmt = tx.prepare("SELECT path FROM documents")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut to_delete: Vec<String> = Vec::new();
        for row in rows {
            let p = row?;
            if !present.contains(p.as_str()) {
                to_delete.push(p);
            }
        }
        drop(stmt);
        for p in &to_delete {
            tx.execute("DELETE FROM documents WHERE path = ?1", params![p])?;
        }
        to_delete.len() as u32
    };
    stats.deleted = purge_count;
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
