//! Live-HTTP smoke test for the real runtime adapters. Dials the actual
//! provider endpoints using an API key from the environment, so the real
//! reqwest + TLS + SSE path gets end-to-end coverage against a *running*
//! Anthropic / OpenRouter backend — not a local wiremock fixture.
//!
//! Off by default. CI stays fully offline unless the operator opts in with
//! `AZOTH_LIVE_ADAPTER=1`. When the gate is off every test in this file is
//! a no-op that returns immediately. When the gate is on but the required
//! provider key is missing the test panics loudly, because a live run
//! without a key is an operator error, not a silent pass.
//!
//! Env vars:
//!   AZOTH_LIVE_ADAPTER=1              — master gate, required for any dial
//!   ANTHROPIC_API_KEY=sk-ant-...      — required for the Anthropic case
//!   AZOTH_LIVE_ANTHROPIC_MODEL=...    — optional override (default: a
//!                                       current small/cheap Claude)
//!   OPENROUTER_API_KEY=sk-or-...      — required for the OpenRouter case
//!   AZOTH_LIVE_OPENROUTER_MODEL=...   — optional override (default: a
//!                                       current small/cheap model)
//!
//! These tests intentionally send a 1-sentence "reply with PONG" prompt
//! with a low max_tokens cap to keep the per-run cost under a fraction of
//! a cent. They do not assert on the exact reply text — providers drift —
//! only that stop_reason is EndTurn, usage is non-zero, at least one Text
//! block came back, and the stream sink saw at least one event.

use std::time::Duration;

use azoth_core::adapter::{
    AnthropicMessagesAdapter, OpenAiChatCompletionsAdapter, ProviderAdapter, ProviderProfile,
};
use azoth_core::authority::SecretHandle;
use azoth_core::schemas::{
    ContentBlock, Message, ModelTurnRequest, RequestMetadata, Role, StopReason, StreamEvent,
};
use tokio::sync::mpsc;

const LIVE_GATE: &str = "AZOTH_LIVE_ADAPTER";

fn live_enabled() -> bool {
    matches!(
        std::env::var(LIVE_GATE).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

fn ping_request() -> ModelTurnRequest {
    ModelTurnRequest {
        system: "You are a terse test harness. Reply with a single word.".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Reply with the single word PONG and nothing else.".into(),
            }],
        }],
        tools: vec![],
        max_tokens: 32,
        cache_hints: Default::default(),
        metadata: RequestMetadata {
            run_id: "run_live_smoke".into(),
            turn_id: "t_live_smoke".into(),
        },
    }
}

fn drain_sink(sink_rx: &mut mpsc::Receiver<StreamEvent>) -> usize {
    let mut n = 0;
    while sink_rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

#[tokio::test]
async fn anthropic_live_invoke_roundtrip() {
    if !live_enabled() {
        eprintln!("skipping: {LIVE_GATE} not set");
        return;
    }
    let key = std::env::var("ANTHROPIC_API_KEY")
        .expect("AZOTH_LIVE_ADAPTER=1 but ANTHROPIC_API_KEY is missing");
    let model = std::env::var("AZOTH_LIVE_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());

    let profile = ProviderProfile::anthropic_default(model);
    let adapter = AnthropicMessagesAdapter::new(profile, SecretHandle::new(key));

    let (sink_tx, mut sink_rx) = mpsc::channel::<StreamEvent>(256);
    let resp = tokio::time::timeout(
        Duration::from_secs(30),
        adapter.invoke(ping_request(), sink_tx),
    )
    .await
    .expect("adapter timed out after 30s")
    .expect("live anthropic invoke");

    assert!(
        matches!(resp.stop_reason, StopReason::EndTurn | StopReason::MaxTokens),
        "unexpected stop_reason: {:?}",
        resp.stop_reason
    );
    assert!(resp.usage.input_tokens > 0, "input_tokens should be > 0");
    assert!(resp.usage.output_tokens > 0, "output_tokens should be > 0");

    let text_blocks: Vec<&String> = resp
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        !text_blocks.is_empty(),
        "expected at least one Text block, got {:?}",
        resp.content
    );
    eprintln!("anthropic live reply: {:?}", text_blocks);

    let events = drain_sink(&mut sink_rx);
    assert!(events > 0, "sink should have received at least one StreamEvent");
}

#[tokio::test]
async fn openrouter_live_invoke_roundtrip() {
    if !live_enabled() {
        eprintln!("skipping: {LIVE_GATE} not set");
        return;
    }
    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("AZOTH_LIVE_ADAPTER=1 but OPENROUTER_API_KEY is missing");
    let model = std::env::var("AZOTH_LIVE_OPENROUTER_MODEL")
        .unwrap_or_else(|_| "openai/gpt-4o-mini".into());

    let profile = ProviderProfile::openrouter_default(model);
    let adapter = OpenAiChatCompletionsAdapter::new(profile, SecretHandle::new(key));

    let (sink_tx, mut sink_rx) = mpsc::channel::<StreamEvent>(256);
    let resp = tokio::time::timeout(
        Duration::from_secs(30),
        adapter.invoke(ping_request(), sink_tx),
    )
    .await
    .expect("adapter timed out after 30s")
    .expect("live openrouter invoke");

    assert!(
        matches!(resp.stop_reason, StopReason::EndTurn | StopReason::MaxTokens),
        "unexpected stop_reason: {:?}",
        resp.stop_reason
    );
    assert!(resp.usage.input_tokens > 0, "input_tokens should be > 0");
    assert!(resp.usage.output_tokens > 0, "output_tokens should be > 0");

    let text_blocks: Vec<&String> = resp
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        !text_blocks.is_empty(),
        "expected at least one Text block, got {:?}",
        resp.content
    );
    eprintln!("openrouter live reply: {:?}", text_blocks);

    let events = drain_sink(&mut sink_rx);
    assert!(events > 0, "sink should have received at least one StreamEvent");
}
