//! Host capability probes. Used by the runtime to decide whether a given
//! sandbox tier can be applied on this host, or whether to fall back.
//!
//! ## Threading model
//!
//! The user-ns probe forks. `fork(2)` from a multi-threaded process is
//! POSIX-restricted: only async-signal-safe calls are legal in the
//! child until `_exit` / `execve`, and glibc locks held by other
//! parent threads are inherited dead in the child. `probe_unprivileged_userns`
//! therefore carries a SAFETY precondition that it runs from a
//! single-threaded context. Callers inside the tokio runtime MUST go
//! through [`probe_unprivileged_userns_cached`], which pays the fork
//! once from whichever thread calls first. Binaries pre-warm the
//! cache from `fn main()` before spawning tokio via [`warm_userns_cache`]
//! so the first cache fill happens single-threaded; every subsequent
//! call is a plain atomic load.

use std::sync::OnceLock;

/// Returns `true` iff this host supports unprivileged `CLONE_NEWUSER`.
///
/// We probe by forking a child that attempts the unshare; we do **not**
/// collapse the current process into a new user namespace — that would
/// permanently alter the parent's credential view for the rest of its life.
#[cfg(target_os = "linux")]
pub fn probe_unprivileged_userns() -> bool {
    use nix::sched::{unshare, CloneFlags};
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, ForkResult};

    // SAFETY: the probe must be called from a single-threaded context.
    // The child does no allocation beyond the unshare syscall and exits
    // immediately; the parent blocks on waitpid. Callers that might
    // already be inside the tokio multi-threaded runtime MUST call
    // `probe_unprivileged_userns_cached` instead (which pays this fork
    // at most once and caches the answer process-wide).
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let rc = unshare(CloneFlags::CLONE_NEWUSER);
            unsafe { nix::libc::_exit(if rc.is_ok() { 0 } else { 1 }) };
        }
        Ok(ForkResult::Parent { child }) => {
            matches!(waitpid(child, None), Ok(WaitStatus::Exited(_, 0)))
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_unprivileged_userns() -> bool {
    false
}

/// Process-wide cache for the user-ns probe. Populated on the first
/// call; subsequent calls return the cached value without forking.
static USERNS_CACHE: OnceLock<bool> = OnceLock::new();

/// Cached variant of [`probe_unprivileged_userns`] safe for callers
/// inside the tokio runtime. The actual fork happens **once** per
/// process. Subsequent calls are lock-free atomic loads.
///
/// The very first caller still runs `fork()`. Binaries should pre-warm
/// the cache from single-threaded startup via [`warm_userns_cache`]
/// before spawning the tokio runtime; after that point every call site
/// — `SandboxPolicy::from_env()`, `bash::build_bash_command`, tests —
/// hits the cache.
pub fn probe_unprivileged_userns_cached() -> bool {
    *USERNS_CACHE.get_or_init(probe_unprivileged_userns)
}

/// Pre-warm the user-ns probe cache from a single-threaded context.
/// Idempotent. Intended for `fn main()` before the tokio runtime
/// starts. Cheap enough (one fork on first call, atomic load
/// thereafter) to call unconditionally.
pub fn warm_userns_cache() {
    let _ = probe_unprivileged_userns_cached();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache either returns the same value across two reads, or
    /// never executes the probe body at all on the second call. This
    /// test doesn't isolate the cache across tests, so we only assert
    /// consistency, not a specific tier.
    #[test]
    fn cache_is_consistent_across_calls() {
        let a = probe_unprivileged_userns_cached();
        let b = probe_unprivileged_userns_cached();
        assert_eq!(a, b, "cache returned different values across calls");
    }

    #[test]
    fn warm_userns_cache_is_idempotent() {
        warm_userns_cache();
        warm_userns_cache();
        // If warming was not idempotent, the second call would panic
        // or produce a different value — neither happens here.
        let _ = probe_unprivileged_userns_cached();
    }
}
