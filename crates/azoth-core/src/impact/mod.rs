//! TDAD — test impact selection.
//!
//! Sprint 5 introduces the sibling subsystem that sits next to
//! `validators` in the `TurnDriver` pipeline. A `DiffSource` emits a
//! `Diff` describing what the turn changed; an `ImpactSelector`
//! consumes that `Diff` plus the run's `Contract` and produces a
//! `TestPlan` — the ordered list of tests the runtime would schedule
//! under a real `TestRunner` (runner impl is deferred to v2.1).
//!
//! Why a new trait family rather than extending `Validator`:
//! - `Validator::check(&Contract) -> ValidatorReport` is synchronous
//!   and pure-contract. Impact selection needs `async` to shell out
//!   to `cargo test --list`, query the symbol index, and hit the
//!   co-edit graph — all I/O.
//! - `Validator` returns a single pass/fail; an `ImpactValidator`
//!   carries a whole `TestPlan` payload alongside the verdict so the
//!   driver can persist it to the `test_impact` SQLite mirror.
//! - Separation keeps the existing `validators` slot in `TurnDriver`
//!   byte-for-byte compatible with v1.5 replays.
//!
//! The concrete `CargoTestImpact` lives in `azoth-repo` so
//! `azoth-core` stays free of tree-sitter, sqlite, and git deps.

pub use crate::schemas::{Diff, TestId, TestPlan};

use crate::schemas::Contract;
use async_trait::async_trait;
use thiserror::Error;

/// Errors an `ImpactSelector` can surface to the caller. Parse and
/// I/O failures are boxed so selectors can forward tool-specific
/// errors (e.g. a `cargo test --list --format json` parse failure)
/// without every error type being baked into azoth-core.
#[derive(Debug, Error)]
pub enum ImpactError {
    #[error("cargo metadata failed: {0}")]
    CargoMetadata(String),
    #[error("test discovery failed: {0}")]
    TestDiscovery(String),
    #[error("diff source failed: {0}")]
    DiffSource(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

/// Emits a `Diff` describing what the current turn changed. The
/// `TurnDriver` queries this at the validate phase so impact
/// selection has a concrete input. Defaults to `NullDiffSource`
/// (empty diff → impact validators no-op), preserving byte-for-byte
/// v1.5 behaviour when no diff source is wired.
///
/// The trait is async because production impls shell out to `git
/// status --porcelain` or walk a fuse-overlayfs merged view. The
/// default `NullDiffSource` returns immediately.
#[async_trait]
pub trait DiffSource: Send + Sync {
    fn name(&self) -> &'static str;
    async fn diff(&self) -> Result<Diff, ImpactError>;
}

/// Always returns an empty diff. The `TurnDriver` uses this when no
/// diff source is attached, so impact validators observe zero
/// changed files and should emit an empty `TestPlan`.
pub struct NullDiffSource;

#[async_trait]
impl DiffSource for NullDiffSource {
    fn name(&self) -> &'static str {
        "null"
    }

    async fn diff(&self) -> Result<Diff, ImpactError> {
        Ok(Diff::empty())
    }
}

/// Selects the minimal relevant set of tests for a given `Diff` under
/// a given `Contract`. Returns an ordered `TestPlan` — test order is
/// selector-defined (direct matches first, adjacency-derived last)
/// so forensic diffs across turns stay stable.
#[async_trait]
pub trait ImpactSelector: Send + Sync {
    /// Human-readable selector name. Written to JSONL under
    /// `SessionEvent::ImpactComputed.selector`.
    fn name(&self) -> &'static str;

    /// Opaque impl version — bump when the heuristic changes so
    /// replay can detect plan drift without re-running the selector.
    fn version(&self) -> u32;

    async fn select(&self, diff: &Diff, contract: &Contract) -> Result<TestPlan, ImpactError>;
}

/// Zero-behaviour selector that always returns an empty `TestPlan`.
/// Used as the default when no real selector is wired, so a
/// `TurnDriver` configured with `impact_validators` but no selector
/// still ticks through the validate phase cleanly.
pub struct NullImpactSelector;

#[async_trait]
impl ImpactSelector for NullImpactSelector {
    fn name(&self) -> &'static str {
        "null"
    }

    fn version(&self) -> u32 {
        0
    }

    async fn select(&self, _diff: &Diff, _contract: &Contract) -> Result<TestPlan, ImpactError> {
        Ok(TestPlan::empty(self.version()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::{Contract, ContractId, EffectBudget, Scope};

    fn stub_contract() -> Contract {
        Contract {
            id: ContractId::new(),
            goal: "test".into(),
            non_goals: Vec::new(),
            success_criteria: Vec::new(),
            scope: Scope::default(),
            effect_budget: EffectBudget::default(),
            notes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn null_diff_source_emits_empty() {
        let src = NullDiffSource;
        let d = src.diff().await.unwrap();
        assert!(d.is_empty());
    }

    #[tokio::test]
    async fn null_selector_emits_empty_plan_with_version_zero() {
        let sel = NullImpactSelector;
        let plan = sel.select(&Diff::empty(), &stub_contract()).await.unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.selector_version, 0);
        assert!(plan.is_well_formed());
    }
}
