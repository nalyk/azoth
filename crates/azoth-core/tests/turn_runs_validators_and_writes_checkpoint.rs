//! Validator wiring — on the natural `EndTurn` exit path, a driver configured
//! with a non-empty validator slice and a persisted contract must:
//!   1. Emit one `ValidatorResult` per validator, in order.
//!   2. On all-pass: append a fresh `Checkpoint` event, THEN `TurnCommitted`.
//!   3. On any-fail: append `TurnAborted { reason: ValidatorFail }` and write
//!      NO `Checkpoint`, NO `TurnCommitted`.
//!
//! Two tests: the happy path uses the built-in `ContractGoalValidator` (passes
//! iff contract.goal is non-empty). The failure path uses an inline
//! always-fail validator to avoid contaminating the happy-path fixture.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ContentBlock, Contract, Message, ModelTurnResponse, RunId, SessionEvent,
    StopReason, TurnId, Usage, ValidatorStatus,
};
use azoth_core::turn::TurnDriver;
use azoth_core::validators::{ContractGoalValidator, Validator, ValidatorReport};
use tempfile::tempdir;
use tokio::sync::mpsc;

fn mock_end_turn_only() -> MockScript {
    MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 4,
                output_tokens: 2,
                ..Default::default()
            },
        }],
    }
}

/// Always-fail validator for the negative path. Emits a deterministic detail
/// so we can assert the text round-trips through the aborted detail.
struct AlwaysFailValidator;

impl Validator for AlwaysFailValidator {
    fn name(&self) -> &'static str {
        "always_fail"
    }
    fn check(&self, _contract: &Contract) -> ValidatorReport {
        ValidatorReport {
            name: self.name(),
            status: ValidatorStatus::Fail,
            detail: Some("deliberate".into()),
        }
    }
}

async fn drive_with_validators(
    session_path: &std::path::Path,
    artifacts_root: &std::path::Path,
    repo_root: &std::path::Path,
    contract_ref: &Contract,
    validators: &[&dyn Validator],
) -> Vec<SessionEvent> {
    let mut writer = JsonlWriter::open(session_path).unwrap();
    let artifacts = ArtifactStore::open(artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        mock_end_turn_only(),
    );
    let run_id = RunId::from("run_validators".to_string());
    let turn_id = TurnId::from("t_validators_1".to_string());
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
            validators,
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
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

fn persist_contract(seed_path: &std::path::Path, goal: &str) -> Contract {
    let mut seed = JsonlWriter::open(seed_path).unwrap();
    let mut drafted = contract::draft(goal);
    drafted.success_criteria.push("validators wired".into());
    let persisted =
        contract::accept_and_persist(&mut seed, drafted, "2026-04-16T00:00:00Z".to_string())
            .expect("persist ok");
    drop(seed);
    persisted
}

#[tokio::test]
async fn happy_path_writes_validator_result_checkpoint_then_committed() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let persisted = persist_contract(&seed_path, "exercise the validator");

    let goal_v = ContractGoalValidator;
    let validators: &[&dyn Validator] = &[&goal_v];

    let session_path = repo_root.join(".azoth/sessions/happy.jsonl");
    let events = drive_with_validators(
        &session_path,
        &artifacts_root,
        &repo_root,
        &persisted,
        validators,
    )
    .await;

    // Locate the three events we care about and confirm their ORDER:
    // ValidatorResult(Pass) → Checkpoint → TurnCommitted.
    let positions: Vec<(usize, &'static str)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, ev)| match ev {
            SessionEvent::ValidatorResult {
                validator, status, ..
            } if validator == "contract_goal_nonempty"
                && matches!(status, ValidatorStatus::Pass) =>
            {
                Some((i, "validator"))
            }
            SessionEvent::Checkpoint { .. } => Some((i, "checkpoint")),
            SessionEvent::TurnCommitted { .. } => Some((i, "committed")),
            _ => None,
        })
        .collect();

    assert_eq!(
        positions.iter().map(|(_, k)| *k).collect::<Vec<_>>(),
        vec!["validator", "checkpoint", "committed"],
        "happy-path ordering wrong: {:?}",
        positions
    );

    // And no TurnAborted of any kind must have been written.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnAborted { .. })),
        "happy path must not write TurnAborted"
    );
}

#[tokio::test]
async fn failing_validator_writes_abort_and_no_checkpoint_or_commit() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let persisted = persist_contract(&seed_path, "deliberately fails");

    let fail_v = AlwaysFailValidator;
    let validators: &[&dyn Validator] = &[&fail_v];

    let session_path = repo_root.join(".azoth/sessions/fail.jsonl");
    let events = drive_with_validators(
        &session_path,
        &artifacts_root,
        &repo_root,
        &persisted,
        validators,
    )
    .await;

    // Exactly one Fail ValidatorResult + one TurnAborted(ValidatorFail).
    let validator_fail_count = events
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
    assert_eq!(validator_fail_count, 1);

    let aborted = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::TurnAborted { reason, detail, .. } => Some((*reason, detail.clone())),
            _ => None,
        })
        .expect("TurnAborted must be present on the validator-fail path");
    assert_eq!(aborted.0, AbortReason::ValidatorFail);
    let detail = aborted.1.expect("detail populated");
    assert!(detail.contains("always_fail"));
    assert!(detail.contains("deliberate"));

    // NO Checkpoint, NO TurnCommitted on the failure path.
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
async fn empty_validators_slice_still_emits_checkpoint_per_invariant_5() {
    // With a contract but no validators wired, the driver must still emit
    // a Checkpoint on each committed turn — invariant #5 ("every run
    // leaves structured evidence including checkpoints") applies even when
    // no validator is there to attest. This was a latent is_some() gate
    // (see .claude memory: graceful-degradation-bypasses-invariants) that
    // had previously been locked in as "pre-validators byte shape" — but
    // the invariant takes precedence over the byte-shape preservation.
    //
    // Still no ValidatorResult (nothing to report) and, of course, the
    // commit marker. Contract-less runs keep the no-Checkpoint path —
    // that's tested in other integration tests.
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let persisted = persist_contract(&seed_path, "no validators attached");

    let session_path = repo_root.join(".azoth/sessions/empty.jsonl");
    let events =
        drive_with_validators(&session_path, &artifacts_root, &repo_root, &persisted, &[]).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ValidatorResult { .. })),
        "empty validators slice must not emit ValidatorResult"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Checkpoint { .. })),
        "contract-scoped commit must emit Checkpoint even when validators is empty"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnCommitted { .. })),
        "empty validators slice must still commit"
    );
}
