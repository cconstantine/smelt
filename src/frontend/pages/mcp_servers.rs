use std::collections::HashMap;

use dioxus::prelude::*;

use crate::api::mcp::{
    McpConnectionStatus, McpServerSummary, create_mcp_server, delete_mcp_server, disconnect_mcp_server_oauth,
    get_mcp_server, list_mcp_servers, mcp_server_status, start_mcp_server_oauth, update_mcp_server,
};
use crate::frontend::Route;

/// Renders a signal-backed list of `(header name, header value)` rows with
/// add/remove controls, shared by the "add a server" page and the edit
/// page's "replace headers" flow. A plain function (not a `#[component]`)
/// since it only needs to close over a `Signal` — signals are `Copy`, so
/// this works the same as any other closure-capturing helper already used
/// in this file (see `ConversationSidebar` for the same pattern with
/// per-row closures).
fn header_rows(mut headers: Signal<Vec<(String, String)>>, value_placeholder: &'static str) -> Element {
    rsx! {
        div { class: "mcp-header-rows",
            for (index , (name , value)) in headers().into_iter().enumerate() {
                div { key: "{index}", class: "mcp-header-row",
                    input {
                        r#type: "text",
                        class: "mcp-header-name",
                        placeholder: "Header name (e.g. Authorization)",
                        value: "{name}",
                        oninput: move |e| headers.write()[index].0 = e.value(),
                    }
                    input {
                        r#type: "text",
                        class: "mcp-header-value",
                        placeholder: value_placeholder,
                        value: "{value}",
                        oninput: move |e| headers.write()[index].1 = e.value(),
                    }
                    button {
                        r#type: "button",
                        class: "mcp-remove-header",
                        onclick: move |_| {
                            headers.write().remove(index);
                        },
                        "Remove"
                    }
                }
            }
            button {
                r#type: "button",
                class: "mcp-add-header",
                onclick: move |_| headers.write().push((String::new(), String::new())),
                "+ Add header"
            }
        }
    }
}

/// Turns the rows a header editor produced into the map the API expects —
/// rows with a blank name are dropped (an empty "+ Add header" row left
/// untouched shouldn't become a header named `""`).
fn headers_from_rows(rows: Vec<(String, String)>) -> HashMap<String, String> {
    rows.into_iter().filter(|(name, _)| !name.trim().is_empty()).collect()
}

/// A server's live connection status, fetched with a real connection
/// attempt (`crate::api::mcp::mcp_server_status`) rather than a cached
/// guess — see that function's doc comment for why. Shared rendering used
/// by both the index badge and the edit page's fuller status section.
fn status_summary(tool_names: &[String]) -> String {
    format!("Connected \u{2014} {} tool{}", tool_names.len(), if tool_names.len() == 1 { "" } else { "s" })
}

/// Lists every configured server with a live "connected" indicator — see
/// `docs/projects/completed/20260817-mcp-servers.md`. Each row links to
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
                Err(e) => list_error.set(Some(e.to_string())),
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

            if let Some(err) = list_error() {
                p { class: "error", "{err}" }
            }

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
        Some(Err(e)) => rsx! {
            span { class: "mcp-status mcp-status-unreachable", title: "{e}", "Error" }
        },
    }
}

/// A dedicated page for creating a new server — on success, navigates
/// straight to that server's edit page so its live status is the first
/// thing you see.
#[component]
pub fn McpServerNew() -> Element {
    let navigator = use_navigator();
    let mut name: Signal<String> = use_signal(String::new);
    let mut url: Signal<String> = use_signal(String::new);
    let headers: Signal<Vec<(String, String)>> = use_signal(|| vec![(String::new(), String::new())]);
    // "static_headers" or "oauth" — see McpServerSummary::auth_mode. Fixed
    // for the lifetime of a server once created (see McpServerEdit's doc
    // comment on why switching modes isn't supported yet); this radio is
    // the only place it's ever chosen.
    let mut auth_mode: Signal<String> = use_signal(|| "static_headers".to_string());
    // A provider with no OAuth discovery metadata and no Dynamic Client
    // Registration support (GitHub, confirmed live — see
    // docs/projects/plans/mcp-oauth.md's "Live verification") needs a
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
        let client_secret_value = (!oauth_client_secret().trim().is_empty()).then(|| oauth_client_secret());
        spawn(async move {
            match create_mcp_server(name_value, url_value, headers_value, auth_mode_value, client_id_value, client_secret_value)
                .await
            {
                Ok(summary) => {
                    navigator.push(Route::McpServerEditRoute { id: summary.id });
                }
                Err(e) => submit_error.set(Some(e.to_string())),
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
                if let Some(err) = submit_error() {
                    p { class: "error", "{err}" }
                }
                button { r#type: "submit", "Add server" }
            }
        }
    }
}

/// The full-detail view for one server: its live connection status (every
/// tool it currently exposes, or why it's unreachable), editable
/// name/URL, either in-place header editing or an OAuth connection panel
/// depending on `auth_mode` (see `McpServerEdit`'s headers section below
/// for how a header value's real content, which the browser never
/// receives, can still be edited without retyping every other header),
/// and delete.
///
/// `auth_mode` itself is fixed once a server is created — chosen only on
/// `McpServerNew`'s form — so there's no control here to switch a server
/// between static headers and OAuth in place; see
/// docs/projects/plans/mcp-oauth.md's "UI flow" for why (delete and
/// re-create is the escape hatch for this pass).
#[component]
pub fn McpServerEdit(id: i64) -> Element {
    let navigator = use_navigator();

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
                    existing_header_edits.set(summary.header_names.into_iter().map(|name| (name, String::new())).collect());
                }
                Err(e) => load_error.set(Some(e.to_string())),
            }
            loaded.set(true);
        }
    });

    // A failed OAuth attempt lands back here as `?oauth_error=...` (see
    // `mcp_oauth::callback_handler`) — read once on mount via a JS eval
    // (there's no query-param extraction wired into `Route` yet) and strip
    // it from the URL so a later refresh doesn't keep re-showing it.
    let mut oauth_error: Signal<Option<String>> = use_signal(|| None);
    #[cfg(feature = "web")]
    use_effect(move || {
        spawn(async move {
            let script = "const p = new URLSearchParams(window.location.search);
                 const err = p.get('oauth_error');
                 if (err !== null) {
                     p.delete('oauth_error');
                     const rest = p.toString();
                     window.history.replaceState({}, '', window.location.pathname + (rest ? '?' + rest : ''));
                 }
                 return err;";
            if let Ok(value) = document::eval(script).await {
                if let Some(err) = value.as_str() {
                    oauth_error.set(Some(err.to_string()));
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

    // Names marked for removal via an existing header row's "Remove"
    // button — separate from `existing_header_edits` since removing a
    // header is a distinct intent from "leave it alone" (a blank value
    // row).
    let mut removed_header_names: Signal<Vec<String>> = use_signal(Vec::new);
    // Brand-new headers to add, same shape/helper as the create-server form.
    let mut new_header_rows: Signal<Vec<(String, String)>> = use_signal(Vec::new);
    let mut save_error: Signal<Option<String>> = use_signal(|| None);
    let mut saving = use_signal(|| false);

    // Whether the form currently holds anything not yet saved — drives the
    // Save button's unchanged/changed state. A header row only counts once
    // it actually carries a pending value/name; an untouched "(unchanged)"
    // row or an empty "+ Add header" row isn't a real change.
    let is_dirty = edit_name() != saved_name()
        || edit_url() != saved_url()
        || existing_header_edits().iter().any(|(_, value)| !value.trim().is_empty())
        || !removed_header_names().is_empty()
        || new_header_rows().iter().any(|(name, _)| !name.trim().is_empty());

    // The whole page is one form with one save action: name, URL, and
    // every header change all go to the server together — see
    // `crate::api::mcp::update_mcp_server`'s doc comment for why headers
    // are sent as an upsert/remove pair rather than a full value dump.
    let save_all = move |evt: Event<FormData>| {
        evt.prevent_default();
        let name = edit_name();
        let url = edit_url();
        let mut upsert: HashMap<String, String> =
            existing_header_edits().into_iter().filter(|(_, value)| !value.trim().is_empty()).collect();
        upsert.extend(headers_from_rows(new_header_rows()));
        let remove = removed_header_names();
        let auth_mode = edit_auth_mode();
        saving.set(true);
        spawn(async move {
            match update_mcp_server(id, name, url, upsert, remove, auth_mode).await {
                Ok(summary) => {
                    saved_name.set(summary.name.clone());
                    saved_url.set(summary.url.clone());
                    edit_name.set(summary.name);
                    edit_url.set(summary.url);
                    existing_header_edits.set(summary.header_names.into_iter().map(|name| (name, String::new())).collect());
                    removed_header_names.set(Vec::new());
                    new_header_rows.set(Vec::new());
                    save_error.set(None);
                    status_reload.set(status_reload() + 1);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
            saving.set(false);
        });
    };

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
                    oauth_error.set(Some(e.to_string()));
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
                Err(e) => disconnect_error.set(Some(e.to_string())),
            }
        });
    };

    // --- Delete (arm/confirm, same pattern as ConversationSidebar) ---
    let mut pending_delete = use_signal(|| false);
    let mut delete_error: Signal<Option<String>> = use_signal(|| None);

    let request_delete = move |_| {
        if pending_delete() {
            spawn(async move {
                match delete_mcp_server(id).await {
                    Ok(()) => {
                        navigator.push(Route::McpServersRoute {});
                    }
                    Err(e) => delete_error.set(Some(e.to_string())),
                }
            });
        } else {
            pending_delete.set(true);
        }
    };

    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::McpServersRoute {}, class: "mcp-back-link", "\u{2190} Back to MCP servers" }
                h1 { "Edit MCP server" }
            }

            if let Some(err) = load_error() {
                p { class: "error", "{err}" }
            }

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
                        Some(Err(e)) => rsx! {
                            div { class: "mcp-status mcp-status-unreachable",
                                p { class: "error", "{e}" }
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

                form { class: "mcp-edit-form", onsubmit: save_all,
                    label { r#for: "mcp-edit-name", "Name" }
                    input {
                        id: "mcp-edit-name",
                        class: "mcp-edit-name",
                        r#type: "text",
                        value: "{edit_name}",
                        oninput: move |e| edit_name.set(e.value()),
                    }
                    label { r#for: "mcp-edit-url", "URL" }
                    input {
                        id: "mcp-edit-url",
                        class: "mcp-edit-url",
                        r#type: "text",
                        value: "{edit_url}",
                        oninput: move |e| edit_url.set(e.value()),
                    }

                    if edit_auth_mode() == "static_headers" {
                        label { "Headers" }
                        if !existing_header_edits().is_empty() {
                            div { class: "mcp-header-rows",
                                for (index , (name , value)) in existing_header_edits().into_iter().enumerate() {
                                    div { key: "{name}", class: "mcp-header-row",
                                        span { class: "mcp-header-existing-name", "{name}" }
                                        input {
                                            r#type: "text",
                                            class: "mcp-header-value",
                                            placeholder: "(unchanged)",
                                            value: "{value}",
                                            oninput: move |e| existing_header_edits.write()[index].1 = e.value(),
                                        }
                                        button {
                                            r#type: "button",
                                            class: "mcp-remove-header",
                                            onclick: move |_| {
                                                let (removed_name, _) = existing_header_edits.write().remove(index);
                                                removed_header_names.write().push(removed_name);
                                            },
                                            "Remove"
                                        }
                                    }
                                }
                            }
                        }
                        {header_rows(new_header_rows, "Header value")}
                    }

                    if let Some(err) = save_error() {
                        p { class: "error", "{err}" }
                    }
                    button {
                        class: if saving() { "mcp-save-edit saving" } else { "mcp-save-edit" },
                        r#type: "submit",
                        disabled: saving() || !is_dirty,
                        if saving() {
                            span { class: "mcp-save-spinner" }
                            "Saving\u{2026}"
                        } else {
                            "Save"
                        }
                    }
                }

                if edit_auth_mode() == "oauth" {
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
                        if let Some(err) = oauth_error() {
                            p { class: "error", "{err}" }
                        }
                        if let Some(err) = disconnect_error() {
                            p { class: "error", "{err}" }
                        }
                        div { class: "mcp-oauth-actions",
                            button {
                                class: "mcp-oauth-connect",
                                r#type: "button",
                                disabled: oauth_connecting(),
                                onclick: connect_oauth,
                                if matches!(status(), Some(Ok(McpConnectionStatus::Connected { .. }))) {
                                    "Reconnect"
                                } else {
                                    "Connect"
                                }
                            }
                            if matches!(status(), Some(Ok(McpConnectionStatus::Connected { .. } | McpConnectionStatus::Unreachable { .. }))) {
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

                div { class: "mcp-danger-section",
                    h2 { "Delete" }
                    if let Some(err) = delete_error() {
                        p { class: "error", "{err}" }
                    }
                    button {
                        class: if pending_delete() { "mcp-delete confirm" } else { "mcp-delete" },
                        r#type: "button",
                        onclick: request_delete,
                        if pending_delete() { "Confirm delete?" } else { "Delete server" }
                    }
                }
            }
        }
    }
}
