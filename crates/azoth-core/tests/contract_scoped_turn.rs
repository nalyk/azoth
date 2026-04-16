//! TurnDriver honors a persisted Contract.
//!
//! 1. `accept_and_persist` appends a ContractAccepted event to the JSONL log.
//! 2. `JsonlReader::last_accepted_contract()` rehydrates the full contract.
//! 3. A `TurnDriver` constructed with `contract: Some(&rehydrated)` drives a
//!    one-turn mock script cleanly; the contract round-trips unchanged.
//! 4. A contract with `scope.max_turns = 0` aborts the next turn at the
//!    door without invoking the adapter, and writes a TurnAborted marker.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    AbortReason, ContentBlock, Message, ModelTurnResponse, RunId, SessionEvent, StopReason,
    TurnId, Usage,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn mock_end_turn_only() -> MockScript {
    MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 4,
                output_tokens: 2,
                ..Default::default()
            },
        }],
    }
}

#[tokio::test]
async fn driver_honors_persisted_contract_round_trip() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_ctr.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    // 1. Persist a contract via the real accept_and_persist path.
    let mut drafted = contract::draft("ship feature x");
    drafted.success_criteria.push("tests pass".into());
    let persisted = contract::accept_and_persist(
        &mut writer,
        drafted.clone(),
        "2026-04-15T00:00:00Z".to_string(),
    )
    .expect("persist ok");

    // 2. Rehydrate through the reader — this is what the worker does on
    // startup/resume to stash its local Option<Contract>.
    let rehydrated = JsonlReader::open(&session_path)
        .last_accepted_contract()
        .unwrap()
        .expect("contract present");
    assert_eq!(rehydrated, persisted);
    assert_eq!(rehydrated.goal, "ship feature x");

    // 3. Drive one turn with the contract threaded in. Script is EndTurn-only
    //    so the adapter runs exactly once and commits.
    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        mock_end_turn_only(),
    );
    let run_id = RunId::from("run_ctr".to_string());
    let turn_id = TurnId::from("t_ctr_1".to_string());
    let ctx = ExecutionContext {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        artifacts,
        cancellation: CancellationToken::new(),
        repo_root: repo_root.clone(),
    };
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
            contract: Some(&rehydrated),
            turns_completed: 0,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
        };
        // The driver observes the contract via the new field — assert the
        // round-trip before we even drive.
        assert_eq!(driver.contract.unwrap(), &persisted);
        driver
            .drive_turn(turn_id.clone(), "system".into(), vec![Message::user_text("go")])
            .await
            .expect("turn drives cleanly under contract");
    }
    drop(writer);

    // The replayable projection must still see a TurnCommitted — the
    // contract did not disrupt the commit path.
    let replay = JsonlReader::open(&session_path).replayable().unwrap();
    assert!(
        replay
            .iter()
            .any(|e| matches!(&e.0, SessionEvent::TurnCommitted { .. })),
        "expected TurnCommitted after contract-scoped turn"
    );
}

#[tokio::test]
async fn driver_aborts_when_contract_max_turns_reached() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_maxed.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    // Build a contract with max_turns = 1 and pretend one turn is already
    // completed — the next drive_turn call must abort at the door.
    let mut maxed = contract::draft("bounded run");
    maxed.success_criteria.push("done".into());
    maxed.scope.max_turns = Some(1);
    contract::lint(&maxed).expect("contract lints clean");

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    // Adapter script *would* emit EndTurn; if the guard works, it is never
    // consulted.
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        mock_end_turn_only(),
    );
    let run_id = RunId::from("run_maxed".to_string());
    let turn_id = TurnId::from("t_maxed_over".to_string());
    let ctx = ExecutionContext {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        artifacts,
        cancellation: CancellationToken::new(),
        repo_root: repo_root.clone(),
    };
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
            contract: Some(&maxed),
            // Already at the limit — the next call must abort.
            turns_completed: 1,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
        };
        let usage = driver
            .drive_turn(turn_id.clone(), "system".into(), vec![Message::user_text("go")])
            .await
            .expect("abort returns Ok with empty usage");
        assert_eq!(usage.output_tokens, 0, "no adapter call should have happened");
    }
    drop(writer);

    // The JSONL must contain a TurnAborted(TokenBudget) for this turn.
    let replay = JsonlReader::open(&session_path)
        .replayable()
        .expect("replay ok");
    // `replayable()` drops non-committed turns whole (CRIT-1). A max_turns
    // abort is non-committed, so it MUST NOT appear in the replay projection.
    assert!(
        !replay.iter().any(|e| matches!(
            &e.0,
            SessionEvent::TurnStarted { turn_id: tid, .. } if tid == &turn_id
        )),
        "aborted turn must not appear in replayable projection"
    );

    // But the forensic projection must see the TurnAborted marker with
    // AbortReason::TokenBudget.
    let forensic = JsonlReader::open(&session_path)
        .forensic()
        .expect("forensic ok");
    let saw_abort = forensic.iter().any(|e| matches!(
        &e.event,
        SessionEvent::TurnAborted {
            turn_id: tid,
            reason: AbortReason::TokenBudget,
            ..
        } if tid == &turn_id
    ));
    assert!(saw_abort, "expected TurnAborted(TokenBudget) for maxed turn");
}
