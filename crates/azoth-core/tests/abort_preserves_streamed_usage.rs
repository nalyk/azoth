//! Chronon CP-2 round-3 regression — mid-invoke aborts preserve the
//! streamed-but-not-yet-accumulated output tokens.
//!
//! `TurnDriver` only folds `response.usage` into `total_usage` *after*
//! `invoke_fut` returns. When a turn aborts mid-stream — wall-clock
//! deadline, user cancel, or adapter error — every token already delivered
//! via `MessageDelta` would otherwise be dropped from the persisted abort
//! record, understating run-level accounting for exactly the turn that was
//! cut short.
//!
//! The fix (see `turn/mod.rs`) folds the per-invoke `stream_output_tokens`
//! counter into the usage persisted with the abort. These tests pin the
//! behavior via the wall-timeout and user-cancel branches.

use async_trait::async_trait;
use azoth_core::adapter::{AdapterError, ProviderAdapter, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ModelTurnRequest, ModelTurnResponse, RunId, SessionEvent, StopReason, StreamEvent,
    TurnId, Usage, UsageDelta,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

/// Emits a `MessageDelta` carrying `streamed_in` input tokens and
/// `streamed_out` output tokens *before* blocking forever (virtual time in
/// the tests below). The streaming happens synchronously in-call so the
/// driver's drain arm consumes the delta before any deadline or
/// cancellation is evaluated.
struct StreamingStallAdapter {
    profile: ProviderProfile,
    streamed_in: u32,
    streamed_out: u32,
}

#[async_trait]
impl ProviderAdapter for StreamingStallAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        // Push a single MessageDelta carrying the streamed token deltas.
        // The driver's drain arm accumulates both into `stream_input_tokens`
        // / `stream_output_tokens`.
        sink.send(StreamEvent::MessageDelta {
            stop_reason: None,
            usage_delta: UsageDelta {
                input_tokens: self.streamed_in,
                output_tokens: self.streamed_out,
            },
        })
        .await
        .ok();
        // Far exceeds any reasonable test budget. Under `tokio::time::pause()`
        // this is virtual, so the test stays fast.
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
async fn time_exceeded_abort_preserves_streamed_output_tokens() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_wall_stream.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let mut drafted = contract::draft("stall with stream");
    drafted.scope.max_wall_secs = Some(30);
    drafted.success_criteria.push("ship cp-2 round 3".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-20T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = StreamingStallAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        // Both fields exercised — adapters that stream `input_tokens` mid-call
        // (Anthropic's `message_delta.usage` in some contexts) previously had
        // that delta silently dropped on mid-invoke abort.
        streamed_in: 11,
        streamed_out: 42,
    };
    let run_id = RunId::from("run_wall_stream".to_string());
    let turn_id = TurnId::from("t_wall_stream_1".to_string());
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

        let outcome = driver
            .drive_turn(turn_id.clone(), "sys".to_string(), vec![])
            .await
            .unwrap();
        assert!(outcome.final_assistant.is_none());
        assert_eq!(
            outcome.usage.input_tokens, 11,
            "TurnOutcome.usage must reflect streamed input tokens on time-exceeded abort"
        );
        assert_eq!(
            outcome.usage.output_tokens, 42,
            "TurnOutcome.usage must reflect streamed output tokens on time-exceeded abort"
        );
    }

    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();
    let aborted = forensic
        .iter()
        .find_map(|fev| match &fev.event {
            SessionEvent::TurnAborted {
                reason: AbortReason::TimeExceeded,
                usage,
                ..
            } => Some(usage.clone()),
            _ => None,
        })
        .expect("expected TurnAborted { TimeExceeded } in forensic projection");
    assert_eq!(
        aborted.input_tokens, 11,
        "persisted TurnAborted.usage must carry the streamed input tokens"
    );
    assert_eq!(
        aborted.output_tokens, 42,
        "persisted TurnAborted.usage must carry the streamed output tokens"
    );
}

#[tokio::test(start_paused = true)]
async fn user_cancel_interrupt_preserves_streamed_output_tokens() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_cancel_stream.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    // No wall budget — this test exercises the cancellation branch only.
    let mut drafted = contract::draft("cancel with stream");
    drafted.success_criteria.push("ship cp-2 round 3".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-20T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = StreamingStallAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        streamed_in: 5,
        streamed_out: 17,
    };
    let run_id = RunId::from("run_cancel_stream".to_string());
    let turn_id = TurnId::from("t_cancel_stream_1".to_string());

    let cancellation = CancellationToken::new();
    let cancel_trigger = cancellation.clone();
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .cancellation(cancellation)
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

        // Race the driver against a task that cancels after letting the
        // adapter stream its deltas. Virtual time lets us schedule the
        // cancel *after* the deltas drain but *before* the adapter's
        // 3600s sleep ends.
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel_trigger.cancel();
        });

        let outcome = driver
            .drive_turn(turn_id.clone(), "sys".to_string(), vec![])
            .await
            .unwrap();
        cancel_task.await.unwrap();
        assert!(outcome.final_assistant.is_none());
        assert_eq!(
            outcome.usage.input_tokens, 5,
            "TurnOutcome.usage must reflect streamed input tokens on user-cancel"
        );
        assert_eq!(
            outcome.usage.output_tokens, 17,
            "TurnOutcome.usage must reflect streamed output tokens on user-cancel"
        );
    }

    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();
    let interrupt = forensic
        .iter()
        .find_map(|fev| match &fev.event {
            SessionEvent::TurnInterrupted {
                reason: AbortReason::UserCancel,
                partial_usage,
                ..
            } => Some(partial_usage.clone()),
            _ => None,
        })
        .expect("expected TurnInterrupted { UserCancel } in forensic projection");
    assert_eq!(
        interrupt.input_tokens, 5,
        "persisted TurnInterrupted.partial_usage must carry the streamed input tokens"
    );
    assert_eq!(
        interrupt.output_tokens, 17,
        "persisted TurnInterrupted.partial_usage must carry the streamed output tokens"
    );
}
