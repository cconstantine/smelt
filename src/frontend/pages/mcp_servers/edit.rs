use dioxus::prelude::*;

use crate::api::mcp::{McpConnectionStatus, get_mcp_server, mcp_server_status};
use crate::frontend::Route;

use super::form::McpServerEditForm;
use super::oauth::{McpDeleteSection, McpOAuthPanel};
use super::status_summary;
use super::tool_classes::McpToolClassesSection;

/// The full-detail view for one server: its live connection status (every
/// tool it currently exposes, or why it's unreachable), the Tool-classes
/// section (SME-240), editable
/// name/URL, either in-place header editing or an OAuth connection panel
/// depending on `auth_mode` (see `McpServerEdit`'s headers section below
/// for how a header value's real content, which the browser never
/// receives, can still be edited without retyping every other header),
/// and delete.
///
/// `auth_mode` itself is fixed once a server is created — chosen only on
/// `McpServerNew`'s form — so there's no control here to switch a server
/// between static headers and OAuth in place; see
/// SME-16's "UI flow" for why (delete and
/// re-create is the escape hatch for this pass).
#[component]
pub fn McpServerEdit(id: i64) -> Element {

    let initial_server = use_resource(move || get_mcp_server(id));
    let mut loaded = use_signal(|| false);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);

    let mut edit_name: Signal<String> = use_signal(String::new);
    let mut edit_url: Signal<String> = use_signal(String::new);
    let mut edit_auth_mode: Signal<String> = use_signal(|| "static_headers".to_string());
    // Read-only display only (see McpServerNew's doc comment on why these
    // aren't editable here).
    let mut oauth_client_id: Signal<Option<String>> = use_signal(|| None);
    let mut has_oauth_client_secret: Signal<bool> = use_signal(|| false);
    // The last-saved name/URL — compared against `edit_name`/`edit_url`
    // below to decide whether the form actually has unsaved changes.
    // Updated on load and again after every successful save (see
    // `save_all`), never by typing.
    let mut saved_name: Signal<String> = use_signal(String::new);
    let mut saved_url: Signal<String> = use_signal(String::new);
    // One row per currently-configured header: `(name, new_value)`, value
    // starting blank meaning "leave this header exactly as it is" — the
    // browser never has the real value to prefill, so blank can't mean
    // "clear it" the way it would in a normal form.
    let mut existing_header_edits: Signal<Vec<(String, String)>> = use_signal(Vec::new);

    use_effect(move || {
        if let Some(result) = initial_server() {
            match result {
                Ok(summary) => {
                    edit_name.set(summary.name.clone());
                    edit_url.set(summary.url.clone());
                    edit_auth_mode.set(summary.auth_mode);
                    oauth_client_id.set(summary.oauth_client_id);
                    has_oauth_client_secret.set(summary.has_oauth_client_secret);
                    saved_name.set(summary.name);
                    saved_url.set(summary.url);
                    existing_header_edits.set(
                        summary
                            .header_names
                            .into_iter()
                            .map(|name| (name, String::new()))
                            .collect(),
                    );
                }
                Err(e) => load_error.set(Some(super::server_error_message(&e))),
            }
            loaded.set(true);
        }
    });

    // A failed OAuth attempt lands back here as `?oauth_callback=1` (see
    // `mcp_oauth::callback_handler`): the error itself stays on the server,
    // and is taken from there once. Its text never comes from the URL, so
    // a link can't put its own words in smelt's error styling (SME-168).
    // The flag is read on mount via a JS eval (there's no query-param
    // extraction wired into `Route` yet) and stripped, so a refresh doesn't
    // ask again.
    let oauth_error: Signal<Option<String>> = use_signal(|| None);
    #[cfg(feature = "web")]
    use_effect(move || {
        spawn(async move {
            let mut oauth_error = oauth_error;
            let script = "const p = new URLSearchParams(window.location.search);
                 const flagged = p.has('oauth_callback');
                 if (flagged) {
                     p.delete('oauth_callback');
                     const rest = p.toString();
                     window.history.replaceState({}, '', window.location.pathname + (rest ? '?' + rest : ''));
                 }
                 return flagged;";
            let flagged = document::eval(script).await.ok().and_then(|v| v.as_bool());
            if flagged == Some(true) {
                if let Ok(Some(text)) = crate::api::mcp::take_mcp_server_oauth_error(id).await {
                    oauth_error.set(Some(text));
                }
            }
        });
    });

    // Bumping this re-runs the status resource below — used after a
    // successful save, since `update_mcp_server` evicts the old cached
    // connection server-side (see `crate::mcp::evict`), so the previous
    // status reading is stale the moment a save succeeds.
    let mut status_reload: Signal<u32> = use_signal(|| 0);
    let status = use_resource(move || {
        let _ = status_reload();
        mcp_server_status(id)
    });
    // `Resource::state()` flips to `Pending` the instant the resource's
    // future restarts (both the initial load and every later bump of
    // `status_reload`) and back to `Ready` once it resolves — exactly the
    // "is a connection check in flight right now" signal the Refresh
    // button's spinner needs, without a second signal to keep in sync.
    let status_loading = matches!(status.state()(), UseResourceState::Pending);

    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::McpServersRoute {}, class: "mcp-back-link", "\u{2190} Back to MCP servers" }
                h1 { "Edit MCP server" }
            }

            super::ErrorText { message: load_error() }

            if !loaded() {
                p { class: "muted", "Loading..." }
            } else {
                div { class: "mcp-status-section",
                    h2 { "Status" }
                    {match status() {
                        None => rsx! { p { class: "muted", "Checking connection\u{2026}" } },
                        Some(Ok(McpConnectionStatus::Connected { tool_names })) => rsx! {
                            div { class: "mcp-status mcp-status-connected",
                                p { "{status_summary(&tool_names)}" }
                                ul { class: "mcp-tool-list",
                                    for name in tool_names {
                                        li { key: "{name}", "{name}" }
                                    }
                                }
                            }
                        },
                        Some(Ok(McpConnectionStatus::Unreachable { error })) => rsx! {
                            div { class: "mcp-status mcp-status-unreachable",
                                p { "Unreachable" }
                                p { class: "error", "{error}" }
                            }
                        },
                        Some(Ok(McpConnectionStatus::NotConnected)) => rsx! {
                            div { class: "mcp-status mcp-status-not-connected",
                                p { "Not connected \u{2014} use Connect below." }
                            }
                        },
                        Some(Ok(McpConnectionStatus::NeedsReconnect { error })) => rsx! {
                            div { class: "mcp-status mcp-status-needs-reconnect",
                                p { "Sign-in expired \u{2014} use Reconnect below." }
                                p { class: "mcp-status-detail", "{error}" }
                            }
                        },
                        Some(Err(e)) => rsx! {
                            div { class: "mcp-status mcp-status-unreachable",
                                super::ErrorText { message: Some(super::server_error_message(&e)) }
                            }
                        },
                    }}
                    button {
                        class: if status_loading { "mcp-refresh-status loading" } else { "mcp-refresh-status" },
                        r#type: "button",
                        disabled: status_loading,
                        onclick: move |_| status_reload.set(status_reload() + 1),
                        if status_loading {
                            span { class: "mcp-save-spinner mcp-refresh-spinner" }
                            "Refreshing\u{2026}"
                        } else {
                            "Refresh"
                        }
                    }
                }

                McpToolClassesSection { id }

                McpServerEditForm {
                    id,
                    edit_name,
                    edit_url,
                    edit_auth_mode,
                    saved_name,
                    saved_url,
                    existing_header_edits,
                    status_reload,
                }

                if edit_auth_mode() == "oauth" {
                    McpOAuthPanel {
                        id,
                        oauth_client_id,
                        has_oauth_client_secret,
                        oauth_error,
                        status_reload,
                        signed_in: matches!(status(), Some(Ok(McpConnectionStatus::Connected { .. } | McpConnectionStatus::NeedsReconnect { .. }))),
                        can_disconnect: matches!(status(), Some(Ok(McpConnectionStatus::Connected { .. } | McpConnectionStatus::Unreachable { .. } | McpConnectionStatus::NeedsReconnect { .. }))),
                    }
                }

                McpDeleteSection { id }
            }
        }
    }
}
