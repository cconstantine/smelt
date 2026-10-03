mod chat;
mod git;
mod language_servers;
mod mcp_servers;
mod pods;
mod providers;
mod sandbox_volumes;

pub use chat::Chat;
pub use git::GitSettingsPage;
pub use language_servers::{LanguageServerEdit, LanguageServerNew, LanguageServersIndex};
pub use mcp_servers::{McpServerEdit, McpServerNew, McpServersIndex};
pub use pods::PodsIndex;
pub(crate) use providers::ModelPicker;
pub use providers::{ProviderEdit, ProviderNew, ProvidersIndex};
pub use sandbox_volumes::{SandboxVolumeNew, SandboxVolumesIndex};

use dioxus::prelude::*;

/// A server function's error as the viewer should read it — the server's
/// own message, without the wrapper `ServerFnError`'s `Display` adds around
/// it ("error running server function: … (details: None)").
pub(crate) fn server_error_message(error: &ServerFnError) -> String {
    match error {
        ServerFnError::ServerError { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// The label of a two-step button (click once to arm, again to confirm).
/// Both labels sit in the same spot and the inactive one is hidden, so the
/// button is always as wide as the longer label: arming it doesn't resize
/// it or move anything around it, and the confirming click lands where the
/// first one did. Hidden text is left out of `innerText` and of what a
/// screen reader reads.
#[component]
pub(crate) fn TwoStepLabel(armed: bool, idle: &'static str, confirm: &'static str) -> Element {
    rsx! {
        span { class: "two-step-label",
            span { class: if armed { "two-step-text hidden" } else { "two-step-text" }, "{idle}" }
            span { class: if armed { "two-step-text two-step-confirm" } else { "two-step-text two-step-confirm hidden" }, "{confirm}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_error_message_shows_just_the_server_message() {
        let error = ServerFnError::ServerError {
            message: "failed to load https://x/: net::ERR_BLOCKED_BY_CLIENT".to_string(),
            code: 500,
            details: None,
        };
        assert_eq!(
            server_error_message(&error),
            "failed to load https://x/: net::ERR_BLOCKED_BY_CLIENT"
        );
    }
}
