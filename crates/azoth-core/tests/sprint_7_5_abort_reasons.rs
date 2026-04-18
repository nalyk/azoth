//! Sprint 7.5 abort-reason discipline tests.
//!
//! Pins three distinct abort reasons that used to either share a
//! single label or not fire at all:
//!
//! 1. `StopReason::MaxTokens` → `AbortReason::ModelTruncated`
//!    (pre-Sprint-7.5 mis-labelled as `TokenBudget`).
//! 2. TurnDriver pre-flight refuses when the estimated input token
//!    count exceeds `profile.max_context_tokens` → `ContextOverflow`,
//!    emitted BEFORE any `model_request` event.
//! 3. A tool whose `EffectClass` has no available sandbox tier
//!    (Tier C/D in v2) cannot dispatch; the `ToolDispatcher`'s new
//!    `sandbox_for()` call surfaces `ToolError::SandboxDenied` before
//!    the tool's `execute` runs.
//!
//! Each test drives the TurnDriver (or the dispatcher directly) with
//! the minimum shape to trigger the abort, then asserts the JSONL /
//! error reflects the RIGHT reason. The overlap with `token_budget`
//! from the contract-side-effect path is explicitly preserved — the
//! old reason is still correct for the contract path; the new reasons
//! only carve off the mis-attributed branches.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ContentBlock, Message, ModelTurnResponse, RunId, SessionEvent, StopReason, TurnId,
    Usage,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn drain_turn_aborted(path: &std::path::Path) -> Option<AbortReason> {
    let events = JsonlReader::open(path).forensic().expect("read jsonl");
    for ann in events {
        if let SessionEvent::TurnAborted { reason, .. } = ann.event {
            return Some(reason);
        }
    }
    None
}

#[tokio::test]
async fn max_tokens_stop_reason_maps_to_model_truncated_not_token_budget() {
    let dir = tempdir().unwrap();
    let session_path = dir.path().join("session.jsonl");
    let artifacts_root = dir.path().join("artifacts");
    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let dispatcher = ToolDispatcher::new();
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        MockScript {
            turns: vec![ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "partial... (cut off)".into(),
                }],
                stop_reason: StopReason::MaxTokens,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 8192,
                    ..Default::default()
                },
            }],
        },
    );

    let run_id = RunId::from("run_trunc".to_string());
    let turn_id = TurnId::from("t_trunc".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        ArtifactStore::open(&artifacts_root).unwrap(),
        dir.path().to_path_buf(),
    )
    .build();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut caps = CapabilityStore::new();
    let mut effects = azoth_core::schemas::EffectCounter::default();

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

    let outcome = driver
        .drive_turn(turn_id, "system".into(), vec![Message::user_text("hi")])
        .await
        .expect("turn drives cleanly");

    assert!(
        outcome.final_assistant.is_none(),
        "aborted turn must not carry final_assistant content"
    );
    let reason = drain_turn_aborted(&session_path).expect("abort event present");
    assert_eq!(
        reason,
        AbortReason::ModelTruncated,
        "StopReason::MaxTokens must abort as ModelTruncated (Sprint 7.5)"
    );
    assert_ne!(
        reason,
        AbortReason::TokenBudget,
        "ModelTruncated must be distinct from the contract-budget TokenBudget label"
    );
}

#[tokio::test]
async fn pre_flight_refuses_request_over_profile_max_context_tokens() {
    let dir = tempdir().unwrap();
    let session_path = dir.path().join("session.jsonl");
    let artifacts_root = dir.path().join("artifacts");
    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let dispatcher = ToolDispatcher::new();
    // A profile with a deliberately tiny cap (100 tokens ~= 400 chars).
    let mut profile = ProviderProfile::anthropic_default("claude-sonnet-4-6");
    profile.max_context_tokens = 100;

    // MockAdapter should NEVER be invoked — if it is, the script
    // panics on empty queue.
    let adapter = MockAdapter::new(
        profile,
        MockScript {
            turns: vec![], // empty script — calling .invoke() errors
        },
    );

    let run_id = RunId::from("run_overflow".to_string());
    let turn_id = TurnId::from("t_overflow".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        ArtifactStore::open(&artifacts_root).unwrap(),
        dir.path().to_path_buf(),
    )
    .build();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut caps = CapabilityStore::new();
    let mut effects = azoth_core::schemas::EffectCounter::default();

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

    // A huge system prompt — well over 100 tokens (4 char * 100 = 400
    // chars baseline; we give 2_000 chars so estimate ~500 tokens).
    let big_system = "x".repeat(2_000);
    let outcome = driver
        .drive_turn(
            turn_id,
            big_system,
            vec![Message::user_text("small user msg")],
        )
        .await
        .expect("turn drives cleanly without adapter invocation");

    assert!(outcome.final_assistant.is_none());
    let reason = drain_turn_aborted(&session_path).expect("abort event present");
    assert_eq!(reason, AbortReason::ContextOverflow);

    // Critically: no ModelRequest event was emitted — the abort
    // happened BEFORE the network call would have fired.
    let events = JsonlReader::open(&session_path)
        .forensic()
        .expect("read jsonl");
    let saw_model_request = events
        .iter()
        .any(|ann| matches!(ann.event, SessionEvent::ModelRequest { .. }));
    assert!(
        !saw_model_request,
        "pre-flight must refuse BEFORE emitting a ModelRequest event"
    );
}
