//! `GitStatusDiffSource` — shells out to `git status --porcelain`
//! inside the repo root and parses the output into a [`Diff`] for
//! the TurnDriver's impact-validator phase.
//!
//! ## Wire format
//!
//! `git status --porcelain` emits lines shaped
//! ```text
//! XY <path>
//! XY <old> -> <new>      (rename/copy)
//! ```
//! where `X` is the index state and `Y` the worktree state. We
//! treat any entry whose state is not `??` (untracked-but-ignored is
//! filtered by `--porcelain` already skipping `.gitignore`'d paths)
//! plus any rename target as "changed". For renames we take the new
//! path — that is what the selector will look up in the test
//! universe.
//!
//! ## Why shell-out instead of gix
//!
//! Consistent with the Sprint 3 co-edit graph decision (v2 plan
//! §A3, §Sprint 3): no new git dep, no OpenSSL dance,
//! sandbox-clean. A future v2.1 can swap the impl to gix if the
//! need arises — the `DiffSource` trait signature doesn't move.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use azoth_core::impact::{Diff, DiffSource, ImpactError};

pub struct GitStatusDiffSource {
    repo_root: PathBuf,
}

impl GitStatusDiffSource {
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[async_trait]
impl DiffSource for GitStatusDiffSource {
    fn name(&self) -> &'static str {
        "git_status"
    }

    async fn diff(&self) -> Result<Diff, ImpactError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("status")
            .arg("--porcelain=v1")
            .stderr(Stdio::null())
            .output()
            .await
            .map_err(|e| ImpactError::DiffSource(format!("git status spawn: {e}")))?;
        if !out.status.success() {
            return Err(ImpactError::DiffSource(format!(
                "git status failed ({})",
                out.status
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(parse_porcelain(&text))
    }
}

/// Pure parser for `git status --porcelain=v1` output. Separated
/// from the shell-out path so unit tests can feed canned fixtures.
pub fn parse_porcelain(text: &str) -> Diff {
    let mut paths: Vec<String> = Vec::new();
    for line in text.lines() {
        // `--porcelain=v1` lines are `XY <path>` with exactly two
        // status chars followed by a space. Untracked (`??`) entries
        // are included — a newly-added test file is legitimately
        // part of the impact diff.
        if line.len() < 4 {
            continue;
        }
        let rest = &line[3..];
        let path = match rest.split(" -> ").last() {
            Some(p) => p.trim().trim_matches('"').to_string(),
            None => rest.trim().to_string(),
        };
        if !path.is_empty() && !paths.iter().any(|p| p == &path) {
            paths.push(path);
        }
    }
    Diff::from_paths(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_added_and_deleted() {
        // Explicit newline separation — do NOT use `\` continuations
        // here, because rustc preserves the leading whitespace of the
        // continued line inside the string literal, which would
        // defeat the byte-offset parser.
        let out = " M src/foo.rs\nA  src/new.rs\n D src/gone.rs\n";
        let d = parse_porcelain(out);
        assert_eq!(
            d.changed_files,
            vec!["src/foo.rs", "src/new.rs", "src/gone.rs"]
        );
    }

    #[test]
    fn parses_rename_and_keeps_new_path() {
        let out = "R  src/old.rs -> src/new.rs\n";
        let d = parse_porcelain(out);
        assert_eq!(d.changed_files, vec!["src/new.rs"]);
    }

    #[test]
    fn includes_untracked() {
        let out = "?? src/fresh.rs\n";
        let d = parse_porcelain(out);
        assert_eq!(d.changed_files, vec!["src/fresh.rs"]);
    }

    #[test]
    fn dedupes_duplicate_entries() {
        let out = " M src/foo.rs\n M src/foo.rs\n";
        let d = parse_porcelain(out);
        assert_eq!(d.changed_files, vec!["src/foo.rs"]);
    }

    #[test]
    fn empty_output_is_empty_diff() {
        let d = parse_porcelain("");
        assert!(d.is_empty());
    }
}
