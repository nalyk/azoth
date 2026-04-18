//! Runtime sandbox policy — what tools should actually enforce
//! when the dispatcher hands them an `ApplyLocal` / `ApplyRepo`
//! effect class. Driven by the `AZOTH_SANDBOX` env var so the
//! default (OFF) preserves the 349-green-tests invariant v2.0.0
//! shipped with; operators explicitly opt in to subprocess
//! isolation.
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
//! `Off` is the default (empty / missing / `off` env value). Tools
//! run in-process, same as v2.0.0.
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
    /// Read `AZOTH_SANDBOX`. Unknown values and parse errors both
    /// degrade to `Off` with a warning — the v2.0.0 default wins
    /// when in doubt. Operators who care will see the warning in
    /// the TUI log and fix their env var.
    pub fn from_env() -> Self {
        match std::env::var("AZOTH_SANDBOX").as_deref() {
            Ok("" | "off") | Err(_) => SandboxPolicy::Off,
            Ok("tier_a" | "a" | "A") => SandboxPolicy::TierA,
            Ok("tier_b" | "b" | "B") => SandboxPolicy::TierB,
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
    fn from_env_defaults_to_off_when_unset() {
        std::env::remove_var("AZOTH_SANDBOX");
        assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::Off);
        assert!(SandboxPolicy::from_env().is_off());
    }

    #[test]
    fn from_env_parses_off_empty_and_unknown_as_off() {
        std::env::set_var("AZOTH_SANDBOX", "off");
        assert_eq!(SandboxPolicy::from_env(), SandboxPolicy::Off);
        std::env::set_var("AZOTH_SANDBOX", "");
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
