//! Tier B: Tier A + fuse-overlayfs writable workspace.
//!
//! Mounts a fuse-overlayfs merged view of the repo root so tools can write
//! freely inside a turn without touching the real tree. The lower dir is the
//! repo root (read-only), upper and work dirs live in a tmpdir scoped to
//! the turn. On success the caller can commit the upper layer; on abort the
//! tmpdir is dropped and the repo stays pristine.

use super::{Sandbox, SandboxError};
use crate::schemas::SandboxTier;

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

pub struct TierB {
    pub fuse_overlayfs_present: bool,
}

impl TierB {
    pub fn new() -> Self {
        Self {
            fuse_overlayfs_present: probe_fuse_overlayfs(),
        }
    }
}

impl Default for TierB {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for TierB {
    fn tier(&self) -> SandboxTier {
        SandboxTier::B
    }

    fn prepare(&self) -> Result<(), SandboxError> {
        #[cfg(not(target_os = "linux"))]
        {
            return Err(SandboxError::Unsupported);
        }
        #[cfg(target_os = "linux")]
        {
            if !self.fuse_overlayfs_present {
                tracing::warn!(
                    "fuse-overlayfs not found on PATH; Tier B running degraded \
                     (tmpfs workspace, no diff view)"
                );
            }
            Ok(())
        }
    }
}

/// A mounted fuse-overlayfs workspace. Dropping this struct unmounts the
/// overlay and removes the temp dir.
#[cfg(target_os = "linux")]
pub struct OverlayWorkspace {
    pub merged: PathBuf,
    pub upper: PathBuf,
    _tmpdir: tempfile::TempDir,
    mounted: bool,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for OverlayWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayWorkspace")
            .field("merged", &self.merged)
            .field("upper", &self.upper)
            .field("mounted", &self.mounted)
            .finish()
    }
}

#[cfg(target_os = "linux")]
impl OverlayWorkspace {
    /// Mount a fuse-overlayfs overlay of `lower` into a freshly created
    /// temp directory. Returns a handle whose `merged` field is the path
    /// tools should use as their working directory.
    pub fn mount(lower: &Path) -> Result<Self, SandboxError> {
        let tmpdir =
            tempfile::tempdir().map_err(|e| SandboxError::Syscall(format!("tmpdir: {e}")))?;
        let upper = tmpdir.path().join("upper");
        let work = tmpdir.path().join("work");
        let merged = tmpdir.path().join("merged");

        std::fs::create_dir_all(&upper)
            .map_err(|e| SandboxError::Syscall(format!("mkdir upper: {e}")))?;
        std::fs::create_dir_all(&work)
            .map_err(|e| SandboxError::Syscall(format!("mkdir work: {e}")))?;
        std::fs::create_dir_all(&merged)
            .map_err(|e| SandboxError::Syscall(format!("mkdir merged: {e}")))?;

        let status = std::process::Command::new("fuse-overlayfs")
            .arg("-o")
            .arg(format!(
                "lowerdir={},upperdir={},workdir={}",
                lower.display(),
                upper.display(),
                work.display(),
            ))
            .arg(&merged)
            .status()
            .map_err(|e| {
                SandboxError::MissingDependency(Box::leak(
                    format!("fuse-overlayfs: {e}").into_boxed_str(),
                ))
            })?;

        if !status.success() {
            return Err(SandboxError::Syscall(format!(
                "fuse-overlayfs exited {}",
                status.code().unwrap_or(-1),
            )));
        }

        Ok(Self {
            merged,
            upper,
            _tmpdir: tmpdir,
            mounted: true,
        })
    }

    /// Unmount the overlay. Called automatically on drop, but can be
    /// invoked earlier for explicit error handling.
    pub fn unmount(&mut self) -> Result<(), SandboxError> {
        if !self.mounted {
            return Ok(());
        }
        let status = std::process::Command::new("fusermount")
            .arg("-u")
            .arg(&self.merged)
            .status()
            .map_err(|e| SandboxError::Syscall(format!("fusermount -u: {e}")))?;
        self.mounted = false;
        if !status.success() {
            return Err(SandboxError::Syscall(format!(
                "fusermount -u exited {}",
                status.code().unwrap_or(-1),
            )));
        }
        Ok(())
    }

    /// List files that were created or modified in the upper layer.
    /// These are the files the tool wrote during its execution.
    pub fn changed_files(&self) -> Result<Vec<PathBuf>, SandboxError> {
        let mut files = Vec::new();
        collect_files(&self.upper, &self.upper, &mut files)
            .map_err(|e| SandboxError::Syscall(format!("walk upper: {e}")))?;
        Ok(files)
    }
}

/// Walk `dir` (non-recursive into symlinked dirs — see SECURITY
/// note) and push every file-like entry as a path relative to
/// `root`.
///
/// ## Security — symlink-directory traversal (codex round-6 P1 on PR #14)
///
/// The previous implementation used `path.is_dir()`, which FOLLOWS
/// symlinks. A Tier-B command like `ln -s /etc leak` placed a
/// symlink in the overlay upper layer; `path.is_dir()` resolved
/// to `/etc` and returned true, so the walker descended into
/// `/etc` and returned entries like `leak/passwd`. The downstream
/// `stage_overlay_back` then copied `ws.upper.join("leak/passwd")`
/// (which resolves through the symlink to `/etc/passwd`) into the
/// real repo. **Host files leaked into the repo via a
/// model-issued bash command — a data-integrity breach.**
///
/// Fix: use `symlink_metadata()` which does NOT follow symlinks.
/// Symlinks are recorded as entries (stage_overlay_back recreates
/// them via `std::os::unix::fs::symlink`) but never recursed
/// into. Regular directories recurse as before; regular files are
/// recorded; other file types (sockets, fifos, block/char
/// devices) are also recorded as entries so the whiteout logic
/// in stage-back can handle char-device whiteouts.
#[cfg(target_os = "linux")]
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let dir_meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if !dir_meta.file_type().is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            // Record the symlink itself; DO NOT follow into it.
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        } else if ft.is_dir() {
            // Real directory — recurse.
            collect_files(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            // Regular file, char device, fifo, socket — record.
            out.push(rel.to_path_buf());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
impl Drop for OverlayWorkspace {
    fn drop(&mut self) {
        if self.mounted {
            let _ = self.unmount();
        }
    }
}

/// Probe whether `fuse-overlayfs` is available on PATH.
pub fn probe_fuse_overlayfs() -> bool {
    #[cfg(target_os = "linux")]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            if dir.join("fuse-overlayfs").is_file() {
                return true;
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_bool() {
        let _ = probe_fuse_overlayfs();
    }

    #[test]
    fn tier_b_reports_correct_tier() {
        let b = TierB::new();
        assert_eq!(b.tier(), SandboxTier::B);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_mount_fails_gracefully_without_fuse_overlayfs() {
        if probe_fuse_overlayfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let err = OverlayWorkspace::mount(dir.path()).unwrap_err();
        match err {
            SandboxError::MissingDependency(_) | SandboxError::Syscall(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_mount_and_write_if_available() {
        if !probe_fuse_overlayfs() {
            eprintln!("fuse-overlayfs not on PATH, skipping");
            return;
        }
        let lower = tempfile::tempdir().unwrap();
        std::fs::write(lower.path().join("existing.txt"), "original").unwrap();

        let mut ws = OverlayWorkspace::mount(lower.path()).unwrap();

        // Write a new file through the merged view.
        std::fs::write(ws.merged.join("new.txt"), "created").unwrap();

        // The lower dir is untouched.
        assert!(!lower.path().join("new.txt").exists());

        // The upper layer captured the write.
        let changed = ws.changed_files().unwrap();
        assert!(changed.iter().any(|p| p.ends_with("new.txt")));

        // The merged view shows both files.
        assert!(ws.merged.join("existing.txt").exists());
        assert!(ws.merged.join("new.txt").exists());

        ws.unmount().unwrap();
    }
}
