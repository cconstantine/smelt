use dioxus::prelude::*;

use crate::api::mcp::{delete_mcp_server, disconnect_mcp_server_oauth, start_mcp_server_oauth};
use crate::frontend::Route;

/// The OAuth connection: which client signs in, and Connect, Reconnect
/// and Disconnect. `signed_in` and `can_disconnect` come from the page's
/// connection status.
#[component]
pub(super) fn McpOAuthPanel(id: i64, oauth_client_id: Signal<Option<String>>, has_oauth_client_secret: Signal<bool>, oauth_error: Signal<Option<String>>, status_reload: Signal<u32>, signed_in: bool, can_disconnect: bool) -> Element {
    let mut oauth_error = oauth_error;
    let mut status_reload = status_reload;
    // --- OAuth connect/reconnect/disconnect ---
    let mut oauth_connecting = use_signal(|| false);

    let connect_oauth = move |_| {
        oauth_connecting.set(true);
        oauth_error.set(None);
        spawn(async move {
            match start_mcp_server_oauth(id).await {
                // Prefixed with `_` since it's only read under `#[cfg(feature =
                // "web")]` below — the server build (e.g. `cargo test
                // --features server`) never touches it.
                // B4 (SME-122): the server refuses anything else already;
                // this keeps the page from navigating to a non-http(s) URL
                // even if that check is ever lost.
                Ok(url) if !crate::api::mcp::is_navigable_authorization_url(&url) => {
                    oauth_error.set(Some(
                        "The sign-in address isn't an http or https URL, so smelt won't open it."
                            .to_string(),
                    ));
                    oauth_connecting.set(false);
                }
                Ok(_url) => {
                    // A real full-page navigation, not a client-side route
                    // push — this has to leave smelt entirely for the
                    // provider's own login/consent page. `document::eval`
                    // JSON-encodes the URL into the script rather than
                    // string-interpolating it directly, so nothing in it
                    // can break out of the JS string literal.
                    #[cfg(feature = "web")]
                    {
                        let script = format!(
                            "window.location.href = {};",
                            serde_json::to_string(&_url).unwrap_or_default()
                        );
                        let _ = document::eval(&script).await;
                    }
                }
                Err(e) => {
                    oauth_error.set(Some(super::server_error_message(&e)));
                    oauth_connecting.set(false);
                }
            }
        });
    };

    let mut disconnect_error: Signal<Option<String>> = use_signal(|| None);
    let disconnect_oauth = move |_| {
        spawn(async move {
            match disconnect_mcp_server_oauth(id).await {
                Ok(()) => {
                    disconnect_error.set(None);
                    status_reload.set(status_reload() + 1);
                }
                Err(e) => disconnect_error.set(Some(super::server_error_message(&e))),
            }
        });
    };

    rsx! {
        div { class: "mcp-oauth-panel",
            h2 { "OAuth connection" }
            p { class: "muted",
                if let Some(client_id) = oauth_client_id() {
                    "Pre-registered client ID: {client_id}"
                    if has_oauth_client_secret() {
                        " (with a client secret)"
                    }
                } else {
                    "Using dynamic client registration \u{2014} no pre-registered client."
                }
            }
            super::ErrorText { message: oauth_error() }
            super::ErrorText { message: disconnect_error() }
            div { class: "mcp-oauth-actions",
                button {
                    class: "mcp-oauth-connect",
                    r#type: "button",
                    disabled: oauth_connecting(),
                    onclick: connect_oauth,
                    if signed_in {
                        "Reconnect"
                    } else {
                        "Connect"
                    }
                }
                if can_disconnect {
                    button {
                        class: "mcp-oauth-disconnect",
                        r#type: "button",
                        onclick: disconnect_oauth,
                        "Disconnect"
                    }
                }
            }
        }
    }
}

/// Deleting the server, in two steps; a delete that works goes back to
/// the list.
#[component]
pub(super) fn McpDeleteSection(id: i64) -> Element {
    let navigator = use_navigator();
    // --- Delete (arm/confirm, same pattern as ConversationSidebar) ---
    let mut pending_delete = use_signal(|| false);
    let mut delete_error: Signal<Option<String>> = use_signal(|| None);

    let confirm_delete = move |_| {
        spawn(async move {
            match delete_mcp_server(id).await {
                Ok(()) => {
                    navigator.push(Route::McpServersRoute {});
                }
                Err(e) => delete_error.set(Some(super::server_error_message(&e))),
            }
        });
    };

    rsx! {
        div { class: "mcp-danger-section",
            h2 { "Delete" }
            super::ErrorText { message: delete_error() }
            super::TwoStepButton {
                armed: pending_delete(),
                class: "mcp-delete",
                idle: "Delete server",
                confirm: "Confirm delete?",
                on_arm: move |_| pending_delete.set(true),
                on_confirm: confirm_delete,
            }
        }
    }
}
