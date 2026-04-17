//! `ImpactValidator` — the turn-exit validator shape that owns a
//! `TestPlan` payload alongside its pass/fail verdict.
//!
//! Kept separate from `Validator`:
//! - `Validator::check(&Contract)` is synchronous and carries no
//!   payload. `ImpactValidator::validate(&Contract, &Diff)` is async
//!   and returns a plan.
//! - The TurnDriver persists an `ImpactComputed` event **per**
//!   `ImpactValidator` call, plus a `ValidatorResult` for the
//!   pass/fail summary.
//! - `SelectorBackedImpactValidator` is the concrete v2 wrapper
//!   around any `ImpactSelector`. v2 ships plan-only (no real
//!   `TestRunner`); a selector-derived plan with at least one
//!   selected test is treated as `Pass`, an empty plan as `Pass`
//!   (nothing to run ≠ failure), and selector errors as `Fail`.
//! - `run_tests` stays `false` in v2. v2.1 plugs in a `TestRunner`
//!   and flips it to `true` without reshaping this trait.

use crate::impact::{ImpactError, ImpactSelector};
use crate::schemas::{Contract, Diff, TestPlan, ValidatorStatus};
use async_trait::async_trait;
use std::sync::Arc;

/// Outcome of an `ImpactValidator::validate` call. Mirrors
/// `ValidatorReport` but carries the `TestPlan` payload and a
/// selector-stable name (`&'static str` — every concrete selector
/// has a fixed name).
pub struct ImpactValidatorReport {
    pub name: &'static str,
    pub status: ValidatorStatus,
    pub detail: Option<String>,
    /// The plan the validator produced, if any. `None` when the
    /// validator failed before producing a plan (e.g. selector
    /// errored). `Some(plan)` even when `plan.is_empty()` — an empty
    /// plan is a valid v2 outcome (nothing impacted).
    pub plan: Option<TestPlan>,
}

/// Turn-exit validator that owns an `ImpactSelector` + `TestPlan`
/// payload. Implementors are typically wrappers around a concrete
/// selector.
#[async_trait]
pub trait ImpactValidator: Send + Sync {
    /// Stable name. Written to JSONL as the
    /// `SessionEvent::ValidatorResult.validator` string so replay
    /// can correlate plan + verdict.
    fn name(&self) -> &'static str;

    async fn validate(&self, contract: &Contract, diff: &Diff) -> ImpactValidatorReport;
}

/// Generic wrapper that turns any `ImpactSelector` into an
/// `ImpactValidator`. v2 runs plan-only: success = the selector
/// returned `Ok(plan)` (empty or not); failure = the selector
/// errored. v2.1 layers a real `TestRunner` behind this wrapper
/// without touching the trait.
///
/// `name_static` is taken at construction time because the `name()`
/// trait method returns `&'static str`; callers pass in a string
/// literal (or a `Box::leak`ed owned string) at wire time.
pub struct SelectorBackedImpactValidator {
    name_static: &'static str,
    selector: Arc<dyn ImpactSelector>,
    /// Reserved for v2.1 when a `TestRunner` trait lands. Kept as a
    /// field so the v2 wire shape stays identical under the switch.
    run_tests: bool,
}

impl SelectorBackedImpactValidator {
    pub fn new(name_static: &'static str, selector: Arc<dyn ImpactSelector>) -> Self {
        Self {
            name_static,
            selector,
            run_tests: false,
        }
    }

    /// Exposes the underlying selector's impl version so callers can
    /// annotate `SessionEvent::ImpactComputed.selector_version`
    /// without downcasting.
    pub fn selector_version(&self) -> u32 {
        self.selector.version()
    }

    /// Exposes the underlying selector's wire name (used by the
    /// TurnDriver when writing `ImpactComputed.selector`).
    pub fn selector_name(&self) -> &'static str {
        self.selector.name()
    }

    /// Whether this wrapper would actually execute the plan under a
    /// real `TestRunner`. Stays `false` through v2; flips to `true`
    /// in v2.1.
    pub fn runs_tests(&self) -> bool {
        self.run_tests
    }
}

#[async_trait]
impl ImpactValidator for SelectorBackedImpactValidator {
    fn name(&self) -> &'static str {
        self.name_static
    }

    async fn validate(&self, contract: &Contract, diff: &Diff) -> ImpactValidatorReport {
        match self.selector.select(diff, contract).await {
            Ok(plan) => {
                debug_assert!(
                    plan.is_well_formed(),
                    "selector `{}` returned malformed TestPlan (rationale/confidence misalignment)",
                    self.selector.name()
                );
                let detail = if plan.is_empty() {
                    Some("no impacted tests".into())
                } else {
                    Some(format!("{} test(s) selected", plan.len()))
                };
                ImpactValidatorReport {
                    name: self.name_static,
                    status: ValidatorStatus::Pass,
                    detail,
                    plan: Some(plan),
                }
            }
            Err(ImpactError::Backend(e)) => ImpactValidatorReport {
                name: self.name_static,
                status: ValidatorStatus::Fail,
                detail: Some(format!("selector backend error: {e}")),
                plan: None,
            },
            Err(e) => ImpactValidatorReport {
                name: self.name_static,
                status: ValidatorStatus::Fail,
                detail: Some(format!("selector error: {e}")),
                plan: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::impact::{ImpactSelector, NullImpactSelector};
    use crate::schemas::{ContractId, EffectBudget, Scope, TestId, TestPlan};

    fn stub_contract() -> Contract {
        Contract {
            id: ContractId::new(),
            goal: "tdad".into(),
            non_goals: Vec::new(),
            success_criteria: Vec::new(),
            scope: Scope::default(),
            effect_budget: EffectBudget::default(),
            notes: Vec::new(),
        }
    }

    struct FixedPlanSelector(TestPlan);

    #[async_trait]
    impl ImpactSelector for FixedPlanSelector {
        fn name(&self) -> &'static str {
            "fixed"
        }
        fn version(&self) -> u32 {
            self.0.selector_version
        }
        async fn select(
            &self,
            _diff: &Diff,
            _contract: &Contract,
        ) -> Result<TestPlan, ImpactError> {
            Ok(self.0.clone())
        }
    }

    struct ErroringSelector;

    #[async_trait]
    impl ImpactSelector for ErroringSelector {
        fn name(&self) -> &'static str {
            "erroring"
        }
        fn version(&self) -> u32 {
            1
        }
        async fn select(
            &self,
            _diff: &Diff,
            _contract: &Contract,
        ) -> Result<TestPlan, ImpactError> {
            Err(ImpactError::TestDiscovery("synthetic failure".into()))
        }
    }

    #[tokio::test]
    async fn empty_plan_is_still_pass() {
        let v = SelectorBackedImpactValidator::new("impact:null", Arc::new(NullImpactSelector));
        let r = v.validate(&stub_contract(), &Diff::empty()).await;
        assert_eq!(r.status, ValidatorStatus::Pass);
        assert!(r.plan.unwrap().is_empty());
        assert_eq!(r.name, "impact:null");
    }

    #[tokio::test]
    async fn populated_plan_is_pass_and_detail_counts() {
        let plan = TestPlan {
            tests: vec![TestId::new("crate::foo::tests::bar")],
            rationale: vec!["direct match".into()],
            confidence: vec![1.0],
            selector_version: 7,
        };
        let v =
            SelectorBackedImpactValidator::new("impact:fixed", Arc::new(FixedPlanSelector(plan)));
        let r = v
            .validate(&stub_contract(), &Diff::from_paths(["src/foo.rs"]))
            .await;
        assert_eq!(r.status, ValidatorStatus::Pass);
        assert_eq!(r.detail.as_deref(), Some("1 test(s) selected"));
        assert_eq!(r.plan.unwrap().selector_version, 7);
    }

    #[tokio::test]
    async fn selector_error_becomes_fail_and_drops_plan() {
        let v = SelectorBackedImpactValidator::new("impact:err", Arc::new(ErroringSelector));
        let r = v.validate(&stub_contract(), &Diff::empty()).await;
        assert_eq!(r.status, ValidatorStatus::Fail);
        assert!(r.plan.is_none());
        assert!(r.detail.unwrap().contains("synthetic failure"));
    }
}
