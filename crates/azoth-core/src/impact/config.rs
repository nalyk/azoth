//! `ImpactConfig` — Sprint 5 knob controlling whether the TurnDriver
//! wires up the TDAD impact-selection pipeline.
//!
//! Default `enabled = false` because v2 is plan-only: the subsystem
//! is mechanistically in place (selector, validator, diff source,
//! mirror projection) but flipping it on at every launch would burn
//! `cargo test --no-run` wall-clock on users who don't want the
//! feature. Sprint 7's ship gate will flip the default to `true`
//! once the cost/value tradeoff has been measured on the eval plane.
//!
//! The env-var shape mirrors the existing `RetrievalConfig::from_env`
//! pattern: a single `AZOTH_IMPACT_ENABLED=true|false` flag, with
//! unknown values falling back to the default with a
//! `tracing::warn!` so misconfiguration is visible but not fatal.

use serde::{Deserialize, Serialize};

/// Sprint 5 impact-selection knob. Lives as a standalone struct for
/// now; folds into the future central `AzothConfig` alongside
/// `RetrievalConfig` without reshaping its field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactConfig {
    /// When `true`, the TUI worker constructs a concrete
    /// `ImpactSelector` + `DiffSource` pair and wires them into the
    /// `TurnDriver::impact_validators` / `diff_source` slots.
    /// When `false` (default), both slots stay empty and no
    /// `ImpactComputed` events emit — byte-for-byte identical to the
    /// pre-Sprint-5 TurnDriver wire shape.
    #[serde(default)]
    pub enabled: bool,
}

impl ImpactConfig {
    /// Resolve from environment. Unknown values fall back to the
    /// default (`enabled = false`) with a visible warning.
    pub fn from_env() -> Self {
        let enabled = match std::env::var("AZOTH_IMPACT_ENABLED") {
            Ok(raw) => parse_bool(raw.trim()).unwrap_or_else(|| {
                tracing::warn!(
                    value = %raw,
                    "unknown AZOTH_IMPACT_ENABLED; falling back to default (false)"
                );
                false
            }),
            Err(_) => false,
        };
        Self { enabled }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    // Explicit allow-list. No surprise positives like "yes" or "1".
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" => Some(true),
        "false" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_so_upgrade_is_a_no_op() {
        let cfg = ImpactConfig::default();
        assert!(!cfg.enabled);
    }

    #[test]
    fn parse_bool_accepts_explicit_values_only() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("ON"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        // No surprise positives.
        assert_eq!(parse_bool("1"), None);
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn v1_5_config_deserialises_without_enabled_field() {
        // Additive schema guard — pre-Sprint-5 serialised config has
        // no `enabled` field; must parse via `#[serde(default)]`.
        let cfg: ImpactConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!cfg.enabled);
    }
}
