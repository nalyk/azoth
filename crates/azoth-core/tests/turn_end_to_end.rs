//! End-to-end smoke: the TUI worker path in isolation.
//!
//! Opens a tapped `JsonlWriter`, registers `repo.search`, runs a scripted
//! `MockAdapter` (ToolUse → EndTurn) through `TurnDriver`, then asserts:
//!  * the tap saw the full canonical sequence for the turn,
//!  * the JSONL file on disk has a matching `TurnCommitted` line, and
//!  * the replayable projection trusts the whole turn.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    CommitOutcome, ContentBlock, Message, ModelTurnResponse, RunId, SessionEvent, StopReason,
    ToolUseId, TurnId, Usage,
};
use azoth_core::tools::RepoSearchTool;
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn tui_worker_pipeline_drives_full_turn_sequence() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();

    // A file the repo.search tool can find.
    std::fs::write(repo_root.join("needle.txt"), "azoth sentinel line\n").unwrap();

    let session_path = repo_root.join(".azoth/sessions/run_test.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<SessionEvent>();
    writer.set_tap(tap_tx);

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();

    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(RepoSearchTool);

    let script = MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "repo.search".into(),
                    input: serde_json::json!({ "q": "sentinel", "limit": 3 }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                    ..Default::default()
                },
            },
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "found sentinel".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 5,
                    ..Default::default()
                },
            },
        ],
    };
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        script,
    );

    let run_id = RunId::from("run_test".to_string());
    let turn_id = TurnId::from("t_test".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
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
            contract: None,
            turns_completed: 0,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
        };
        let usage = driver
            .drive_turn(
                turn_id.clone(),
                "system".into(),
                vec![Message::user_text("find sentinel")],
            )
            .await
            .expect("turn drives cleanly");
        assert!(usage.usage.output_tokens >= 5);
    }
    drop(writer);

    // Drain the tap and check the canonical sequence fired.
    let mut tap_events = Vec::new();
    while let Ok(ev) = tap_rx.try_recv() {
        tap_events.push(ev);
    }

    assert!(
        matches!(tap_events.first(), Some(SessionEvent::TurnStarted { .. })),
        "first tapped event should be TurnStarted, got: {:#?}",
        tap_events.first()
    );

    let saw_tool_use = tap_events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ContentBlock {
                block: ContentBlock::ToolUse { name, .. }, ..
            } if name == "repo.search"
        )
    });
    assert!(
        saw_tool_use,
        "expected a ContentBlock::ToolUse(repo.search)"
    );

    let saw_effect = tap_events
        .iter()
        .any(|e| matches!(e, SessionEvent::EffectRecord { .. }));
    assert!(saw_effect, "expected an EffectRecord");

    let saw_tool_result = tap_events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ToolResult {
                is_error: false,
                ..
            }
        )
    });
    assert!(saw_tool_result, "expected a clean ToolResult");

    let saw_commit = tap_events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::TurnCommitted {
                outcome: CommitOutcome::Success,
                ..
            }
        )
    });
    assert!(saw_commit, "expected a TurnCommitted(Success)");

    // The on-disk JSONL file must round-trip through the replayable projection
    // with the full turn intact — confirms the tap and the writer agree.
    let reader = JsonlReader::open(&session_path);
    let replay = reader.replayable().unwrap();
    assert!(
        replay.iter().any(|e| matches!(
            &e.0,
            SessionEvent::TurnCommitted { turn_id: tid, .. } if tid == &turn_id
        )),
        "replay projection missing TurnCommitted for {turn_id}"
    );
}
