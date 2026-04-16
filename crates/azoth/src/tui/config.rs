//! Profile registry and adapter construction. Profiles are named
//! configurations that pair an adapter kind (wire format) with a provider
//! endpoint. The same model (e.g. Qwen via Ollama) can be served through
//! different endpoint styles — the profile captures this choice.
//!
//! Selection: `AZOTH_PROFILE` env var (default: `ollama-qwen-anthropic`).
//! Per-session overrides: `AZOTH_API_KEY`, `AZOTH_BASE_URL`, `AZOTH_MODEL`.

use azoth_core::adapter::{
    AnthropicMessagesAdapter, OpenAiChatCompletionsAdapter, ProviderAdapter, ProviderProfile,
    TokenizerFamily,
};
use azoth_core::authority::SecretHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    AnthropicMessages,
    OpenAiChatCompletions,
}

#[derive(Debug, Clone)]
pub struct ProfileEntry {
    pub name: String,
    pub adapter_kind: AdapterKind,
    pub base_url: String,
    pub model_id: String,
    pub api_key_env: Option<String>,
    pub tokenizer_family: TokenizerFamily,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

pub fn built_in_profiles() -> Vec<ProfileEntry> {
    vec![
        ProfileEntry {
            name: "ollama-qwen-anthropic".into(),
            adapter_kind: AdapterKind::AnthropicMessages,
            base_url: "http://localhost:11434".into(),
            model_id: "nalyk-qwen35-opus-9b".into(),
            api_key_env: None,
            tokenizer_family: TokenizerFamily::SentencepieceLlama,
            max_context_tokens: 32_768,
            max_output_tokens: 8_192,
        },
        ProfileEntry {
            name: "ollama-qwen-openai".into(),
            adapter_kind: AdapterKind::OpenAiChatCompletions,
            base_url: "http://localhost:11434/v1".into(),
            model_id: "nalyk-qwen35-opus-9b".into(),
            api_key_env: None,
            tokenizer_family: TokenizerFamily::SentencepieceLlama,
            max_context_tokens: 32_768,
            max_output_tokens: 8_192,
        },
        ProfileEntry {
            name: "anthropic".into(),
            adapter_kind: AdapterKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            model_id: "claude-sonnet-4-6".into(),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            tokenizer_family: TokenizerFamily::Anthropic,
            max_context_tokens: 200_000,
            max_output_tokens: 8_192,
        },
        ProfileEntry {
            name: "openai".into(),
            adapter_kind: AdapterKind::OpenAiChatCompletions,
            base_url: "https://api.openai.com/v1".into(),
            model_id: "gpt-4o".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
            tokenizer_family: TokenizerFamily::OpenAiO200k,
            max_context_tokens: 128_000,
            max_output_tokens: 16_384,
        },
        ProfileEntry {
            name: "openrouter".into(),
            adapter_kind: AdapterKind::OpenAiChatCompletions,
            base_url: "https://openrouter.ai/api/v1".into(),
            model_id: "anthropic/claude-sonnet-4-6".into(),
            api_key_env: Some("OPENROUTER_API_KEY".into()),
            tokenizer_family: TokenizerFamily::OpenAiCl100k,
            max_context_tokens: 128_000,
            max_output_tokens: 8_192,
        },
    ]
}

pub fn resolve_profile() -> ProfileEntry {
    let profile_name = env_or("AZOTH_PROFILE", "ollama-qwen-anthropic");
    let profiles = built_in_profiles();

    let mut entry = profiles
        .into_iter()
        .find(|p| p.name == profile_name)
        .unwrap_or_else(|| {
            tracing::warn!(
                profile = %profile_name,
                "unknown profile, falling back to generic anthropic"
            );
            ProfileEntry {
                name: profile_name.clone(),
                adapter_kind: AdapterKind::AnthropicMessages,
                base_url: "http://localhost:11434".into(),
                model_id: profile_name,
                api_key_env: None,
                tokenizer_family: TokenizerFamily::SentencepieceLlama,
                max_context_tokens: 32_768,
                max_output_tokens: 8_192,
            }
        });

    if let Ok(v) = std::env::var("AZOTH_BASE_URL") {
        entry.base_url = v;
    }
    if let Ok(v) = std::env::var("AZOTH_MODEL") {
        entry.model_id = v;
    }
    if std::env::var("AZOTH_API_KEY").is_ok() {
        entry.api_key_env = Some("AZOTH_API_KEY".into());
    }

    entry
}

pub fn build_adapter(entry: &ProfileEntry) -> Box<dyn ProviderAdapter> {
    let api_key = entry
        .api_key_env
        .as_ref()
        .and_then(|var| std::env::var(var).ok())
        .unwrap_or_default();
    let secret = SecretHandle::new(api_key);

    match entry.adapter_kind {
        AdapterKind::AnthropicMessages => {
            let mut profile = ProviderProfile::ollama_anthropic(&entry.model_id);
            profile.base_url = entry.base_url.clone();
            profile.id = entry.name.clone();
            profile.tokenizer_family = entry.tokenizer_family;
            profile.max_context_tokens = entry.max_context_tokens;
            profile.max_output_tokens = entry.max_output_tokens;
            if entry.name == "anthropic" {
                profile.supports_native_cache = true;
            }
            Box::new(AnthropicMessagesAdapter::new(profile, secret))
        }
        AdapterKind::OpenAiChatCompletions => {
            let mut profile = ProviderProfile::ollama_openai(&entry.model_id);
            profile.base_url = entry.base_url.clone();
            profile.id = entry.name.clone();
            profile.tokenizer_family = entry.tokenizer_family;
            profile.max_context_tokens = entry.max_context_tokens;
            profile.max_output_tokens = entry.max_output_tokens;
            Box::new(OpenAiChatCompletionsAdapter::new(profile, secret))
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::adapter::ToolUseShape;

    #[test]
    fn built_in_profiles_has_all_providers() {
        let profiles = built_in_profiles();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"ollama-qwen-anthropic"));
        assert!(names.contains(&"ollama-qwen-openai"));
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"openrouter"));
    }

    #[test]
    fn ollama_anthropic_profile_uses_correct_adapter() {
        let profiles = built_in_profiles();
        let p = profiles
            .iter()
            .find(|p| p.name == "ollama-qwen-anthropic")
            .unwrap();
        assert_eq!(p.adapter_kind, AdapterKind::AnthropicMessages);
        assert_eq!(p.base_url, "http://localhost:11434");
        assert_eq!(p.model_id, "nalyk-qwen35-opus-9b");
        assert!(p.api_key_env.is_none());
    }

    #[test]
    fn ollama_openai_profile_uses_correct_adapter() {
        let profiles = built_in_profiles();
        let p = profiles
            .iter()
            .find(|p| p.name == "ollama-qwen-openai")
            .unwrap();
        assert_eq!(p.adapter_kind, AdapterKind::OpenAiChatCompletions);
        assert_eq!(p.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn build_adapter_anthropic_kind() {
        let entry = ProfileEntry {
            name: "test".into(),
            adapter_kind: AdapterKind::AnthropicMessages,
            base_url: "http://test:1234".into(),
            model_id: "test-model".into(),
            api_key_env: None,
            tokenizer_family: TokenizerFamily::Anthropic,
            max_context_tokens: 100_000,
            max_output_tokens: 4_096,
        };
        let adapter = build_adapter(&entry);
        assert_eq!(adapter.profile().base_url, "http://test:1234");
        assert_eq!(adapter.profile().model_id, "test-model");
        assert_eq!(adapter.profile().tool_use_shape, ToolUseShape::ContentBlock);
    }

    #[test]
    fn build_adapter_openai_kind() {
        let entry = ProfileEntry {
            name: "test-oai".into(),
            adapter_kind: AdapterKind::OpenAiChatCompletions,
            base_url: "http://test:5678/v1".into(),
            model_id: "test-model-oai".into(),
            api_key_env: None,
            tokenizer_family: TokenizerFamily::OpenAiCl100k,
            max_context_tokens: 64_000,
            max_output_tokens: 4_096,
        };
        let adapter = build_adapter(&entry);
        assert_eq!(adapter.profile().base_url, "http://test:5678/v1");
        assert_eq!(
            adapter.profile().tool_use_shape,
            ToolUseShape::FlatToolCalls
        );
    }
}
