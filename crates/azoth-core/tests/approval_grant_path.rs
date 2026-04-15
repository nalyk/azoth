//! Approval-gate end-to-end: a scripted `fs.write` ToolUse drives the
//! authority engine through `RequireApproval`, a responder task grants or
//! denies, and we assert the JSONL event sequence, capability store state,
//! and on-disk file presence match the chosen path.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{
    ApprovalRequestMsg, ApprovalResponse, CapabilityStore,
};
use azoth_core::event_store::JsonlWriter;
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ApprovalScope, CommitOutcome, ContentBlock, EffectClass, Message,
    ModelTurnResponse, RunId, SessionEvent, StopReason, ToolUseId, TurnId, Usage,
};
use azoth_core::tools::FsWriteTool;
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn fs_write_script() -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "fs.write".into(),
                    input: serde_json::json!({
                        "path": ".azoth/tmp/hello.txt",
                        "contents": "hello from approval path",
                    }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage { input_tokens: 10, output_tokens: 3, ..Default::default() },
            },
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "wrote hello".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage { input_tokens: 12, output_tokens: 5, ..Default::default() },
            },
        ],
    }
}

async fn drive_with_responder(
    respond_with: Option<ApprovalResponse>,
) -> (tempfile::TempDir, std::path::PathBuf, Vec<SessionEvent>, bool) {
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_test.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<SessionEvent>();
    writer.set_tap(tap_tx);

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();

    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        fs_write_script(),
    );

    let run_id = RunId::from("run_test".to_string());
    let turn_id = TurnId::from("t_test".to_string());
    let ctx = ExecutionContext {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        artifacts,
        cancellation: CancellationToken::new(),
        repo_root: repo_root.clone(),
    };

    let (atx, mut arx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let responder = tokio::spawn(async move {
        if let Some(req) = arx.recv().await {
            if let Some(resp) = respond_with {
                let _ = req.responder.send(resp);
            }
            // If None, drop the responder to simulate a closed bridge.
        }
    });

    let mut caps = CapabilityStore::new();
    let had_any_cap;
    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: atx,
            contract: None,
            turns_completed: 0,
            kernel: None,
            validators: &[],
        };
        let _ = driver
            .drive_turn(
                turn_id.clone(),
                "system".into(),
                vec![Message::user_text("write hello")],
            )
            .await
            .expect("driver returns Ok even on deny");
        had_any_cap = caps
            .find("fs.write", EffectClass::ApplyLocal, Some(".azoth/tmp/hello.txt"))
            .is_some();
    }
    drop(writer);
    let _ = responder.await;

    let mut events = Vec::new();
    while let Ok(ev) = tap_rx.try_recv() {
        events.push(ev);
    }

    (dir, repo_root, events, had_any_cap)
}

#[tokio::test]
async fn approval_grant_path_mints_token_and_writes_file() {
    let (_dir, repo_root, events, had_cap) = drive_with_responder(Some(
        ApprovalResponse::Grant {
            scope: ApprovalScope::Session,
        },
    ))
    .await;

    // Sequence check: ApprovalRequest → ApprovalGranted → EffectRecord(ApplyLocal)
    // → ToolResult(ok) → TurnCommitted(Success).
    let idx_req = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ApprovalRequest { .. }))
        .expect("ApprovalRequest missing");
    let idx_grant = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ApprovalGranted { .. }))
        .expect("ApprovalGranted missing");
    let idx_effect = events
        .iter()
        .position(|e| matches!(
            e,
            SessionEvent::EffectRecord { effect, .. } if effect.class == EffectClass::ApplyLocal
        ))
        .expect("EffectRecord(ApplyLocal) missing");
    let idx_result = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ToolResult { is_error: false, .. }))
        .expect("clean ToolResult missing");
    let idx_commit = events
        .iter()
        .position(|e| matches!(
            e,
            SessionEvent::TurnCommitted { outcome: CommitOutcome::Success, .. }
        ))
        .expect("TurnCommitted missing");
    assert!(idx_req < idx_grant);
    assert!(idx_grant < idx_effect);
    assert!(idx_effect < idx_result);
    assert!(idx_result < idx_commit);

    assert!(had_cap, "capability token should have been minted");

    let body = tokio::fs::read_to_string(repo_root.join(".azoth/tmp/hello.txt"))
        .await
        .expect("hello.txt must exist inside repo_root");
    assert_eq!(body, "hello from approval path");
}

#[tokio::test]
async fn approval_deny_path_aborts_turn_and_writes_nothing() {
    let (_dir, repo_root, events, had_cap) =
        drive_with_responder(Some(ApprovalResponse::Deny)).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::ApprovalRequest { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::ApprovalDenied { .. })));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::TurnAborted { reason: AbortReason::ApprovalDenied, .. }
    )));
    assert!(!events
        .iter()
        .any(|e| matches!(e, SessionEvent::EffectRecord { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, SessionEvent::ToolResult { .. })));
    assert!(!had_cap, "no capability should be minted on deny");

    let missing = tokio::fs::metadata(repo_root.join(".azoth/tmp/hello.txt")).await;
    assert!(missing.is_err(), "no file should be written on deny");
}
