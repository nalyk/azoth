//! Retrieval configuration — Sprint 1 knob for `lexical_backend`.
//!
//! The `AzothConfig` object prescribed by `docs/v2_plan.md` §Sprint 1
//! does not yet exist as a single struct in the codebase. Rather than
//! invent one too early, this module ships the minimal shape the plan
//! needs — an enum and a struct — with an env-var resolver so the bin
//! crate can honour `AZOTH_LEXICAL_BACKEND` today. When the config
//! system gets folded into a central struct (a later sprint), this
//! nests under it as `retrieval.lexical_backend`.
//!
//! Sprint 1 default: `ripgrep` — no behavior change on upgrade. Sprint
//! 5 eval flips to `both`; Sprint 7 flips to `fts` as the ship default.

use serde::{Deserialize, Serialize};

/// Which lexical retrieval engine the runtime should query for evidence.
/// `Both` runs the two in parallel (Sprint 5 eval uses this).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalBackend {
    /// In-process ripgrep via `grep-searcher` + `ignore::WalkBuilder`.
    /// Ships as the Sprint 1 default so no behavior changes by upgrading.
    #[default]
    Ripgrep,
    /// SQLite FTS5 over `documents` content table (Sprint 1 ships the
    /// backend; Sprint 7 flips the default).
    Fts,
    /// Run both and union — used by the Sprint 5 retrieval parity eval.
    Both,
}

impl LexicalBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            LexicalBackend::Ripgrep => "ripgrep",
            LexicalBackend::Fts => "fts",
            LexicalBackend::Both => "both",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ripgrep" => Some(LexicalBackend::Ripgrep),
            "fts" => Some(LexicalBackend::Fts),
            "both" => Some(LexicalBackend::Both),
            _ => None,
        }
    }
}

/// Sprint 3 — co-edit graph knobs. The graph is a read-only overlay on
/// `git log`; no knob actively changes behaviour at query time, so the
/// two tunables here both shape the *build* pass (migration m0004 +
/// `history::co_edit::build`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoEditConfig {
    /// How many recent commits to walk when building the graph. Plan
    /// §Sprint 3 fixes this at 500. Larger windows produce denser
    /// graphs but mask recent locality; smaller windows lose signal
    /// on older files that are still load-bearing.
    #[serde(default = "default_co_edit_window")]
    pub window: u32,
    /// Commits that touched more than this many files are skipped,
    /// mitigating the "squash-merge degeneracy" called out in the
    /// v2 plan risk ledger #3: a 100-file squash would add
    /// C(100, 2) = 4950 dense edges of nearly-equal weight, all
    /// signal-free. Default 50. `0` means "never skip".
    #[serde(default = "default_skip_large_commits")]
    pub skip_large_commits: u32,
}

fn default_co_edit_window() -> u32 {
    500
}

fn default_skip_large_commits() -> u32 {
    50
}

impl Default for CoEditConfig {
    fn default() -> Self {
        Self {
            window: default_co_edit_window(),
            skip_large_commits: default_skip_large_commits(),
        }
    }
}

/// Retrieval subtree of the (future) central config. Kept small so the
/// lexical_backend knob can flow through the runtime without invoking
/// a broader config migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    #[serde(default)]
    pub lexical_backend: LexicalBackend,
    #[serde(default)]
    pub co_edit: CoEditConfig,
}

impl RetrievalConfig {
    /// Resolve from environment. Unknown values fall back to the
    /// default with a `tracing::warn!` so misconfiguration is visible
    /// but not fatal.
    pub fn from_env() -> Self {
        let lexical_backend = match std::env::var("AZOTH_LEXICAL_BACKEND") {
            Ok(raw) => LexicalBackend::parse(raw.trim()).unwrap_or_else(|| {
                tracing::warn!(
                    value = %raw,
                    "unknown AZOTH_LEXICAL_BACKEND; falling back to default (ripgrep)"
                );
                LexicalBackend::default()
            }),
            Err(_) => LexicalBackend::default(),
        };

        let co_edit_defaults = CoEditConfig::default();
        let window = parse_u32_env("AZOTH_CO_EDIT_WINDOW", co_edit_defaults.window);
        let skip_large_commits = parse_u32_env(
            "AZOTH_CO_EDIT_SKIP_LARGE_COMMITS",
            co_edit_defaults.skip_large_commits,
        );

        Self {
            lexical_backend,
            co_edit: CoEditConfig {
                window,
                skip_large_commits,
            },
        }
    }
}

fn parse_u32_env(var: &str, fallback: u32) -> u32 {
    match std::env::var(var) {
        Ok(raw) => raw.trim().parse::<u32>().unwrap_or_else(|_| {
            tracing::warn!(
                value = %raw,
                var,
                fallback,
                "unparseable u32 env var; using fallback"
            );
            fallback
        }),
        Err(_) => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ripgrep_so_upgrade_is_a_no_op() {
        let cfg = RetrievalConfig::default();
        assert_eq!(cfg.lexical_backend, LexicalBackend::Ripgrep);
    }

    #[test]
    fn parse_round_trips() {
        for b in [
            LexicalBackend::Ripgrep,
            LexicalBackend::Fts,
            LexicalBackend::Both,
        ] {
            assert_eq!(LexicalBackend::parse(b.as_str()), Some(b));
        }
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(LexicalBackend::parse("nope"), None);
        assert_eq!(LexicalBackend::parse(""), None);
    }

    #[test]
    fn serde_uses_snake_case() {
        let json = serde_json::to_string(&LexicalBackend::Fts).unwrap();
        assert_eq!(json, "\"fts\"");
        let parsed: LexicalBackend = serde_json::from_str("\"both\"").unwrap();
        assert_eq!(parsed, LexicalBackend::Both);
    }
}
