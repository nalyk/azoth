//! Chronon CP-2 — wall-clock budget enforcement.
//!
//! Contract carrying `scope.max_wall_secs` arms a deadline race inside
//! `TurnDriver::drive_turn`. When the deadline fires before the adapter
//! returns, the driver emits `TurnAborted { reason: TimeExceeded }` and
//! stops the turn without panicking.
//!
//! Uses `tokio::time::pause()` so the test runs in virtual time — real
//! wall-clock is unaffected, the suite stays fast.

use async_trait::async_trait;
use azoth_core::adapter::{AdapterError, ProviderAdapter, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ModelTurnRequest, ModelTurnResponse, RunId, SessionEvent, StopReason, StreamEvent,
    TurnId, Usage,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

/// Adapter whose `invoke` sleeps far longer than any test wall-budget.
/// Under `tokio::time::pause()` the sleep is virtual, so the test is
/// still instant.
struct StallAdapter {
    profile: ProviderProfile,
}

#[async_trait]
impl ProviderAdapter for StallAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        _sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        // Far exceeds any reasonable test budget. If the deadline race
        // is wired, the driver aborts before this future ever returns.
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        Ok(ModelTurnResponse {
            content: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    async fn count_tokens(
        &self,
        _req: &ModelTurnRequest,
    ) -> Result<azoth_core::adapter::TokenCount, AdapterError> {
        Ok(azoth_core::adapter::TokenCount { input_tokens: 0 })
    }
}

#[tokio::test(start_paused = true)]
async fn contract_max_wall_aborts_stalling_turn_with_time_exceeded() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_wall.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    // Contract with a 30-second wall budget.
    let mut drafted = contract::draft("stall experiment");
    drafted.scope.max_wall_secs = Some(30);
    drafted.success_criteria.push("ship cp-2".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-20T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = StallAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
    };
    let run_id = RunId::from("run_wall".to_string());
    let turn_id = TurnId::from("t_wall_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();
    let mut caps = CapabilityStore::new();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
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
            contract: Some(&persisted),
            turns_completed: 0,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        // Drive one turn. The driver should time-out in virtual 30s,
        // far before the stall adapter's 3600s sleep.
        let outcome = driver
            .drive_turn(turn_id.clone(), "sys".to_string(), vec![])
            .await
            .unwrap();
        assert!(
            outcome.final_assistant.is_none(),
            "stalled turn must not commit"
        );
    }

    // Re-read the JSONL and assert the TurnAborted marker names the
    // TimeExceeded reason.
    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();
    let saw_time_exceeded = forensic.iter().any(|fev| {
        matches!(
            &fev.event,
            SessionEvent::TurnAborted {
                reason: AbortReason::TimeExceeded,
                detail: Some(d),
                ..
            } if d.contains("wall-clock budget 30s")
        )
    });
    assert!(
        saw_time_exceeded,
        "JSONL should contain TurnAborted with reason=TimeExceeded and budget detail"
    );
}

#[tokio::test(start_paused = true)]
async fn contract_without_max_wall_does_not_arm_deadline() {
    // Same setup but no `max_wall_secs` — the driver should NOT time
    // out; we still expect the turn to run to the mock's EndTurn.
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_nowall.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let mut drafted = contract::draft("no wall budget");
    drafted.success_criteria.push("run clean".into());
    assert!(drafted.scope.max_wall_secs.is_none());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-20T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let script = azoth_core::adapter::MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![azoth_core::schemas::ContentBlock::Text {
                text: "done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }],
    };
    let adapter = azoth_core::adapter::MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        script,
    );
    let run_id = RunId::from("run_nowall".to_string());
    let turn_id = TurnId::from("t_nowall_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();
    let mut caps = CapabilityStore::new();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut effects = azoth_core::schemas::EffectCounter::default();

    {
        let mut driver = TurnDriver {
            run_id,
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(&persisted),
            turns_completed: 0,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        let outcome = driver
            .drive_turn(turn_id, "sys".to_string(), vec![])
            .await
            .unwrap();
        assert!(
            outcome.final_assistant.is_some(),
            "un-budgeted turn should commit"
        );
    }
}
