//! Resume flow: drive a turn into a fresh session, drop the writer, reopen
//! via `JsonlWriter::open_existing`, drive a second turn against the same
//! `run_id`, and assert the replayable projection holds *both* committed
//! turns. Exercises the contract that `open_existing` is safe to call on a
//! clean (fully-committed) session and leaves the file usable for further
//! appends.

use std::path::Path;

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ContentBlock, Message, ModelTurnResponse, RunId, SessionEvent, StopReason, ToolUseId, TurnId,
    Usage,
};
use azoth_core::tools::RepoSearchTool;
use azoth_core::turn::TurnDriver;
use tempfile::{tempdir, TempDir};
use tokio::sync::mpsc;

fn scripted_search(query: &str) -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "repo.search".into(),
                    input: serde_json::json!({ "q": query, "limit": 3 }),
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
                    text: format!("found {query}"),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 5,
                    ..Default::default()
                },
            },
        ],
    }
}

async fn drive_one_turn(
    repo_root: &Path,
    session_path: &Path,
    artifacts: &ArtifactStore,
    run_id: &RunId,
    turn_id: TurnId,
    user_text: &str,
    reopen: bool,
) {
    let mut writer = if reopen {
        JsonlWriter::open_existing(session_path).expect("open_existing succeeds on clean file")
    } else {
        JsonlWriter::open(session_path).expect("first open creates file")
    };

    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(RepoSearchTool);

    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        scripted_search(user_text),
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
        kernel: None,
        validators: &[],
        effects_consumed: &mut effects,
        evidence_collector: None,
    };

    driver
        .drive_turn(
            turn_id,
            "system".into(),
            vec![Message::user_text(user_text)],
        )
        .await
        .expect("turn drives cleanly");
}

fn fresh_repo() -> (TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    std::fs::write(repo_root.join("needle.txt"), "azoth sentinel line\n").unwrap();
    (dir, repo_root)
}

#[tokio::test]
async fn resume_replays_prior_commits() {
    let (_dir, repo_root) = fresh_repo();
    let session_path = repo_root.join(".azoth/sessions/run_resume.jsonl");
    let artifacts = ArtifactStore::open(repo_root.join(".azoth/artifacts")).unwrap();
    let run_id = RunId::from("run_resume".to_string());
    let t1 = TurnId::from("t_first".to_string());
    let t2 = TurnId::from("t_second".to_string());

    drive_one_turn(
        &repo_root,
        &session_path,
        &artifacts,
        &run_id,
        t1.clone(),
        "first",
        false,
    )
    .await;

    drive_one_turn(
        &repo_root,
        &session_path,
        &artifacts,
        &run_id,
        t2.clone(),
        "second",
        true,
    )
    .await;

    let reader = JsonlReader::open(&session_path);
    let replay = reader.replayable().unwrap();

    let committed: Vec<TurnId> = replay
        .iter()
        .filter_map(|e| match &e.0 {
            SessionEvent::TurnCommitted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        committed.contains(&t1) && committed.contains(&t2),
        "expected both committed turns, got: {committed:?}",
    );

    // Forensic count of TurnInterrupted must be zero — `open_existing` ran
    // recovery on a clean file, so no synthetic crash markers were written.
    let interrupted = reader
        .forensic()
        .unwrap()
        .iter()
        .filter(|f| matches!(&f.event, SessionEvent::TurnInterrupted { .. }))
        .count();
    assert_eq!(
        interrupted, 0,
        "clean reopen must not synthesize crash markers"
    );
}
