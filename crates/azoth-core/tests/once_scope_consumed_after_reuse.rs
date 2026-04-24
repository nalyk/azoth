//! F0 (2026-04-25) regression: `ApprovalScope::Once` is contracted with
//! the user as a one-shot grant, but before the fix `CapabilityStore::find`
//! matched Once tokens on every subsequent authorize call. A live E2E run
//! on 2026-04-25 observed one `approve once` for `fs_write` on
//! `/tmp/smoke.txt` (which the tool rejected at its repo-root guard)
//! silently cover two follow-up writes — one to `docs/E2E_MARKER.md` and
//! a full rewrite of `Cargo.toml`.
//!
//! This test drives a turn with TWO `fs_write` ToolUse blocks in a single
//! model response. The responder grants `Once` on the first approval and
//! tracks approval-request count. With the fix wired into the driver's
//! `Reuse(id)` arm, the Once token is consumed after the first tool
//! dispatch; the second `fs_write` therefore sees no matching cap and
//! produces a second `ApprovalRequest`. Without the fix, exactly one
//! request ever appears in the JSONL tap.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, ApprovalResponse, CapabilityStore};
use azoth_core::event_store::JsonlWriter;
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ApprovalScope, ContentBlock, Message, ModelTurnResponse, RunId, SessionEvent, StopReason,
    ToolUseId, TurnId, Usage,
};
use azoth_core::tools::FsWriteTool;
use azoth_core::turn::TurnDriver;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn two_writes_script() -> MockScript {
    MockScript {
        turns: vec![
            // One model round that emits TWO tool_use blocks, both fs_write.
            ModelTurnResponse {
                content: vec![
                    ContentBlock::ToolUse {
                        id: ToolUseId::new(),
                        name: "fs_write".into(),
                        input: serde_json::json!({
                            "path": ".azoth/tmp/a.txt",
                            "contents": "first",
                        }),
                        call_group: None,
                    },
                    ContentBlock::ToolUse {
                        id: ToolUseId::new(),
                        name: "fs_write".into(),
                        input: serde_json::json!({
                            "path": ".azoth/tmp/b.txt",
                            "contents": "second",
                        }),
                        call_group: None,
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    ..Default::default()
                },
            },
            // Closing round after both tool_results return.
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "both written".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    ..Default::default()
                },
            },
        ],
    }
}

#[tokio::test]
async fn once_scope_is_consumed_after_first_reuse() {
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_f0.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<SessionEvent>();
    writer.set_tap(tap_tx);

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();

    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        two_writes_script(),
    );

    let run_id = RunId::from("run_f0".to_string());
    let turn_id = TurnId::from("t_f0".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    // Responder: grant Once on every approval request. Without the F0
    // fix the second approval never fires; with the fix it does, and
    // this responder happily grants it — letting us count requests in
    // the JSONL tap below.
    let grants = Arc::new(AtomicUsize::new(0));
    let grants_in_task = grants.clone();
    let (atx, mut arx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let responder = tokio::spawn(async move {
        while let Some(req) = arx.recv().await {
            grants_in_task.fetch_add(1, Ordering::SeqCst);
            let _ = req.responder.send(ApprovalResponse::Grant {
                scope: ApprovalScope::Once,
            });
        }
    });

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
            approval_bridge: atx,
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
        let _ = driver
            .drive_turn(
                turn_id.clone(),
                "system".into(),
                vec![Message::user_text("write two files")],
            )
            .await
            .expect("driver returns Ok");
    }
    drop(writer);
    let _ = responder.await;

    let mut events = Vec::new();
    while let Ok(ev) = tap_rx.try_recv() {
        events.push(ev);
    }

    let approval_requests = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::ApprovalRequest { .. }))
        .count();
    let approval_grants = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::ApprovalGranted { .. }))
        .count();

    assert_eq!(
        approval_requests, 2,
        "F0: each apply_local call under a consumed Once grant must re-prompt; \
         expected 2 ApprovalRequest events, got {approval_requests}. Events: {events:#?}"
    );
    assert_eq!(
        approval_grants, 2,
        "Responder should have granted twice — one per ApprovalRequest"
    );
    assert_eq!(
        grants.load(Ordering::SeqCst),
        2,
        "Responder task counter confirms 2 bridge messages"
    );

    // The consumed Once tokens leave no residue in the store.
    assert!(
        caps.iter().next().is_none(),
        "After two Once grants + two consumptions, the store must be empty"
    );
}
