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

use azoth_core::sandbox::{probe_unprivileged_userns, warm_userns_cache, SandboxPolicy};

#[test]
fn unset_env_yields_tier_a_when_userns_supported() {
    warm_userns_cache();
    std::env::remove_var("AZOTH_SANDBOX");
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
    std::env::remove_var("AZOTH_SANDBOX");
    let unset = SandboxPolicy::from_env();
    std::env::set_var("AZOTH_SANDBOX", "");
    let empty = SandboxPolicy::from_env();
    std::env::remove_var("AZOTH_SANDBOX");
    assert_eq!(unset, empty);
}

#[test]
fn explicit_off_always_yields_off() {
    std::env::set_var("AZOTH_SANDBOX", "off");
    assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::Off);
    std::env::remove_var("AZOTH_SANDBOX");
}
