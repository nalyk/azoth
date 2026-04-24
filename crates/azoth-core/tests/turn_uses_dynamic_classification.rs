//! End-to-end proof that `Tool::effect_class_for` flows through the
//! turn driver's budget gate.
//!
//! Shape:
//!   - Contract caps `max_apply_local` at 1.
//!   - The per-run `EffectCounter` is pre-seeded AT the cap
//!     (`apply_local = 1`).
//!   - The model script emits a `bash` ToolUse with `command="ls"` —
//!     a bare read-only allowlist entry — followed by `EndTurn`.
//!
//! Pre-α behaviour: `bash` had static `effect_class() = ApplyLocal`;
//! the turn driver would short-circuit with `TurnAborted { reason:
//! RuntimeError, detail: "effect budget exhausted: apply_local 1/1" }`
//! before dispatching the tool.
//!
//! Post-α behaviour: `BashTool::effect_class_for` inspects the raw
//! command, recognises `"ls"` as read-only, returns `Observe`. The
//! budget gate doesn't trigger. The tool runs. The turn commits
//! with the counter still at 1 because `Observe` is not a budgeted
//! class.
//!
//! The test is sandbox-neutral: `AZOTH_SANDBOX=off` is set so the
//! bash tool runs via plain `tokio::process::Command` without
//! user-ns / Landlock / fuse-overlayfs — the gate we care about is
//! the BUDGET gate, not the mechanical-safety jail, and the WSL2
//! CI/dev environment has unstable Tier B behaviour documented in
//! auto-memory.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, ApprovalResponse, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::JsonlWriter;
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ApprovalScope, ContentBlock, Contract, EffectCounter, Message, ModelTurnResponse, RunId,
    SessionEvent, StopReason, ToolUseId, TurnId, Usage,
};
use azoth_core::tools::BashTool;
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn bash_ls_then_end() -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "ls" }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    ..Default::default()
                },
            },
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
    c.effect_budget.max_apply_local = 1;
    c
}

#[tokio::test]
async fn bare_bash_read_only_does_not_bump_budget() {
    // SAFETY: set BEFORE any parallel test reads it. `cargo test`
    // runs integration tests each in their own process, so env-var
    // bleed into sibling *_ctx.rs tests is impossible here — and
    // even if a dispatcher retry fired on a container path, `off`
    // is the safest default.
    std::env::set_var("AZOTH_SANDBOX", "off");

    let dir = tempdir().unwrap();
    let repo_root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let session_path = repo_root.join(".azoth/sessions/run_classifier.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<SessionEvent>();
    writer.set_tap(tap_tx);

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(BashTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        bash_ls_then_end(),
    );

    let contract_val = capped_contract("verify observe bash doesn't bump apply_local");
    let run_id = RunId::from("run_classifier".to_string());
    let turn_id = TurnId::from("t_classifier_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();

    // Pre-seed the counter AT the cap. Pre-α this would short-circuit
    // because BashTool's static class is ApplyLocal. Post-α the dynamic
    // classifier says `ls` is Observe, so the gate doesn't fire.
    let mut caps = CapabilityStore::new();
    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let _ = req.responder.send(ApprovalResponse::Grant {
                scope: ApprovalScope::Session,
            });
        }
    });
    let mut effects = EffectCounter {
        apply_local: 1,
        apply_repo: 0,
        network_reads: 0,
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
                vec![Message::user_text("ls the repo root")],
            )
            .await
            .expect("turn drives cleanly — no short-circuit, no abort");
    }
    drop(writer);

    let mut events = Vec::new();
    while let Ok(ev) = tap_rx.try_recv() {
        events.push(ev);
    }

    // Turn committed (not aborted).
    let committed = events
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnCommitted { .. }));
    assert!(
        committed,
        "TurnCommitted must be present — the Observe classification bypassed the budget gate. Events: {events:#?}"
    );

    // No budget abort.
    let aborted_on_budget = events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::TurnAborted { detail: Some(d), .. } if d.contains("effect budget exhausted")
        )
    });
    assert!(
        !aborted_on_budget,
        "Observe-classified bash must not trigger effect-budget abort"
    );

    // Counter unchanged — `Observe` is not a budgeted class, so the
    // increment path (`EffectClass::ApplyLocal` / `ApplyRepo` arms)
    // never fires.
    assert_eq!(
        effects.apply_local, 1,
        "Observe class should not bump apply_local counter"
    );

    // EffectRecord written with class=Observe — downstream replay
    // determinism depends on the recorded class matching what the
    // budget gate saw.
    let effect_class = events.iter().find_map(|e| match e {
        SessionEvent::EffectRecord { effect, .. } => Some(effect.class),
        _ => None,
    });
    assert_eq!(
        effect_class,
        Some(azoth_core::schemas::EffectClass::Observe),
        "EffectRecord.class must reflect the dynamic classifier decision"
    );
}
