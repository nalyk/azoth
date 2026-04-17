//! Backpressure smoke (draft_plan §"Required automated coverage" #8, MED-3).
//!
//! Mock adapter emits 10_000 `TextDelta`s as fast as possible into the
//! driver's bounded (cap=64) stream channel. Two invariants are locked in:
//!
//! 1. **No deadlock under flood.** Historically the driver drained the
//!    channel *after* invoke returned, so a >64-event stream would stall
//!    at `send().await`. The driver now drains concurrently via
//!    `tokio::select!`, so this test would hang forever on the old code.
//!
//! 2. **Cancel registers mid-stream within 100 ms** and writes
//!    `TurnInterrupted { reason: UserCancel }`. We spawn a cancel task that
//!    fires ~10 ms after `drive_turn` starts, then bound the whole thing
//!    under a 100 ms timeout. The `biased;` branch order in the driver
//!    guarantees the cancel arm beats the drain arm.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ContentBlock, EffectCounter, Message, ModelTurnResponse, RunId, SessionEvent,
    StopReason, TurnId, Usage,
};
use azoth_core::turn::TurnDriver;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::sync::mpsc;

/// How many `TextDelta`-bearing content blocks the flood response carries.
/// Each block produces roughly four `StreamEvent`s (ContentBlockStart,
/// TextDelta, ContentBlockStop, and the shared MessageStart/MessageDelta/
/// MessageStop trio), so this is ~40_000 sends through a cap=64 channel.
const FLOOD_BLOCKS: usize = 10_000;

fn flood_response() -> ModelTurnResponse {
    let content: Vec<ContentBlock> = (0..FLOOD_BLOCKS)
        .map(|i| ContentBlock::Text {
            text: format!("d{i}"),
        })
        .collect();
    ModelTurnResponse {
        content,
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 1,
            output_tokens: FLOOD_BLOCKS as u32,
            ..Default::default()
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_flood_writes_turn_interrupted_under_100ms() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_flood.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();

    let script = MockScript {
        turns: vec![flood_response()],
    };
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        script,
    );

    let run_id = RunId::from("run_flood".to_string());
    let turn_id = TurnId::from("t_flood".to_string());
    let cancellation = CancellationToken::new();
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .cancellation(cancellation.clone())
    .build();

    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut caps = CapabilityStore::new();
    let mut effects = EffectCounter::default();

    // Fire cancellation ~10 ms after drive_turn starts.
    let cancel_handle = {
        let tok = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tok.cancel();
        })
    };

    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(100), async {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: None,
            turns_completed: 0,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };
        driver
            .drive_turn(
                turn_id.clone(),
                "system".into(),
                vec![Message::user_text("flood me")],
            )
            .await
    })
    .await;
    let elapsed = start.elapsed();

    cancel_handle.abort();

    assert!(
        result.is_ok(),
        "drive_turn did not honor cancel within 100 ms (stuck in flood drain?)"
    );
    result.unwrap().expect("drive_turn returned Err");

    assert!(
        elapsed < Duration::from_millis(100),
        "cancel took too long: {elapsed:?}"
    );

    drop(writer);

    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().expect("forensic projection");
    let interrupted = forensic.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::TurnInterrupted {
                turn_id: tid,
                reason: AbortReason::UserCancel,
                ..
            } if *tid == turn_id
        )
    });
    assert!(
        interrupted,
        "expected TurnInterrupted(UserCancel) for {turn_id} in JSONL"
    );

    // Replayable projection must NOT surface the interrupted turn — the
    // standing invariant from `resume_replays_prior_commits` still holds.
    let replay = reader.replayable().unwrap();
    let committed_this_turn = replay.iter().any(|e| {
        matches!(
            &e.0,
            SessionEvent::TurnCommitted { turn_id: tid, .. } if *tid == turn_id
        )
    });
    assert!(
        !committed_this_turn,
        "interrupted turn must not appear as committed in replay"
    );
}
