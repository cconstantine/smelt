pub mod types;

#[cfg(feature = "server")]
pub mod stream;

pub mod tools;

pub use types::{ContentBlock, TokenUsage, ToolDefinition};
// The rest of the Messages API request/response shape is only built and
// sent by `stream.rs`, which is itself server-only (see its own `#[cfg]`
// above) — the `web` (browser) build never touches them, only the shared
// types above.
#[cfg(feature = "server")]
pub use types::{AnthropicMessage, CreateMessageRequest, ThinkingConfig};

/// Known context-window size (real token count) for a recognized
/// `claude-*` model id — nothing in the Messages API surfaces this, so it
/// can't be derived or queried, only looked up. `None` for anything else
/// (a gateway or local Ollama model has no "Anthropic model name" to match
/// at all) — `api::chat`'s caller falls back to `ANTHROPIC_CONTEXT_WINDOW`
/// in that case. See docs/projects/plans/auto-compaction.md.
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

/// Shared by every test (in this module, `stream.rs`, and `api::chat`) that
/// points the process-global `ANTHROPIC_BASE_URL` env var at a mock upstream
/// — without a shared lock, two such tests running on different OS threads
/// could each set the var to their own mock server's address and race, with
/// one test's HTTP client ending up pointed at the other's server.
#[cfg(feature = "server")]
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static ANTHROPIC_BASE_URL_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the lock for the duration of one test's mock-upstream
    /// interaction. Recovers from poisoning rather than propagating it, so
    /// one test panicking while holding this doesn't cascade into every
    /// other test that touches `ANTHROPIC_BASE_URL`.
    pub(crate) fn lock_anthropic_base_url() -> MutexGuard<'static, ()> {
        ANTHROPIC_BASE_URL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}
