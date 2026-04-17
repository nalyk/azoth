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

/// Retrieval subtree of the (future) central config. Kept small so the
/// lexical_backend knob can flow through the runtime without invoking
/// a broader config migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    #[serde(default)]
    pub lexical_backend: LexicalBackend,
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
        Self { lexical_backend }
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
