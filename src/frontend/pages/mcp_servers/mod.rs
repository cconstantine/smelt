//! The MCP servers pages (`/mcp-servers`, SME-15): the index with live
//! status badges, the add and edit pages, the edit form, the OAuth panel
//! and the delete section. Split out of one file (SME-240's plan) so the
//! Tool-classes section has room to grow under the size ceiling.

use std::collections::HashMap;

use dioxus::prelude::*;

// Re-exported so the moved components' `super::ErrorText`,
// `super::TwoStepButton` and `super::server_error_message` call sites
// (the pages module's shared pieces) keep resolving from submodules.
use super::{ErrorText, TwoStepButton, server_error_message};

mod edit;
mod form;
mod index;
mod new;
mod oauth;

pub use edit::*;
pub use index::*;
pub use new::*;

/// Renders a signal-backed list of `(header name, header value)` rows with
/// add/remove controls, shared by the "add a server" page and the edit
/// page's "replace headers" flow. A plain function (not a `#[component]`)
/// since it only needs to close over a `Signal` — signals are `Copy`, so
/// this works the same as any other closure-capturing helper already used
/// in this file (see `ConversationSidebar` for the same pattern with
/// per-row closures).
pub(super) fn header_rows(
    mut headers: Signal<Vec<(String, String)>>,
    value_placeholder: &'static str,
) -> Element {
    rsx! {
        div { class: "mcp-header-rows",
            for (index , (name , value)) in headers().into_iter().enumerate() {
                div { key: "{index}", class: "mcp-header-row",
                    input {
                        r#type: "text",
                        class: "mcp-header-name",
                        "aria-label": "Header name",
                        placeholder: "Header name (e.g. Authorization)",
                        value: "{name}",
                        oninput: move |e| headers.write()[index].0 = e.value(),
                    }
                    input {
                        r#type: "text",
                        class: "mcp-header-value",
                        "aria-label": "Header value",
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
pub(super) fn headers_from_rows(rows: Vec<(String, String)>) -> HashMap<String, String> {
    rows.into_iter()
        .filter(|(name, _)| !name.trim().is_empty())
        .collect()
}

/// A server's live connection status, fetched with a real connection
/// attempt (`crate::api::mcp::mcp_server_status`) rather than a cached
/// guess — see that function's doc comment for why. Shared rendering used
/// by both the index badge and the edit page's fuller status section.
pub(super) fn status_summary(tool_names: &[String]) -> String {
    format!(
        "Connected \u{2014} {} tool{}",
        tool_names.len(),
        if tool_names.len() == 1 { "" } else { "s" }
    )
}
