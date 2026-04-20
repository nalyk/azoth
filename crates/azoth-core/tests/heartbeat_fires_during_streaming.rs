//! Chronon CP-2 — heartbeat liveness during long streaming calls.
//!
//! Regression for codex P1 (PR #18): the drain branch in the
//! `TurnDriver` select loop used to discard every `StreamEvent` without
//! updating progress counters. Because `content_blocks_so_far`,
//! `tool_calls_so_far`, and `total_usage.output_tokens` were only bumped
//! *after* `invoke_fut` returned, mid-flight heartbeats always saw the
//! same zeros as `last_heartbeat_progress` and emitted nothing — the
//! exact liveness signal the heartbeat was designed to prove was silent
//! for the duration of every long streaming call.
//!
//! This test exercises a slow-streaming adapter under `tokio::time::pause()`:
//! heartbeats tick at 2-second virtual intervals, the adapter drips one
//! stream event every 3 seconds, and the turn must emit at least one
//! `TurnHeartbeat` event carrying non-zero progress before the invoke
//! future finally returns.

use async_trait::async_trait;
use azoth_core::adapter::{AdapterError, ProviderAdapter, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ContentBlock, ContentBlockStub, ModelTurnRequest, ModelTurnResponse, RunId, SessionEvent,
    StopReason, StreamEvent, TurnId, Usage, UsageDelta,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

/// Adapter that drips one stream event every 3 seconds then returns.
/// Total wall-time spent inside `invoke` is ~18s — long enough for at
/// least three 2-second heartbeat ticks to fire.
struct DrippingStreamAdapter {
    profile: ProviderProfile,
}

#[async_trait]
impl ProviderAdapter for DrippingStreamAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        // Drip the canonical synthetic-stream sequence with 3s gaps so
        // the heartbeat (2s interval) fires between consecutive events.
        let gap = std::time::Duration::from_secs(3);

        let _ = sink.send(StreamEvent::MessageStart).await;
        tokio::time::sleep(gap).await;

        let _ = sink
            .send(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStub::Text,
            })
            .await;
        tokio::time::sleep(gap).await;

        let _ = sink
            .send(StreamEvent::TextDelta {
                index: 0,
                text: "hello".into(),
            })
            .await;
        tokio::time::sleep(gap).await;

        let _ = sink.send(StreamEvent::ContentBlockStop { index: 0 }).await;
        tokio::time::sleep(gap).await;

        let _ = sink
            .send(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage_delta: UsageDelta {
                    input_tokens: 10,
                    output_tokens: 42,
                },
            })
            .await;
        tokio::time::sleep(gap).await;

        let _ = sink.send(StreamEvent::MessageStop).await;

        Ok(ModelTurnResponse {
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 42,
                ..Usage::default()
            },
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
async fn heartbeat_fires_during_streaming_with_streamed_progress() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_hb_stream.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = DrippingStreamAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
    };
    let run_id = RunId::from("run_hb_stream".to_string());
    let turn_id = TurnId::from("t_hb_stream_1".to_string());
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
            contract: None,
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
            .drive_turn(turn_id.clone(), "sys".to_string(), vec![])
            .await
            .unwrap();
        assert!(
            outcome.final_assistant.is_some(),
            "streamed turn should commit"
        );
    }

    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();

    // At least one heartbeat must have fired mid-stream and its progress
    // must carry non-zero streaming counters (content_blocks or
    // tokens_out). Pre-fix, streaming discarded all events and every
    // heartbeat saw the same (0,0,0) as `last_heartbeat_progress` → no
    // TurnHeartbeat was ever appended.
    let heartbeats: Vec<_> = forensic
        .iter()
        .filter_map(|fev| match &fev.event {
            SessionEvent::TurnHeartbeat { progress, .. } => Some(progress.clone()),
            _ => None,
        })
        .collect();

    assert!(
        !heartbeats.is_empty(),
        "at least one TurnHeartbeat should fire mid-stream (got none — \
         drain branch is probably discarding StreamEvents again)"
    );

    let saw_nonzero_progress = heartbeats
        .iter()
        .any(|p| p.content_blocks > 0 || p.tokens_out > 0);
    assert!(
        saw_nonzero_progress,
        "at least one heartbeat must carry non-zero progress; got {heartbeats:?}"
    );
}

/// Adapter that hangs forever without ever sending a single
/// `StreamEvent` and never returns. Models the worst-case "the
/// upstream provider deadlocked before producing anything" scenario
/// — the exact case the heartbeat exists to surface.
struct SilentDeadlockAdapter {
    profile: ProviderProfile,
}

#[async_trait]
impl ProviderAdapter for SilentDeadlockAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        _sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        // Hang well past any reasonable test budget. Cancellation
        // via `tokio::time::timeout` (below) wraps this future so
        // the test still terminates.
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

/// Regression for gemini MED 3114141537 (PR #18 round 5): when a turn
/// deadlocks on its very first invoke without producing any content
/// or tokens, the prior heartbeat gate (`progress != last`) suppressed
/// every emit — both sides equal `HeartbeatProgress::default()`. The
/// liveness signal stayed silent for exactly the failure mode it was
/// designed to surface.
///
/// Fix: `last_heartbeat_progress: Option<HeartbeatProgress>` initial
/// `None`. The first tick is unconditional (`None != Some(_)`); subsequent
/// ticks compare-and-emit. Fast turns (sub-2s) still emit nothing
/// because the tick branch never runs.
///
/// Test design: a SilentDeadlockAdapter sleeps 3600s under
/// `tokio::time::pause()`. We wrap `drive_turn` in a `tokio::time::
/// timeout(10s)` so the test terminates; the timeout *consumer*-side
/// cancels the driver future, but at least one heartbeat MUST have
/// landed by the time the timeout fires (heartbeat interval is 2s, so
/// at t+2s the first tick runs and the new code unconditionally emits).
#[tokio::test(start_paused = true)]
async fn heartbeat_fires_on_silent_deadlock_without_progress() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_hb_silent.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = SilentDeadlockAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
    };
    let run_id = RunId::from("run_hb_silent".to_string());
    let turn_id = TurnId::from("t_hb_silent_1".to_string());
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
            contract: None,
            turns_completed: 0,
            run_started_tokio: None,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        // Wrap the driver in a 10s virtual timeout so the test
        // terminates. The driver future is cancelled at t+10s; the
        // partial JSONL written up to that point is what we inspect.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            driver.drive_turn(turn_id.clone(), "sys".to_string(), vec![]),
        )
        .await;
    }

    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();

    // Pre-fix: zero TurnHeartbeat events because both `progress` and
    // `last_heartbeat_progress` were `(0,0,0)` on every tick.
    // Post-fix: at least one heartbeat lands at the first tick (t+2s)
    // because the gate now compares `Some(progress)` against the
    // initial `None` — never equal.
    let heartbeats: Vec<_> = forensic
        .iter()
        .filter_map(|fev| match &fev.event {
            SessionEvent::TurnHeartbeat { progress, .. } => Some(progress.clone()),
            _ => None,
        })
        .collect();

    assert!(
        !heartbeats.is_empty(),
        "at least one TurnHeartbeat must fire even when a turn deadlocks \
         before producing any content/tokens — pre-fix the equality gate \
         silently suppressed every emit, masking the exact stall the \
         heartbeat is meant to surface"
    );

    // The first heartbeat must carry the zero-progress shape (proving
    // it fired BECAUSE of the unconditional first-emit, not because
    // of latent stale state from another test).
    let first = &heartbeats[0];
    assert_eq!(first.content_blocks, 0);
    assert_eq!(first.tool_calls, 0);
    assert_eq!(first.tokens_out, 0);
}
