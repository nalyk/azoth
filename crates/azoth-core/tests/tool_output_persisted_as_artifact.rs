//! Dogfood regression: tool output must be durably persisted to the
//! `ArtifactStore` so the replayable projection can reconstruct full
//! conversation fidelity after resume.
//!
//! Before this fix, `SessionEvent::ToolResult.content_artifact` was hardcoded
//! `None`, so every committed session held 23+ tool calls but the
//! `.azoth/artifacts/` directory stayed empty — on resume the model would
//! see "that" a tool ran but not "what it returned", producing context-
//! incomplete replays.

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
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn tool_result_event_carries_artifact_id_and_blob_lands_on_disk() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    std::fs::write(repo_root.join("needle.txt"), "azoth sentinel line\n").unwrap();

    let session_path = repo_root.join(".azoth/sessions/run_artifact.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();
    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();

    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(RepoSearchTool);

    let script = MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::from("tu_search".to_string()),
                    name: "repo_search".into(),
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

    let run_id = RunId::from("run_artifact".to_string());
    let turn_id = TurnId::from("t_artifact".to_string());
    // A separate ArtifactStore handle pointing at the same root — both
    // should converge on the same content-addressed blob.
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        ArtifactStore::open(&artifacts_root).unwrap(),
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
                vec![Message::user_text("find sentinel")],
            )
            .await
            .expect("turn drives cleanly");
    }
    drop(writer);

    // Read the replayable projection and find the ToolResult event.
    let reader = JsonlReader::open(&session_path);
    let events = reader.replayable().expect("replayable projection");
    let tool_result = events
        .iter()
        .find_map(|e| match &e.0 {
            SessionEvent::ToolResult {
                tool_use_id,
                content_artifact,
                is_error,
                ..
            } if tool_use_id.as_str() == "tu_search" => Some((content_artifact.clone(), *is_error)),
            _ => None,
        })
        .expect("ToolResult for tu_search present in replayable projection");

    let (artifact_id, is_error) = tool_result;
    assert!(!is_error, "tool did not error");
    let artifact_id =
        artifact_id.expect("content_artifact must be Some — the whole point of this fix");

    // The artifact blob must exist on disk and round-trip to the original
    // ContentBlock shape — that's what replay will need to re-hydrate the
    // conversation for a resuming turn.
    let bytes = artifacts
        .get(&artifact_id)
        .expect("artifact bytes readable from store");
    let restored: Vec<ContentBlock> =
        serde_json::from_slice(&bytes).expect("artifact JSON deserializes as Vec<ContentBlock>");
    assert!(
        !restored.is_empty(),
        "restored content should have at least one block"
    );
    // repo.search returns a JSON document as a single Text block.
    let has_text = restored
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("needle.txt")));
    assert!(
        has_text,
        "restored artifact should contain the search hit for needle.txt, got: {restored:#?}"
    );
}
