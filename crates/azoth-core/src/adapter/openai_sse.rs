//! OpenAI Chat Completions SSE stream parser.
//!
//! Consumes a byte stream of `data: {ChatCompletionChunk}\n\n` frames
//! terminated by `data: [DONE]\n\n`, emits `StreamEvent`s on the sink as
//! frames arrive, and returns the fully-assembled `ModelTurnResponse` when
//! the stream closes cleanly.
//!
//! Kept stream-generic so tests can feed fixture bytes without touching the
//! network: any `Stream<Item = Result<Bytes, AdapterError>>` is accepted.
//!
//! Shape notes:
//! - No `event:` header lines; just `data:` (and optional `:` keep-alive
//!   comments).
//! - `delta.content` streams as a sequence of STRING fragments for one
//!   logical text block. There is no `content_block_start` — the text slot
//!   is created lazily on the first non-null `delta.content`.
//! - `delta.tool_calls[i]` is a flat array; each element carries its own
//!   `index`. The first chunk for an index has `id`, `type: "function"`,
//!   and `function.name`; later chunks for the same index carry only
//!   `function.arguments` as a STRING fragment. We keep a per-`index` slot
//!   and accumulate the arguments string, parsing it once on
//!   `finish_reason` / stream close.
//! - All tool slots created during a single stream share one
//!   `CallGroupId::new()` minted at stream start (HIGH-3 parallel ordering).
//! - `usage` appears only in the terminal chunk when the request body set
//!   `stream_options.include_usage = true` — which `build_body` does
//!   unconditionally for streaming.

use super::error::AdapterError;
use crate::schemas::{
    AdapterErrorCode, CallGroupId, ContentBlock, ContentBlockStub, ModelTurnResponse, StopReason,
    StreamEvent, ToolUseId, Usage, UsageDelta,
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Map, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub(super) async fn consume_openai_sse<S>(
    mut stream: S,
    sink: &mpsc::Sender<StreamEvent>,
) -> Result<ModelTurnResponse, AdapterError>
where
    S: Stream<Item = Result<Bytes, AdapterError>> + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut builder = ResponseBuilder::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(frame_bytes) = take_frame(&mut buf) {
            if frame_bytes.is_empty() {
                continue;
            }
            if process_frame(&frame_bytes, &mut builder, sink).await? {
                return builder.finish();
            }
        }
    }
    // Residual frame without a trailing blank line.
    if !buf.is_empty() {
        let tail = std::mem::take(&mut buf);
        process_frame(&tail, &mut builder, sink).await?;
    }
    if !builder.terminated {
        builder.close_all_open(sink).await;
        let _ = sink.send(StreamEvent::MessageStop).await;
        builder.terminated = true;
    }
    builder.finish()
}

/// Parse one SSE frame's `data:` payload and dispatch it. Returns `Ok(true)`
/// if the terminal `[DONE]` sentinel fired and the caller should finish.
async fn process_frame(
    frame_bytes: &[u8],
    builder: &mut ResponseBuilder,
    sink: &mpsc::Sender<StreamEvent>,
) -> Result<bool, AdapterError> {
    let Some(data) = parse_data_line(frame_bytes) else {
        return Ok(false);
    };
    if data == "[DONE]" {
        builder.close_all_open(sink).await;
        let _ = sink.send(StreamEvent::MessageStop).await;
        builder.terminated = true;
        return Ok(true);
    }
    let json: Value = serde_json::from_str(&data)
        .map_err(|e| AdapterError::invalid_request(format!("sse data not json: {e}")))?;
    handle_chunk(json, builder, sink).await?;
    Ok(false)
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

fn parse_data_line(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut out = String::new();
    for raw in s.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("data:") {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

async fn handle_chunk(
    data: Value,
    builder: &mut ResponseBuilder,
    sink: &mpsc::Sender<StreamEvent>,
) -> Result<(), AdapterError> {
    // Inline error payload (some OpenAI-compatible providers deliver errors
    // on a 200 response).
    if let Some(err) = data.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
        let code = classify_error(kind);
        let retryable = matches!(
            code,
            AdapterErrorCode::RateLimited | AdapterErrorCode::Network | AdapterErrorCode::Timeout
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

    builder.ensure_started(sink).await;

    // Top-level usage (usually arrives with/after finish_reason).
    if let Some(usage) = data.get("usage") {
        let prompt = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let completion = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        builder.usage.input_tokens = prompt;
        builder.usage.output_tokens = completion;
        builder.pending_usage_delta = Some(UsageDelta {
            input_tokens: prompt,
            output_tokens: completion,
        });
    }

    let choice = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

    // Text fragment.
    if let Some(text_frag) = delta.get("content").and_then(Value::as_str) {
        if !text_frag.is_empty() {
            let idx = builder.ensure_text_slot(sink).await;
            if let Some(BlockSlot::Text { text }) = builder.slot_mut(idx) {
                text.push_str(text_frag);
            }
            let _ = sink
                .send(StreamEvent::TextDelta {
                    index: idx,
                    text: text_frag.to_string(),
                })
                .await;
        }
    }

    // Tool-call fragments.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            handle_tool_call_fragment(tc, builder, sink).await;
        }
    }

    // Finish reason (terminal chunk).
    let finish = choice.get("finish_reason").and_then(Value::as_str);
    if let Some(reason) = finish {
        let stop = parse_finish_reason(reason);
        builder.stop_reason = Some(stop);
        let usage_delta = builder.pending_usage_delta.take().unwrap_or(UsageDelta {
            input_tokens: 0,
            output_tokens: 0,
        });
        let _ = sink
            .send(StreamEvent::MessageDelta {
                stop_reason: Some(stop),
                usage_delta,
            })
            .await;
    } else if let Some(delta_usage) = builder.pending_usage_delta.take() {
        // Usage arrived without finish_reason in the same chunk.
        let _ = sink
            .send(StreamEvent::MessageDelta {
                stop_reason: None,
                usage_delta: delta_usage,
            })
            .await;
    }

    Ok(())
}

async fn handle_tool_call_fragment(
    tc: &Value,
    builder: &mut ResponseBuilder,
    sink: &mpsc::Sender<StreamEvent>,
) {
    let tool_index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
    let function = tc.get("function").cloned().unwrap_or(Value::Null);

    // Lazily create the slot on the first chunk for this tool_index. The
    // first chunk is the one that carries `id` and `function.name`.
    let stream_index = if let Some(&idx) = builder.tool_slot_index.get(&tool_index) {
        idx
    } else {
        let id = tc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let idx = builder.append_slot(BlockSlot::ToolUse {
            id: ToolUseId::from(id.clone()),
            name: name.clone(),
            partial_json: String::new(),
            input: None,
        });
        builder.tool_slot_index.insert(tool_index, idx);
        let _ = sink
            .send(StreamEvent::ContentBlockStart {
                index: idx,
                block: ContentBlockStub::ToolUse {
                    id: ToolUseId::from(id),
                    name,
                },
            })
            .await;
        idx
    };

    if let Some(args_frag) = function.get("arguments").and_then(Value::as_str) {
        if !args_frag.is_empty() {
            if let Some(BlockSlot::ToolUse { partial_json, .. }) = builder.slot_mut(stream_index) {
                partial_json.push_str(args_frag);
            }
            let _ = sink
                .send(StreamEvent::InputJsonDelta {
                    index: stream_index,
                    partial_json: args_frag.to_string(),
                })
                .await;
        }
    }
}

fn classify_error(kind: &str) -> AdapterErrorCode {
    if kind.contains("rate_limit") || kind == "overloaded_error" {
        AdapterErrorCode::RateLimited
    } else if kind.contains("auth") || kind == "permission_error" || kind == "invalid_api_key" {
        AdapterErrorCode::AuthFailed
    } else if kind == "invalid_request_error" {
        AdapterErrorCode::InvalidRequest
    } else if kind == "context_length_exceeded" {
        AdapterErrorCode::ContextTooLong
    } else {
        AdapterErrorCode::Network
    }
}

fn parse_finish_reason(s: &str) -> StopReason {
    match s {
        "stop" | "end_turn" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

struct ResponseBuilder {
    slots: Vec<Option<BlockSlot>>,
    open: Vec<bool>,
    text_slot_index: Option<usize>,
    tool_slot_index: HashMap<u64, usize>,
    call_group: CallGroupId,
    started: bool,
    stop_reason: Option<StopReason>,
    usage: Usage,
    pending_usage_delta: Option<UsageDelta>,
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
}

impl ResponseBuilder {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            open: Vec::new(),
            text_slot_index: None,
            tool_slot_index: HashMap::new(),
            call_group: CallGroupId::new(),
            started: false,
            stop_reason: None,
            usage: Usage::default(),
            pending_usage_delta: None,
            terminated: false,
        }
    }

    async fn ensure_started(&mut self, sink: &mpsc::Sender<StreamEvent>) {
        if !self.started {
            self.started = true;
            let _ = sink.send(StreamEvent::MessageStart).await;
        }
    }

    async fn ensure_text_slot(&mut self, sink: &mpsc::Sender<StreamEvent>) -> usize {
        if let Some(idx) = self.text_slot_index {
            return idx;
        }
        let idx = self.append_slot(BlockSlot::Text {
            text: String::new(),
        });
        self.text_slot_index = Some(idx);
        let _ = sink
            .send(StreamEvent::ContentBlockStart {
                index: idx,
                block: ContentBlockStub::Text,
            })
            .await;
        idx
    }

    fn append_slot(&mut self, slot: BlockSlot) -> usize {
        let idx = self.slots.len();
        self.slots.push(Some(slot));
        self.open.push(true);
        idx
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut BlockSlot> {
        self.slots.get_mut(index).and_then(Option::as_mut)
    }

    /// Close any still-open slot: parse accumulated tool_use arguments and
    /// emit `ContentBlockStop` events in stream order.
    async fn close_all_open(&mut self, sink: &mpsc::Sender<StreamEvent>) {
        for idx in 0..self.slots.len() {
            if !self.open[idx] {
                continue;
            }
            if let Some(BlockSlot::ToolUse {
                partial_json,
                input,
                ..
            }) = self.slots[idx].as_mut()
            {
                let parsed = if partial_json.is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str::<Value>(partial_json)
                        .unwrap_or_else(|_| Value::Object(Map::new()))
                };
                *input = Some(parsed);
            }
            self.open[idx] = false;
            let _ = sink
                .send(StreamEvent::ContentBlockStop { index: idx })
                .await;
        }
    }

    fn finish(self) -> Result<ModelTurnResponse, AdapterError> {
        let group = self.call_group;
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
                    call_group: Some(group),
                },
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
    use futures::stream;

    fn fixture_stream(bytes: &'static [u8]) -> impl Stream<Item = Result<Bytes, AdapterError>> {
        // 17-byte chunks force cross-chunk frame splitting.
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
        let result = consume_openai_sse(Box::pin(stream), &tx).await;
        drop(tx);
        let events = collector.await.unwrap();
        (result, events)
    }

    const TEXT_ONLY: &[u8] = include_bytes!("../../tests/fixtures/openai/text_only.sse");
    const PARALLEL_TOOL_CALLS: &[u8] =
        include_bytes!("../../tests/fixtures/openai/parallel_tool_calls.sse");
    const ERROR_FRAME: &[u8] = include_bytes!("../../tests/fixtures/openai/error_frame.sse");

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
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "Hel")));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "lo ")));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "world")));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn parses_parallel_tool_calls_into_one_group() {
        let (resp, events) = drain(PARALLEL_TOOL_CALLS).await;
        let resp = resp.expect("parallel_tool_calls fixture should parse");
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 20);
        assert_eq!(resp.usage.output_tokens, 9);
        assert_eq!(resp.content.len(), 2);

        let groups: Vec<Option<CallGroupId>> = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { call_group, .. } => Some(*call_group),
                _ => None,
            })
            .collect();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].is_some(), "tool blocks must carry a call_group");
        assert_eq!(groups[0], groups[1], "parallel calls must share a group");

        match &resp.content[0] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id.as_str(), "tu_a");
                assert_eq!(name, "repo.search");
                assert_eq!(input.get("q").and_then(Value::as_str), Some("x"));
            }
            other => panic!("expected tool_use block, got {other:?}"),
        }
        match &resp.content[1] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id.as_str(), "tu_b");
                assert_eq!(name, "repo.read");
                assert_eq!(input.get("p").and_then(Value::as_str), Some("/a"));
            }
            other => panic!("expected tool_use block, got {other:?}"),
        }

        // Both tool blocks announced via ContentBlockStart with distinct
        // monotonic stream indices.
        let starts: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![0, 1]);

        // At least two InputJsonDelta events per tool slot (arguments split).
        let deltas_tu0 = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::InputJsonDelta { index: 0, .. }))
            .count();
        let deltas_tu1 = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::InputJsonDelta { index: 1, .. }))
            .count();
        assert_eq!(deltas_tu0, 2);
        assert_eq!(deltas_tu1, 2);

        // ContentBlockStop fires for both in order before MessageStop.
        let stops: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockStop { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(stops, vec![0, 1]);
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
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
