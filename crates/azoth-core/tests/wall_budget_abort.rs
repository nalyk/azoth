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
            run_started_tokio: None,
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
            run_started_tokio: None,
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

/// Stateful adapter for the multi-turn regression below: turn 1 commits
/// after a fixed virtual delay; turn 2 stalls forever. Driven by an
/// `AtomicUsize` so a single instance can be shared across multiple
/// `TurnDriver` constructions (one per turn).
struct CommitThenStallAdapter {
    profile: ProviderProfile,
    call: std::sync::atomic::AtomicUsize,
    turn1_secs: u64,
}

#[async_trait]
impl ProviderAdapter for CommitThenStallAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            // Turn 1: burn `turn1_secs` of virtual time, then commit.
            tokio::time::sleep(std::time::Duration::from_secs(self.turn1_secs)).await;
            // Synthesise a minimal end-of-turn signal so the driver
            // exits the inner loop cleanly without a content block.
            let _ = sink
                .send(StreamEvent::MessageDelta {
                    stop_reason: Some(StopReason::EndTurn),
                    usage_delta: azoth_core::schemas::UsageDelta::default(),
                })
                .await;
            let _ = sink.send(StreamEvent::MessageStop).await;
            Ok(ModelTurnResponse {
                content: vec![azoth_core::schemas::ContentBlock::Text { text: "ok".into() }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        } else {
            // Turn 2: stall forever. The deadline race must fire.
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(ModelTurnResponse {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    async fn count_tokens(
        &self,
        _req: &ModelTurnRequest,
    ) -> Result<azoth_core::adapter::TokenCount, AdapterError> {
        Ok(azoth_core::adapter::TokenCount { input_tokens: 0 })
    }
}

/// Regression for codex P1 (PR #18 round 5): `scope.max_wall_secs` is
/// documented as the budget for the *entire session*, but the prior
/// implementation re-armed the deadline from `Instant::now()` at the
/// start of every `drive_turn`. A 30-second budget therefore reset
/// fully on every turn, letting a multi-turn run burn far past its
/// declared cap.
///
/// Fix: when the worker threads `run_started_tokio` through, the
/// driver computes the deadline as `anchor + budget` (absolute), so
/// each turn's remaining budget tightens as the run ages.
///
/// This test runs two turns under `tokio::time::pause()`. Turn 1
/// commits after burning 25s of virtual time. Turn 2 stalls forever.
/// With the run anchor wired:
///   - Turn 2 has 5s of budget remaining (30 - 25), fires TimeExceeded.
///   - Total virtual time consumed by turn 2 ≤ 7s (5s budget + slack).
///
/// Without the fix (per-turn deadline):
///   - Turn 2 gets a fresh full 30s budget.
///   - Total virtual time consumed by turn 2 ≈ 30s.
///
/// The 7s assertion gives a 2s slack but still fails loudly under the
/// pre-fix per-turn semantics (where elapsed would be ≈ 30s).
#[tokio::test(start_paused = true)]
async fn max_wall_secs_enforced_across_multiple_turns() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_wall_multi.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let mut drafted = contract::draft("multi-turn wall budget");
    drafted.scope.max_wall_secs = Some(30);
    drafted.success_criteria.push("ship cp-2".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-20T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = CommitThenStallAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        call: std::sync::atomic::AtomicUsize::new(0),
        turn1_secs: 25,
    };
    let run_id = RunId::from("run_wall_multi".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        TurnId::from("t_wall_multi_1".to_string()),
        artifacts,
        repo_root.clone(),
    )
    .build();
    let mut caps = CapabilityStore::new();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut effects = azoth_core::schemas::EffectCounter::default();

    // The worker captures this anchor once, before its turn loop, and
    // hands it to every TurnDriver. We mirror that here.
    let run_started_tokio = tokio::time::Instant::now();

    // --- Turn 1: commits after 25s ---
    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx.clone(),
            contract: Some(&persisted),
            turns_completed: 0,
            run_started_tokio: Some(run_started_tokio),
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        let outcome = driver
            .drive_turn(
                TurnId::from("t_wall_multi_1".to_string()),
                "sys".to_string(),
                vec![],
            )
            .await
            .unwrap();
        assert!(
            outcome.final_assistant.is_some(),
            "turn 1 must commit (25s burn well under 30s budget)"
        );
    }

    // --- Turn 2: stalls; deadline must fire on remaining 5s budget ---
    let t2_start = tokio::time::Instant::now();
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
            turns_completed: 1,
            run_started_tokio: Some(run_started_tokio),
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        let outcome = driver
            .drive_turn(
                TurnId::from("t_wall_multi_2".to_string()),
                "sys".to_string(),
                vec![],
            )
            .await
            .unwrap();
        assert!(
            outcome.final_assistant.is_none(),
            "turn 2 must NOT commit (deadline race fires)"
        );
    }
    let t2_elapsed = t2_start.elapsed();

    // The discriminator: with the fix, turn 2 consumes ≤ 5s of virtual
    // time (30s budget minus 25s already spent on turn 1, plus a small
    // tokio scheduling slack). Without the fix, turn 2 would have its
    // own fresh 30s budget and elapse ~30s before TimeExceeded fires.
    // 7s gives 2s slack while still failing loudly under the pre-fix
    // per-turn semantics.
    assert!(
        t2_elapsed <= std::time::Duration::from_secs(7),
        "turn 2 must consume only the remaining session budget (~5s); \
         saw {t2_elapsed:?}. Per-turn budget reset bug is back: \
         contract `max_wall_secs=30` must cap the *whole run*, not each turn."
    );

    // And the JSONL must record TimeExceeded with the *run-elapsed*
    // spent number — under the fix, `spent ≈ 30s` measured from the
    // run anchor, not from turn 2's local start (which would be ≈ 5s).
    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();
    let saw_time_exceeded = forensic.iter().any(|fev| {
        matches!(
            &fev.event,
            SessionEvent::TurnAborted {
                reason: AbortReason::TimeExceeded,
                detail: Some(d),
                turn_id,
                ..
            } if turn_id.0 == "t_wall_multi_2" && d.contains("wall-clock budget 30s")
        )
    });
    assert!(
        saw_time_exceeded,
        "turn 2's TurnAborted must name TimeExceeded with the budget"
    );
}
