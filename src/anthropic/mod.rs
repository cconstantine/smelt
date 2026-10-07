pub mod types;

#[cfg(feature = "server")]
pub mod stream;

#[cfg(feature = "server")]
pub mod models;

pub mod tools;

pub use types::{ContentBlock, Effort, TokenUsage, ToolDefinition};
// The rest of the Messages API request/response shape is only built and
// sent by `stream.rs`, which is itself server-only (see its own `#[cfg]`
// above) — the `web` (browser) build never touches them, only the shared
// types above.
#[cfg(feature = "server")]
pub use types::{AnthropicMessage, CreateMessageRequest, OutputConfig, ThinkingConfig};

/// Known context-window size (real token count) for a recognized
/// `claude-*` model id — nothing in the Messages API surfaces this, so it
/// can't be derived or queried, only looked up. `None` for anything else
/// (a gateway or local Ollama model has no "Anthropic model name" to match
/// at all) — `providers::context_window` uses a provider's reported size
/// or the user's override first (SME-72). See SME-18.
#[cfg(feature = "server")]
pub fn context_window_for(model: &str) -> Option<u32> {
    if model.starts_with("claude-") {
        Some(200_000)
    } else {
        None
    }
}

#[cfg(feature = "server")]
#[cfg(test)]
mod context_window_tests {
    use super::context_window_for;

    #[test]
    fn test_context_window_for_recognizes_claude_model_ids() {
        assert_eq!(context_window_for("claude-opus-4-8"), Some(200_000));
        assert_eq!(context_window_for("claude-sonnet-4-5"), Some(200_000));
    }

    #[test]
    fn test_context_window_for_unrecognized_model_is_none() {
        assert_eq!(context_window_for("gpt-oss:120b_128k"), None);
        assert_eq!(context_window_for("llama3"), None);
    }
}
