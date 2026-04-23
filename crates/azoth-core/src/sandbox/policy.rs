//! Runtime sandbox policy — what tools should actually enforce
//! when the dispatcher hands them an `ApplyLocal` / `ApplyRepo`
//! effect class. Driven by the `AZOTH_SANDBOX` env var.
//!
//! v2.0.x default was `Off` (opt-in isolation). v2.1-H flipped it:
//! when the env var is unset or empty, the runtime now routes bash
//! through TierA (user-ns + Landlock) if the host supports
//! unprivileged `CLONE_NEWUSER`, and falls back to `Off` with a
//! `tracing::warn` otherwise. Operators opt OUT via
//! `AZOTH_SANDBOX=off`.
//!
//! ## Why this lives on a free enum rather than `ExecutionContext`
//!
//! Tools read the policy at dispatch time. Threading it through
//! `ExecutionContext` would force every call-site (tests,
//! MockAdapter fixtures, headless eval) to set a field — lots of
//! churn for a knob most tests want to leave at default. An
//! env-var read is a single line and matches how the sandbox's
//! existing `AZOTH_SKIP_TIER_A` test-bypass knob works.
//!
//! ## Semantics
//!
//! Unset / empty env → `TierA` on hosts with unprivileged user-ns,
//! `Off` otherwise (with a `tracing::warn` on degradation). Explicit
//! `off` → `Off`. Explicit `tier_a` / `tier_b` → that tier.
//!
//! `Off` routes tools through a direct host command, same as v2.0.0.
//!
//! `TierA` sends out-of-process tools (bash today; future scripted
//! writes) through the user-ns + net-ns + Landlock + (permissive)
//! seccomp jail from `sandbox::tier_a::spawn_jailed`. Landlock
//! allow_read covers the whole repo root; allow_write is limited
//! to `/tmp`. Writes outside those bounds return EACCES.
//!
//! `TierB` adds a `fuse-overlayfs` merged mount rooted at the repo
//! so tool writes land in an upper layer and the real repo stays
//! pristine. The dispatcher collects
//! `OverlayWorkspace::changed_files()` at turn commit and stages
//! those deltas back explicitly via `apply_*` effects.
//!
//! Tier B needs `fuse-overlayfs` on PATH — the probe at
//! `sandbox::tier_b::probe_fuse_overlayfs()` decides degradation.
//! Tier A needs unprivileged user-ns — probe at
//! `sandbox::probe::probe_unprivileged_userns()`. When either
//! probe fails on the host, tools log a warning and fall back to
//! `Off`; the session continues rather than refusing to run. That
//! matches the rest of the v2.0.0 retrieval-plane degradation
//! model ("best-effort with tracing::warn on missing deps").

/// Three-way toggle that tools consult when deciding whether to
/// route their out-of-process work through the jail sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxPolicy {
    #[default]
    Off,
    TierA,
    TierB,
}

impl SandboxPolicy {
    /// Read `AZOTH_SANDBOX`.
    ///
    /// - `off` → `Off` (explicit opt-out).
    /// - `tier_a` / `a` / `A` → `TierA`.
    /// - `tier_b` / `b` / `B` → `TierB`.
    /// - unset / empty → v2.1-H default: `TierA` if the host supports
    ///   unprivileged `CLONE_NEWUSER`, else `Off` with a
    ///   `tracing::warn`. This degradation matches how bash.rs
    ///   already falls back when `spawn_jailed` can't construct a
    ///   user-namespace — "best-effort with tracing::warn on
    ///   missing deps".
    /// - any other value → `Off` with a warning (unknown setting is
    ///   safer to treat as "operator-intended opt-out" than to
    ///   silently route through an unexpected tier).
    pub fn from_env() -> Self {
        match std::env::var("AZOTH_SANDBOX").as_deref() {
            Ok("off") => SandboxPolicy::Off,
            Ok("tier_a" | "a" | "A") => SandboxPolicy::TierA,
            Ok("tier_b" | "b" | "B") => SandboxPolicy::TierB,
            Ok("") | Err(_) => {
                // `probe_unprivileged_userns_cached` forks at most
                // once per process, so this call is fork-free after
                // the first hit (ideally pre-warmed from main()).
                // The tracing::warn only fires on the first-call
                // degradation — after that `get_or_init` returns the
                // cached false without re-logging, which keeps the
                // log clean even for bots / CLI that call `from_env`
                // per tool invocation.
                if crate::sandbox::probe_unprivileged_userns_cached() {
                    SandboxPolicy::TierA
                } else {
                    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                    WARNED.get_or_init(|| {
                        tracing::warn!(
                            "unprivileged user-ns unavailable; sandbox default degrades to Off"
                        );
                    });
                    SandboxPolicy::Off
                }
            }
            Ok(other) => {
                tracing::warn!(
                    value = other,
                    "AZOTH_SANDBOX has unknown value; degrading to Off"
                );
                SandboxPolicy::Off
            }
        }
    }

    pub fn is_off(self) -> bool {
        matches!(self, SandboxPolicy::Off)
    }

    pub fn is_tier_b(self) -> bool {
        matches!(self, SandboxPolicy::TierB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-var parsing is the whole testable surface of this
    /// module. The mutations + restore are manual rather than
    /// a lock because `cargo test` runs with --test-threads=1
    /// project-wide (see `pattern_backpressure_smoke_flaky` memo).
    /// Running multi-threaded would still be safe thanks to the
    /// per-test cleanup, but we match the repo-wide convention.
    #[test]
    fn from_env_defaults_to_tier_a_when_userns_available() {
        // v2.1-H: default flipped Off → TierA (with graceful Off
        // fallback on hosts that can't `unshare(CLONE_NEWUSER)`).
        // We can't force-disable user-ns for the "else" branch in a
        // unit test, so the assertion is probe-conditional.
        std::env::remove_var("AZOTH_SANDBOX");
        let got = SandboxPolicy::from_env();
        let want = if crate::sandbox::probe_unprivileged_userns() {
            SandboxPolicy::TierA
        } else {
            SandboxPolicy::Off
        };
        assert_eq!(got, want);
        std::env::set_var("AZOTH_SANDBOX", "");
        let got = SandboxPolicy::from_env();
        assert_eq!(got, want, "empty env should behave the same as unset");
        std::env::remove_var("AZOTH_SANDBOX");
    }

    #[test]
    fn from_env_parses_explicit_off_and_unknown_as_off() {
        std::env::set_var("AZOTH_SANDBOX", "off");
        assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::Off);
        std::env::set_var("AZOTH_SANDBOX", "garbage");
        assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::Off);
        std::env::remove_var("AZOTH_SANDBOX");
    }

    #[test]
    fn from_env_parses_tier_a_and_tier_b() {
        std::env::set_var("AZOTH_SANDBOX", "tier_a");
        assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::TierA);
        std::env::set_var("AZOTH_SANDBOX", "tier_b");
        assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::TierB);
        assert!(SandboxPolicy::from_env().is_tier_b());
        std::env::remove_var("AZOTH_SANDBOX");
    }
}
