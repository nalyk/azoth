//! Thin wrapper around `git log` for the Sprint 3 co-edit graph.
//!
//! Per plan §Scope decisions #3, v2 shells out to the `git` binary
//! rather than pulling `gix`/`git2-rs`. Trade-off: no typed errors, no
//! OpenSSL pain, sandbox-clean (one subprocess, read-only on the repo,
//! no network). If shell-out proves insufficient (LFS weirdness,
//! perf ceiling on 100k-commit repos), the plan defers the upgrade
//! decision to v2.1.
//!
//! ## Output contract
//!
//! Equivalent to `git log --no-merges --name-only --format='%H%n%ct'`
//! but uses an explicit `AZOTH_COMMIT|sha|ct` sentinel line instead of
//! the plain-header format. The sentinel eliminates ambiguity between
//! "40-hex file name" and "40-hex SHA" — an edge case that is
//! astronomically unlikely but cheap to rule out.
//!
//! `--no-merges` is deliberate: merge commits with `--name-only` emit
//! nothing by default (they show combined diffs only under `-m`), and
//! counting them as zero-file commits adds noise without signal.
//!
//! ## Renames
//!
//! v2 does not pass `-M`/`--follow`. `--name-only` therefore shows a
//! rename as `delete(old)` + `add(new)` — both paths appear in the
//! same commit and pick up co-edit weight with every other file
//! touched. That is the honest behavior for the graph's purpose:
//! "files that change together" includes "the old path and its
//! replacement", which is exactly the neighborhood a rename
//! establishes.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

const COMMIT_MARK: &str = "AZOTH_COMMIT|";

#[derive(Debug, Error)]
pub enum GitError {
    #[error("spawn `git`: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git exited {code}: {stderr}")]
    NonZero { code: i32, stderr: String },
    #[error("git output was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("parse commit log: {0}")]
    Parse(String),
}

/// One commit's worth of co-edit input: the sha, its committer
/// timestamp, and the files it touched (unordered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub committed_at: i64,
    pub files: Vec<String>,
}

/// Run `git log` in `repo_root` and return the most-recent `window`
/// commits (newest first). Ordering matches git's default — no
/// explicit `--reverse` — because the co-edit accumulator is order-
/// invariant.
///
/// `window == 0` returns an empty vec without spawning git.
pub fn recent_commits(repo_root: &Path, window: u32) -> Result<Vec<Commit>, GitError> {
    if window == 0 {
        return Ok(Vec::new());
    }
    let format = format!("{COMMIT_MARK}%H|%ct");
    let n = window.to_string();
    let out = run_git(
        repo_root,
        &[
            "log",
            "--no-merges",
            "--name-only",
            "-n",
            &n,
            &format!("--format={format}"),
        ],
    )?;
    parse(&out)
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if !out.status.success() {
        return Err(GitError::NonZero {
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn parse(stdout: &str) -> Result<Vec<Commit>, GitError> {
    let mut commits: Vec<Commit> = Vec::new();
    let mut current: Option<Commit> = None;

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(COMMIT_MARK) {
            if let Some(c) = current.take() {
                commits.push(c);
            }
            let (sha, ct) = rest
                .split_once('|')
                .ok_or_else(|| GitError::Parse(format!("malformed sentinel: {line}")))?;
            let committed_at = ct
                .parse::<i64>()
                .map_err(|e| GitError::Parse(format!("bad committer ts {ct:?}: {e}")))?;
            current = Some(Commit {
                sha: sha.to_owned(),
                committed_at,
                files: Vec::new(),
            });
            continue;
        }

        if line.is_empty() {
            continue;
        }

        match current.as_mut() {
            Some(c) => c.files.push(line.to_owned()),
            None => {
                return Err(GitError::Parse(format!(
                    "unexpected file line before any commit sentinel: {line}"
                )))
            }
        }
    }

    if let Some(c) = current.take() {
        commits.push(c);
    }
    Ok(commits)
}

/// Convenience wrapper — probes whether `repo_root` is inside a git
/// work tree. Used by the graph builder to emit a clear "not a repo"
/// error rather than forwarding git's raw stderr.
pub fn is_git_repo(repo_root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Resolve the work-tree root — returns `repo_root` unchanged if it
/// already is the top-level. Callers that canonicalise paths against
/// this handle `.azoth/state.sqlite`-style subpath repos correctly.
#[allow(dead_code)]
pub fn work_tree_root(repo_root: &Path) -> Result<PathBuf, GitError> {
    let out = run_git(repo_root, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_happy_path_two_commits() {
        let input = "\
AZOTH_COMMIT|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|100

src/foo.rs
src/bar.rs

AZOTH_COMMIT|bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb|50

src/baz.rs
";
        let c = parse(input).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].sha.len(), 40);
        assert_eq!(c[0].committed_at, 100);
        assert_eq!(c[0].files, vec!["src/foo.rs", "src/bar.rs"]);
        assert_eq!(c[1].committed_at, 50);
        assert_eq!(c[1].files, vec!["src/baz.rs"]);
    }

    #[test]
    fn parse_commit_with_no_files() {
        // `--no-merges` removes empty-file commits in practice, but a
        // root commit with `--allow-empty` would land here. The parser
        // must not choke.
        let input = "AZOTH_COMMIT|cccccccccccccccccccccccccccccccccccccccc|1\n\n";
        let c = parse(input).unwrap();
        assert_eq!(c.len(), 1);
        assert!(c[0].files.is_empty());
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn parse_rejects_file_without_sentinel() {
        let input = "src/foo.rs\n";
        assert!(parse(input).is_err());
    }

    #[test]
    fn parse_rejects_malformed_sentinel() {
        assert!(parse("AZOTH_COMMIT|no_pipe_here\n").is_err());
        assert!(parse("AZOTH_COMMIT|sha|not_an_int\n").is_err());
    }

    #[test]
    fn recent_commits_window_zero_skips_spawn() {
        // Passing a non-repo path would normally error; window=0 must
        // short-circuit before the spawn.
        let out = recent_commits(Path::new("/definitely/does/not/exist"), 0).unwrap();
        assert!(out.is_empty());
    }
}
