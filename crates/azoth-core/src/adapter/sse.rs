//! Anthropic Messages SSE stream parser.
//!
//! Consumes a byte stream of Anthropic `text/event-stream` frames, emits
//! `StreamEvent`s onto the provided `sink` as frames arrive, and returns the
//! fully-assembled `ModelTurnResponse` when the stream closes cleanly.
//!
//! Kept stream-generic so tests can feed it fixture bytes without touching
//! the network: any `Stream<Item = Result<Bytes, AdapterError>>` is accepted.

use super::{error::AdapterError, stream::map_http_status};
use crate::schemas::{
    AdapterErrorCode, ContentBlock, ContentBlockStub, ModelTurnResponse, StopReason, StreamEvent,
    ToolUseId, Usage, UsageDelta,
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

pub(super) async fn consume_anthropic_sse<S>(
    mut stream: S,
    sink: &mpsc::Sender<StreamEvent>,
) -> Result<ModelTurnResponse, AdapterError>
where
    S: Stream<Item = Result<Bytes, AdapterError>> + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut builder = ResponseBuilder::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(frame_bytes) = take_frame(&mut buf) {
            if frame_bytes.is_empty() {
                continue;
            }
            if let Some(frame) = parse_frame(&frame_bytes) {
                handle_frame(frame, &mut builder, sink).await?;
            }
            if builder.terminated {
                return builder.finish();
            }
        }
    }
    // Residual frame without trailing blank line.
    if !buf.is_empty() {
        if let Some(frame) = parse_frame(&buf) {
            handle_frame(frame, &mut builder, sink).await?;
        }
    }
    builder.finish()
}

struct SseFrame {
    event: Option<String>,
    data: String,
}

fn take_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < buf.len() {
        if i + 1 < buf.len() && buf[i] == b'\n' && buf[i + 1] == b'\n' {
            let frame = buf[..i].to_vec();
            buf.drain(..i + 2);
            return Some(frame);
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            let frame = buf[..i].to_vec();
            buf.drain(..i + 4);
            return Some(frame);
        }
        i += 1;
    }
    None
}

fn parse_frame(bytes: &[u8]) -> Option<SseFrame> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut event = None;
    let mut data = String::new();
    for raw in s.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    Some(SseFrame { event, data })
}

async fn handle_frame(
    frame: SseFrame,
    builder: &mut ResponseBuilder,
    sink: &mpsc::Sender<StreamEvent>,
) -> Result<(), AdapterError> {
    if frame.data.is_empty() {
        return Ok(());
    }
    let data: Value = serde_json::from_str(&frame.data)
        .map_err(|e| AdapterError::invalid_request(format!("sse data not json: {e}")))?;
    let ev_name = frame
        .event
        .as_deref()
        .or_else(|| data.get("type").and_then(Value::as_str))
        .unwrap_or("");

    match ev_name {
        "ping" => {}
        "message_start" => {
            if let Some(usage) = data.pointer("/message/usage") {
                builder.usage.input_tokens = u32_from(usage, "input_tokens");
                builder.usage.cache_read_tokens = u32_from(usage, "cache_read_input_tokens");
                builder.usage.cache_creation_tokens =
                    u32_from(usage, "cache_creation_input_tokens");
            }
            let _ = sink.send(StreamEvent::MessageStart).await;
        }
        "content_block_start" => {
            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let cb = data.get("content_block").cloned().unwrap_or(Value::Null);
            let ty = cb.get("type").and_then(Value::as_str).unwrap_or("");
            let (slot, stub) = match ty {
                "text" => {
                    let text = cb
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (BlockSlot::Text { text }, ContentBlockStub::Text)
                }
                "tool_use" => {
                    let id = ToolUseId::from(
                        cb.get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    );
                    let name = cb
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (
                        BlockSlot::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            partial_json: String::new(),
                            input: None,
                        },
                        ContentBlockStub::ToolUse { id, name },
                    )
                }
                "thinking" => {
                    let text = cb
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let signature = cb
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    (
                        BlockSlot::Thinking { text, signature },
                        ContentBlockStub::Thinking,
                    )
                }
                _ => return Ok(()),
            };
            builder.put(index, slot);
            let _ = sink
                .send(StreamEvent::ContentBlockStart { index, block: stub })
                .await;
        }
        "content_block_delta" => {
            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = data.get("delta").cloned().unwrap_or(Value::Null);
            let dty = delta.get("type").and_then(Value::as_str).unwrap_or("");
            match dty {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(BlockSlot::Text { text: acc }) = builder.slot_mut(index) {
                        acc.push_str(&text);
                    }
                    let _ = sink.send(StreamEvent::TextDelta { index, text }).await;
                }
                "input_json_delta" => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(BlockSlot::ToolUse { partial_json, .. }) = builder.slot_mut(index) {
                        partial_json.push_str(&partial);
                    }
                    let _ = sink
                        .send(StreamEvent::InputJsonDelta {
                            index,
                            partial_json: partial,
                        })
                        .await;
                }
                "thinking_delta" => {
                    let text = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(BlockSlot::Thinking { text: acc, .. }) = builder.slot_mut(index) {
                        acc.push_str(&text);
                    }
                }
                "signature_delta" => {
                    let sig = delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(BlockSlot::Thinking { signature, .. }) = builder.slot_mut(index) {
                        *signature = Some(sig);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(BlockSlot::ToolUse {
                partial_json,
                input,
                ..
            }) = builder.slot_mut(index)
            {
                let parsed = if partial_json.is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str::<Value>(partial_json).map_err(|e| {
                        AdapterError::invalid_request(format!("tool_use input json invalid: {e}"))
                    })?
                };
                *input = Some(parsed);
            }
            let _ = sink.send(StreamEvent::ContentBlockStop { index }).await;
        }
        "message_delta" => {
            let stop_reason = data
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
                .map(parse_stop_reason);
            if let Some(sr) = stop_reason {
                builder.stop_reason = Some(sr);
            }
            let out_tokens = data
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            if out_tokens > builder.usage.output_tokens {
                builder.usage.output_tokens = out_tokens;
            }
            let _ = sink
                .send(StreamEvent::MessageDelta {
                    stop_reason,
                    usage_delta: UsageDelta {
                        input_tokens: 0,
                        output_tokens: out_tokens,
                    },
                })
                .await;
        }
        "message_stop" => {
            let _ = sink.send(StreamEvent::MessageStop).await;
            builder.terminated = true;
        }
        "error" => {
            let message = data
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let kind = data
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("");
            let code = match kind {
                "rate_limit_error" | "overloaded_error" => AdapterErrorCode::RateLimited,
                "authentication_error" | "permission_error" => AdapterErrorCode::AuthFailed,
                "invalid_request_error" => AdapterErrorCode::InvalidRequest,
                "api_error" => AdapterErrorCode::Network,
                _ => map_http_status(500),
            };
            let retryable = matches!(
                code,
                AdapterErrorCode::RateLimited
                    | AdapterErrorCode::Network
                    | AdapterErrorCode::Timeout
            );
            let _ = sink
                .send(StreamEvent::Error {
                    code,
                    message: message.clone(),
                    retryable,
                })
                .await;
            return Err(AdapterError {
                code,
                retryable,
                provider_status: None,
                detail: message,
            });
        }
        _ => {}
    }
    Ok(())
}

fn u32_from(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

#[derive(Default)]
struct ResponseBuilder {
    slots: Vec<Option<BlockSlot>>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    terminated: bool,
}

enum BlockSlot {
    Text {
        text: String,
    },
    ToolUse {
        id: ToolUseId,
        name: String,
        partial_json: String,
        input: Option<Value>,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
}

impl ResponseBuilder {
    fn put(&mut self, index: usize, slot: BlockSlot) {
        while self.slots.len() <= index {
            self.slots.push(None);
        }
        self.slots[index] = Some(slot);
    }
    fn slot_mut(&mut self, index: usize) -> Option<&mut BlockSlot> {
        self.slots.get_mut(index).and_then(Option::as_mut)
    }
    fn finish(self) -> Result<ModelTurnResponse, AdapterError> {
        let content = self
            .slots
            .into_iter()
            .flatten()
            .map(|s| match s {
                BlockSlot::Text { text } => ContentBlock::Text { text },
                BlockSlot::ToolUse {
                    id, name, input, ..
                } => ContentBlock::ToolUse {
                    id,
                    name,
                    input: input.unwrap_or_else(|| Value::Object(Map::new())),
                    call_group: None,
                },
                BlockSlot::Thinking { text, signature } => {
                    ContentBlock::Thinking { text, signature }
                }
            })
            .collect();
        Ok(ModelTurnResponse {
            content,
            stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::ContentBlock;
    use futures::stream;

    fn fixture_stream(bytes: &'static [u8]) -> impl Stream<Item = Result<Bytes, AdapterError>> {
        // Split into 17-byte chunks to exercise cross-chunk frame splitting.
        let chunks: Vec<Result<Bytes, AdapterError>> = bytes
            .chunks(17)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        stream::iter(chunks)
    }

    async fn drain(
        fixture: &'static [u8],
    ) -> (Result<ModelTurnResponse, AdapterError>, Vec<StreamEvent>) {
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
        let stream = fixture_stream(fixture);
        let collector = tokio::spawn(async move {
            let mut out = Vec::new();
            while let Some(ev) = rx.recv().await {
                out.push(ev);
            }
            out
        });
        let result = consume_anthropic_sse(Box::pin(stream), &tx).await;
        drop(tx);
        let events = collector.await.unwrap();
        (result, events)
    }

    const TEXT_ONLY: &[u8] = include_bytes!("../../tests/fixtures/anthropic/text_only.sse");
    const TOOL_USE: &[u8] = include_bytes!("../../tests/fixtures/anthropic/tool_use.sse");
    const ERROR_FRAME: &[u8] = include_bytes!("../../tests/fixtures/anthropic/error.sse");

    #[tokio::test]
    async fn parses_text_only_stream() {
        let (resp, events) = drain(TEXT_ONLY).await;
        let resp = resp.expect("text_only fixture should parse");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 25);
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected text block, got {other:?}"),
        }
        assert!(matches!(events.first(), Some(StreamEvent::MessageStart)));
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "Hello")));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == " world")));
    }

    #[tokio::test]
    async fn parses_tool_use_with_split_input_json_delta() {
        let (resp, events) = drain(TOOL_USE).await;
        let resp = resp.expect("tool_use fixture should parse");
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 50);
        assert_eq!(resp.usage.output_tokens, 15);
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id.as_str(), "tu_abc");
                assert_eq!(name, "repo.search");
                assert_eq!(input.get("q").and_then(Value::as_str), Some("hello"));
            }
            other => panic!("expected tool_use block, got {other:?}"),
        }
        let input_deltas: usize = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::InputJsonDelta { .. }))
            .count();
        assert_eq!(input_deltas, 2, "both partial_json fragments should stream");
    }

    #[tokio::test]
    async fn propagates_error_frame_as_adapter_error() {
        let (resp, events) = drain(ERROR_FRAME).await;
        let err = resp.expect_err("error fixture should fail");
        assert_eq!(err.code, AdapterErrorCode::RateLimited);
        assert!(err.retryable);
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Error { .. })));
    }
}
