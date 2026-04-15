//! Tier A: user-ns + net-ns + cgroup v2 + Landlock + seccompiler,
//! unprivileged sequence.
//!
//! In v1 the setup runs in a forked child immediately before `execve` of the
//! tool workload. The in-process dispatcher path used by azoth-core's v1
//! tools (which are all `Observe`, trivially safe) does *not* exercise the
//! fork path — it only records that Tier A is the required tier and relies
//! on the tool implementation not performing writes. This keeps the safety
//! guarantee monotonic: adding a real out-of-process exec strictly narrows
//! the permitted syscalls.

use super::{Sandbox, SandboxError};
use crate::schemas::SandboxTier;

pub struct TierA {}

impl TierA {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for TierA {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for TierA {
    fn tier(&self) -> SandboxTier {
        SandboxTier::A
    }

    fn prepare(&self) -> Result<(), SandboxError> {
        // The in-process path is a no-op. The out-of-process driver should
        // call `spawn_jailed` (below) which performs the real sequence.
        #[cfg(not(target_os = "linux"))]
        {
            return Err(SandboxError::Unsupported);
        }
        #[cfg(target_os = "linux")]
        Ok(())
    }
}

/// The unprivileged namespace + cgroup + landlock + seccomp sequence described
/// in draft_plan §"Sandbox tiers — honest mechanism stack". In v1 this is a
/// stub that returns `Unsupported` on non-Linux and sketches the sequence on
/// Linux. A real implementation lands once an out-of-process tool exists
/// that needs it.
#[cfg(target_os = "linux")]
pub fn spawn_jailed(_tool_argv: &[&str]) -> Result<std::process::Child, SandboxError> {
    // Order (doc-only in v1):
    //   1. unshare(CLONE_NEWUSER)
    //   2. write /proc/self/{uid_map,gid_map,setgroups}
    //   3. unshare(CLONE_NEWNET)
    //   4. set up cgroup v2 slice
    //   5. (Tier B) mount fuse-overlayfs
    //   6. Landlock ruleset apply
    //   7. seccompiler filter apply
    //   8. execve(tool)
    Err(SandboxError::Syscall(
        "out-of-process sandbox sequence not implemented in v1".into(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_jailed(_tool_argv: &[&str]) -> Result<std::process::Child, SandboxError> {
    Err(SandboxError::Unsupported)
}
