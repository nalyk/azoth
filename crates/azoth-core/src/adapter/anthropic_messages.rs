//! Native-shape adapter. Translates `ModelTurnRequest` into the Anthropic
//! Messages wire JSON, sends it, parses the non-streaming response body, and
//! converts it back into `ModelTurnResponse`.
//!
//! Streaming is intentionally simplified in v1: the adapter makes a
//! non-streaming request and emits a single synthetic `MessageStart`/
//! `ContentBlockStart`/`ContentBlockStop`/`MessageStop` sequence so
//! downstream code is exercised. Real SSE parsing is a follow-up once the
//! turn driver needs it.

use super::stream::{emit_synthetic_stream, map_http_status};
use super::{error::TokenCount, AdapterError, ProviderAdapter, ProviderProfile, ToolUseShape};
use crate::authority::SecretHandle;
use crate::schemas::{
    AdapterErrorCode, CacheHints, ContentBlock, Message, ModelTurnRequest, ModelTurnResponse, Role,
    StopReason, StreamEvent, ToolDefinition, ToolUseId, Usage,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub struct AnthropicMessagesAdapter {
    profile: ProviderProfile,
    api_key: SecretHandle,
    http: reqwest::Client,
}

impl AnthropicMessagesAdapter {
    pub fn new(profile: ProviderProfile, api_key: SecretHandle) -> Self {
        debug_assert!(matches!(profile.tool_use_shape, ToolUseShape::ContentBlock));
        Self {
            profile,
            api_key,
            http: reqwest::Client::new(),
        }
    }

    fn build_body(&self, req: &ModelTurnRequest) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(message_to_wire).collect();
        let tools: Vec<Value> = req.tools.iter().map(tool_to_wire).collect();

        let mut body = json!({
            "model": self.profile.model_id,
            "system": system_with_cache(&req.system, &req.cache_hints),
            "messages": messages,
            "max_tokens": req.max_tokens,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        body
    }
}

fn system_with_cache(system: &str, hints: &CacheHints) -> Value {
    if hints.constitution_boundary {
        // System as an array lets us pin a cache_control breakpoint at the
        // end of the constitution lane, which is what the Context Kernel
        // places at the beginning of `system`.
        json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }])
    } else {
        Value::String(system.to_string())
    }
}

fn message_to_wire(msg: &Message) -> Value {
    json!({
        "role": match msg.role { Role::User => "user", Role::Assistant => "assistant" },
        "content": msg.content.iter().map(block_to_wire).collect::<Vec<_>>(),
    })
}

fn block_to_wire(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ContentBlock::ToolUse { id, name, input, .. } => json!({
            "type": "tool_use",
            "id": id.as_str(),
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult { tool_use_id, content, is_error } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id.as_str(),
            "content": content.iter().map(block_to_wire).collect::<Vec<_>>(),
            "is_error": is_error,
        }),
        ContentBlock::Thinking { text, signature } => json!({
            "type": "thinking",
            "thinking": text,
            "signature": signature,
        }),
    }
}

fn tool_to_wire(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn parse_response_body(body: &Value) -> Result<ModelTurnResponse, AdapterError> {
    let content_arr = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::invalid_request("missing `content` array"))?;

    let mut content: Vec<ContentBlock> = Vec::with_capacity(content_arr.len());
    for item in content_arr {
        let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "text" => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                content.push(ContentBlock::Text { text });
            }
            "tool_use" => {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let input = item.get("input").cloned().unwrap_or(Value::Null);
                content.push(ContentBlock::ToolUse {
                    id: ToolUseId::from(id),
                    name,
                    input,
                    call_group: None,
                });
            }
            "thinking" => {
                let text = item.get("thinking").and_then(Value::as_str).unwrap_or("").to_string();
                let signature = item.get("signature").and_then(Value::as_str).map(str::to_string);
                content.push(ContentBlock::Thinking { text, signature });
            }
            _ => {
                // Unknown content types are ignored rather than failing the whole turn.
                tracing::debug!(kind = %ty, "ignoring unknown anthropic content block");
            }
        }
    }

    let stop_reason = body
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(|s| match s {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        })
        .unwrap_or(StopReason::EndTurn);

    let usage = body
        .get("usage")
        .map(|u| Usage {
            input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            cache_read_tokens: u
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            cache_creation_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        })
        .unwrap_or_default();

    Ok(ModelTurnResponse { content, stop_reason, usage })
}

#[async_trait]
impl ProviderAdapter for AnthropicMessagesAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        let body = self.build_body(&req);
        let url = format!("{}/v1/messages", self.profile.base_url.trim_end_matches('/'));

        let mut builder = self
            .http
            .post(&url)
            .header("x-api-key", self.api_key.expose())
            .header("content-type", "application/json");
        for (k, v) in &self.profile.extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder = builder.json(&body);

        let resp = builder.send().await.map_err(|e| AdapterError::network(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AdapterError::network(e.to_string()))?;

        if !status.is_success() {
            let code = map_http_status(status.as_u16());
            return Err(AdapterError {
                code,
                retryable: matches!(
                    code,
                    AdapterErrorCode::RateLimited
                        | AdapterErrorCode::Timeout
                        | AdapterErrorCode::Network
                ),
                provider_status: Some(status.as_u16()),
                detail: text,
            });
        }

        let json: Value = serde_json::from_str(&text)
            .map_err(|e| AdapterError::invalid_request(format!("response not json: {e}")))?;
        let response = parse_response_body(&json)?;
        emit_synthetic_stream(&response, &sink).await;
        Ok(response)
    }

    async fn count_tokens(&self, _req: &ModelTurnRequest) -> Result<TokenCount, AdapterError> {
        // v1: pre-flight validation uses the local Context Kernel tokenizer.
        // The Anthropic `count_tokens` endpoint is a nice-to-have for later.
        Ok(TokenCount { input_tokens: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_anthropic_response() {
        let body = json!({
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "tu_a", "name": "repo.search", "input": {"q": "x"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 7, "output_tokens": 3}
        });
        let resp = parse_response_body(&body).unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.content.len(), 2);
    }
}
