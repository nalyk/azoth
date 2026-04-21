//! Resume amnesia fix: `JsonlReader::rebuild_history` reconstructs the
//! cross-turn `Vec<Message>` a restarted worker needs so the model keeps
//! memory across sessions. Drives two committed turns into a fresh session,
//! drops the writer, reopens via the reader, and asserts the reconstructed
//! history is `[User, Assistant, User, Assistant]` with the original
//! user-input text preserved.

use std::path::Path;

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ContentBlock, Message, ModelTurnResponse, Role, RunId, StopReason, TurnId, Usage,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn scripted_text(reply: &str) -> MockScript {
    MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![ContentBlock::Text {
                text: reply.to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 7,
                output_tokens: 3,
                ..Default::default()
            },
        }],
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_text_turn(
    repo_root: &Path,
    session_path: &Path,
    artifacts: &ArtifactStore,
    run_id: &RunId,
    turn_id: TurnId,
    prior_history: Vec<Message>,
    user_text: &str,
    assistant_reply: &str,
    reopen: bool,
) -> Vec<Message> {
    let mut writer = if reopen {
        JsonlWriter::open_existing(session_path).expect("open_existing")
    } else {
        JsonlWriter::open(session_path).expect("fresh open")
    };

    let dispatcher = ToolDispatcher::new();
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        scripted_text(assistant_reply),
    );

    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts.clone(),
        repo_root.to_path_buf(),
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
        run_started_tokio: None,
        kernel: None,
        validators: &[],
        effects_consumed: &mut effects,
        evidence_collector: None,
        impact_validators: &[],
        diff_source: None,
    };

    let mut messages = prior_history;
    messages.push(Message::user_text(user_text));
    let outcome = driver
        .drive_turn(turn_id, "system".into(), messages.clone())
        .await
        .expect("turn commits");
    if let Some(assistant) = outcome.final_assistant {
        messages.push(Message {
            role: Role::Assistant,
            content: assistant,
        });
    }
    messages
}

#[tokio::test]
async fn resume_rebuilds_user_assistant_history_from_committed_turns() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_rebuild.jsonl");
    let artifacts = ArtifactStore::open(repo_root.join(".azoth/artifacts")).unwrap();
    let run_id = RunId::from("run_rebuild".to_string());

    // Turn 1: user says "hello" → assistant says "world".
    let after_t1 = drive_text_turn(
        &repo_root,
        &session_path,
        &artifacts,
        &run_id,
        TurnId::from("t_1".to_string()),
        Vec::new(),
        "hello",
        "world",
        false,
    )
    .await;
    assert_eq!(after_t1.len(), 2);

    // Turn 2: user says "again" with prior history carried forward →
    // assistant says "ok". Reopen through open_existing to exercise the
    // recovery path too.
    let after_t2 = drive_text_turn(
        &repo_root,
        &session_path,
        &artifacts,
        &run_id,
        TurnId::from("t_2".to_string()),
        after_t1,
        "again",
        "ok",
        true,
    )
    .await;
    assert_eq!(after_t2.len(), 4);

    // Simulate a restart: reopen the session read-only and rebuild history.
    let reader = JsonlReader::open(&session_path);
    let rebuilt = reader.rebuild_history().expect("rebuild_history");

    assert_eq!(
        rebuilt.len(),
        4,
        "expected [user, assistant, user, assistant], got {} messages",
        rebuilt.len()
    );

    // Role pattern must alternate starting with User.
    let roles: Vec<Role> = rebuilt.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::User, Role::Assistant]
    );

    // User texts preserved in order.
    let user_texts: Vec<String> = rebuilt
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["hello".to_string(), "again".to_string()]);

    // Assistant texts preserved in order.
    let asst_texts: Vec<String> = rebuilt
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(asst_texts, vec!["world".to_string(), "ok".to_string()]);
}

#[tokio::test]
async fn rebuild_history_skips_turns_without_rehydrate_fields() {
    // A synthesized session with a TurnCommitted written before v1.5
    // (missing user_input / final_assistant) must NOT push into the
    // rebuilt history — feeding half an exchange to the model is worse
    // than feeding none.
    use azoth_core::schemas::{CommitOutcome, ContractId, SessionEvent};

    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();
    let run_id = RunId::from("run_legacy".to_string());
    let contract_id = ContractId::from("ctr_legacy".to_string());
    let t1 = TurnId::from("t_legacy".to_string());

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id,
        timestamp: "2026-04-15T00:00:00Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: t1.clone(),
        run_id,
        parent_turn: None,
        timestamp: "2026-04-15T00:00:01Z".into(),
    })
    .unwrap();
    // Legacy TurnCommitted: no rehydrate fields.
    w.append(&SessionEvent::TurnCommitted {
        turn_id: t1,
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: None,
    })
    .unwrap();
    drop(w);

    let rebuilt = JsonlReader::open(&path).rebuild_history().unwrap();
    assert!(
        rebuilt.is_empty(),
        "legacy commits must be skipped, got {rebuilt:?}"
    );
}
