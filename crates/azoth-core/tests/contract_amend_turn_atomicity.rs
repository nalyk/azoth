//! β: invariant #7 (turn-scoped atomicity) survives the amend path.
//!
//! Scenario: budget seeded AT the ceiling for `apply_local = 1`. The
//! responder grants BOTH the budget extension AND the per-tool
//! approval. The fs_write then succeeds, the turn commits, and the
//! JSONL log contains exactly one terminal marker for this turn
//! (TurnCommitted), a `ContractAmended` event sitting strictly between
//! TurnStarted and TurnCommitted, and the expected EffectRecord +
//! ToolResult after the amend.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, ApprovalResponse, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::JsonlWriter;
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ApprovalScope, CommitOutcome, ContentBlock, Contract, EffectClass, EffectCounter, Message,
    ModelTurnResponse, RunId, SessionEvent, StopReason, ToolUseId, TurnId, Usage,
};
use azoth_core::tools::FsWriteTool;
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn two_fs_writes_then_end() -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "fs_write".into(),
                    input: serde_json::json!({
                        "path": ".azoth/tmp/two_a.txt",
                        "contents": "first write",
                    }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    ..Default::default()
                },
            },
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "fs_write".into(),
                    input: serde_json::json!({
                        "path": ".azoth/tmp/two_b.txt",
                        "contents": "second write",
                    }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    ..Default::default()
                },
            },
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "both written".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 1,
                    ..Default::default()
                },
            },
        ],
    }
}

fn one_fs_write_then_end() -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "fs_write".into(),
                    input: serde_json::json!({
                        "path": ".azoth/tmp/amend_ok.txt",
                        "contents": "written after amend",
                    }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    ..Default::default()
                },
            },
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "wrote it".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 1,
                    ..Default::default()
                },
            },
        ],
    }
}

fn budget_1_contract(goal: &str) -> Contract {
    let mut c = contract::draft(goal);
    c.success_criteria.push("amend lets it through".into());
    c.effect_budget.max_apply_local = 1;
    c
}

#[tokio::test]
async fn amend_grant_preserves_single_terminal_marker_and_ordering() {
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_amend.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<SessionEvent>();
    writer.set_tap(tap_tx);

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        one_fs_write_then_end(),
    );

    let contract_val = budget_1_contract("atomicity under amend");
    let contract_id = contract_val.id.clone();
    let run_id = RunId::from("run_amend".to_string());
    let turn_id = TurnId::from("t_amend_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    // Grant-everything responder: both budget_extension and the
    // subsequent per-tool approval come through this bridge.
    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let responder = tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let _ = req.responder.send(ApprovalResponse::Grant {
                scope: ApprovalScope::Session,
            });
        }
    });

    let mut caps = CapabilityStore::new();
    // Seed the counter AT the cap so the first fs_write hits the
    // budget-overflow branch.
    let mut effects = EffectCounter {
        apply_local: 1,
        ..Default::default()
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
            run_started_tokio: None,
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
                vec![Message::user_text("write past the cap")],
            )
            .await
            .expect("amend+write returns Ok");
    }
    drop(writer);
    responder.abort();

    let mut events = Vec::new();
    while let Ok(ev) = tap_rx.try_recv() {
        events.push(ev);
    }

    // Invariant #7: exactly one terminal marker for this turn.
    let terminals: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionEvent::TurnCommitted { .. }
                    | SessionEvent::TurnAborted { .. }
                    | SessionEvent::TurnInterrupted { .. }
            )
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "amend must not split the turn or double-terminate; got: {terminals:#?}"
    );
    assert!(
        matches!(
            terminals[0],
            SessionEvent::TurnCommitted {
                outcome: CommitOutcome::Success,
                ..
            }
        ),
        "expected TurnCommitted(Success) after amend+grant+write, got {:?}",
        terminals[0]
    );

    // ContractAmended lands strictly between TurnStarted and the
    // terminal marker. Positional check: find indices and compare.
    let turn_started_idx = events
        .iter()
        .position(|e| matches!(e, SessionEvent::TurnStarted { .. }))
        .expect("TurnStarted present");
    let amend_idx = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ContractAmended { .. }))
        .expect("ContractAmended present");
    let terminal_idx = events
        .iter()
        .position(|e| {
            matches!(
                e,
                SessionEvent::TurnCommitted { .. }
                    | SessionEvent::TurnAborted { .. }
                    | SessionEvent::TurnInterrupted { .. }
            )
        })
        .expect("terminal present");
    assert!(
        turn_started_idx < amend_idx && amend_idx < terminal_idx,
        "ContractAmended must land between TurnStarted and terminal: \
         got turn_started={turn_started_idx} amend={amend_idx} terminal={terminal_idx}"
    );

    // Amend event carries the right contract id and a nonzero delta.
    let (c_id, delta) = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ContractAmended {
                contract_id, delta, ..
            } => Some((contract_id.clone(), delta.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(c_id, contract_id);
    assert_eq!(
        delta.apply_local, 1,
        "β proposes delta=current; current was 1 so delta=1"
    );

    // Counter state AFTER the turn: 2 effects consumed (pre-seeded 1 +
    // the one that succeeded after amend), 1 amend this run, 0 per
    // turn (drive_turn resets on entry, then the grant bumped it to 1;
    // check total reached ≥1 via amends_this_run).
    assert_eq!(
        effects.apply_local, 2,
        "pre-seeded 1 + 1 post-amend write = 2"
    );
    assert_eq!(effects.apply_local_ceiling_bonus, 1);
    assert_eq!(effects.amends_this_run, 1);
    assert_eq!(
        effects.amends_this_turn, 1,
        "one grant observed in this turn"
    );

    // A successful tool dispatch followed the amend (EffectRecord +
    // ToolResult). Order check not strict — the amend flow only cares
    // that the per-tool happy path ran after the ceiling raise.
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::EffectRecord { effect, .. } if effect.class == EffectClass::ApplyLocal)));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::ToolResult { .. })));
}

#[tokio::test]
async fn second_amend_in_same_turn_clamps_against_pending_inclusive_ceiling() {
    // R3 (PR #31 codex P2 + gemini HIGH): when a turn grants TWO
    // amends, the second grant's clamp must use the pending-inclusive
    // ceiling (base + bonus + pending_from_first_amend), not the
    // pre-turn base. Pre-R3 code under-applied: the user granted a
    // 4-unit ceiling but only a 3-unit ceiling landed in applied_delta.
    //
    // Scenario: max_apply_local = 1, pre-seeded apply_local = 1 (at
    // cap). Turn issues two fs_writes.
    //   1st tool: budget check → 1 >= 1 → amend 1 → current=1,
    //     proposed=2, applied=1. pending_apply_local = 1. Tool runs,
    //     apply_local = 2.
    //   2nd tool: budget check → effective_max = 1+0+1 = 2 → 2 >= 2
    //     → amend 2 → current=2, proposed=4.
    //     Pre-R3: applied = clamp(2, base+bonus=1+0=1) = 1 ← BUG.
    //     Post-R3: applied = clamp(2, base+bonus+pending=1+0+1=2) = 2 ← FIX.
    //     Tool runs, apply_local = 3.
    // Commit flush: ceiling_bonus = 1 + 2 = 3.
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_two_amends.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        two_fs_writes_then_end(),
    );

    let contract_val = budget_1_contract("two amends one turn");
    let run_id = RunId::from("run_two_amends".to_string());
    let turn_id = TurnId::from("t_two_amends".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let responder = tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let _ = req.responder.send(ApprovalResponse::Grant {
                scope: ApprovalScope::Session,
            });
        }
    });

    let mut caps = CapabilityStore::new();
    let mut effects = EffectCounter {
        apply_local: 1,
        ..Default::default()
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
            run_started_tokio: None,
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
                vec![Message::user_text("two writes")],
            )
            .await
            .expect("two-amend turn returns Ok");
    }
    drop(writer);
    responder.abort();

    // Post-turn assertions. The key one is ceiling_bonus == 3:
    // pre-R3 this would be 2 because the second clamp under-applied.
    assert_eq!(
        effects.apply_local, 3,
        "pre-seed 1 + two successful writes = 3"
    );
    assert_eq!(
        effects.apply_local_ceiling_bonus, 3,
        "first amend applied 1, second amend applied 2 (pending-inclusive clamp); \
         pre-R3 buggy sum would be 2 (1 + 1 because second clamp used bare bonus)"
    );
    assert_eq!(
        effects.amends_this_run, 2,
        "two grants in this run (and this turn)"
    );
    assert_eq!(effects.amends_this_turn, 2, "two grants within one turn");
}

#[tokio::test]
async fn aborted_turn_after_amend_does_not_persist_amend_state() {
    // R2 (codex PR #31 P1): if a turn grants an amend and then
    // aborts — e.g. the user denies the subsequent per-tool
    // approval — the ceiling bonus + amends_this_run bumps must
    // NOT persist into the next turn. The replayable projection
    // drops the aborted turn whole (and with it the
    // ContractAmended event); the live driver's in-memory counter
    // must follow.
    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_amend_abort.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (_tap_tx, _tap_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(FsWriteTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        one_fs_write_then_end(),
    );

    let contract_val = budget_1_contract("amend then deny");
    let run_id = RunId::from("run_amend_abort".to_string());
    let turn_id = TurnId::from("t_amend_abort".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    // Responder: grant the FIRST approval (the budget extension),
    // deny every subsequent one (the per-tool fs_write). This models
    // a user who raised the ceiling then got cold feet on the actual
    // write.
    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let responder = tokio::spawn(async move {
        let mut first = true;
        while let Some(req) = approval_rx.recv().await {
            if first {
                first = false;
                let _ = req.responder.send(ApprovalResponse::Grant {
                    scope: ApprovalScope::Once,
                });
            } else {
                let _ = req.responder.send(ApprovalResponse::Deny);
            }
        }
    });

    let mut caps = CapabilityStore::new();
    let mut effects = EffectCounter {
        apply_local: 1,
        ..Default::default()
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
            run_started_tokio: None,
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
                vec![Message::user_text("grant then deny")],
            )
            .await
            .expect("abort path returns Ok");
    }
    drop(writer);
    responder.abort();

    // Counter snapshot after the aborted turn: pre-seeded 1 stays,
    // NO bonus from the amend, NO amends_this_run bump. If any of
    // these assertions failed, the pending→commit flush leaked into
    // a non-commit path.
    assert_eq!(
        effects.apply_local, 1,
        "pre-seed preserved; no post-amend effect recorded (tool denied)"
    );
    assert_eq!(
        effects.apply_local_ceiling_bonus, 0,
        "ceiling bonus MUST NOT persist from an aborted turn"
    );
    assert_eq!(
        effects.amends_this_run, 0,
        "per-run brake counter MUST NOT carry uncommitted grants"
    );
    assert_eq!(
        effects.amends_this_turn, 0,
        "per-turn brake counter reset on drive_turn entry; abort leaves it at 0"
    );
}
