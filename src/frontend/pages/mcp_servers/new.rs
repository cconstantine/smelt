use dioxus::prelude::*;

use crate::api::mcp::create_mcp_server;
use crate::frontend::Route;

use super::{header_rows, headers_from_rows};

/// A dedicated page for creating a new server — on success, navigates
/// straight to that server's edit page so its live status is the first
/// thing you see.
#[component]
pub fn McpServerNew() -> Element {
    let navigator = use_navigator();
    let mut name: Signal<String> = use_signal(String::new);
    let mut url: Signal<String> = use_signal(String::new);
    let headers: Signal<Vec<(String, String)>> =
        use_signal(|| vec![(String::new(), String::new())]);
    // "static_headers" or "oauth" — see McpServerSummary::auth_mode. Fixed
    // for the lifetime of a server once created (see McpServerEdit's doc
    // comment on why switching modes isn't supported yet); this radio is
    // the only place it's ever chosen.
    let mut auth_mode: Signal<String> = use_signal(|| "static_headers".to_string());
    // A provider with no OAuth discovery metadata and no Dynamic Client
    // Registration support (GitHub, confirmed live — see
    // SME-16's "Live verification") needs a
    // client pre-registered by hand instead of the default DCR flow.
    // Optional and blank by default — most MCP-spec servers need neither.
    // Set only here, at creation time: same "delete and recreate to
    // change" precedent as `auth_mode` itself.
    let mut oauth_client_id: Signal<String> = use_signal(String::new);
    let mut oauth_client_secret: Signal<String> = use_signal(String::new);
    let mut submit_error: Signal<Option<String>> = use_signal(|| None);

    let submit = move |evt: Event<FormData>| {
        evt.prevent_default();
        let name_value = name();
        let url_value = url();
        let headers_value = headers_from_rows(headers());
        let auth_mode_value = auth_mode();
        let client_id_value = (!oauth_client_id().trim().is_empty()).then(|| oauth_client_id());
        let client_secret_value =
            (!oauth_client_secret().trim().is_empty()).then(|| oauth_client_secret());
        spawn(async move {
            match create_mcp_server(
                name_value,
                url_value,
                headers_value,
                auth_mode_value,
                client_id_value,
                client_secret_value,
            )
            .await
            {
                Ok(summary) => {
                    navigator.push(Route::McpServerEditRoute { id: summary.id });
                }
                Err(e) => submit_error.set(Some(super::server_error_message(&e))),
            }
        });
    };

    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::McpServersRoute {}, class: "mcp-back-link", "\u{2190} Back to MCP servers" }
                h1 { "Add an MCP server" }
            }

            form { class: "mcp-add-form", onsubmit: submit,
                label { r#for: "mcp-new-name", "Name" }
                input {
                    id: "mcp-new-name",
                    r#type: "text",
                    required: true,
                    value: "{name}",
                    oninput: move |e| name.set(e.value()),
                }
                label { r#for: "mcp-new-url", "URL" }
                input {
                    id: "mcp-new-url",
                    r#type: "text",
                    required: true,
                    placeholder: "https://api.githubcopilot.com/mcp/",
                    value: "{url}",
                    oninput: move |e| url.set(e.value()),
                }
                label { "Authentication" }
                div { class: "mcp-auth-mode-choice",
                    label {
                        input {
                            r#type: "radio",
                            name: "mcp-new-auth-mode",
                            checked: auth_mode() == "static_headers",
                            onclick: move |_| auth_mode.set("static_headers".to_string()),
                        }
                        "Static headers"
                    }
                    label {
                        input {
                            r#type: "radio",
                            name: "mcp-new-auth-mode",
                            checked: auth_mode() == "oauth",
                            onclick: move |_| auth_mode.set("oauth".to_string()),
                        }
                        "OAuth"
                    }
                }
                if auth_mode() == "static_headers" {
                    label { "Extra headers (optional)" }
                    {header_rows(headers, "Header value")}
                } else {
                    label { r#for: "mcp-new-oauth-client-id", "Client ID (optional)" }
                    input {
                        id: "mcp-new-oauth-client-id",
                        r#type: "text",
                        placeholder: "Leave blank to use dynamic client registration",
                        value: "{oauth_client_id}",
                        oninput: move |e| oauth_client_id.set(e.value()),
                    }
                    label { r#for: "mcp-new-oauth-client-secret", "Client secret (optional)" }
                    input {
                        id: "mcp-new-oauth-client-secret",
                        r#type: "password",
                        placeholder: "Only needed alongside a client ID",
                        value: "{oauth_client_secret}",
                        oninput: move |e| oauth_client_secret.set(e.value()),
                    }
                    p { class: "muted",
                        "Most MCP servers support dynamic client registration and need neither field. A provider that doesn't (e.g. GitHub) needs an OAuth app registered by hand with its own client ID/secret."
                    }
                    p { class: "muted",
                        "Save the server first, then connect it to its OAuth provider from its own page."
                    }
                }
                super::ErrorText { message: submit_error() }
                button { r#type: "submit", "Add server" }
            }
        }
    }
}
