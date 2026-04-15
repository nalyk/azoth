//! Native-shape adapter. Translates `ModelTurnRequest` into the Anthropic
//! Messages wire JSON, opens a streaming POST, and feeds the SSE byte stream
//! through `super::sse::consume_anthropic_sse`, which emits `StreamEvent`s
//! onto the sink as frames arrive and returns the assembled
//! `ModelTurnResponse` when the stream closes.

use super::sse::consume_anthropic_sse;
use super::stream::map_http_status;
use super::{error::TokenCount, AdapterError, ProviderAdapter, ProviderProfile, ToolUseShape};
use crate::authority::SecretHandle;
use crate::schemas::{
    AdapterErrorCode, CacheHints, ContentBlock, Message, ModelTurnRequest, ModelTurnResponse, Role,
    StreamEvent, ToolDefinition,
};
use async_trait::async_trait;
use futures::StreamExt;
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
            "stream": true,
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
            .header("anthropic-version", "2023-06-01")
            .header("accept", "text/event-stream")
            .header("content-type", "application/json");
        for (k, v) in &self.profile.extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder = builder.json(&body);

        let resp = builder
            .send()
            .await
            .map_err(|e| AdapterError::network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
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

        let byte_stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| AdapterError::network(e.to_string())));
        consume_anthropic_sse(Box::pin(byte_stream), &sink).await
    }

    async fn count_tokens(&self, _req: &ModelTurnRequest) -> Result<TokenCount, AdapterError> {
        // v1: pre-flight validation uses the local Context Kernel tokenizer.
        // The Anthropic `count_tokens` endpoint is a nice-to-have for later.
        Ok(TokenCount { input_tokens: 0 })
    }
}

