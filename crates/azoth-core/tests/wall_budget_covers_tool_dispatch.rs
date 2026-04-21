//! PR #18 round 6 — codex P1 (comment 3114300370) regression.
//!
//! Pre-fix the wall-clock deadline only raced the per-invoke `select!`,
//! so once control entered the `StopReason::ToolUse` arm (tool dispatch,
//! approval wait, validator diff, impact validators) the deadline
//! effectively paused. A `bash` command, slow approval, or large-repo
//! diff could run far past `scope.max_wall_secs` before `TimeExceeded`
//! was checked again on the next invoke iteration — violating the
//! documented session-wide wall budget.
//!
//! Fix: every long-await site now routes through `race_wall_deadline`
//! plus `TurnDriver::record_wall_timeout_abort`. This test exercises
//! the tool-dispatch branch specifically (the canonical failure case
//! codex named) via a `SleepyTool` that sleeps 3600s. Without the fix
//! the turn would ride the sleep to completion, emit
//! `StopReason::EndTurn`, and commit cleanly under a 30-second budget.
//! With the fix it aborts at T+30s with `AbortReason::TimeExceeded`.

use async_trait::async_trait;
use azoth_core::adapter::{AdapterError, ProviderAdapter, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, Tool, ToolDispatcher, ToolError};
use azoth_core::schemas::{
    AbortReason, ContentBlock, EffectClass, ModelTurnRequest, ModelTurnResponse, RunId,
    SessionEvent, StopReason, StreamEvent, ToolUseId, TurnId, Usage,
};
use azoth_core::turn::TurnDriver;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::mpsc;

/// Observe-class tool whose `execute` sleeps far longer than any test
/// wall budget. Under `tokio::time::pause()` the sleep is virtual, so
/// the test is still fast — but the deadline race has to actually fire
/// to cut it short. Observe class avoids the approval gate so the test
/// stays focused on the tool-dispatch race.
struct SleepyTool;

#[derive(Debug, Deserialize)]
struct SleepyInput {
    #[serde(default)]
    _ignored: Option<String>,
}

#[derive(Debug, Serialize)]
struct SleepyOutput {
    completed: bool,
}

#[async_trait]
impl Tool for SleepyTool {
    type Input = SleepyInput;
    type Output = SleepyOutput;

    fn name(&self) -> &'static str {
        "sleepy"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Observe
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _input: Self::Input,
        _ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(SleepyOutput { completed: true })
    }
}

/// Adapter whose first invoke returns `StopReason::ToolUse` with a
/// `ToolUse{ name: "sleepy" }` block; if a second invoke ever lands
/// (post-fix it shouldn't — the deadline aborts the turn during tool
/// dispatch), the turn commits cleanly so pre-fix behaviour is still
/// observable as "committed after ~3600s virtual time, no TimeExceeded".
struct ToolThenCommitAdapter {
    profile: ProviderProfile,
    call: AtomicUsize,
}

#[async_trait]
impl ProviderAdapter for ToolThenCommitAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        _sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Ok(ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::from("tu_sleep_1".to_string()),
                    name: "sleepy".into(),
                    input: json!({}),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            })
        } else {
            Ok(ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    async fn count_tokens(
        &self,
        _req: &ModelTurnRequest,
    ) -> Result<azoth_core::adapter::TokenCount, AdapterError> {
        Ok(azoth_core::adapter::TokenCount { input_tokens: 0 })
    }
}

#[tokio::test(start_paused = true)]
async fn wall_budget_fires_during_tool_dispatch() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_tool_wall.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    // 30-second budget; SleepyTool sleeps 3600s. Pre-fix the tool runs
    // to completion (virtual 3600s) and the turn commits. Post-fix the
    // race fires at T+30s, the turn aborts with TimeExceeded.
    let mut drafted = contract::draft("tool dispatch wall budget");
    drafted.scope.max_wall_secs = Some(30);
    drafted.success_criteria.push("ship round 6".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-21T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(SleepyTool);
    let adapter = ToolThenCommitAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        call: AtomicUsize::new(0),
    };
    let run_id = RunId::from("run_tool_wall".to_string());
    let turn_id = TurnId::from("t_tool_wall_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();
    let mut caps = CapabilityStore::new();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut effects = azoth_core::schemas::EffectCounter::default();

    let test_started = tokio::time::Instant::now();
    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(&persisted),
            turns_completed: 0,
            run_started_tokio: None,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        let outcome = driver
            .drive_turn(turn_id.clone(), "sys".to_string(), vec![])
            .await
            .unwrap();
        assert!(
            outcome.final_assistant.is_none(),
            "turn should have aborted mid-dispatch, not committed"
        );
    }
    let elapsed = test_started.elapsed();

    // Post-fix: elapsed is ~30s (budget). Pre-fix: ~3600s (tool ran to
    // completion then the inner-select fired on the next invoke). The
    // ≤90s bound catches the pre-fix ~3600s virtual elapsed loudly.
    assert!(
        elapsed.as_secs() <= 90,
        "drive_turn should abort at ~30s budget, got {elapsed:?} — pre-fix signature is ~3600s"
    );

    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();

    // There must be a TurnAborted{TimeExceeded} marker; NO TurnCommitted.
    let saw_time_exceeded = forensic.iter().any(|fev| {
        matches!(
            &fev.event,
            SessionEvent::TurnAborted {
                reason: AbortReason::TimeExceeded,
                detail: Some(d),
                ..
            } if d.contains("wall-clock budget 30s")
        )
    });
    assert!(
        saw_time_exceeded,
        "expected TurnAborted{{TimeExceeded}} during tool dispatch, got events: {:?}",
        forensic.iter().map(|f| &f.event).collect::<Vec<_>>()
    );
    let saw_committed = forensic
        .iter()
        .any(|fev| matches!(&fev.event, SessionEvent::TurnCommitted { .. }));
    assert!(
        !saw_committed,
        "turn must not commit when deadline fired mid-dispatch"
    );
}

/// Approval-wait regression for codex P1 3114300370.
///
/// Pre-fix a user leaving the approval modal open past `max_wall_secs`
/// would pause the deadline — `resp_rx.await` is unbounded and was not
/// raced against anything. Post-fix the deadline fires at the budget,
/// aborts the turn with `AbortReason::TimeExceeded`, and the
/// ApprovalRequest marker stays in forensic view (request was made,
/// deadline fired, turn stopped).
///
/// Setup: approval bridge whose receiver never consumes messages nor
/// responds on `resp_tx`. `approval_bridge.send` succeeds (channel has
/// capacity), but the resulting ApprovalRequestMsg sits in the channel
/// and no one sends a Grant/Deny, so `resp_rx.await` hangs forever
/// pre-fix.
///
/// An `ApplyLocal` effect forces the approval path (cf. ApprovalPolicyV1).
struct ApplyLocalTool;

#[async_trait]
impl Tool for ApplyLocalTool {
    type Input = SleepyInput;
    type Output = SleepyOutput;

    fn name(&self) -> &'static str {
        "apply_probe"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::ApplyLocal
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _input: Self::Input,
        _ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        Ok(SleepyOutput { completed: true })
    }
}

struct ApplyToolThenCommitAdapter {
    profile: ProviderProfile,
    call: AtomicUsize,
}

#[async_trait]
impl ProviderAdapter for ApplyToolThenCommitAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        _sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Ok(ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::from("tu_apply_1".to_string()),
                    name: "apply_probe".into(),
                    input: json!({"path": "/tmp/probe"}),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            })
        } else {
            Ok(ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    async fn count_tokens(
        &self,
        _req: &ModelTurnRequest,
    ) -> Result<azoth_core::adapter::TokenCount, AdapterError> {
        Ok(azoth_core::adapter::TokenCount { input_tokens: 0 })
    }
}

#[tokio::test(start_paused = true)]
async fn wall_budget_fires_during_approval_wait() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_approval_wall.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let mut drafted = contract::draft("approval wait wall budget");
    drafted.scope.max_wall_secs = Some(30);
    drafted.success_criteria.push("ship round 6".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-21T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.register(ApplyLocalTool);
    let adapter = ApplyToolThenCommitAdapter {
        profile: ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        call: AtomicUsize::new(0),
    };
    let run_id = RunId::from("run_approval_wall".to_string());
    let turn_id = TurnId::from("t_approval_wall_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();
    let mut caps = CapabilityStore::new();
    // Receiver is held but never drained — the ApprovalRequestMsg sits
    // in the channel, `resp_tx` is never used, `resp_rx.await` hangs
    // indefinitely pre-fix.
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut effects = azoth_core::schemas::EffectCounter::default();

    let test_started = tokio::time::Instant::now();
    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(&persisted),
            turns_completed: 0,
            run_started_tokio: None,
            kernel: None,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };

        let outcome = tokio::time::timeout(
            Duration::from_secs(120),
            driver.drive_turn(turn_id.clone(), "sys".to_string(), vec![]),
        )
        .await
        .expect("drive_turn must return within 120s virtual — pre-fix would hang forever")
        .unwrap();
        assert!(
            outcome.final_assistant.is_none(),
            "turn should have aborted on deadline, not committed"
        );
    }
    let elapsed = test_started.elapsed();
    assert!(
        elapsed.as_secs() <= 60,
        "drive_turn should abort at ~30s budget, got {elapsed:?}"
    );

    drop(writer);
    let reader = JsonlReader::open(&session_path);
    let forensic = reader.forensic().unwrap();
    let saw_time_exceeded = forensic.iter().any(|fev| {
        matches!(
            &fev.event,
            SessionEvent::TurnAborted {
                reason: AbortReason::TimeExceeded,
                detail: Some(d),
                ..
            } if d.contains("wall-clock budget 30s")
        )
    });
    assert!(
        saw_time_exceeded,
        "expected TurnAborted{{TimeExceeded}} during approval wait"
    );
    // ApprovalRequest should be present (the request was made) but no
    // ApprovalGranted/ApprovalDenied marker (deadline fired, not user).
    let saw_approval_request = forensic
        .iter()
        .any(|fev| matches!(&fev.event, SessionEvent::ApprovalRequest { .. }));
    assert!(
        saw_approval_request,
        "ApprovalRequest should remain in forensic view — the request was made \
         before the deadline fired"
    );
}

/// Overflow-protection regression for gemini MED 3114298729. A contract
/// declaring an absurd wall budget (near `u64::MAX` seconds) would pre-fix
/// panic on `deadline_anchor + Duration::from_secs(secs)` inside
/// `drive_turn`. Post-fix `checked_add` falls through to `None`, which
/// disarms the deadline — the turn runs normally without a budget race.
#[tokio::test(start_paused = true)]
async fn deadline_anchor_overflow_does_not_panic() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let session_path = repo_root.join(".azoth/sessions/run_overflow.jsonl");
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let mut writer = JsonlWriter::open(&session_path).unwrap();

    let mut drafted = contract::draft("deadline overflow");
    // `u64::MAX` seconds is larger than tokio::time::Instant can
    // represent; `checked_add` returns `None` and we proceed without a
    // deadline instead of panicking.
    drafted.scope.max_wall_secs = Some(u64::MAX);
    drafted
        .success_criteria
        .push("survive absurd budget".into());
    let persisted =
        contract::accept_and_persist(&mut writer, drafted, "2026-04-21T00:00:00Z".to_string())
            .unwrap();

    let artifacts = ArtifactStore::open(&artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let script = azoth_core::adapter::MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![ContentBlock::Text { text: "ok".into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }],
    };
    let adapter = azoth_core::adapter::MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        script,
    );
    let run_id = RunId::from("run_overflow".to_string());
    let turn_id = TurnId::from("t_overflow_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.clone(),
    )
    .build();
    let mut caps = CapabilityStore::new();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
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
        turns_completed: 0,
        run_started_tokio: None,
        kernel: None,
        validators: &[],
        effects_consumed: &mut effects,
        evidence_collector: None,
        impact_validators: &[],
        diff_source: None,
    };

    // Pre-fix: panics on `anchor + Duration::from_secs(u64::MAX)` inside
    // drive_turn's deadline computation. Post-fix: completes normally
    // because `checked_add` returns None (no deadline armed).
    let outcome = driver
        .drive_turn(turn_id, "sys".to_string(), vec![])
        .await
        .expect("drive_turn must not panic on absurd max_wall_secs");
    assert!(
        outcome.final_assistant.is_some(),
        "turn without armed deadline should commit normally"
    );
}
