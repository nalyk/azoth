use crate::schemas::AdapterErrorCode;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("adapter error [{code:?}, retryable={retryable}]: {detail}")]
pub struct AdapterError {
    pub code: AdapterErrorCode,
    pub retryable: bool,
    pub provider_status: Option<u16>,
    pub detail: String,
}

impl AdapterError {
    pub fn network(detail: impl Into<String>) -> Self {
        Self {
            code: AdapterErrorCode::Network,
            retryable: true,
            provider_status: None,
            detail: detail.into(),
        }
    }

    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self {
            code: AdapterErrorCode::InvalidRequest,
            retryable: false,
            provider_status: None,
            detail: detail.into(),
        }
    }

    pub fn unknown(detail: impl Into<String>) -> Self {
        Self {
            code: AdapterErrorCode::Unknown,
            retryable: false,
            provider_status: None,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCount {
    pub input_tokens: u32,
}
