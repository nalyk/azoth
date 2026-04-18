//! Sandbox tier implementations. v1 ships Tier A (user-ns + net-ns +
//! cgroup v2 + Landlock + seccompiler) as the active enforcement, Tier B
//! stubbed with fuse-overlayfs probing, Tier C/D as EffectNotAvailable hooks.
//!
//! The actual mechanism stack lives in the Linux-only modules below. On
//! non-Linux hosts the `Sandbox` trait is still present but `Tier A` returns
//! a `SandboxUnsupported` error so core compiles cleanly for tests.

use crate::schemas::{EffectClass, SandboxTier};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("effect class {0:?} is not available in v1")]
    EffectNotAvailable(EffectClass),
    #[error("sandbox not supported on this platform")]
    Unsupported,
    #[error("missing runtime dependency: {0}")]
    MissingDependency(&'static str),
    #[error("syscall error: {0}")]
    Syscall(String),
}

pub trait Sandbox: Send + Sync {
    fn tier(&self) -> SandboxTier;
    /// Prepare the sandbox for a single tool invocation. Implementations
    /// will return a handle the caller can use to execute a closure inside
    /// the jailed process; v1 leaves this as a no-op for the in-process
    /// dispatcher path and exposes a separate `spawn_jailed` helper for
    /// out-of-process tools.
    fn prepare(&self) -> Result<(), SandboxError>;
}

pub mod policy;
pub mod probe;
pub mod tier_a;
pub mod tier_b;
pub mod tier_cd;

pub use policy::SandboxPolicy;
pub use probe::probe_unprivileged_userns;
pub use tier_a::TierA;
#[cfg(target_os = "linux")]
pub use tier_b::OverlayWorkspace;
pub use tier_b::{probe_fuse_overlayfs, TierB};
pub use tier_cd::{TierC, TierD};

/// Pick a concrete sandbox for an effect class. Tiers C and D return
/// `EffectNotAvailable` in v1 — the architectural hooks exist but no
/// implementation.
pub fn sandbox_for(ec: EffectClass) -> Result<Box<dyn Sandbox>, SandboxError> {
    if !ec.is_available_in_v1() {
        return Err(SandboxError::EffectNotAvailable(ec));
    }
    let tier: SandboxTier = ec.into();
    Ok(match tier {
        SandboxTier::A => Box::new(TierA::new()),
        SandboxTier::B => Box::new(TierB::new()),
        SandboxTier::C => Box::new(TierC),
        SandboxTier::D => Box::new(TierD),
    })
}
