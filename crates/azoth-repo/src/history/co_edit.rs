//! Co-edit graph builder — accumulates pair weights over recent
//! commits and writes them to `co_edit_edges` (m0004).
//!
//! ## Weight formula
//!
//! Per plan §Sprint 3, for a commit that touched `n` files:
//!
//! ```text
//! w(a, b) += 1 / max(1, n - 1)
//! ```
//!
//! Intuition: a 2-file commit moves 1.0 unit of co-edit evidence onto
//! the one pair it created; a 10-file commit spreads its evidence
//! across C(10, 2) = 45 pairs at `1/9` each, totalling 5.0 units. The
//! `max(1, n-1)` guard keeps single-file commits (zero pairs) from
//! producing NaN — they simply contribute nothing.
//!
//! ## Squash-merge degeneracy (plan risk ledger #3)
//!
//! A 100-file squash-merge explodes into C(100, 2) = 4950 edges of
//! weight `1/99 ≈ 0.010` — dense and nearly uniform, which is the
//! definition of signal-free. `skip_large_commits` (default 50)
//! excludes such commits wholesale. `0` disables the skip.
//!
//! ## Build model
//!
//! `build()` is a **from-scratch replace**. Every call deletes all
//! existing `co_edit_edges` rows in the same transaction that
//! rewrites them. This is the simplest way to honour the `window`
//! knob — shrinking the window must actually narrow the graph, and
//! an accumulative write path would pin old edges from beyond the
//! window forever. Small-repo cost is trivial; large-repo cost is
//! still bounded by `window × skip_large_commits² / 2`.
//!
//! `last_commit_sha` records the **newest** commit that touched a
//! pair — since `git log` returns commits newest-first, the first
//! commit encountered for a given pair wins the slot.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use azoth_core::retrieval::CoEditConfig;
use rusqlite::{params, Connection, TransactionBehavior};
use thiserror::Error;
use tracing::debug;

use super::git_cli::{self, GitError};

#[derive(Debug, Error)]
pub enum CoEditError {
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("conn mutex poisoned")]
    Poisoned,
    #[error("not a git work tree: {0}")]
    NotARepo(std::path::PathBuf),
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CoEditBuildStats {
    /// Commits walked from `git log`.
    pub commits_walked: u32,
    /// Commits included in the weight accumulation.
    pub commits_contributed: u32,
    /// Commits filtered by `skip_large_commits`.
    pub commits_skipped_large: u32,
    /// Distinct `(path_a, path_b)` pairs written.
    pub edges_written: u32,
    /// Wall-clock duration of the build in milliseconds. Useful for
    /// the plan's `≤ 3s on 500 commits` ship gate.
    pub elapsed_ms: u64,
}

/// Key for the in-memory accumulator. Both strings are already
/// canonicalised (`a < b`) so HashMap equality matches the
/// `CHECK (path_a < path_b)` SQLite invariant.
type PairKey = (String, String);

struct Accum {
    weight: f32,
    /// Commit sha from the newest commit that contributed to this
    /// pair. Newness comes from `git log` order (newest first); we
    /// only set this on first-touch.
    last_sha: String,
}

/// Build the co-edit graph by walking the last `cfg.window` commits
/// in `repo_root` and writing the aggregate edges into
/// `co_edit_edges` via the shared mirror connection.
pub fn build(
    conn: &Arc<Mutex<Connection>>,
    repo_root: &Path,
    cfg: CoEditConfig,
) -> Result<CoEditBuildStats, CoEditError> {
    let t0 = Instant::now();

    if !git_cli::is_git_repo(repo_root) {
        return Err(CoEditError::NotARepo(repo_root.to_path_buf()));
    }

    let commits = git_cli::recent_commits(repo_root, cfg.window)?;
    let mut stats = CoEditBuildStats {
        commits_walked: commits.len() as u32,
        ..Default::default()
    };

    let mut pairs: HashMap<PairKey, Accum> = HashMap::new();
    for commit in commits {
        let n = commit.files.len();
        if cfg.skip_large_commits > 0 && n as u32 > cfg.skip_large_commits {
            stats.commits_skipped_large += 1;
            continue;
        }
        if n < 2 {
            // No pairs to emit; still count as contributed for
            // transparency — a 1-file commit is part of the window.
            stats.commits_contributed += 1;
            continue;
        }
        stats.commits_contributed += 1;
        let increment = 1.0_f32 / (n as f32 - 1.0).max(1.0);

        // Dedupe within a commit. Git can list the same path twice
        // if a rename turns into "delete old + add new" under the
        // same name due to a case-only change on case-insensitive
        // filesystems.
        let mut uniq: Vec<&str> = commit.files.iter().map(String::as_str).collect();
        uniq.sort_unstable();
        uniq.dedup();

        for i in 0..uniq.len() {
            for j in (i + 1)..uniq.len() {
                let (a, b) = (uniq[i], uniq[j]);
                // Already sorted → (a, b) is canonical.
                let key: PairKey = (a.to_owned(), b.to_owned());
                match pairs.get_mut(&key) {
                    Some(acc) => {
                        acc.weight += increment;
                        // Keep the newest sha only; commits arrive
                        // newest-first, so first-touch wins.
                    }
                    None => {
                        pairs.insert(
                            key,
                            Accum {
                                weight: increment,
                                last_sha: commit.sha.clone(),
                            },
                        );
                    }
                }
            }
        }
    }

    stats.edges_written = pairs.len() as u32;

    let mut guard = conn.lock().map_err(|_| CoEditError::Poisoned)?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM co_edit_edges", [])?;

    {
        let mut stmt = tx.prepare(
            "INSERT INTO co_edit_edges (path_a, path_b, weight, last_commit_sha) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for ((a, b), acc) in pairs.iter() {
            stmt.execute(params![a, b, acc.weight as f64, acc.last_sha])?;
        }
    }
    tx.commit()?;

    stats.elapsed_ms = t0.elapsed().as_millis() as u64;
    debug!(?stats, "co_edit_graph built");
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_key_sort_matches_canonical_form() {
        // Sanity: the (i, j) with i < j in the sorted vector always
        // yields uniq[i] < uniq[j] because `sort_unstable` gives a
        // lexicographic total order. This is the invariant the
        // SQLite `CHECK (path_a < path_b)` enforces at write time.
        let mut v = vec!["z.rs", "a.rs", "m.rs"];
        v.sort_unstable();
        assert_eq!(v, vec!["a.rs", "m.rs", "z.rs"]);
        for i in 0..v.len() {
            for j in (i + 1)..v.len() {
                assert!(v[i] < v[j]);
            }
        }
    }
}
