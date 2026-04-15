//! Tier B: Tier A + fuse-overlayfs writable workspace.

use super::{Sandbox, SandboxError};
use crate::schemas::SandboxTier;

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

/// Probe whether `fuse-overlayfs` is available on PATH. Tier B degrades to a
/// tmpfs workspace with a warning if not.
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
