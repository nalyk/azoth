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

/// PR #7 review (gemini MEDIUM): a flat `HashMap<(String, String), _>`
/// would allocate `path_a`/`path_b` on every pair lookup — including
/// on hits, which are the common case once a window's graph saturates.
/// Nesting as `path_a → (path_b → Accum)` lets the outer lookup run
/// through `HashMap::get_mut(&str)` without allocating, so we only
/// pay a `to_owned()` on the first sighting of each `path_a` and
/// each `(path_a, path_b)` pair. Net: a 500-commit × 10-file window
/// drops from ≈ 90 k string allocations to ≈ 2 k.
type Accumulator = HashMap<String, HashMap<String, Accum>>;

pub(crate) struct Accum {
    pub(crate) weight: f32,
    /// Commit sha from the newest commit that contributed to this
    /// pair. Newness comes from `git log` order (newest first); we
    /// only set this on first-touch.
    pub(crate) last_sha: String,
}

/// Fold a commit stream into the in-memory pair accumulator. Pulled
/// out of [`build`] so unit tests can feed hand-crafted `Commit`
/// streams without having to synthesise a fast-import repo.
pub(crate) fn accumulate(
    commits: Vec<git_cli::Commit>,
    cfg: CoEditConfig,
) -> (Accumulator, CoEditBuildStats) {
    let mut stats = CoEditBuildStats {
        commits_walked: commits.len() as u32,
        ..Default::default()
    };
    let mut pairs: Accumulator = HashMap::new();
    let mut edge_count: u32 = 0;

    for commit in commits {
        // PR #7 review (codex P2): dedupe BEFORE normalising and
        // BEFORE the skip_large check. Git can list the same path
        // twice — renames that collapse to the same name on case-
        // insensitive filesystems, or pathological `--name-only`
        // edge cases. Using the raw `commit.files.len()` here
        // would make `1 / max(1, n-1)` too small (e.g. `[a,a,b]`
        // emits one edge at 0.5 instead of 1.0) and can also trip
        // `skip_large_commits` on inflated counts.
        let mut uniq: Vec<&str> = commit.files.iter().map(String::as_str).collect();
        uniq.sort_unstable();
        uniq.dedup();
        let n = uniq.len();

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

        for i in 0..uniq.len() {
            for j in (i + 1)..uniq.len() {
                let (a, b) = (uniq[i], uniq[j]);
                // Outer `get_mut(&str)` borrows the existing key —
                // no allocation on a repeat `path_a`.
                match pairs.get_mut(a) {
                    Some(inner) => match inner.get_mut(b) {
                        Some(acc) => {
                            acc.weight += increment;
                            // First-touch wins on `last_sha`; commits
                            // arrive newest-first, so the stored sha
                            // is already the newest one.
                        }
                        None => {
                            inner.insert(
                                b.to_owned(),
                                Accum {
                                    weight: increment,
                                    last_sha: commit.sha.clone(),
                                },
                            );
                            edge_count += 1;
                        }
                    },
                    None => {
                        let mut inner = HashMap::new();
                        inner.insert(
                            b.to_owned(),
                            Accum {
                                weight: increment,
                                last_sha: commit.sha.clone(),
                            },
                        );
                        pairs.insert(a.to_owned(), inner);
                        edge_count += 1;
                    }
                }
            }
        }
    }

    stats.edges_written = edge_count;
    (pairs, stats)
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
    let (pairs, mut stats) = accumulate(commits, cfg);

    let mut guard = conn.lock().map_err(|_| CoEditError::Poisoned)?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM co_edit_edges", [])?;

    {
        let mut stmt = tx.prepare(
            "INSERT INTO co_edit_edges (path_a, path_b, weight, last_commit_sha) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (a, inner) in pairs.iter() {
            for (b, acc) in inner.iter() {
                stmt.execute(params![a, b, acc.weight as f64, acc.last_sha])?;
            }
        }
    }
    tx.commit()?;

    stats.elapsed_ms = t0.elapsed().as_millis() as u64;
    debug!(?stats, "co_edit_graph built");
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::git_cli::Commit;
    use super::*;

    fn c(sha: &str, files: &[&str]) -> Commit {
        Commit {
            sha: sha.to_string(),
            committed_at: 0,
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn pair_weight(pairs: &Accumulator, a: &str, b: &str) -> Option<f32> {
        let (p, q) = if a < b { (a, b) } else { (b, a) };
        pairs
            .get(p)
            .and_then(|inner| inner.get(q))
            .map(|a| a.weight)
    }

    #[test]
    fn dedupes_before_normalising_weight_formula() {
        // PR #7 review (codex P2): a commit with files [a, a, b]
        // must behave like a 2-file commit, emitting one edge of
        // weight 1.0 — not an inflated 3-file denominator of 0.5.
        let (pairs, stats) = accumulate(
            vec![c("s1", &["a.rs", "a.rs", "b.rs"])],
            CoEditConfig::default(),
        );
        assert_eq!(stats.commits_contributed, 1);
        assert_eq!(stats.edges_written, 1);
        let w = pair_weight(&pairs, "a.rs", "b.rs").expect("edge present");
        assert!(
            (w - 1.0).abs() < 1e-6,
            "expected weight 1.0 after dedupe, got {w}"
        );
    }

    #[test]
    fn skip_large_commits_uses_deduped_count() {
        // Pre-PR-#7-review: skip_large=3 + files=[a,a,b] (raw n=3)
        // would SKIP a 2-file commit. Post-fix: dedupe first, n=2,
        // commit contributes.
        let cfg = CoEditConfig {
            window: 10,
            skip_large_commits: 3,
        };
        let (pairs, stats) = accumulate(vec![c("s1", &["a.rs", "a.rs", "b.rs"])], cfg);
        assert_eq!(stats.commits_skipped_large, 0);
        assert_eq!(stats.commits_contributed, 1);
        assert_eq!(stats.edges_written, 1);
        assert!(pair_weight(&pairs, "a.rs", "b.rs").is_some());
    }

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
