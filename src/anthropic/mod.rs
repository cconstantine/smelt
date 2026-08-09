pub mod types;

#[cfg(feature = "server")]
pub mod stream;

pub mod tools;

pub use types::{AnthropicMessage, ContentBlock, CreateMessageRequest, ToolDefinition};

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
