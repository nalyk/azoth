//! Serde types shared across every Azoth subsystem.
//!
//! This is the type hub. Changes here ripple everywhere; stability is the goal.

mod content;
mod contract;
mod effect;
mod event;
mod ids;
mod turn;

pub use content::*;
pub use contract::*;
pub use effect::*;
pub use event::*;
pub use ids::*;
pub use turn::*;

use serde::{Deserialize, Serialize};

/// Token usage reported by a provider adapter for a single model turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

/// Incremental usage delta streamed mid-turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDelta {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

impl Usage {
    pub fn total(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }
}
