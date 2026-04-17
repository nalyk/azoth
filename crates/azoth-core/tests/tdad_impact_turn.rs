//! Sprint 5 — TurnDriver wiring for `impact_validators` + `diff_source`.
//!
//! Two scenarios:
//!   1. Happy path — an impact validator returns a populated
//!      `TestPlan`. The driver emits exactly one `ImpactComputed`
//!      *before* the paired `ValidatorResult`, then `Checkpoint`
//!      and `TurnCommitted` (in that order), with no `TurnAborted`.
//!   2. Failure path — an impact validator returns `Fail`. The
//!      driver emits the `ValidatorResult { Fail }` and aborts under
//!      `AbortReason::ValidatorFail` with no `Checkpoint` or
//!      `TurnCommitted`.
//!
//! Uses inline mock implementations of `DiffSource` and
//! `ImpactValidator` so the test does not depend on `azoth-repo`.

use async_trait::async_trait;
use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::impact::{Diff, DiffSource, ImpactError};
use azoth_core::schemas::{
    AbortReason, ContentBlock, Contract, Message, ModelTurnResponse, RunId, SessionEvent,
    StopReason, TestId, TestPlan, TurnId, Usage, ValidatorStatus,
};
use azoth_core::turn::TurnDriver;
use azoth_core::validators::{ImpactValidator, ImpactValidatorReport};
use tempfile::tempdir;
use tokio::sync::mpsc;

struct StaticDiffSource {
    diff: Diff,
}

#[async_trait]
impl DiffSource for StaticDiffSource {
    fn name(&self) -> &'static str {
        "static"
    }
    async fn diff(&self) -> Result<Diff, ImpactError> {
        Ok(self.diff.clone())
    }
}

struct FixedPlanValidator {
    name_static: &'static str,
    plan: TestPlan,
}

#[async_trait]
impl ImpactValidator for FixedPlanValidator {
    fn name(&self) -> &'static str {
        self.name_static
    }
    async fn validate(&self, _contract: &Contract, _diff: &Diff) -> ImpactValidatorReport {
        ImpactValidatorReport {
            name: self.name_static,
            status: ValidatorStatus::Pass,
            detail: Some(format!("{} test(s)", self.plan.len())),
            plan: Some(self.plan.clone()),
        }
    }
}

struct FailingImpactValidator;

#[async_trait]
impl ImpactValidator for FailingImpactValidator {
    fn name(&self) -> &'static str {
        "impact:always_fail"
    }
    async fn validate(&self, _contract: &Contract, _diff: &Diff) -> ImpactValidatorReport {
        ImpactValidatorReport {
            name: "impact:always_fail",
            status: ValidatorStatus::Fail,
            detail: Some("deliberate impact fail".into()),
            plan: None,
        }
    }
}

fn mock_end_turn() -> MockScript {
    MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![ContentBlock::Text { text: "ok".into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                ..Default::default()
            },
        }],
    }
}

fn persist_contract(seed_path: &std::path::Path, goal: &str) -> Contract {
    let mut seed = JsonlWriter::open(seed_path).unwrap();
    let mut drafted = contract::draft(goal);
    drafted.success_criteria.push("tdad exercised".into());
    let persisted =
        contract::accept_and_persist(&mut seed, drafted, "2026-04-17T00:00:00Z".to_string())
            .expect("persist contract");
    drop(seed);
    persisted
}

async fn drive_with_impact(
    session_path: &std::path::Path,
    artifacts_root: &std::path::Path,
    repo_root: &std::path::Path,
    contract_ref: &Contract,
    impact_validators: &[&dyn ImpactValidator],
    diff_source: Option<&dyn DiffSource>,
) -> Vec<SessionEvent> {
    let mut writer = JsonlWriter::open(session_path).unwrap();
    let artifacts = ArtifactStore::open(artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        mock_end_turn(),
    );
    let run_id = RunId::from("run_impact".to_string());
    let turn_id = TurnId::from("t_impact_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.to_path_buf(),
    )
    .build();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut caps = CapabilityStore::new();
    let mut effects = azoth_core::schemas::EffectCounter::default();

    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(contract_ref),
            turns_completed: 0,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators,
            diff_source,
        };
        driver
            .drive_turn(
                turn_id,
                "you are azoth".into(),
                vec![Message::user_text("go")],
            )
            .await
            .expect("drive_turn resolves");
    }
    drop(writer);

    JsonlReader::open(session_path)
        .forensic()
        .expect("forensic read ok")
        .iter()
        .map(|e| e.event.clone())
        .collect()
}

#[tokio::test]
async fn happy_path_emits_impact_computed_before_validator_result_and_commits() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let persisted = persist_contract(&seed_path, "tdad happy");

    let plan = TestPlan {
        tests: vec![
            TestId::new("my_crate::foo::tests::a"),
            TestId::new("my_crate::bar::tests::b"),
        ],
        rationale: vec!["direct".into(), "co-edit".into()],
        confidence: vec![1.0, 0.6],
        selector_version: 42,
    };
    let iv = FixedPlanValidator {
        name_static: "impact:fixed",
        plan,
    };
    let diff_src = StaticDiffSource {
        diff: Diff::from_paths(["src/foo.rs"]),
    };

    let session_path = repo_root.join(".azoth/sessions/happy.jsonl");
    let events = drive_with_impact(
        &session_path,
        &artifacts_root,
        &repo_root,
        &persisted,
        &[&iv as &dyn ImpactValidator],
        Some(&diff_src as &dyn DiffSource),
    )
    .await;

    // Locate the events we care about and verify ORDER:
    // ImpactComputed → ValidatorResult(Pass) → Checkpoint → TurnCommitted.
    let positions: Vec<(usize, &'static str)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, ev)| match ev {
            SessionEvent::ImpactComputed { .. } => Some((i, "impact")),
            SessionEvent::ValidatorResult {
                validator, status, ..
            } if validator == "impact:fixed" && matches!(status, ValidatorStatus::Pass) => {
                Some((i, "validator"))
            }
            SessionEvent::Checkpoint { .. } => Some((i, "checkpoint")),
            SessionEvent::TurnCommitted { .. } => Some((i, "committed")),
            _ => None,
        })
        .collect();

    assert_eq!(
        positions.iter().map(|(_, k)| *k).collect::<Vec<_>>(),
        vec!["impact", "validator", "checkpoint", "committed"],
        "happy-path ordering wrong: {:?}",
        positions
    );

    // The ImpactComputed must carry the selector_version + changed_files
    // + selected_tests as the validator produced them.
    let impact = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ImpactComputed {
                selector,
                selector_version,
                changed_files,
                selected_tests,
                ..
            } => Some((
                selector.clone(),
                *selector_version,
                changed_files.clone(),
                selected_tests.clone(),
            )),
            _ => None,
        })
        .expect("ImpactComputed present");
    assert_eq!(impact.0, "impact:fixed");
    assert_eq!(impact.1, 42);
    assert_eq!(impact.2, vec!["src/foo.rs"]);
    assert_eq!(
        impact.3,
        vec!["my_crate::foo::tests::a", "my_crate::bar::tests::b"]
    );

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnAborted { .. })),
        "happy path must not abort"
    );
}

#[tokio::test]
async fn failing_impact_validator_aborts_turn_with_validator_fail_reason() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let persisted = persist_contract(&seed_path, "tdad fail");

    let iv = FailingImpactValidator;

    let session_path = repo_root.join(".azoth/sessions/fail.jsonl");
    let events = drive_with_impact(
        &session_path,
        &artifacts_root,
        &repo_root,
        &persisted,
        &[&iv as &dyn ImpactValidator],
        None, // empty diff
    )
    .await;

    // A failing impact validator reports no plan — so there should
    // be NO ImpactComputed, exactly one Fail ValidatorResult, and
    // exactly one TurnAborted(ValidatorFail).
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ImpactComputed { .. })),
        "no plan → no ImpactComputed event"
    );
    let fails = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionEvent::ValidatorResult {
                    status: ValidatorStatus::Fail,
                    ..
                }
            )
        })
        .count();
    assert_eq!(fails, 1);

    let aborted = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::TurnAborted { reason, detail, .. } => Some((*reason, detail.clone())),
            _ => None,
        })
        .expect("TurnAborted present");
    assert_eq!(aborted.0, AbortReason::ValidatorFail);
    let detail = aborted.1.expect("detail populated");
    assert!(detail.contains("impact:always_fail"));
    assert!(detail.contains("deliberate impact fail"));

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Checkpoint { .. })),
        "failure path must not write Checkpoint"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnCommitted { .. })),
        "failure path must not write TurnCommitted"
    );
}

#[tokio::test]
async fn empty_impact_validators_is_byte_compat_with_pre_sprint_5() {
    // Configure a contract but leave impact_validators empty and
    // diff_source unset. The driver must emit no ImpactComputed, no
    // "diff_source:*" ValidatorResult, and the Checkpoint +
    // TurnCommitted pair must be unchanged from the pre-Sprint-5
    // wire shape.
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let persisted = persist_contract(&seed_path, "byte-compat");

    let session_path = repo_root.join(".azoth/sessions/empty.jsonl");
    let events = drive_with_impact(
        &session_path,
        &artifacts_root,
        &repo_root,
        &persisted,
        &[],
        None,
    )
    .await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ImpactComputed { .. })),
        "empty slice must not emit ImpactComputed"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            SessionEvent::ValidatorResult { validator, .. } if validator.starts_with("diff_source:")
        )),
        "empty slice must not query diff_source or emit its ValidatorResult"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Checkpoint { .. })),
        "contract-scoped commit emits Checkpoint"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnCommitted { .. })),
        "contract-scoped commit emits TurnCommitted"
    );
}
