//! `GitStatusDiffSource` — shells out to `git status --porcelain -z`
//! inside the repo root and parses the output into a [`Diff`] for
//! the TurnDriver's impact-validator phase.
//!
//! ## Wire format: `-z` NUL-terminated
//!
//! PR #9 gemini MED called out that the previous line-oriented
//! parser split renames on the literal ` -> ` substring, which
//! breaks when a filename contains that sequence (git quotes such
//! paths in the LF form, but the quote-escape logic is fragile).
//! The `-z` form sidesteps the whole surface:
//!
//! ```text
//! XY <path>\0                          (non-rename entries)
//! XY <new-path>\0<old-path>\0          (rename / copy entries)
//! ```
//!
//! Paths are NUL-terminated, never quoted, can contain any byte
//! except NUL — which filesystems don't allow anyway. That removes
//! every escape/quote ambiguity in one stroke.
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
            .arg("--porcelain")
            .arg("-z")
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ImpactError::DiffSource(format!("git status spawn: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ImpactError::DiffSource(format!(
                "git status failed ({}): {}",
                out.status,
                stderr.trim()
            )));
        }
        Ok(parse_porcelain_z(&out.stdout))
    }
}

/// Pure parser for `git status --porcelain -z` output. Separated
/// from the shell-out path so unit tests can feed canned fixtures.
///
/// The input is a stream of NUL-terminated records. A record whose
/// status byte `X` or `Y` is `R` (rename) or `C` (copy) consumes an
/// additional NUL-terminated token for the old path — we take the
/// new path and discard the old, because the selector looks up
/// stems in the post-rename namespace.
pub fn parse_porcelain_z(bytes: &[u8]) -> Diff {
    let mut paths: Vec<String> = Vec::new();
    // `-z` guarantees NUL is the only record separator, so
    // byte-level split is safe; UTF-8 lossy-decode is only applied
    // at path construction time. A trailing NUL produces an empty
    // trailing token which the skip-short guard below filters.
    let tokens: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        // Valid record: at least 3 bytes for `XY ` plus one for the
        // path. Empty trailing tokens and malformed stubs fall
        // through.
        if tok.len() < 4 {
            i += 1;
            continue;
        }
        let x = tok[0];
        let y = tok[1];
        // `R` / `C` can appear in either status column per git
        // porcelain grammar. Either one means the next token is the
        // old path, which we must consume and discard.
        let is_rename = x == b'R' || x == b'C' || y == b'R' || y == b'C';
        let path = String::from_utf8_lossy(&tok[3..]).into_owned();
        if !path.is_empty() && !paths.iter().any(|p| p == &path) {
            paths.push(path);
        }
        i += if is_rename { 2 } else { 1 };
    }
    Diff::from_paths(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_added_and_deleted() {
        // -z wire shape: NUL separator, no trailing newline on
        // individual entries. Trailing NUL after the last entry is
        // tolerated (produces an empty final token which the parser
        // skips via the `len < 4` guard).
        let bytes = b" M src/foo.rs\0A  src/new.rs\0 D src/gone.rs\0";
        let d = parse_porcelain_z(bytes);
        assert_eq!(
            d.changed_files,
            vec!["src/foo.rs", "src/new.rs", "src/gone.rs"]
        );
    }

    #[test]
    fn parses_rename_consumes_old_path_keeps_new() {
        // Rename entry is TWO NUL-terminated tokens under -z: the
        // first carries the XY status + new path, the second is
        // the old path (which we discard).
        let bytes = b"R  src/new.rs\0src/old.rs\0";
        let d = parse_porcelain_z(bytes);
        assert_eq!(d.changed_files, vec!["src/new.rs"]);
    }

    #[test]
    fn parses_filename_containing_literal_arrow() {
        // PR #9 gemini MED motivating case: the previous line-based
        // parser split on ` -> ` and would truncate this path. The
        // -z parser treats NUL as the only separator, so embedded
        // arrows are fine.
        let bytes = b" M src/weird -> filename.rs\0";
        let d = parse_porcelain_z(bytes);
        assert_eq!(d.changed_files, vec!["src/weird -> filename.rs"]);
    }

    #[test]
    fn includes_untracked() {
        let bytes = b"?? src/fresh.rs\0";
        let d = parse_porcelain_z(bytes);
        assert_eq!(d.changed_files, vec!["src/fresh.rs"]);
    }

    #[test]
    fn mixed_rename_and_modify_roundtrips() {
        // Exercises the index advance through a rename pair +
        // subsequent normal entry; regression guard if the i += 2
        // jump ever slips.
        let bytes = b"R  new.rs\0old.rs\0 M unchanged.rs\0";
        let d = parse_porcelain_z(bytes);
        assert_eq!(d.changed_files, vec!["new.rs", "unchanged.rs"]);
    }

    #[test]
    fn dedupes_duplicate_entries() {
        let bytes = b" M src/foo.rs\0 M src/foo.rs\0";
        let d = parse_porcelain_z(bytes);
        assert_eq!(d.changed_files, vec!["src/foo.rs"]);
    }

    #[test]
    fn empty_output_is_empty_diff() {
        let d = parse_porcelain_z(b"");
        assert!(d.is_empty());
    }
}
