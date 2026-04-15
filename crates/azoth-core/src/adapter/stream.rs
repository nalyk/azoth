//! Shared helpers for adapters that do not yet parse real SSE streams.
//!
//! Every v1 adapter makes a non-streaming HTTP request, parses the response
//! body into a `ModelTurnResponse`, then replays it onto the stream sink as
//! a synthetic `MessageStart` / `ContentBlockStart` / `ContentBlockStop` /
//! `MessageStop` sequence so downstream code is exercised uniformly. This
//! module centralises that replay so the three adapters (Anthropic, OpenAI,
//! Mock) don't each carry their own copy.

use crate::schemas::{
    AdapterErrorCode, ContentBlock, ContentBlockStub, ModelTurnResponse, StreamEvent, UsageDelta,
};
use tokio::sync::mpsc;

/// Replay a fully-parsed response onto the stream sink as if it were
/// streamed. Used by every v1 adapter that does not yet parse real SSE.
pub(super) async fn emit_synthetic_stream(
    response: &ModelTurnResponse,
    sink: &mpsc::Sender<StreamEvent>,
) {
    let _ = sink.send(StreamEvent::MessageStart).await;
    for (index, block) in response.content.iter().enumerate() {
        let stub = match block {
            ContentBlock::Text { .. } => ContentBlockStub::Text,
            ContentBlock::ToolUse { id, name, .. } => ContentBlockStub::ToolUse {
                id: id.clone(),
                name: name.clone(),
            },
            ContentBlock::Thinking { .. } => ContentBlockStub::Thinking,
            // ToolResult blocks never appear in assistant responses.
            ContentBlock::ToolResult { .. } => continue,
        };
        let _ = sink
            .send(StreamEvent::ContentBlockStart { index, block: stub })
            .await;
        if let ContentBlock::Text { text } = block {
            let _ = sink
                .send(StreamEvent::TextDelta { index, text: text.clone() })
                .await;
        }
        let _ = sink.send(StreamEvent::ContentBlockStop { index }).await;
    }
    let _ = sink
        .send(StreamEvent::MessageDelta {
            stop_reason: Some(response.stop_reason),
            usage_delta: UsageDelta {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            },
        })
        .await;
    let _ = sink.send(StreamEvent::MessageStop).await;
}

/// Map an HTTP status code to an adapter error code. Shared across both
/// real provider adapters; their retry policies differ, so retryability is
/// decided at the call site.
pub(super) fn map_http_status(status: u16) -> AdapterErrorCode {
    match status {
        400 | 422 => AdapterErrorCode::InvalidRequest,
        401 | 403 => AdapterErrorCode::AuthFailed,
        413 => AdapterErrorCode::ContextTooLong,
        429 => AdapterErrorCode::RateLimited,
        408 | 504 => AdapterErrorCode::Timeout,
        500..=599 => AdapterErrorCode::Network,
        _ => AdapterErrorCode::Unknown,
    }
}
