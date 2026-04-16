//! Cross-turn memory contract: `drive_turn` returns the final assistant
//! content from the `EndTurn` / `StopSequence` response so the caller can
//! fold it back into the next turn's `messages`. Non-committing outcomes
//! return `None` — callers must never feed aborted turns into subsequent
//! conversations.
//!
//! Fixes the amnesia bug observed in dogfood `run_f465299c1a5e`: the TUI
//! worker had a user-only `history` buffer and never captured assistant
//! responses, so turn 4 said "I don't have any source code provided yet"
//! 90 seconds after turn 3 analyzed the entire codebase.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::JsonlWriter;
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ContentBlock, Message, ModelTurnResponse, RunId, StopReason, TurnId, Usage,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn end_turn_outcome_carries_final_assistant_content() {
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
                    text: "the answer is fact-42".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 7,
                    ..Default::default()
                },
            }],
        },
    );

    let run_id = RunId::from("run_mem".to_string());
    let turn_id = TurnId::from("t_mem".to_string());
    let ctx = ExecutionContext {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        artifacts: ArtifactStore::open(&artifacts_root).unwrap(),
        cancellation: CancellationToken::new(),
        repo_root: dir.path().to_path_buf(),
    };
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
    };
    let outcome = driver
        .drive_turn(
            turn_id.clone(),
            "system".into(),
            vec![Message::user_text("what's the answer?")],
        )
        .await
        .expect("turn drives cleanly");

    assert_eq!(outcome.usage.output_tokens, 7);
    let final_blocks = outcome
        .final_assistant
        .expect("EndTurn outcome must carry final_assistant content");
    let has_fact = final_blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("fact-42")));
    assert!(
        has_fact,
        "final_assistant should contain the model's response text, got {final_blocks:#?}"
    );
}

#[tokio::test]
async fn contract_max_turns_abort_returns_none_final_assistant() {
    use azoth_core::contract;
    use azoth_core::schemas::{EffectBudget, Scope};

    let dir = tempdir().unwrap();
    let session_path = dir.path().join("session.jsonl");
    let artifacts_root = dir.path().join("artifacts");
    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let dispatcher = ToolDispatcher::new();

    // Adapter with ONE scripted turn, but we'll set max_turns=0 so the
    // driver aborts before calling invoke.
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        MockScript {
            turns: vec![ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "should never be seen".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }],
        },
    );

    let mut draft = contract::draft("test goal".to_string());
    draft.success_criteria = vec!["any".into()];
    draft.scope = Scope {
        max_turns: Some(1),
        ..draft.scope
    };
    draft.effect_budget = EffectBudget::default();
    let persisted = contract::accept_and_persist(&mut writer, draft, "2026-04-16T00:00:00Z")
        .expect("contract persists");

    let run_id = RunId::from("run_abort".to_string());
    let turn_id = TurnId::from("t_abort".to_string());
    let ctx = ExecutionContext {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        artifacts: ArtifactStore::open(&artifacts_root).unwrap(),
        cancellation: CancellationToken::new(),
        repo_root: dir.path().to_path_buf(),
    };
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut caps = CapabilityStore::new();
    let mut effects = azoth_core::schemas::EffectCounter::default();

    let mut driver = TurnDriver {
        run_id,
        adapter: &adapter,
        dispatcher: &dispatcher,
        writer: &mut writer,
        ctx: &ctx,
        capabilities: &mut caps,
        approval_bridge: approval_tx,
        contract: Some(&persisted),
        // Already at the cap → drive_turn aborts before calling invoke.
        turns_completed: 1,
        kernel: None,
        validators: &[],
        effects_consumed: &mut effects,
        evidence_collector: None,
    };
    let outcome = driver
        .drive_turn(turn_id, "system".into(), vec![Message::user_text("go")])
        .await
        .expect("abort returns Ok");

    assert!(
        outcome.final_assistant.is_none(),
        "aborted turns must NOT carry final_assistant — would leak into cross-turn memory"
    );
}
