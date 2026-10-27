use std::collections::HashMap;

use dioxus::prelude::*;

use crate::api::mcp::update_mcp_server;

use super::{header_rows, headers_from_rows};

/// The line under the name field for a server whose stored name no
/// `smelt.tools` pattern can name (SME-241): a `/` or a `*` in it puts
/// its tools out of reach of a named rule, so only its class rules them
/// until the name is changed.
pub(super) fn unrulable_name_note(name_unrulable: bool) -> Option<&'static str> {
    name_unrulable.then(|| {
        "this name holds `/` or `*`, so no tool rule can name it: its tools are ruled on by class only \u{2014} rename it to rule on them"
    })
}

/// The edit form: name, URL and headers, saved together. Its own state is
/// the header rows being added or removed and the save in flight; the
/// values it edits are the page's, which loads them.
#[component]
pub(super) fn McpServerEditForm(id: i64, name_unrulable: bool, edit_name: Signal<String>, edit_url: Signal<String>, edit_auth_mode: Signal<String>, saved_name: Signal<String>, saved_url: Signal<String>, existing_header_edits: Signal<Vec<(String, String)>>, status_reload: Signal<u32>) -> Element {
    let mut edit_name = edit_name;
    let mut edit_url = edit_url;
    let mut saved_name = saved_name;
    let mut saved_url = saved_url;
    let mut existing_header_edits = existing_header_edits;
    let mut status_reload = status_reload;
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
        || existing_header_edits()
            .iter()
            .any(|(_, value)| !value.trim().is_empty())
        || !removed_header_names().is_empty()
        || new_header_rows()
            .iter()
            .any(|(name, _)| !name.trim().is_empty());

    // The whole page is one form with one save action: name, URL, and
    // every header change all go to the server together — see
    // `crate::api::mcp::update_mcp_server`'s doc comment for why headers
    // are sent as an upsert/remove pair rather than a full value dump.
    let save_all = move |evt: Event<FormData>| {
        evt.prevent_default();
        let name = edit_name();
        let url = edit_url();
        let mut upsert: HashMap<String, String> = existing_header_edits()
            .into_iter()
            .filter(|(_, value)| !value.trim().is_empty())
            .collect();
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
                    existing_header_edits.set(
                        summary
                            .header_names
                            .into_iter()
                            .map(|name| (name, String::new()))
                            .collect(),
                    );
                    removed_header_names.set(Vec::new());
                    new_header_rows.set(Vec::new());
                    save_error.set(None);
                    status_reload.set(status_reload() + 1);
                }
                Err(e) => save_error.set(Some(super::server_error_message(&e))),
            }
            saving.set(false);
        });
    };

    rsx! {
        form { class: "mcp-edit-form", onsubmit: save_all,
            label { r#for: "mcp-edit-name", "Name" }
            input {
                id: "mcp-edit-name",
                class: "mcp-edit-name",
                r#type: "text",
                value: "{edit_name}",
                oninput: move |e| edit_name.set(e.value()),
            }
            if let Some(note) = unrulable_name_note(name_unrulable) {
                p { class: "mcp-name-unrulable", "{note}" }
            }
            label { r#for: "mcp-edit-url", "URL" }
            input {
                id: "mcp-edit-url",
                class: "mcp-edit-url",
                r#type: "text",
                value: "{edit_url}",
                oninput: move |e| edit_url.set(e.value()),
            }

            // An OAuth server takes extra headers too (SME-76: GitHub's
            // X-MCP-Toolsets); OAuth sets its Authorization.
            label {
                if edit_auth_mode() == "oauth" {
                    "Extra headers (sent with the OAuth token)"
                } else {
                    "Headers"
                }
            }
            if !existing_header_edits().is_empty() {
                div { class: "mcp-header-rows",
                    for (index , (name , value)) in existing_header_edits().into_iter().enumerate() {
                        div { key: "{name}", class: "mcp-header-row",
                            span { class: "mcp-header-existing-name", "{name}" }
                            input {
                                r#type: "text",
                                class: "mcp-header-value",
                                "aria-label": "New value for {name}",
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

            super::ErrorText { message: save_error() }
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
    }
}
