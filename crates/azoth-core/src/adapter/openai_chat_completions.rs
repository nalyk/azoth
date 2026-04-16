//! Downcast adapter: Azoth's internal content-block shape → OpenAI Chat
//! Completions wire, and back. Preserves parallel-tool ordering via
//! `call_group` (HIGH-3).

use super::openai_sse::consume_openai_sse;
use super::stream::map_http_status;
use super::{error::TokenCount, AdapterError, ProviderAdapter, ProviderProfile, ToolUseShape};
use crate::authority::SecretHandle;
use crate::schemas::{
    ContentBlock, Message, ModelTurnRequest, ModelTurnResponse, Role, StreamEvent, ToolDefinition,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub struct OpenAiChatCompletionsAdapter {
    profile: ProviderProfile,
    api_key: SecretHandle,
    http: reqwest::Client,
}

impl OpenAiChatCompletionsAdapter {
    pub fn new(profile: ProviderProfile, api_key: SecretHandle) -> Self {
        debug_assert!(matches!(
            profile.tool_use_shape,
            ToolUseShape::FlatToolCalls
        ));
        Self {
            profile,
            api_key,
            http: reqwest::Client::new(),
        }
    }

    fn build_body(&self, req: &ModelTurnRequest) -> Value {
        let mut msgs: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if !req.system.is_empty() {
            msgs.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            extend_openai_messages(&mut msgs, m);
        }

        let tools: Vec<Value> = req.tools.iter().map(openai_tool).collect();

        let mut body = json!({
            "model": self.profile.model_id,
            "messages": msgs,
            "max_tokens": req.max_tokens,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!("auto");
        }
        body
    }
}

fn openai_tool(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        }
    })
}

/// Azoth Message → one-or-more OpenAI messages. A single assistant message
/// carrying N parallel `tool_use` blocks becomes one assistant turn with a
/// `tool_calls` array; every `tool_result` block becomes its own subsequent
/// `role: tool` message.
fn extend_openai_messages(out: &mut Vec<Value>, msg: &Message) {
    match msg.role {
        Role::User => {
            // User messages may contain tool_result blocks (results of
            // client-executed tools in the OpenAI shape) OR text.
            let mut text_buf = String::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text_buf.is_empty() {
                            text_buf.push('\n');
                        }
                        text_buf.push_str(text);
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        if !text_buf.is_empty() {
                            out.push(json!({"role": "user", "content": text_buf.clone()}));
                            text_buf.clear();
                        }
                        let content_str = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id.as_str(),
                            "content": content_str,
                        }));
                    }
                    _ => {}
                }
            }
            if !text_buf.is_empty() {
                out.push(json!({"role": "user", "content": text_buf}));
            }
        }
        Role::Assistant => {
            let mut text_buf = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text_buf.is_empty() {
                            text_buf.push('\n');
                        }
                        text_buf.push_str(text);
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        tool_calls.push(json!({
                            "id": id.as_str(),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                            }
                        }));
                    }
                    _ => {}
                }
            }
            let mut m = json!({"role": "assistant"});
            if !text_buf.is_empty() {
                m["content"] = Value::String(text_buf);
            } else {
                m["content"] = Value::Null;
            }
            if !tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(tool_calls);
            }
            out.push(m);
        }
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiChatCompletionsAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        let body = self.build_body(&req);
        let url = format!(
            "{}/chat/completions",
            self.profile.base_url.trim_end_matches('/')
        );

        let mut builder = self
            .http
            .post(&url)
            .bearer_auth(self.api_key.expose())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
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
            return Err(AdapterError {
                code: map_http_status(status.as_u16()),
                retryable: matches!(status.as_u16(), 429 | 408 | 500..=599),
                provider_status: Some(status.as_u16()),
                detail: text,
            });
        }

        let byte_stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| AdapterError::network(e.to_string())));
        consume_openai_sse(Box::pin(byte_stream), &sink).await
    }

    async fn count_tokens(&self, _req: &ModelTurnRequest) -> Result<TokenCount, AdapterError> {
        Ok(TokenCount { input_tokens: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::schemas::ToolUseId;

    #[test]
    fn request_body_includes_tool_calls_for_prior_assistant_turn() {
        let profile = ProviderProfile::openrouter_default("openai/gpt-4");
        let adapter = OpenAiChatCompletionsAdapter::new(profile, SecretHandle::new("sk-test"));
        let req = ModelTurnRequest {
            system: "sys".into(),
            messages: vec![
                Message::user_text("go"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolUseId::from("tu_a".to_string()),
                        name: "repo.search".into(),
                        input: json!({"q": "x"}),
                        call_group: None,
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: ToolUseId::from("tu_a".to_string()),
                        content: vec![ContentBlock::Text { text: "ok".into() }],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 256,
            cache_hints: Default::default(),
            metadata: crate::schemas::RequestMetadata {
                run_id: "r".into(),
                turn_id: "t".into(),
            },
        };
        let body = adapter.build_body(&req);
        let msgs = body.get("messages").and_then(|m| m.as_array()).unwrap();
        // system + user + assistant(tool_calls) + tool
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert!(msgs[2]["tool_calls"].is_array());
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "tu_a");
    }
}
