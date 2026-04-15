//! Local tokenizer dispatch. Packing decisions never call the network
//! (MED-1 fix).

pub use crate::adapter::TokenizerFamily;

/// Approximate tokens for a given text. For OpenAI families we use
/// `tiktoken-rs`; for Anthropic and sentencepiece we fall back to a cheap
/// chars/4 heuristic that overestimates slightly — good enough for packing
/// decisions, and never used for billing.
pub fn count_tokens(text: &str, family: TokenizerFamily) -> usize {
    match family {
        TokenizerFamily::OpenAiCl100k => count_with_tiktoken(text, "cl100k_base"),
        TokenizerFamily::OpenAiO200k => count_with_tiktoken(text, "o200k_base"),
        TokenizerFamily::Anthropic | TokenizerFamily::SentencepieceLlama => {
            approx_chars_div_four(text)
        }
    }
}

fn count_with_tiktoken(text: &str, model: &str) -> usize {
    // `tiktoken-rs` exposes synchronous helpers. Any construction failure
    // falls back to the heuristic so the Context Kernel never panics on an
    // unusual environment.
    let bpe = match model {
        "cl100k_base" => tiktoken_rs::cl100k_base().ok(),
        "o200k_base" => tiktoken_rs::o200k_base().ok(),
        _ => None,
    };
    match bpe {
        Some(b) => b.encode_with_special_tokens(text).len(),
        None => approx_chars_div_four(text),
    }
}

fn approx_chars_div_four(text: &str) -> usize {
    (text.chars().count() + 3) / 4
}
