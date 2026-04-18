//! Headless integration test for `OpenAiChatCompletionsAdapter`. Mirror of
//! `adapter_fixture.rs` — starts a local wiremock server, serves the on-disk
//! OpenAI SSE fixtures as the body of `POST /chat/completions`, and exercises
//! the real reqwest + `consume_openai_sse` path that the other integration
//! tests skip (they all use `MockAdapter`).
//!
//! The sub-frame chunk tests in `adapter/openai_sse.rs` feed bytes through
//! `consume_openai_sse` directly without invoking the adapter; these tests
//! are the only place the OpenAI adapter's HTTP client is actually dialed
//! against a running socket.

use azoth_core::adapter::{
    AdapterError, OpenAiChatCompletionsAdapter, ProviderAdapter, ProviderProfile,
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

fn adapter_for(base_url: &str) -> OpenAiChatCompletionsAdapter {
    // `openrouter_default` ships with `base_url = "https://openrouter.ai/api/v1"`
    // but we overwrite that; the adapter appends `/chat/completions` to whatever
    // base_url we hand it, so mounting the mock on `/chat/completions` is
    // enough — no `/v1` prefix required.
    let mut profile = ProviderProfile::openrouter_default("openai/gpt-4");
    profile.base_url = base_url.to_string();
    OpenAiChatCompletionsAdapter::new(profile, SecretHandle::new("sk-fixture"))
}

#[tokio::test]
async fn happy_path_parses_text_only_fixture() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/openai/text_only.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-fixture"))
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
    // usage: prompt_tokens=12, completion_tokens=25 on the closing chunk.
    assert_eq!(resp.usage.input_tokens, 12);
    assert_eq!(resp.usage.output_tokens, 25);
    assert_eq!(resp.content.len(), 1);
    match &resp.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
        other => panic!("expected Text block, got {other:?}"),
    }

    let mut saw_any = false;
    while sink_rx.try_recv().is_ok() {
        saw_any = true;
    }
    assert!(
        saw_any,
        "sink should have received at least one StreamEvent"
    );
}

#[tokio::test]
async fn parallel_tool_calls_fixture_yields_two_tool_uses() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/openai/parallel_tool_calls.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
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
    let resp = adapter
        .invoke(empty_request(), sink_tx)
        .await
        .expect("adapter invoke");

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.input_tokens, 20);
    assert_eq!(resp.usage.output_tokens, 9);
    assert_eq!(resp.content.len(), 2);

    // HIGH-3: both parallel tool slots must share one CallGroupId minted
    // once per stream.
    let groups: Vec<_> = resp
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { call_group, .. } => Some(*call_group),
            _ => None,
        })
        .collect();
    assert_eq!(groups.len(), 2);
    assert!(groups[0].is_some());
    assert_eq!(groups[0], groups[1]);

    match &resp.content[0] {
        ContentBlock::ToolUse { id, name, .. } => {
            assert_eq!(id.as_str(), "tu_a");
            assert_eq!(name, "repo_search");
        }
        other => panic!("expected ToolUse block, got {other:?}"),
    }
    match &resp.content[1] {
        ContentBlock::ToolUse { id, name, .. } => {
            assert_eq!(id.as_str(), "tu_b");
            assert_eq!(name, "repo.read");
        }
        other => panic!("expected ToolUse block, got {other:?}"),
    }
}

#[tokio::test]
async fn error_frame_in_200_body_maps_to_rate_limited() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/openai/error_frame.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
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
        .expect_err("rate_limit_exceeded must surface as AdapterError");
    assert_eq!(err.code, AdapterErrorCode::RateLimited);
    assert!(err.retryable);
}

#[tokio::test]
async fn non_200_status_maps_to_adapter_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(
                "{\"error\":{\"type\":\"invalid_api_key\",\"message\":\"bad key\"}}",
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
