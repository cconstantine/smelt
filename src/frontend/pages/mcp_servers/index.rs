use dioxus::prelude::*;

use crate::api::mcp::{
    McpConnectionStatus, McpServerSummary, list_mcp_servers, mcp_server_status,
};
use crate::frontend::Route;

use super::status_summary;

/// Lists every configured server with a live "connected" indicator — see
/// SME-15. Each row links to
/// `Route::McpServerEditRoute`; adding a server is its own page
/// (`Route::McpServerNewRoute`) rather than an inline form here.
#[component]
pub fn McpServersIndex() -> Element {
    let initial_servers = use_resource(list_mcp_servers);
    let mut servers: Signal<Vec<McpServerSummary>> = use_signal(Vec::new);
    let mut loaded = use_signal(|| false);
    let mut list_error: Signal<Option<String>> = use_signal(|| None);

    use_effect(move || {
        if let Some(result) = initial_servers() {
            match result {
                Ok(list) => servers.set(list),
                Err(e) => list_error.set(Some(super::server_error_message(&e))),
            }
            loaded.set(true);
        }
    });

    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::Home {}, class: "mcp-back-link", "\u{2190} Back to conversations" }
                h1 { "MCP servers" }
                p { class: "muted",
                    "Every tool a configured server exposes is available to the model in every conversation."
                }
                Link { to: Route::McpServerNewRoute {}, class: "mcp-new-server-link", "+ Add a server" }
            }

            super::ErrorText { message: list_error() }

            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if servers().is_empty() {
                p { class: "muted", "No MCP servers configured yet." }
            } else {
                div { class: "mcp-server-list",
                    for server in servers() {
                        Link {
                            key: "{server.id}",
                            to: Route::McpServerEditRoute { id: server.id },
                            class: "mcp-server-row mcp-server-row-link",
                            div { class: "mcp-server-summary",
                                span { class: "mcp-server-name", "{server.name}" }
                                span { class: "mcp-server-url", "{server.url}" }
                            }
                            McpServerStatusBadge { id: server.id }
                        }
                    }
                }
            }
        }
    }
}

/// One server's live status, fetched independently per row so a slow or
/// unreachable server doesn't hold up the rest of the list from
/// rendering.
#[component]
fn McpServerStatusBadge(id: i64) -> Element {
    let status = use_resource(move || mcp_server_status(id));

    match status() {
        None => rsx! { span { class: "mcp-status mcp-status-checking", "Checking\u{2026}" } },
        Some(Ok(McpConnectionStatus::Connected { tool_names })) => rsx! {
            span { class: "mcp-status mcp-status-connected", "{status_summary(&tool_names)}" }
        },
        Some(Ok(McpConnectionStatus::Unreachable { error })) => rsx! {
            span { class: "mcp-status mcp-status-unreachable", title: "{error}", "Unreachable" }
        },
        Some(Ok(McpConnectionStatus::NotConnected)) => rsx! {
            span { class: "mcp-status mcp-status-not-connected", "Not connected" }
        },
        Some(Ok(McpConnectionStatus::NeedsReconnect { error })) => rsx! {
            span { class: "mcp-status mcp-status-needs-reconnect", title: "{error}", "Sign-in expired" }
        },
        Some(Err(e)) => rsx! {
            span { class: "mcp-status mcp-status-unreachable", title: "{super::server_error_message(&e)}", "Error" }
        },
    }
}
