//! The edit page's Tool-classes section (SME-240): the server's tools
//! with the class each one has and where it came from — its own
//! annotations, the unannotated default, or the user's override — and a
//! select per tool to set or undo that override.
//!
//! The section refetches after every save (`classes_reload`), so the
//! row re-renders from what the server just stored: the GET re-reads
//! both the live annotations and the table, and the call path reads the
//! override fresh at every call (`crate::mcp::call_tool_classed`) — so
//! a save takes effect at the next call with no refresh dance of its
//! own.

use dioxus::prelude::*;

use crate::api::mcp::{McpToolClassRow, get_mcp_tool_classes, set_mcp_tool_class};

/// The select's first option: no override, back to whatever the server's
/// annotations say (or the default, when it doesn't annotate).
pub(super) const SERVER_ANSWER: &str = "Server's answer";

/// What the row's select shows: the saved override's class text, or
/// `SERVER_ANSWER` when there is none.
pub(super) fn class_select_value(row: &McpToolClassRow) -> String {
    row.override_class
        .clone()
        .unwrap_or_else(|| SERVER_ANSWER.to_string())
}

/// The row's origin note — the section's whole point: the effective
/// class first, then where it comes from. The two override states the
/// API distinguishes — a listed-with-override tool and one the server
/// stopped listing — say different things; an override whose live state
/// is unknown (the server is down) claims only that it's yours.
pub(super) fn tool_class_origin_note(row: &McpToolClassRow) -> String {
    if let Some(class) = row.override_class.as_deref() {
        return if row.listed == Some(false) {
            format!("{class} \u{2014} your override (the server no longer lists this tool)")
        } else {
            format!("{class} \u{2014} your override")
        };
    }
    match (row.server_class.as_deref(), row.annotated) {
        (Some(class), Some(false)) => format!("{class} \u{2014} the server doesn't annotate it (the default)"),
        (Some(class), _) => format!("{class} \u{2014} from the server"),
        (None, _) => String::new(),
    }
}

/// The Tool-classes section between the edit page's Status section and
/// its edit form. Its own resource and reload bump; a failed save shows
/// here, not on the form. When the server is unreachable or not
/// connected yet, the GET answers with just the saved overrides and a
/// note — the rows stay editable, so an override can be cleared while
/// the server is down (the plan's states table).
#[component]
pub fn McpToolClassesSection(id: i64) -> Element {
    let mut classes_reload: Signal<u32> = use_signal(|| 0);
    let classes = use_resource(move || {
        let _ = classes_reload();
        get_mcp_tool_classes(id)
    });
    let mut save_error: Signal<Option<String>> = use_signal(|| None);

    rsx! {
        div { class: "mcp-tool-classes-section",
            h2 { "Tool classes" }
            match classes() {
                None => rsx! { p { class: "muted", "Reading classes\u{2026}" } },
                Some(Err(e)) => rsx! {
                    super::ErrorText { message: Some(super::server_error_message(&e)) }
                },
                Some(Ok(classes)) => rsx! {
                    if let Some(note) = classes.server_note {
                        p { class: "mcp-tool-classes-note", "{note}" }
                    }
                    if classes.rows.is_empty() {
                        p { class: "muted", "No tools to class yet." }
                    }
                    for row in classes.rows {
                        {
                            let tool_name = row.tool_name.clone();
                            let select_value = class_select_value(&row);
                            let note = tool_class_origin_note(&row);
                            rsx! {
                                div { key: "{tool_name}", class: "mcp-tool-class-row",
                                    span { class: "mcp-tool-class-name", "{tool_name}" }
                                    select {
                                        class: "mcp-tool-class-select",
                                        "aria-label": "Tool class for {tool_name}",
                                        onchange: move |evt| {
                                            let sent = evt.value();
                                            let class = if sent == SERVER_ANSWER { None } else { Some(sent) };
                                            let tool_name = tool_name.clone();
                                            save_error.set(None);
                                            spawn(async move {
                                                match set_mcp_tool_class(id, tool_name, class).await {
                                                    Ok(()) => classes_reload.set(classes_reload() + 1),
                                                    Err(e) => save_error.set(Some(super::server_error_message(&e))),
                                                }
                                            });
                                        },
                                        // The current choice goes on the options, not a `value:` on
                                        // the select: Dioxus doesn't reflect a select's value attribute
                                        // to the DOM, so the select would render blank — the pattern
                                        // providers/models.rs already uses.
                                        option { value: SERVER_ANSWER, selected: select_value == SERVER_ANSWER, "{SERVER_ANSWER}" }
                                        option { value: "read-only", selected: select_value == "read-only", "read-only" }
                                        option { value: "writing", selected: select_value == "writing", "writing" }
                                        option { value: "destructive", selected: select_value == "destructive", "destructive" }
                                    }
                                    span { class: "mcp-tool-class-origin", "{note}" }
                                }
                            }
                        }
                    }
                },
            }
            super::ErrorText { message: save_error() }
        }
    }
}
