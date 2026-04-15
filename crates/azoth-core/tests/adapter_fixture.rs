//! Headless integration test for `AnthropicMessagesAdapter`. Starts a local
//! wiremock server, serves the on-disk SSE fixtures as the body of
//! `POST /v1/messages`, and exercises the real HTTP + SSE parser path that
//! the other integration tests skip (they all use `MockAdapter`).
//!
//! These tests are the only place the adapter's reqwest client is actually
//! dialed against a running socket; the sub-frame chunk tests in
//! `adapter/sse.rs` feed bytes through `consume_anthropic_sse` directly
//! without involving `invoke()`.

use azoth_core::adapter::{
    AdapterError, AnthropicMessagesAdapter, ProviderAdapter, ProviderProfile,
};
use azoth_core::authority::SecretHandle;
use azoth_core::schemas::{
    AdapterErrorCode, ContentBlock, Message, ModelTurnRequest, RequestMetadata, Role, StopReason,
    StreamEvent,
};
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn empty_request() -> ModelTurnRequest {
    ModelTurnRequest {
        system: "you are a test".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }],
        tools: vec![],
        max_tokens: 256,
        cache_hints: Default::default(),
        metadata: RequestMetadata {
            run_id: "run_fixture".into(),
            turn_id: "t_fixture".into(),
        },
    }
}

fn adapter_for(base_url: &str) -> AnthropicMessagesAdapter {
    let mut profile = ProviderProfile::anthropic_default("claude-sonnet");
    profile.base_url = base_url.to_string();
    AnthropicMessagesAdapter::new(profile, SecretHandle::new("sk-fixture"))
}

#[tokio::test]
async fn happy_path_parses_text_only_fixture() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/anthropic/text_only.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-fixture"))
        .and(header("accept", "text/event-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let adapter = adapter_for(&server.uri());
    let (sink_tx, mut sink_rx) = mpsc::channel::<StreamEvent>(64);
    let resp = adapter
        .invoke(empty_request(), sink_tx)
        .await
        .expect("adapter invoke");

    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    // usage: input_tokens from message_start (12), output_tokens from
    // message_delta (25).
    assert_eq!(resp.usage.input_tokens, 12);
    assert_eq!(resp.usage.output_tokens, 25);
    assert_eq!(resp.content.len(), 1);
    match &resp.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
        other => panic!("expected Text block, got {other:?}"),
    }

    // The sink must have received at least one StreamEvent — we don't pin the
    // exact shape here (the sub-frame chunk tests in adapter/sse.rs cover
    // that), but the invoke contract says stream events are emitted as frames
    // arrive.
    let mut saw_any = false;
    while sink_rx.try_recv().is_ok() {
        saw_any = true;
    }
    assert!(saw_any, "sink should have received at least one StreamEvent");
}

#[tokio::test]
async fn error_frame_in_200_body_maps_to_rate_limited() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/anthropic/error.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let adapter = adapter_for(&server.uri());
    let (sink_tx, _sink_rx) = mpsc::channel::<StreamEvent>(64);
    let err: AdapterError = adapter
        .invoke(empty_request(), sink_tx)
        .await
        .expect_err("overloaded_error must surface as AdapterError");
    assert_eq!(err.code, AdapterErrorCode::RateLimited);
}

#[tokio::test]
async fn non_200_status_maps_to_adapter_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(
                "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"bad key\"}}",
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let adapter = adapter_for(&server.uri());
    let (sink_tx, _sink_rx) = mpsc::channel::<StreamEvent>(64);
    let err = adapter
        .invoke(empty_request(), sink_tx)
        .await
        .expect_err("401 must surface as AdapterError");
    assert_eq!(err.code, AdapterErrorCode::AuthFailed);
    assert_eq!(err.provider_status, Some(401));
}
