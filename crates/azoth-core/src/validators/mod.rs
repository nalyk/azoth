//! Deterministic turn-exit validators.

pub mod impact;

pub use impact::{ImpactValidator, ImpactValidatorReport, SelectorBackedImpactValidator};

use crate::schemas::{Contract, ValidatorStatus};

pub struct ValidatorReport {
    pub name: &'static str,
    pub status: ValidatorStatus,
    pub detail: Option<String>,
}

pub trait Validator: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, contract: &Contract) -> ValidatorReport;
}

/// v1: a single trivial validator that passes iff the contract has a
/// non-empty goal. Real validators (impact tests, lints, compile) slot in
/// here without signature change.
pub struct ContractGoalValidator;

impl Validator for ContractGoalValidator {
    fn name(&self) -> &'static str {
        "contract_goal_nonempty"
    }
    fn check(&self, contract: &Contract) -> ValidatorReport {
        let (status, detail) = if contract.goal.trim().is_empty() {
            (ValidatorStatus::Fail, Some("goal is empty".into()))
        } else {
            (ValidatorStatus::Pass, None)
        };
        ValidatorReport {
            name: self.name(),
            status,
            detail,
        }
    }
}
