use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerFamily {
    Anthropic,
    OpenAiCl100k,
    OpenAiO200k,
    SentencepieceLlama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUseShape {
    /// Anthropic-style content-block `tool_use` / `tool_result`.
    ContentBlock,
    /// OpenAI-style flat `tool_calls` array with separate `role=tool`
    /// reply messages.
    FlatToolCalls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: String,
    pub base_url: String,
    pub model_id: String,
    pub tokenizer_family: TokenizerFamily,
    pub supports_native_cache: bool,
    pub supports_strict_json_schema: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub tool_use_shape: ToolUseShape,
    /// Extra headers. Values are *not* secrets — see `SecretHandle` for the
    /// auth header, injected at call time by the adapter implementation.
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
}

impl ProviderProfile {
    pub fn anthropic_default(model_id: impl Into<String>) -> Self {
        Self {
            id: "anthropic_messages".into(),
            base_url: "https://api.anthropic.com".into(),
            model_id: model_id.into(),
            tokenizer_family: TokenizerFamily::Anthropic,
            supports_native_cache: true,
            supports_strict_json_schema: false,
            max_context_tokens: 200_000,
            max_output_tokens: 8_192,
            tool_use_shape: ToolUseShape::ContentBlock,
            // `anthropic-version` is set unconditionally by the adapter's
            // `invoke()` — it's a protocol requirement, not a per-profile
            // knob. Pre-v2 shipped it in `extra_headers` AND in the
            // builder chain, sending the header twice on every request.
            // Gemini MED on PR #12 caught the duplicate; cleaned up here.
            extra_headers: vec![],
        }
    }

    pub fn openrouter_default(model_id: impl Into<String>) -> Self {
        Self {
            id: "openai_chat_completions".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            model_id: model_id.into(),
            tokenizer_family: TokenizerFamily::OpenAiCl100k,
            supports_native_cache: false,
            supports_strict_json_schema: true,
            max_context_tokens: 128_000,
            max_output_tokens: 8_192,
            tool_use_shape: ToolUseShape::FlatToolCalls,
            extra_headers: vec![("X-OpenRouter-Strict".into(), "1".into())],
        }
    }

    pub fn openai_default(model_id: impl Into<String>) -> Self {
        Self {
            id: "openai_chat_completions".into(),
            base_url: "https://api.openai.com/v1".into(),
            model_id: model_id.into(),
            tokenizer_family: TokenizerFamily::OpenAiO200k,
            supports_native_cache: false,
            supports_strict_json_schema: true,
            max_context_tokens: 128_000,
            max_output_tokens: 16_384,
            tool_use_shape: ToolUseShape::FlatToolCalls,
            extra_headers: vec![],
        }
    }

    pub fn ollama_anthropic(model_id: impl Into<String>) -> Self {
        Self {
            id: "ollama_anthropic".into(),
            base_url: "http://localhost:11434".into(),
            model_id: model_id.into(),
            tokenizer_family: TokenizerFamily::SentencepieceLlama,
            supports_native_cache: false,
            supports_strict_json_schema: false,
            // F3 2026-04-24: bumped from 32_768 → 131_072. On 2026-04-24 E2E
            // run_f9c7978e66de the packet compiler estimated 36 072 tokens
            // by turn 3 on qwen3.6:27b — azoth-side budget rejected before
            // the network call. qwen3/3.5/3.6 Modelfiles default to 128k+
            // num_ctx; 131_072 is the floor that lets retrieval-heavy
            // conversations run past the third exchange. Ollama-side
            // num_ctx is a separate plumbing concern (we don't forward
            // it in build_body today) — tracked as F3(b) follow-up.
            max_context_tokens: 131_072,
            max_output_tokens: 8_192,
            tool_use_shape: ToolUseShape::ContentBlock,
            // See `anthropic_default` for why this is empty — the adapter
            // sends `anthropic-version` itself; leaving it in extras
            // caused a duplicate on every outgoing request (gemini MED
            // on PR #12).
            extra_headers: vec![],
        }
    }

    pub fn ollama_openai(model_id: impl Into<String>) -> Self {
        Self {
            id: "ollama_openai".into(),
            base_url: "http://localhost:11434/v1".into(),
            model_id: model_id.into(),
            tokenizer_family: TokenizerFamily::SentencepieceLlama,
            supports_native_cache: false,
            supports_strict_json_schema: false,
            // F3 2026-04-24: see ollama_anthropic for rationale.
            max_context_tokens: 131_072,
            max_output_tokens: 8_192,
            tool_use_shape: ToolUseShape::FlatToolCalls,
            extra_headers: vec![],
        }
    }

    pub fn generic(
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        tool_use_shape: ToolUseShape,
    ) -> Self {
        Self {
            id: "generic".into(),
            base_url: base_url.into(),
            model_id: model_id.into(),
            tokenizer_family: TokenizerFamily::OpenAiCl100k,
            supports_native_cache: false,
            supports_strict_json_schema: false,
            max_context_tokens: 128_000,
            max_output_tokens: 8_192,
            tool_use_shape,
            extra_headers: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_anthropic_uses_content_block_shape() {
        let p = ProviderProfile::ollama_anthropic("qwen-9b");
        assert_eq!(p.tool_use_shape, ToolUseShape::ContentBlock);
        assert_eq!(p.base_url, "http://localhost:11434");
        assert_eq!(p.model_id, "qwen-9b");
        assert!(!p.supports_native_cache);
        // F3 2026-04-24: pinned at 131_072 so qwen3/3.5/3.6
        // conversations don't saturate azoth-side budget after 3
        // turns. If this is lowered back to 32_768, reconcile with
        // the E2E observation documented in .azoth/test-reports/.
        assert_eq!(
            p.max_context_tokens, 131_072,
            "ollama context budget pinned at 131072 (F3 2026-04-24)"
        );
    }

    #[test]
    fn ollama_openai_uses_flat_tool_calls() {
        let p = ProviderProfile::ollama_openai("qwen-9b");
        assert_eq!(p.tool_use_shape, ToolUseShape::FlatToolCalls);
        assert_eq!(p.base_url, "http://localhost:11434/v1");
        assert_eq!(
            p.max_context_tokens, 131_072,
            "ollama context budget pinned at 131072 (F3 2026-04-24)"
        );
    }

    #[test]
    fn openai_default_targets_api_openai() {
        let p = ProviderProfile::openai_default("gpt-4o");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.tool_use_shape, ToolUseShape::FlatToolCalls);
    }

    #[test]
    fn generic_accepts_arbitrary_base_url() {
        let p = ProviderProfile::generic(
            "http://my-vllm:8000/v1",
            "my-model",
            ToolUseShape::FlatToolCalls,
        );
        assert_eq!(p.id, "generic");
        assert_eq!(p.base_url, "http://my-vllm:8000/v1");
    }
}
