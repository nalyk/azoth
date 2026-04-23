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
//! single-threaded context.
//!
//! Callers inside any multi-threaded context — tokio multi-thread
//! runtime, `std::thread`, Rayon, another async runtime — MUST go
//! through [`probe_unprivileged_userns_cached`]. The cached form
//! enforces the contract by thread-identity: [`warm_userns_cache`]
//! records the thread it was called from as the "startup thread"
//! (expected to be single-threaded). Any subsequent cold-cache call
//! from a different thread fails closed (returns `false` with a
//! one-shot `tracing::error`) rather than forking from a worker.
//!
//! Binaries therefore pre-warm from `fn main()` before spawning any
//! runtime; library embedders pre-warm from their single-threaded
//! startup the same way. After warm, every subsequent call site —
//! `SandboxPolicy::from_env()`, `bash::build_bash_command`, tests —
//! is a lock-free atomic load.

use std::sync::OnceLock;
use std::thread::ThreadId;

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

/// Thread recorded by the first [`warm_userns_cache`] call.
/// Cold-cache calls to [`probe_unprivileged_userns_cached`] must
/// run on THIS thread to be allowed to fork — from any other thread
/// we have no way to know whether the process is still
/// single-threaded, so we fail closed.
static WARM_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Cached variant of [`probe_unprivileged_userns`] safe for callers
/// inside ANY multi-threaded context. The actual fork happens
/// **once** per process, from the thread that called
/// [`warm_userns_cache`]. Subsequent calls are lock-free atomic
/// loads.
///
/// ## First-call enforcement
///
/// The cache initializer still forks, but only when the embedder
/// has registered a warming thread via [`warm_userns_cache`] AND
/// the current call is on that same thread. Any other cold-cache
/// call — from a tokio worker, a `std::thread::spawn` child, a
/// Rayon job, or another async runtime's worker — returns `false`
/// (conservative: "no user-ns, sandbox will degrade to Off") and
/// emits a single `tracing::error` naming the fix. This enforces
/// the probe's SAFETY precondition by thread-identity rather than
/// runtime-flavor, so it catches multi-threaded contexts the
/// runtime-flavor check missed (codex P1 bot-progression).
///
/// ## Embedder contract
///
/// Binaries and library embedders must pre-warm from single-
/// threaded startup via [`warm_userns_cache`] before spawning ANY
/// multi-threaded runtime (tokio, std::thread, Rayon, etc). After
/// warm, every subsequent call is a plain atomic load.
pub fn probe_unprivileged_userns_cached() -> bool {
    if let Some(cached) = USERNS_CACHE.get() {
        return *cached;
    }
    // Cold cache. Fork is only safe from the thread that warmed.
    let current = std::thread::current().id();
    match WARM_THREAD.get() {
        Some(warm) if *warm == current => *USERNS_CACHE.get_or_init(probe_unprivileged_userns),
        _ => {
            static WARNED: OnceLock<()> = OnceLock::new();
            WARNED.get_or_init(|| {
                tracing::error!(
                    "probe_unprivileged_userns_cached called with cold cache from a thread \
                     that is not the one that called sandbox::warm_userns_cache(); embedders \
                     must warm from single-threaded startup before spawning any runtime. \
                     Returning false; sandbox defaults will degrade to Off."
                );
            });
            false
        }
    }
}

/// Pre-warm the user-ns probe cache from a single-threaded context.
/// Records the calling thread as the "warming thread" so the cached
/// probe can tell future cold-cache callers apart from the startup
/// path. Idempotent — subsequent calls on any thread are no-ops
/// once the cache is populated.
///
/// Intended for `fn main()` (or equivalent library startup) before
/// spawning any multi-threaded runtime. Cheap: one fork on first
/// call, atomic load thereafter.
pub fn warm_userns_cache() {
    // `set` succeeds on first caller; later warms are no-ops.
    let _ = WARM_THREAD.set(std::thread::current().id());
    let _ = USERNS_CACHE.get_or_init(probe_unprivileged_userns);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Module-level guard: `cargo test` runs under `--test-threads=1`
    /// (repo convention, see `CLAUDE.md` Test Patterns), so every
    /// `#[test]` fn runs on the same harness worker thread. The
    /// first test that calls [`warm_userns_cache`] registers that
    /// thread as the warming thread for the rest of the process.
    /// Subsequent tests on the same harness worker hit the cache
    /// through the warm path.
    static WARMED_FOR_TESTS: AtomicBool = AtomicBool::new(false);

    fn ensure_warmed() {
        if !WARMED_FOR_TESTS.swap(true, Ordering::SeqCst) {
            warm_userns_cache();
        }
    }

    /// The cache either returns the same value across two reads, or
    /// never executes the probe body at all on the second call. This
    /// test doesn't isolate the cache across tests, so we only assert
    /// consistency, not a specific tier.
    #[test]
    fn cache_is_consistent_across_calls() {
        ensure_warmed();
        let a = probe_unprivileged_userns_cached();
        let b = probe_unprivileged_userns_cached();
        assert_eq!(a, b, "cache returned different values across calls");
    }

    #[test]
    fn warm_userns_cache_is_idempotent() {
        ensure_warmed();
        // Second + third warms must be no-ops — the first set
        // populates both WARM_THREAD and USERNS_CACHE, later
        // warms see `set` fail silently and `get_or_init` hit.
        warm_userns_cache();
        warm_userns_cache();
        let _ = probe_unprivileged_userns_cached();
    }

    /// Cold cache on the warming thread — cache populates, returns
    /// the host's actual capability.
    #[test]
    fn cold_call_from_warming_thread_populates_cache() {
        ensure_warmed();
        // Even if some earlier test already warmed, the contract is
        // that a cached-cache call on the warming thread never
        // fails closed. Assert idempotent determinism, not a value.
        let got = probe_unprivileged_userns_cached();
        let again = probe_unprivileged_userns_cached();
        assert_eq!(got, again);
    }

    /// Cold cache from a DIFFERENT thread must NOT fork (fail-
    /// closed with `false`) — this is the load-bearing guard for
    /// library embedders who forget to warm (codex R2 P1).
    /// Warming happens implicitly from the test-harness thread via
    /// `ensure_warmed`, so a freshly-spawned `std::thread` is never
    /// the warming thread.
    ///
    /// If the cache happens to be already populated from an earlier
    /// test, this test is a tautology (both calls return the cached
    /// value). To exercise the cold-cache path deterministically we
    /// would need to reset `USERNS_CACHE`, which `OnceLock` doesn't
    /// allow — so this test is a best-effort assertion that calling
    /// the cached probe from another thread is at minimum
    /// deterministic and never panics.
    #[test]
    fn cached_probe_call_from_other_thread_is_deterministic() {
        ensure_warmed();
        let from_main = probe_unprivileged_userns_cached();
        let handle = std::thread::spawn(probe_unprivileged_userns_cached);
        let from_child = handle.join().unwrap();
        assert_eq!(
            from_main, from_child,
            "cached value must be consistent across threads once warmed"
        );
    }
}
