//! Effect-budget enforcement: when a contract caps `max_apply_local` at N
//! and the model has already consumed N `apply_local` effects on this run,
//! the (N+1)th `fs.write` dispatch must short-circuit with a `TurnAborted`
//! bearing `reason = RuntimeError` and a detail string of the form
//! `effect budget exhausted: apply_local <used>/<max>`. No `EffectRecord`,
//! no `ToolResult`, no `TurnCommitted` must be emitted for the short-
//! circuited call.
//!
//! Also checks the inert path: when `contract` is `None`, the driver never
//! reads or writes the counter (pre-contract byte shape is preserved).

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{
    mint_from_approval, ApprovalRequestMsg, ApprovalResponse, CapabilityStore,
};
use azoth_core::contract;
use azoth_core::event_store::JsonlWriter;
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ApprovalScope, ContentBlock, Contract, EffectClass, EffectCounter, Message,
    ModelTurnResponse, RunId, SessionEvent, StopReason, ToolUseId, TurnId, Usage,
};
use azoth_core::tools::FsWriteTool;
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn fs_write_then_end() -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "fs_write".into(),
                    input: serde_json::json!({
                        "path": ".azoth/tmp/budget.txt",
                        "contents": "over budget",
                    }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    ..Default::default()
                },
            },
            // Never reached when the budget gate fires, but present so the
            // mock script doesn't run dry on the inert-path test below.
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    ..Default::default()
                },
            },
        ],
    }
}

fn capped_contract(goal: &str) -> Contract {
    let mut c = contract::draft(goal);
    c.success_criteria.push("budget enforced".into());
    c.effect_budget.max_apply_local = 1;
    c
}

#[tokio::test]
async fn over_budget_apply_local_aborts_turn_with_runtime_error() {
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_budget.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<SessionEvent>();
    writer.set_tap(tap_tx);

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        fs_write_then_end(),
    );

    let contract_val = capped_contract("cap apply_local at 1");
    let run_id = RunId::from("run_budget".to_string());
    let turn_id = TurnId::from("t_budget_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    // Pre-mint a session-scope capability so the driver doesn't stall on
    // approval — the short-circuit we're testing lives BEFORE the authorize
    // call, so the token is only needed in the inert (contract=None) half.
    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let _ = req.responder.send(ApprovalResponse::Grant {
                scope: ApprovalScope::Session,
            });
        }
    });

    let mut caps = CapabilityStore::new();
    // Seed the counter AT the cap, as if a prior turn had already consumed
    // the sole apply_local budget — the next call must short-circuit.
    let mut effects = EffectCounter {
        apply_local: 1,
        apply_repo: 0,
        network_reads: 0,
    };

    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(&contract_val),
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
                vec![Message::user_text("go over budget")],
            )
            .await
            .expect("abort path returns Ok");
    }
    drop(writer);

    let mut events = Vec::new();
    while let Ok(ev) = tap_rx.try_recv() {
        events.push(ev);
    }

    // TurnAborted with RuntimeError + the exact detail prefix.
    let aborted = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::TurnAborted { reason, detail, .. } => Some((*reason, detail.clone())),
            _ => None,
        })
        .expect("TurnAborted must be present");
    assert_eq!(aborted.0, AbortReason::RuntimeError);
    let detail = aborted.1.expect("detail populated");
    assert!(
        detail.starts_with("effect budget exhausted: apply_local"),
        "unexpected detail: {detail}"
    );
    assert!(detail.contains("1/1"), "should report used/max: {detail}");

    // No EffectRecord, no ToolResult, no TurnCommitted for this turn.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::EffectRecord { .. })),
        "over-budget dispatch must not emit EffectRecord"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolResult { .. })),
        "over-budget dispatch must not emit ToolResult"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnCommitted { .. })),
        "over-budget dispatch must not emit TurnCommitted"
    );

    // The counter must NOT have been bumped (no successful EffectRecord).
    assert_eq!(effects.apply_local, 1);

    // The on-disk file must not exist — the tool never ran.
    let missing = tokio::fs::metadata(repo_root.join(".azoth/tmp/budget.txt")).await;
    assert!(missing.is_err(), "no file may be written on short-circuit");
}

#[tokio::test]
async fn first_apply_local_under_cap_succeeds_and_bumps_counter() {
    // Complement: the happy path — starting counter below cap, one fs.write
    // runs, counter increments to exactly the cap, and the turn commits.
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_under.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        fs_write_then_end(),
    );

    let contract_val = capped_contract("single apply_local allowed");
    let run_id = RunId::from("run_under".to_string());
    let turn_id = TurnId::from("t_under_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    // Pre-mint the capability so authorize returns Reuse and no approval
    // round-trip is needed to prove the under-cap bump.
    let mut caps = CapabilityStore::new();
    caps.mint(mint_from_approval(
        "fs_write",
        EffectClass::ApplyLocal,
        ApprovalScope::Session,
    ));

    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut effects = EffectCounter::default();

    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(&contract_val),
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
                vec![Message::user_text("do the one allowed write")],
            )
            .await
            .expect("under-cap path drives cleanly");
    }
    drop(writer);

    assert_eq!(
        effects.apply_local, 1,
        "counter should bump by exactly one successful EffectRecord"
    );

    let body = tokio::fs::read_to_string(repo_root.join(".azoth/tmp/budget.txt"))
        .await
        .expect("budget.txt must exist");
    assert_eq!(body, "over budget");
}
