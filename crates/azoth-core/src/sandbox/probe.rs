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
/// ## First-call enforcement
///
/// The cache initializer still forks. If the first call to this
/// function happens from inside a **multi-thread** tokio runtime
/// (`RuntimeFlavor::MultiThread`), the fork would violate the raw
/// probe's SAFETY precondition. To enforce the contract without
/// deadlocking the caller, this function checks
/// `Handle::try_current().runtime_flavor()` on a cold cache: if the
/// caller is inside a multi-thread runtime, the function returns
/// `false` (conservative — "no user-ns, sandbox will degrade to
/// Off") and emits a single `tracing::error` pointing the embedder
/// at [`warm_userns_cache`]. Single-thread tokio (`CurrentThread`,
/// the default `#[tokio::test]` flavor) and non-tokio contexts are
/// allowed to fork because the child only invokes the async-signal-
/// safe `unshare` + `_exit` sequence.
///
/// ## Embedder contract
///
/// Binaries and embedders should pre-warm the cache from
/// single-threaded startup via [`warm_userns_cache`] before spawning
/// the multi-thread tokio runtime; after that point every call site
/// — `SandboxPolicy::from_env()`, `bash::build_bash_command`, tests
/// — hits the cache as a plain atomic load.
pub fn probe_unprivileged_userns_cached() -> bool {
    if let Some(cached) = USERNS_CACHE.get() {
        return *cached;
    }
    // Cold cache. Refuse to fork from multi-thread tokio; fallback
    // to Off with a one-shot error so the embedder sees exactly
    // what to fix.
    if cold_call_from_multi_thread_runtime() {
        static WARNED: OnceLock<()> = OnceLock::new();
        WARNED.get_or_init(|| {
            tracing::error!(
                "probe_unprivileged_userns_cached called with cold cache from a multi-thread \
                 tokio runtime; embedders must call sandbox::warm_userns_cache() before \
                 spawning the runtime. Returning false; sandbox defaults will degrade to Off."
            );
        });
        return false;
    }
    *USERNS_CACHE.get_or_init(probe_unprivileged_userns)
}

/// True iff we're inside a **multi-thread** tokio runtime. Single-
/// thread tokio is safe to fork from (no sibling worker threads
/// hold glibc locks); non-tokio contexts are trivially safe.
fn cold_call_from_multi_thread_runtime() -> bool {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Err(_) => false, // not inside any tokio runtime
        Ok(h) => !matches!(h.runtime_flavor(), RuntimeFlavor::CurrentThread),
    }
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

    /// Detector-only unit test: outside tokio, the guard says "safe".
    #[test]
    fn cold_call_guard_is_false_outside_tokio() {
        assert!(!cold_call_from_multi_thread_runtime());
    }

    /// Under current-thread tokio the guard still says "safe" —
    /// matches `#[tokio::test]` default flavor, which is what the
    /// existing bash.rs suite relies on.
    #[test]
    fn cold_call_guard_is_false_under_current_thread_tokio() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let inside = rt.block_on(async { cold_call_from_multi_thread_runtime() });
        assert!(!inside, "current-thread runtime must not trigger the guard");
    }

    /// Under multi-thread tokio the guard says "unsafe to fork".
    /// A process-isolated subtest prevents leaking the spawned
    /// runtime into the rest of the suite.
    #[test]
    fn cold_call_guard_is_true_under_multi_thread_tokio() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let inside = rt.block_on(async { cold_call_from_multi_thread_runtime() });
        assert!(inside, "multi-thread runtime must trigger the guard");
    }
}
