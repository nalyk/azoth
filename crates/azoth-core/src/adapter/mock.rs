//! Deterministic mock adapter for TurnDriver tests and TUI smoke.

use super::stream::emit_synthetic_stream;
use super::{error::TokenCount, AdapterError, ProviderAdapter, ProviderProfile};
use crate::schemas::{
    ContentBlock, ModelTurnRequest, ModelTurnResponse, StopReason, StreamEvent, Usage,
};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// A script the mock adapter replays turn by turn.
#[derive(Debug, Clone, Default)]
pub struct MockScript {
    pub turns: Vec<ModelTurnResponse>,
}

pub struct MockAdapter {
    profile: ProviderProfile,
    script: std::sync::Mutex<std::collections::VecDeque<ModelTurnResponse>>,
}

impl MockAdapter {
    pub fn new(profile: ProviderProfile, script: MockScript) -> Self {
        Self {
            profile,
            script: std::sync::Mutex::new(script.turns.into()),
        }
    }

    pub fn echo(profile: ProviderProfile) -> Self {
        Self::new(
            profile,
            MockScript {
                turns: vec![ModelTurnResponse {
                    content: vec![ContentBlock::Text { text: "hello from mock".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage { input_tokens: 1, output_tokens: 4, ..Default::default() },
                }],
            },
        )
    }
}

#[async_trait]
impl ProviderAdapter for MockAdapter {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn invoke(
        &self,
        _req: ModelTurnRequest,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError> {
        let response = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AdapterError::invalid_request("mock script exhausted"))?;

        emit_synthetic_stream(&response, &sink).await;
        Ok(response)
    }

    async fn count_tokens(&self, _req: &ModelTurnRequest) -> Result<TokenCount, AdapterError> {
        Ok(TokenCount { input_tokens: 0 })
    }
}
