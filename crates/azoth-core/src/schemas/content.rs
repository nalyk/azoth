//! Internal model protocol — Anthropic Messages content-block shape.
//!
//! The Azoth runtime speaks this shape everywhere; the OpenAI Chat Completions
//! adapter downcasts it on the wire. See `adapter::openai_chat_completions`.

use super::{CallGroupId, ToolUseId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// Content block. The `type` discriminator matches Anthropic wire shape so
/// serde round-trips with minimal adaptation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        /// Preserves the original OpenAI parallel-tool batch ordering across
        /// the Azoth ↔ adapter boundary. None for Anthropic-native turns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_group: Option<CallGroupId>,
    },
    ToolResult {
        tool_use_id: ToolUseId,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// JSON Schema for a tool — not a free-form string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
