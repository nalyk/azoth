//! Provider adapters. The runtime speaks Anthropic Messages content-block
//! shape internally; OpenAI Chat Completions is downcast on the wire.
//!
//! No stateful chaining on the provider side — Azoth owns continuity.

mod error;
pub mod profile;
mod sse;
mod stream;
mod anthropic_messages;
mod openai_chat_completions;
mod mock;

pub use error::{AdapterError, TokenCount};
pub use profile::{ProviderProfile, TokenizerFamily, ToolUseShape};
pub use anthropic_messages::AnthropicMessagesAdapter;
pub use openai_chat_completions::OpenAiChatCompletionsAdapter;
pub use mock::{MockAdapter, MockScript};

use crate::schemas::{ModelTurnRequest, ModelTurnResponse, StreamEvent};
use async_trait::async_trait;
use tokio::sync::mpsc;

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn profile(&self) -> &ProviderProfile;

    /// Drive one turn through the provider. StreamEvents are pushed onto
    /// `sink` as they arrive; the final `ModelTurnResponse` is returned when
    /// the stream closes cleanly.
    async fn invoke(
        &self,
        req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError>;

    /// Pre-flight token count for the final packet. This is *not* used for
    /// in-loop budgeting — the Context Kernel tokenizes locally (MED-1).
    async fn count_tokens(&self, req: &ModelTurnRequest) -> Result<TokenCount, AdapterError>;
}
