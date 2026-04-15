//! TurnDriver: the state machine that drives one turn end-to-end.
//!
//! plan → compile → invoke → dispatch → validate → commit/abort

use crate::adapter::ProviderAdapter;
use crate::authority::{
    mint_from_approval, ApprovalPolicyV1, ApprovalRequestMsg, ApprovalResponse, AuthorityDecision,
    AuthorityEngine, CapabilityStore, Origin, Tainted,
};
use crate::event_store::JsonlWriter;
use crate::execution::{ExecutionContext, ToolDispatcher, ToolError};
use crate::schemas::{
    AbortReason, CommitOutcome, ContentBlock, EffectClass, EffectRecord, EffectRecordId, Message,
    ModelTurnRequest, RequestMetadata, Role, RunId, SessionEvent, StopReason, StreamEvent,
    ToolDefinition, TurnId, Usage,
};
use tokio::sync::oneshot;
use thiserror::Error;
use tokio::sync::mpsc;

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
}

pub struct TurnDriver<'a> {
    pub run_id: RunId,
    pub adapter: &'a dyn ProviderAdapter,
    pub dispatcher: &'a ToolDispatcher,
    pub writer: &'a mut JsonlWriter,
    pub ctx: &'a ExecutionContext,
    pub capabilities: &'a mut CapabilityStore,
    pub approval_bridge: mpsc::Sender<ApprovalRequestMsg>,
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
        self.writer.append(&SessionEvent::TurnAborted {
            turn_id: turn_id.clone(),
            reason,
            detail,
            usage,
        })
    }

    /// Drive a single turn. `messages` is the conversation tail the Context
    /// Kernel has already compiled for this step; the driver appends assistant
    /// + tool_result blocks as it goes.
    pub async fn drive_turn(
        &mut self,
        turn_id: TurnId,
        system: String,
        mut messages: Vec<Message>,
    ) -> Result<Usage, TurnError> {
        self.writer.append(&SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: self.run_id.clone(),
            parent_turn: None,
            timestamp: now_iso(),
        })?;

        let tools: Vec<ToolDefinition> = self.dispatcher.schemas();
        let mut total_usage = Usage::default();

        loop {
            if self.ctx.cancelled() {
                self.writer.append(&SessionEvent::TurnInterrupted {
                    turn_id: turn_id.clone(),
                    reason: AbortReason::UserCancel,
                    partial_usage: Default::default(),
                })?;
                return Ok(total_usage);
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

            // Bounded sink for stream events — drained in-loop after invoke.
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
            let invoke_fut = self.adapter.invoke(req, tx);

            let response = match invoke_fut.await {
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

            // Drain any remaining stream events. In v1 the adapter emits
            // them synthetically inside `invoke`, so by the time we get here
            // the channel is already closed by the sender going out of scope.
            while let Ok(_ev) = rx.try_recv() {}

            total_usage.input_tokens = total_usage.input_tokens.saturating_add(response.usage.input_tokens);
            total_usage.output_tokens = total_usage.output_tokens.saturating_add(response.usage.output_tokens);

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
                        if let ContentBlock::ToolUse { id, name, input, .. } = block {
                            let effect_class = self
                                .dispatcher
                                .tool(name)
                                .map(|t| t.effect_class())
                                .unwrap_or(EffectClass::Observe);

                            let path_hint = input.get("path").and_then(|v| v.as_str());

                            let decision = {
                                let engine = AuthorityEngine::new(
                                    &*self.capabilities,
                                    ApprovalPolicyV1,
                                );
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
                                    return Ok(total_usage);
                                }
                                AuthorityDecision::RequireApproval {
                                    approval_id,
                                    tool_name,
                                    effect_class: ec,
                                } => {
                                    let summary = format!(
                                        "{} → {}",
                                        tool_name,
                                        path_hint.unwrap_or("(no path)")
                                    );
                                    self.writer.append(&SessionEvent::ApprovalRequest {
                                        turn_id: turn_id.clone(),
                                        approval_id: approval_id.clone(),
                                        effect_class: ec,
                                        tool_name: tool_name.clone(),
                                        summary: summary.clone(),
                                    })?;

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
                                        return Ok(total_usage);
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
                                                scope,
                                            })?;
                                        }
                                        Ok(ApprovalResponse::Deny) | Err(_) => {
                                            self.writer.append(&SessionEvent::ApprovalDenied {
                                                turn_id: turn_id.clone(),
                                                approval_id: approval_id.clone(),
                                            })?;
                                            self.record_abort(
                                                &turn_id,
                                                AbortReason::ApprovalDenied,
                                                Some("user denied approval".into()),
                                                total_usage.clone(),
                                            )?;
                                            return Ok(total_usage);
                                        }
                                    }
                                }
                            }

                            let raw = Tainted::new(Origin::ModelOutput, input.clone());
                            let result = crate::execution::dispatch_tool(
                                self.dispatcher,
                                name,
                                raw,
                                self.ctx,
                            )
                            .await;

                            let (content, is_error) = match result {
                                Ok(v) => (
                                    vec![ContentBlock::Text {
                                        text: v.to_string(),
                                    }],
                                    false,
                                ),
                                Err(e) => (
                                    vec![ContentBlock::Text { text: e.to_string() }],
                                    true,
                                ),
                            };

                            self.writer.append(&SessionEvent::EffectRecord {
                                turn_id: turn_id.clone(),
                                effect: EffectRecord {
                                    id: EffectRecordId::new(),
                                    tool_use_id: id.clone(),
                                    class: effect_class,
                                    tool_name: name.clone(),
                                    input_digest: Some(digest(input)),
                                    output_artifact: None,
                                    error: if is_error { Some("tool error".into()) } else { None },
                                },
                            })?;
                            self.writer.append(&SessionEvent::ToolResult {
                                turn_id: turn_id.clone(),
                                tool_use_id: id.clone(),
                                is_error,
                                content_artifact: None,
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
                    self.writer.append(&SessionEvent::TurnCommitted {
                        turn_id: turn_id.clone(),
                        outcome: CommitOutcome::Success,
                        usage: total_usage.clone(),
                    })?;
                    return Ok(total_usage);
                }
                StopReason::MaxTokens => {
                    self.record_abort(
                        &turn_id,
                        AbortReason::TokenBudget,
                        Some("model hit max_tokens".into()),
                        total_usage.clone(),
                    )?;
                    return Ok(total_usage);
                }
                StopReason::ContentFilter => {
                    self.record_abort(
                        &turn_id,
                        AbortReason::RuntimeError,
                        Some("content filter".into()),
                        total_usage.clone(),
                    )?;
                    return Ok(total_usage);
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
