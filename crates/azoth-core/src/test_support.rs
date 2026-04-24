//! Cross-module test helpers for the azoth-core crate.
//!
//! Only compiled under `cfg(test)` so the helpers never bleed into the
//! public API surface. Integration tests in `tests/*.rs` live in their
//! own test binaries and therefore need to redeclare their own guard
//! types — the race is within-process, and cargo gives each `tests/*.rs`
//! file its own process, so a single static `Mutex` inside this module
//! is sufficient for every unit test that runs in the library's
//! `#[cfg(test)]` binary.

use std::sync::Mutex;

/// Serialises every test that mutates the process-wide `AZOTH_SANDBOX`
/// env var. Without this, `cargo test` in its default parallel mode lets
/// two `#[tokio::test]` threads race `set_var` / `remove_var` against
/// each other — the tier-B symlink-stage-back tests then intermittently
/// see "sandbox off" when they expected `tier_b`, the refused entry is
/// staged back through the non-sandbox path, and the assertion fires.
/// Pre-2026-04-24 this was misdiagnosed as a WSL2/fuse-overlayfs flake;
/// the real bug was the ambient env. Sibling sites in `red_team.rs`,
/// `sandbox/policy.rs` were racing under the same root cause.
///
/// `std::env::set_var` is still safe on edition 2021 but has the same
/// race-prone semantics as Rust edition 2024's `unsafe` variant;
/// serialising here is the portable fix.
pub(crate) static SANDBOX_ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard: acquire `SANDBOX_ENV_LOCK`, set `AZOTH_SANDBOX` to the
/// requested tier, remove on drop. Holding the guard across the test
/// body keeps every env mutation (and the test body that depends on it)
/// inside the lock so no parallel test observes a half-set env. On
/// panic, `Drop` still fires — the env is always cleaned up, which
/// the previous explicit `remove_var` lines did not guarantee.
///
/// Pass an empty tier (`""`) to assert the "empty env" branch; pass
/// `"off"` / `"tier_a"` / `"tier_b"` for the named branches; the guard
/// does not interpret the value, only serialises access.
///
/// Example:
/// ```ignore
/// let _env = SandboxEnvGuard::tier("tier_b");
/// // ... test body that expects AZOTH_SANDBOX=tier_b ...
/// // on scope exit: AZOTH_SANDBOX is removed; lock released.
/// ```
pub(crate) struct SandboxEnvGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl SandboxEnvGuard {
    pub(crate) fn tier(tier: &str) -> Self {
        // Rescue poisoned locks: one earlier test panicking must not
        // cascade-fail every subsequent test. We still get mutual
        // exclusion; we just tolerate prior panics.
        let guard = SANDBOX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AZOTH_SANDBOX", tier);
        Self { _guard: guard }
    }

    /// Variant for tests that need to assert behaviour when the env is
    /// explicitly *unset*. Still takes the lock so a parallel test does
    /// not slide a `set_var` in between `remove_var` and whatever we
    /// read next.
    pub(crate) fn unset() -> Self {
        let guard = SANDBOX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AZOTH_SANDBOX");
        Self { _guard: guard }
    }

    /// Change the tier within the same locked scope. Tests that walk
    /// through several env states in one function (e.g. `unset →
    /// empty → tier_a → tier_b`) use this to avoid dropping + retaking
    /// the lock between assertions, which would briefly expose the env
    /// to a parallel test.
    pub(crate) fn set_tier(&self, tier: &str) {
        // `&self` is fine: `_guard` is held for the whole lifetime of
        // this struct, so we already own the global lock.
        std::env::set_var("AZOTH_SANDBOX", tier);
    }

    /// Unset within the same locked scope. Symmetric counterpart to
    /// `set_tier` for tests that need to flip env state mid-body.
    pub(crate) fn clear(&self) {
        std::env::remove_var("AZOTH_SANDBOX");
    }
}

impl Drop for SandboxEnvGuard {
    fn drop(&mut self) {
        // Runs under the lock (MutexGuard drops AFTER this, per Rust
        // struct-field drop order), so the set_var → test → remove_var
        // sequence is atomic against any other guard-using test.
        std::env::remove_var("AZOTH_SANDBOX");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_sets_and_cleans_up_env() {
        {
            let _env = SandboxEnvGuard::tier("tier_b");
            assert_eq!(
                std::env::var("AZOTH_SANDBOX").as_deref(),
                Ok("tier_b"),
                "tier() sets the env inside the guard"
            );
        }
        assert!(
            std::env::var("AZOTH_SANDBOX").is_err(),
            "Drop removes the env on scope exit"
        );
    }

    #[test]
    fn guard_cleans_up_on_panic() {
        // Wrap a panicking closure with catch_unwind so the test itself
        // keeps running after the inner panic fires. The RAII Drop
        // must still remove AZOTH_SANDBOX even on panic — previously
        // the explicit remove_var at the end of each test was skipped
        // when an earlier assertion panicked, leaving a poisoned
        // env that broke subsequent parallel tests.
        let _ = std::panic::catch_unwind(|| {
            let _env = SandboxEnvGuard::tier("tier_a");
            panic!("simulated test failure while env is set");
        });
        assert!(
            std::env::var("AZOTH_SANDBOX").is_err(),
            "Drop fires on unwind; env was cleaned despite panic"
        );
    }

    #[test]
    fn unset_variant_leaves_env_empty() {
        let _env = SandboxEnvGuard::unset();
        assert!(std::env::var("AZOTH_SANDBOX").is_err());
    }
}
