//! TurnDriver: the state machine that drives one turn end-to-end.
//!
//! plan → compile → invoke → dispatch → validate → commit/abort

use crate::adapter::ProviderAdapter;
use crate::authority::{
    mint_from_approval, ApprovalPolicyV1, ApprovalRequestMsg, ApprovalResponse, AuthorityDecision,
    AuthorityEngine, CapabilityStore, Origin, Tainted,
};
use crate::context::{ContextKernel, EvidenceCollector, KernelError, StepInput};
use crate::event_store::JsonlWriter;
use crate::execution::{ExecutionContext, ToolDispatcher, ToolError};
use crate::impact::DiffSource;
use crate::schemas::{
    AbortReason, CheckpointId, CommitOutcome, ContentBlock, Contract, Diff, EffectClass,
    EffectCounter, EffectRecord, EffectRecordId, Message, ModelTurnRequest, RequestMetadata, Role,
    RunId, SessionEvent, StopReason, StreamEvent, ToolDefinition, TurnId, Usage, ValidatorStatus,
};
use crate::telemetry;
use crate::validators::{ImpactValidator, Validator};
use futures::future::join_all;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Result of a single `drive_turn` invocation.
///
/// `final_assistant` carries the content blocks of the last model response
/// (the one that stopped with `EndTurn`/`StopSequence`), so the caller can
/// feed them back as cross-turn memory on the next call. It is deliberately
/// `None` for any non-committing outcome — the caller should never push
/// content from an aborted or interrupted turn into a subsequent conversation.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub usage: Usage,
    pub final_assistant: Option<Vec<ContentBlock>>,
}

impl TurnOutcome {
    fn aborted(usage: Usage) -> Self {
        Self {
            usage,
            final_assistant: None,
        }
    }

    fn committed(usage: Usage, final_assistant: Vec<ContentBlock>) -> Self {
        Self {
            usage,
            final_assistant: Some(final_assistant),
        }
    }
}

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("adapter: {0}")]
    Adapter(#[from] crate::adapter::AdapterError),
    #[error("tool: {0}")]
    Tool(#[from] ToolError),
    #[error("context packet budget exceeded")]
    Budget,
    #[error("kernel: {0}")]
    Kernel(#[from] KernelError),
}

pub struct TurnDriver<'a> {
    pub run_id: RunId,
    pub adapter: &'a dyn ProviderAdapter,
    pub dispatcher: &'a ToolDispatcher,
    pub writer: &'a mut JsonlWriter,
    pub ctx: &'a ExecutionContext,
    pub capabilities: &'a mut CapabilityStore,
    pub approval_bridge: mpsc::Sender<ApprovalRequestMsg>,
    /// Persisted run contract, if one has been accepted. When `Some`, the
    /// driver enforces `scope.max_turns` as an abort guard at the start of
    /// every `drive_turn` call. When `None`, behavior is byte-for-byte
    /// identical to the pre-contract driver.
    pub contract: Option<&'a Contract>,
    /// Count of turns already committed in this session prior to this call.
    /// The caller owns this counter and increments it after a successful
    /// `drive_turn`; the driver compares it against `contract.scope.max_turns`.
    pub turns_completed: u32,
    /// Optional `ContextKernel` used to compile a per-turn `ContextPacket`.
    /// When both `contract` and `kernel` are `Some`, the driver invokes
    /// `kernel.compile` once at the start of every `drive_turn` and shadows
    /// the caller-supplied `system` string with a constitution header derived
    /// from `packet.constitution_lane` — binding the contract digest,
    /// policy version, and tool-schemas digest into `ModelRequest.request_digest`.
    /// When either is `None`, behavior is byte-for-byte identical to the
    /// pre-kernel driver.
    pub kernel: Option<&'a ContextKernel<'a>>,
    /// Deterministic turn-exit validators. Each is consulted on the
    /// `EndTurn` / `StopSequence` branch immediately before `TurnCommitted`
    /// is written. Every validator's report emits a `ValidatorResult` event.
    /// If any validator returns `ValidatorStatus::Fail`, the driver writes
    /// `TurnAborted { reason: ValidatorFail }` and does NOT write a
    /// `Checkpoint` or `TurnCommitted`. If all pass, a fresh `Checkpoint`
    /// event is appended before `TurnCommitted`. Behavior is byte-for-byte
    /// identical to the pre-validators driver when this slice is empty
    /// or when `contract` is `None` (validators need a contract to check).
    pub validators: &'a [&'a dyn Validator],
    /// Cumulative per-run effect tally, compared against
    /// `contract.effect_budget` before every tool dispatch. When `contract`
    /// is `Some` and a tool's `EffectClass` maps to a budgeted counter that
    /// has already reached its cap, the driver records a `TurnAborted`
    /// with reason `RuntimeError` and detail `effect budget exhausted:
    /// <class> <used>/<max>` — mirroring the existing `NotAvailable`
    /// short-circuit path. The counter is bumped after every successful
    /// `EffectRecord` append. When `contract` is `None`, the counter is
    /// never read or written, so the pre-contract byte shape is preserved.
    pub effects_consumed: &'a mut EffectCounter,
    /// Optional evidence collector. When both `contract` and `kernel` are
    /// `Some`, the driver calls `collector.collect(contract.goal, 20)` to
    /// populate `StepInput.evidence`. When `None`, evidence stays
    /// `Vec::new()` — byte-for-byte compatible with the pre-evidence driver.
    pub evidence_collector: Option<&'a dyn EvidenceCollector>,
    /// Async, turn-scoped impact validators (Sprint 5, TDAD). Each is
    /// called at the `EndTurn` / `StopSequence` branch *after* the
    /// classical `validators` slice passes, with the current `Diff`
    /// from `diff_source`. Every call writes a `SessionEvent::
    /// ImpactComputed` (selector + plan + selected tests) plus a
    /// `SessionEvent::ValidatorResult`. A `Fail` verdict from any
    /// impact validator aborts the turn under `AbortReason::
    /// ValidatorFail` — identical wire shape to a classical
    /// validator failure. Empty slice + `None` diff_source = no-op,
    /// byte-for-byte compatible with the pre-Sprint-5 driver.
    pub impact_validators: &'a [&'a dyn ImpactValidator],
    /// Optional `DiffSource` queried once at the validate phase to
    /// materialise the `Diff` handed to every `impact_validators`
    /// entry. When `None`, impact validators observe
    /// `Diff::empty()`; they are free to treat that as a no-op
    /// (selectors keyed on changed paths emit an empty plan, which
    /// counts as `Pass`). Shell-based sources (`git status
    /// --porcelain`) live in `azoth-repo` so `azoth-core` stays
    /// dep-thin.
    pub diff_source: Option<&'a dyn DiffSource>,
}

impl<'a> TurnDriver<'a> {
    /// Append a `TurnAborted` marker with the given reason and detail.
    fn record_abort(
        &mut self,
        turn_id: &TurnId,
        reason: AbortReason,
        detail: Option<String>,
        usage: Usage,
    ) -> Result<(), std::io::Error> {
        let reason_label = format!("{reason:?}");
        self.writer.append(&SessionEvent::TurnAborted {
            turn_id: turn_id.clone(),
            reason,
            detail,
            usage,
        })?;
        telemetry::emit_turn_aborted(&self.run_id.0, &turn_id.0, &reason_label);
        Ok(())
    }

    /// Drive a single turn. `messages` is the conversation tail the Context
    /// Kernel has already compiled for this step; the driver appends assistant
    /// + tool_result blocks as it goes.
    ///
    /// The returned `TurnOutcome::final_assistant` is populated with the
    /// assistant content blocks from the final `EndTurn` / `StopSequence`
    /// model response, so the caller can fold them back into the next turn's
    /// `messages` argument and give the model cross-turn memory. It stays
    /// `None` for any non-committing outcome (aborted / interrupted /
    /// validator-failed).
    pub async fn drive_turn(
        &mut self,
        turn_id: TurnId,
        system: String,
        mut messages: Vec<Message>,
    ) -> Result<TurnOutcome, TurnError> {
        // Capture the triggering user input before tool-loop pushes any
        // tool_result User messages. Persisted on TurnCommitted so a
        // restarted worker can rebuild the full history from JSONL alone.
        let user_input_content: Option<Vec<ContentBlock>> = messages
            .last()
            .filter(|m| matches!(m.role, Role::User))
            .map(|m| m.content.clone());
        // Contract-scoped guard: refuse to even open the turn if the
        // persisted contract has set a max_turns and we are at/over it.
        if let Some(c) = self.contract {
            if let Some(max) = c.scope.max_turns {
                if self.turns_completed >= max {
                    self.writer.append(&SessionEvent::TurnStarted {
                        turn_id: turn_id.clone(),
                        run_id: self.run_id.clone(),
                        parent_turn: None,
                        timestamp: now_iso(),
                    })?;
                    self.record_abort(
                        &turn_id,
                        AbortReason::TokenBudget,
                        Some(format!(
                            "contract max_turns {} reached (completed={})",
                            max, self.turns_completed
                        )),
                        Usage::default(),
                    )?;
                    return Ok(TurnOutcome::aborted(Usage::default()));
                }
            }
        }

        let tools: Vec<ToolDefinition> = self.dispatcher.schemas();

        // When both a contract and a kernel are attached, compile a
        // `ContextPacket` and shadow `system` with a constitution header so
        // the contract digest + policy version + tool-schemas digest flow
        // into the `ModelRequest.request_digest`. Budget overflow maps to a
        // clean TokenBudget abort; any other kernel error bubbles as
        // `TurnError::Kernel`.
        let system = match (self.contract, self.kernel) {
            (Some(contract), Some(kernel)) => {
                let tool_schemas_digest = digest(&tools);

                // Collect evidence when a collector is wired in.
                let evidence = match self.evidence_collector {
                    Some(collector) => match collector.collect(&contract.goal, 20).await {
                        Ok(items) => items,
                        Err(e) => {
                            eprintln!("[azoth] evidence collection failed: {e}");
                            Vec::new()
                        }
                    },
                    None => Vec::new(),
                };
                let evidence_count = evidence.len();

                let input = StepInput {
                    contract,
                    turn_id: turn_id.clone(),
                    step_goal: contract.goal.clone(),
                    rubric: contract.success_criteria.clone(),
                    working_set: Vec::new(),
                    evidence,
                    last_checkpoint: None,
                    system_prompt: system,
                    tool_schemas_digest,
                };
                match kernel.compile(input) {
                    Ok(packet) => {
                        // Emit a ContextPacket event so the TUI can show
                        // the last compiled packet via `/context`.
                        let _ = self.writer.append(&SessionEvent::ContextPacket {
                            turn_id: turn_id.clone(),
                            packet_id: packet.id.clone(),
                            packet_digest: packet.digest.clone(),
                        });
                        telemetry::emit_context_compiled(
                            &self.run_id.0,
                            &turn_id.0,
                            0,
                            evidence_count,
                        );
                        let lane = &packet.constitution_lane;
                        format!(
                            "[azoth.constitution]\n\
                             contract_digest={}\n\
                             policy_version={}\n\
                             tool_schemas_digest={}\n\n\
                             {}",
                            lane.contract_digest,
                            lane.policy_version,
                            lane.tool_schemas_digest,
                            lane.system_prompt,
                        )
                    }
                    Err(KernelError::OverBudget(used, max)) => {
                        self.writer.append(&SessionEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                            run_id: self.run_id.clone(),
                            parent_turn: None,
                            timestamp: now_iso(),
                        })?;
                        self.record_abort(
                            &turn_id,
                            AbortReason::TokenBudget,
                            Some(format!("context packet over budget: {used} > {max}")),
                            Usage::default(),
                        )?;
                        return Ok(TurnOutcome::aborted(Usage::default()));
                    }
                    Err(e) => return Err(TurnError::Kernel(e)),
                }
            }
            _ => system,
        };

        self.writer.append(&SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: self.run_id.clone(),
            parent_turn: None,
            timestamp: now_iso(),
        })?;
        telemetry::emit_turn_started(&self.run_id.0, &turn_id.0);

        let mut total_usage = Usage::default();

        loop {
            if self.ctx.cancelled() {
                self.writer.append(&SessionEvent::TurnInterrupted {
                    turn_id: turn_id.clone(),
                    reason: AbortReason::UserCancel,
                    partial_usage: Default::default(),
                })?;
                telemetry::emit_turn_interrupted(&self.run_id.0, &turn_id.0, "user_cancel");
                return Ok(TurnOutcome::aborted(total_usage));
            }

            let req = ModelTurnRequest {
                system: system.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: 2048,
                cache_hints: Default::default(),
                metadata: RequestMetadata {
                    run_id: self.run_id.0.clone(),
                    turn_id: turn_id.0.clone(),
                },
            };

            self.writer.append(&SessionEvent::ModelRequest {
                turn_id: turn_id.clone(),
                request_digest: digest(&req),
                profile_id: self.adapter.profile().id.clone(),
            })?;
            telemetry::emit_model_request(&self.run_id.0, &turn_id.0, &self.adapter.profile().id);

            // Bounded sink for stream events. The driver must drain this
            // concurrently with `invoke` — otherwise an adapter that emits
            // more than 64 events (e.g. a long response) would deadlock at
            // channel capacity. The drain branch also turns the bounded
            // channel into useful backpressure: the adapter's `send().await`
            // is immediately unblocked as we pull events here.
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
            let invoke_fut = self.adapter.invoke(req, tx);
            tokio::pin!(invoke_fut);

            let invoke_result = loop {
                tokio::select! {
                    biased;
                    // Cancellation first so a mid-stream Ctrl+C is never
                    // starved by a flood of deltas — matches the TUI's
                    // top-level `biased` select discipline (MED-3 fix).
                    _ = self.ctx.cancellation.wait_cancelled() => {
                        self.writer.append(&SessionEvent::TurnInterrupted {
                            turn_id: turn_id.clone(),
                            reason: AbortReason::UserCancel,
                            partial_usage: crate::schemas::UsageDelta {
                                input_tokens: total_usage.input_tokens,
                                output_tokens: total_usage.output_tokens,
                            },
                        })?;
                        telemetry::emit_turn_interrupted(&self.run_id.0, &turn_id.0, "user_cancel");
                        return Ok(TurnOutcome::aborted(total_usage));
                    }
                    res = &mut invoke_fut => break res,
                    Some(_ev) = rx.recv() => { /* drain, continue */ }
                }
            };

            let response = match invoke_result {
                Ok(r) => r,
                Err(e) => {
                    self.record_abort(
                        &turn_id,
                        AbortReason::AdapterError,
                        Some(e.to_string()),
                        total_usage.clone(),
                    )?;
                    return Err(TurnError::Adapter(e));
                }
            };

            // Drain any stream events still queued after invoke returns.
            while let Ok(_ev) = rx.try_recv() {}

            total_usage.input_tokens = total_usage
                .input_tokens
                .saturating_add(response.usage.input_tokens);
            total_usage.output_tokens = total_usage
                .output_tokens
                .saturating_add(response.usage.output_tokens);

            for (idx, block) in response.content.iter().enumerate() {
                self.writer.append(&SessionEvent::ContentBlock {
                    turn_id: turn_id.clone(),
                    index: idx,
                    block: block.clone(),
                })?;
            }

            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            match response.stop_reason {
                StopReason::ToolUse => {
                    // Collect tool_use blocks, dispatch each. Parallel tool
                    // calls within one call_group execute concurrently in v2;
                    // v1 serializes them in order.
                    let mut tool_results: Vec<ContentBlock> = Vec::new();
                    for block in response.content.iter() {
                        if let ContentBlock::ToolUse {
                            id, name, input, ..
                        } = block
                        {
                            let effect_class = self
                                .dispatcher
                                .tool(name)
                                .map(|t| t.effect_class())
                                .unwrap_or(EffectClass::Observe);

                            let path_hint = input.get("path").and_then(|v| v.as_str());

                            // Effect-budget short-circuit: if a contract is
                            // active and the projected class would push the
                            // per-run counter past its cap, abort the turn
                            // with RuntimeError. Classes not tracked by
                            // `EffectBudget` (Observe, Stage, remote/*) are
                            // left alone. A cap of 0 means "none allowed".
                            if let Some(c) = self.contract {
                                let (used, max, label) = match effect_class {
                                    EffectClass::ApplyLocal => (
                                        self.effects_consumed.apply_local,
                                        c.effect_budget.max_apply_local,
                                        "apply_local",
                                    ),
                                    EffectClass::ApplyRepo => (
                                        self.effects_consumed.apply_repo,
                                        c.effect_budget.max_apply_repo,
                                        "apply_repo",
                                    ),
                                    _ => (0, u32::MAX, ""),
                                };
                                if !label.is_empty() && used >= max {
                                    telemetry::emit_effect_budget_exhausted(
                                        &self.run_id.0,
                                        &turn_id.0,
                                        label,
                                        used,
                                        max,
                                    );
                                    self.record_abort(
                                        &turn_id,
                                        AbortReason::RuntimeError,
                                        Some(format!(
                                            "effect budget exhausted: {label} {used}/{max}"
                                        )),
                                        total_usage.clone(),
                                    )?;
                                    return Ok(TurnOutcome::aborted(total_usage));
                                }
                            }

                            let decision = {
                                let engine =
                                    AuthorityEngine::new(&*self.capabilities, ApprovalPolicyV1);
                                engine.authorize(name, effect_class, path_hint)
                            };

                            match decision {
                                AuthorityDecision::Auto | AuthorityDecision::Reuse(_) => {}
                                AuthorityDecision::NotAvailable { hint } => {
                                    self.record_abort(
                                        &turn_id,
                                        AbortReason::RuntimeError,
                                        Some(format!("effect not available: {hint}")),
                                        total_usage.clone(),
                                    )?;
                                    return Ok(TurnOutcome::aborted(total_usage));
                                }
                                AuthorityDecision::RequireApproval {
                                    approval_id,
                                    tool_name,
                                    effect_class: ec,
                                } => {
                                    let summary = format!(
                                        "{} → {}",
                                        tool_name,
                                        approval_hint(&tool_name, input)
                                    );
                                    self.writer.append(&SessionEvent::ApprovalRequest {
                                        turn_id: turn_id.clone(),
                                        approval_id: approval_id.clone(),
                                        effect_class: ec,
                                        tool_name: tool_name.clone(),
                                        summary: summary.clone(),
                                    })?;
                                    telemetry::emit_approval_requested(
                                        &self.run_id.0,
                                        &turn_id.0,
                                        &tool_name,
                                        ec,
                                    );

                                    let (resp_tx, resp_rx) = oneshot::channel::<ApprovalResponse>();
                                    let msg = ApprovalRequestMsg {
                                        turn_id: turn_id.clone(),
                                        approval_id: approval_id.clone(),
                                        tool_name: tool_name.clone(),
                                        effect_class: ec,
                                        summary,
                                        responder: resp_tx,
                                    };
                                    if self.approval_bridge.send(msg).await.is_err() {
                                        self.writer.append(&SessionEvent::ApprovalDenied {
                                            turn_id: turn_id.clone(),
                                            approval_id: approval_id.clone(),
                                        })?;
                                        self.record_abort(
                                            &turn_id,
                                            AbortReason::ApprovalDenied,
                                            Some("approval bridge closed".into()),
                                            total_usage.clone(),
                                        )?;
                                        return Ok(TurnOutcome::aborted(total_usage));
                                    }

                                    match resp_rx.await {
                                        Ok(ApprovalResponse::Grant { scope }) => {
                                            let tok =
                                                mint_from_approval(&tool_name, ec, scope.clone());
                                            let tok_id = tok.id.clone();
                                            self.capabilities.mint(tok);
                                            self.writer.append(&SessionEvent::ApprovalGranted {
                                                turn_id: turn_id.clone(),
                                                approval_id: approval_id.clone(),
                                                token: tok_id,
                                                scope: scope.clone(),
                                            })?;
                                            telemetry::emit_approval_granted(
                                                &self.run_id.0,
                                                &turn_id.0,
                                                &tool_name,
                                                &format!("{scope:?}"),
                                            );
                                        }
                                        Ok(ApprovalResponse::Deny) | Err(_) => {
                                            self.writer.append(&SessionEvent::ApprovalDenied {
                                                turn_id: turn_id.clone(),
                                                approval_id: approval_id.clone(),
                                            })?;
                                            telemetry::emit_approval_denied(
                                                &self.run_id.0,
                                                &turn_id.0,
                                                &tool_name,
                                            );
                                            self.record_abort(
                                                &turn_id,
                                                AbortReason::ApprovalDenied,
                                                Some("user denied approval".into()),
                                                total_usage.clone(),
                                            )?;
                                            return Ok(TurnOutcome::aborted(total_usage));
                                        }
                                    }
                                }
                            }

                            telemetry::emit_tool_dispatch(
                                &self.run_id.0,
                                &turn_id.0,
                                name,
                                effect_class,
                            );
                            let raw = Tainted::new(Origin::ModelOutput, input.clone());
                            let tool_start = std::time::Instant::now();
                            let result = crate::execution::dispatch_tool(
                                self.dispatcher,
                                name,
                                raw,
                                self.ctx,
                            )
                            .await;
                            let tool_duration_ms = tool_start.elapsed().as_millis() as u64;

                            let (content, is_error) = match result {
                                Ok(v) => (
                                    vec![ContentBlock::Text {
                                        text: v.to_string(),
                                    }],
                                    false,
                                ),
                                Err(e) => (
                                    vec![ContentBlock::Text {
                                        text: e.to_string(),
                                    }],
                                    true,
                                ),
                            };
                            telemetry::emit_tool_result(
                                &self.run_id.0,
                                &turn_id.0,
                                name,
                                is_error,
                                tool_duration_ms,
                            );

                            self.writer.append(&SessionEvent::EffectRecord {
                                turn_id: turn_id.clone(),
                                effect: EffectRecord {
                                    id: EffectRecordId::new(),
                                    tool_use_id: id.clone(),
                                    class: effect_class,
                                    tool_name: name.clone(),
                                    input_digest: Some(digest(input)),
                                    output_artifact: None,
                                    error: if is_error {
                                        Some("tool error".into())
                                    } else {
                                        None
                                    },
                                },
                            })?;
                            // Bump the per-run counter after the EffectRecord
                            // is durably appended. Only the two v1-budgeted
                            // classes are tracked; Observe/Stage are free.
                            if self.contract.is_some() {
                                match effect_class {
                                    EffectClass::ApplyLocal => {
                                        self.effects_consumed.apply_local =
                                            self.effects_consumed.apply_local.saturating_add(1);
                                    }
                                    EffectClass::ApplyRepo => {
                                        self.effects_consumed.apply_repo =
                                            self.effects_consumed.apply_repo.saturating_add(1);
                                    }
                                    _ => {}
                                }
                            }
                            let content_artifact =
                                persist_tool_output(&self.ctx.artifacts, &content);
                            self.writer.append(&SessionEvent::ToolResult {
                                turn_id: turn_id.clone(),
                                tool_use_id: id.clone(),
                                is_error,
                                content_artifact,
                                call_group: None,
                            })?;

                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content,
                                is_error,
                            });
                        }
                    }

                    messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                    continue;
                }
                StopReason::EndTurn | StopReason::StopSequence => {
                    // Contract-scoped commit gate. Validators run if any are
                    // wired; Checkpoint emits on every successful contract
                    // turn regardless of validator count, because invariant
                    // #5 calls for a checkpoint per committed turn — the
                    // per-turn attestation shouldn't require a validator to
                    // exist, just a contract to attest against. Contract-less
                    // runs still skip both branches and keep the pre-contract
                    // byte shape (no Checkpoint, no ValidatorResult).
                    if let Some(contract) = self.contract {
                        let mut failed: Option<(String, Option<String>)> = None;
                        for v in self.validators.iter() {
                            let report = v.check(contract);
                            let name = report.name.to_string();
                            self.writer.append(&SessionEvent::ValidatorResult {
                                turn_id: turn_id.clone(),
                                validator: name.clone(),
                                status: report.status,
                                detail: report.detail.clone(),
                            })?;
                            telemetry::emit_validator_result(
                                &self.run_id.0,
                                &turn_id.0,
                                &name,
                                report.status,
                            );
                            if matches!(report.status, ValidatorStatus::Fail) && failed.is_none() {
                                failed = Some((name, report.detail));
                            }
                        }
                        if let Some((name, detail)) = failed {
                            let msg = match detail {
                                Some(d) => format!("{name}: {d}"),
                                None => name,
                            };
                            self.record_abort(
                                &turn_id,
                                AbortReason::ValidatorFail,
                                Some(msg),
                                total_usage.clone(),
                            )?;
                            return Ok(TurnOutcome::aborted(total_usage));
                        }

                        // Sprint 5: TDAD impact validators. Compute
                        // the diff once (default empty when no source
                        // is wired), then fan-out to the validator
                        // slice. Each produces an `ImpactComputed`
                        // (plan detail) plus a `ValidatorResult`
                        // (pass/fail). A `Fail` aborts the turn under
                        // the same `AbortReason::ValidatorFail` the
                        // classical path uses — forensic diffs across
                        // subsystems stay consistent.
                        //
                        // PR #9 reviews addressed in this block:
                        //  - gemini MED: diff_source failures were
                        //    recorded as ValidatorStatus::Fail without
                        //    triggering abort, which was inconsistent.
                        //    Now emitted as Skip (degraded-mode marker)
                        //    + detail; we proceed with empty diff.
                        //  - gemini MED: validators ran sequentially.
                        //    Now driven via `futures::future::join_all`
                        //    so concurrent I/O (cargo, git, graph
                        //    queries) overlap. `join_all` preserves
                        //    input order in its result Vec, so JSONL
                        //    emission stays deterministic even when
                        //    validator wall-clock completion orders
                        //    vary — cache-prefix stability preserved.
                        //  - codex P2: events now carry full TestPlan
                        //    payload (rationale + confidence +
                        //    selector_version + ran_at).
                        //  - gemini HIGH: ran_at timestamp populated
                        //    at emission time so the SQLite mirror can
                        //    project into `test_impact.ran_at NOT NULL`.
                        if !self.impact_validators.is_empty() {
                            let diff: Diff = match self.diff_source {
                                Some(src) => match src.diff().await {
                                    Ok(d) => d,
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            source = src.name(),
                                            "diff_source failed; proceeding with empty diff"
                                        );
                                        // Skip, not Fail: the subsystem
                                        // is opt-in and a diff-source
                                        // outage is genuinely
                                        // non-fatal. The event still
                                        // lands in JSONL so eval can
                                        // measure how often it fires.
                                        let vname = format!("diff_source:{}", src.name());
                                        self.writer.append(&SessionEvent::ValidatorResult {
                                            turn_id: turn_id.clone(),
                                            validator: vname.clone(),
                                            status: ValidatorStatus::Skip,
                                            detail: Some(format!(
                                                "{e}; proceeding with empty diff"
                                            )),
                                        })?;
                                        telemetry::emit_validator_result(
                                            &self.run_id.0,
                                            &turn_id.0,
                                            &vname,
                                            ValidatorStatus::Skip,
                                        );
                                        Diff::empty()
                                    }
                                },
                                None => Diff::empty(),
                            };

                            // Fan-out. join_all returns reports in
                            // input order regardless of completion
                            // order, so the emission loop below stays
                            // deterministic. First-fail-wins semantics
                            // preserved by iterating the ordered Vec.
                            let reports = join_all(
                                self.impact_validators
                                    .iter()
                                    .map(|iv| iv.validate(contract, &diff)),
                            )
                            .await;

                            let mut impact_failed: Option<(String, Option<String>)> = None;
                            for report in reports {
                                let vname = report.name.to_string();
                                let ran_at = now_iso();

                                // Persist plan detail whenever the
                                // validator produced one (including
                                // empty plans). Rationale +
                                // confidence preserve forensic
                                // provenance; ran_at is required by
                                // the SQLite mirror's m0005 schema.
                                if let Some(plan) = report.plan.as_ref() {
                                    let selected_tests: Vec<String> =
                                        plan.tests.iter().map(|t| t.0.clone()).collect();
                                    self.writer.append(&SessionEvent::ImpactComputed {
                                        turn_id: turn_id.clone(),
                                        selector: vname.clone(),
                                        selector_version: plan.selector_version,
                                        ran_at: ran_at.clone(),
                                        changed_files: diff.changed_files.clone(),
                                        selected_tests,
                                        rationale: plan.rationale.clone(),
                                        confidence: plan.confidence.clone(),
                                    })?;
                                }

                                self.writer.append(&SessionEvent::ValidatorResult {
                                    turn_id: turn_id.clone(),
                                    validator: vname.clone(),
                                    status: report.status,
                                    detail: report.detail.clone(),
                                })?;
                                telemetry::emit_validator_result(
                                    &self.run_id.0,
                                    &turn_id.0,
                                    &vname,
                                    report.status,
                                );
                                if matches!(report.status, ValidatorStatus::Fail)
                                    && impact_failed.is_none()
                                {
                                    impact_failed = Some((vname, report.detail));
                                }
                            }

                            if let Some((name, detail)) = impact_failed {
                                let msg = match detail {
                                    Some(d) => format!("{name}: {d}"),
                                    None => name,
                                };
                                self.record_abort(
                                    &turn_id,
                                    AbortReason::ValidatorFail,
                                    Some(msg),
                                    total_usage.clone(),
                                )?;
                                return Ok(TurnOutcome::aborted(total_usage));
                            }
                        }

                        self.writer.append(&SessionEvent::Checkpoint {
                            turn_id: turn_id.clone(),
                            checkpoint_id: CheckpointId::new(),
                        })?;
                    }
                    self.writer.append(&SessionEvent::TurnCommitted {
                        turn_id: turn_id.clone(),
                        outcome: CommitOutcome::Success,
                        usage: total_usage.clone(),
                        user_input: user_input_content.clone(),
                        final_assistant: Some(response.content.clone()),
                    })?;
                    telemetry::emit_turn_committed(
                        &self.run_id.0,
                        &turn_id.0,
                        total_usage.input_tokens,
                        total_usage.output_tokens,
                    );
                    return Ok(TurnOutcome::committed(total_usage, response.content));
                }
                StopReason::MaxTokens => {
                    self.record_abort(
                        &turn_id,
                        AbortReason::TokenBudget,
                        Some("model hit max_tokens".into()),
                        total_usage.clone(),
                    )?;
                    return Ok(TurnOutcome::aborted(total_usage));
                }
                StopReason::ContentFilter => {
                    self.record_abort(
                        &turn_id,
                        AbortReason::RuntimeError,
                        Some("content filter".into()),
                        total_usage.clone(),
                    )?;
                    return Ok(TurnOutcome::aborted(total_usage));
                }
            }
        }
    }
}

fn digest<T: serde::Serialize>(value: &T) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(&bytes);
    format!("sha256:{}", hex::encode(h.finalize()))
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Persist a tool's output content blocks to the content-addressed artifact
/// store and return the resulting `ArtifactId` for the `ToolResult` event.
///
/// Serializes the `Vec<ContentBlock>` as JSON (stable wire format matching
/// the schema) and hands the bytes to `ArtifactStore::put`. On failure the
/// error is logged and `None` is returned — the tool output still flows
/// inline to the model through the in-memory message list, so turn execution
/// is unaffected. Only forensic/replay fidelity degrades, and we surface that
/// via tracing.
fn persist_tool_output(
    artifacts: &crate::artifacts::ArtifactStore,
    content: &[crate::schemas::ContentBlock],
) -> Option<crate::schemas::ArtifactId> {
    let bytes = match serde_json::to_vec(content) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "serialize tool output for artifact store");
            return None;
        }
    };
    match artifacts.put(&bytes) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(error = %e, "persist tool output to artifact store");
            None
        }
    }
}

/// Human-readable hint describing what a tool is about to do, shown in the
/// approval modal. Tool-specific extractors take priority over the generic
/// JSON fallback so `bash → rm -rf /` renders meaningfully instead of
/// `bash → (no path)`.
fn approval_hint(tool_name: &str, input: &serde_json::Value) -> String {
    if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
        return p.to_string();
    }
    match tool_name {
        "bash" => {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                return truncate_single_line(cmd, 80);
            }
        }
        "repo.search" => {
            if let Some(q) = input.get("q").and_then(|v| v.as_str()) {
                return format!("q={}", truncate_single_line(q, 60));
            }
        }
        _ => {}
    }
    let raw = input.to_string();
    truncate_single_line(&raw, 80)
}

fn truncate_single_line(s: &str, n: usize) -> String {
    let one_line: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if one_line.chars().count() <= n {
        one_line
    } else {
        let head: String = one_line.chars().take(n).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod approval_hint_tests {
    use super::*;

    #[test]
    fn bash_hint_shows_command() {
        let v = serde_json::json!({ "command": "ls -la /tmp" });
        assert_eq!(approval_hint("bash", &v), "ls -la /tmp");
    }

    #[test]
    fn bash_multiline_command_collapses_to_one_line() {
        let v = serde_json::json!({ "command": "set -e\necho hi" });
        assert_eq!(approval_hint("bash", &v), "set -e echo hi");
    }

    #[test]
    fn path_based_tool_prefers_path() {
        let v = serde_json::json!({ "path": "src/foo.rs", "command": "ignored" });
        assert_eq!(approval_hint("fs.write", &v), "src/foo.rs");
    }

    #[test]
    fn search_hint_shows_query() {
        let v = serde_json::json!({ "q": "refresh_token" });
        assert_eq!(approval_hint("repo.search", &v), "q=refresh_token");
    }

    #[test]
    fn long_command_gets_truncated_with_ellipsis() {
        let long_cmd = "x".repeat(200);
        let v = serde_json::json!({ "command": long_cmd });
        let hint = approval_hint("bash", &v);
        assert!(hint.ends_with('…'));
        assert!(hint.chars().count() <= 81); // 80 chars + ellipsis
    }

    #[test]
    fn unknown_tool_falls_back_to_json_snippet() {
        let v = serde_json::json!({ "weird": "thing" });
        let hint = approval_hint("custom.tool", &v);
        assert!(hint.contains("weird"));
    }
}
