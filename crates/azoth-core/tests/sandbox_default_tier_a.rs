//! Integration-level pin for the v2.1-H default flip.
//!
//! The policy module's own unit tests live at
//! `sandbox/policy.rs::tests`; this file exists so a caller who
//! reaches `SandboxPolicy::from_env()` from *outside* the crate
//! (which is the real TUI / embedder path) sees the same three
//! branches:
//!
//! 1. Unset env → `TierA` on hosts with unprivileged user-ns, `Off`
//!    otherwise.
//! 2. Empty env → same as unset.
//! 3. Explicit `off` → `Off`, regardless of host capability.
//!
//! The probe is re-queried at the integration layer instead of
//! hard-coding the expected tier because CI runners and WSL2
//! kernels without user-ns enabled degrade to `Off` by design.
//!
//! v2.1-H R3: each test warms the cache via `warm_userns_cache()`
//! first so the cached-probe path can populate. Without this, the
//! thread-id guard (added to catch library embedders who forgot
//! to warm) would fail-closed on the first-call from a cold cache
//! and force every test to see `Off`, masking the real branches.
//!
//! Parallel safety: cargo compiles every `tests/*.rs` file into its
//! own binary (its own process), so this file's tests share a
//! process with each other but NOT with the lib's unit tests.
//! The env-var race therefore only needs a within-binary Mutex,
//! which we declare locally. The lib's `crate::test_support` guard
//! is `#[cfg(test)]` and lives in a different crate compilation,
//! so we can't `use` it here — it would force the test-support
//! module out of `#[cfg(test)]` visibility.

use azoth_core::sandbox::{probe_unprivileged_userns, warm_userns_cache, SandboxPolicy};
use std::sync::Mutex;

/// Serialise `AZOTH_SANDBOX` mutations across the three tests in
/// this binary. Without this, `cargo test` in its default parallel
/// mode races the set_var / remove_var pairs and one test briefly
/// sees another's env state, producing nondeterministic results.
static SANDBOX_ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard — takes the lock, sets env on entry, cleans on Drop.
/// Drop runs under the still-held lock, so the full set_var →
/// test body → remove_var sequence is atomic against any other
/// guard-using test in this binary. Survives panic (Rust RAII).
struct SandboxEnvGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl SandboxEnvGuard {
    fn tier(tier: &str) -> Self {
        let guard = SANDBOX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AZOTH_SANDBOX", tier);
        Self { _guard: guard }
    }

    fn unset() -> Self {
        let guard = SANDBOX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AZOTH_SANDBOX");
        Self { _guard: guard }
    }

    fn set_tier(&self, tier: &str) {
        std::env::set_var("AZOTH_SANDBOX", tier);
    }
}

impl Drop for SandboxEnvGuard {
    fn drop(&mut self) {
        std::env::remove_var("AZOTH_SANDBOX");
    }
}

#[test]
fn unset_env_yields_tier_a_when_userns_supported() {
    warm_userns_cache();
    let _env = SandboxEnvGuard::unset();
    let got = SandboxPolicy::from_env();
    let want = if probe_unprivileged_userns() {
        SandboxPolicy::TierA
    } else {
        SandboxPolicy::Off
    };
    assert_eq!(got, want);
}

#[test]
fn empty_env_matches_unset_env() {
    warm_userns_cache();
    let env = SandboxEnvGuard::unset();
    let unset = SandboxPolicy::from_env();
    env.set_tier("");
    let empty = SandboxPolicy::from_env();
    assert_eq!(unset, empty);
}

#[test]
fn explicit_off_always_yields_off() {
    let _env = SandboxEnvGuard::tier("off");
    assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::Off);
}
