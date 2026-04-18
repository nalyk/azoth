//! Post-v2 Sprint-7.5 adapter enhancement: Anthropic Messages adapter
//! accepts both `sk-ant-api03-*` API keys and `sk-ant-oat01-*` OAuth
//! tokens, picking the right auth header shape per token type.
//!
//! Header contract (pinned here, verified empirically via wiremock
//! header matchers):
//!
//! ```text
//!   sk-ant-api03-*  →  x-api-key: <token>
//!                      (no Authorization, no anthropic-beta: oauth-*)
//!
//!   sk-ant-oat01-*  →  Authorization: Bearer <token>
//!                      anthropic-beta: oauth-2025-04-20
//!                      (no x-api-key)
//! ```
//!
//! If Anthropic changes the OAuth beta flag name, THESE tests will keep
//! passing (they assert the literal `oauth-2025-04-20`) while production
//! will 401. That is deliberate: the test's job is to pin today's wire
//! shape so future drift is detected as a production incident rather
//! than a silent regression in the test suite.

use azoth_core::adapter::{AnthropicMessagesAdapter, ProviderAdapter, ProviderProfile};
use azoth_core::authority::SecretHandle;
use azoth_core::schemas::{
    ContentBlock, Message, ModelTurnRequest, RequestMetadata, Role, StopReason, StreamEvent,
};
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

/// Asserts a specific header is ABSENT from the request. wiremock's
/// built-in `header()` matcher only proves presence; when two token
/// types share an endpoint, we also need to prove the OTHER auth shape
/// is NOT sent alongside (Anthropic rejects ambiguous auth).
struct HeaderAbsent(&'static str);

impl Match for HeaderAbsent {
    fn matches(&self, request: &Request) -> bool {
        request.headers.get(self.0).is_none()
    }
}

fn empty_request() -> ModelTurnRequest {
    ModelTurnRequest {
        system: String::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        tools: vec![],
        max_tokens: 128,
        cache_hints: Default::default(),
        metadata: RequestMetadata {
            run_id: "run_auth_test".into(),
            turn_id: "t_auth_test".into(),
        },
    }
}

fn adapter_with(base_url: &str, token: &str) -> AnthropicMessagesAdapter {
    let mut profile = ProviderProfile::anthropic_default("claude-sonnet");
    profile.base_url = base_url.to_string();
    AnthropicMessagesAdapter::new(profile, SecretHandle::new(token))
}

#[tokio::test]
async fn api_key_token_emits_x_api_key_and_no_oauth_headers() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/anthropic/text_only.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-api03-demo"))
        .and(HeaderAbsent("authorization"))
        .and(HeaderAbsent("anthropic-beta"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let adapter = adapter_with(&server.uri(), "sk-ant-api03-demo");
    let (sink_tx, mut sink_rx) = mpsc::channel::<StreamEvent>(64);
    let resp = adapter
        .invoke(empty_request(), sink_tx)
        .await
        .expect("adapter invoke");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    while let Some(_ev) = sink_rx.recv().await {}
}

#[tokio::test]
async fn oauth_token_emits_bearer_plus_beta_header_and_no_x_api_key() {
    let server = MockServer::start().await;
    let body = tokio::fs::read("tests/fixtures/anthropic/text_only.sse")
        .await
        .expect("read fixture");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer sk-ant-oat01-demo"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let adapter = adapter_with(&server.uri(), "sk-ant-oat01-demo");
    let (sink_tx, mut sink_rx) = mpsc::channel::<StreamEvent>(64);
    let resp = adapter
        .invoke(empty_request(), sink_tx)
        .await
        .expect("adapter invoke");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    while let Some(_ev) = sink_rx.recv().await {}
}
