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
//!
//! It refetches on the page's `status_reload` bump as well (SME-240
//! review 1, F1): saving the form, or an OAuth connect/disconnect,
//! evicts the server's cached connection server-side
//! (`crate::mcp::evict`), so the live half of this section's answer —
//! which tools exist and how they're annotated — is stale the moment
//! Status refetches. The two sections read the same connection, so they
//! reload on the same signal; without that, the page shows a Status of
//! "Unreachable" beside a tool list read from the connection that's
//! gone.

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

/// The more reaching of two class texts (read-only < writing < destructive),
/// an unknown one counted as destructive, as the server judges it.
pub(super) fn stricter<'a>(a: &'a str, b: &'a str) -> &'a str {
    let rank = |class: &str| ["read-only", "writing", "destructive"].iter().position(|c| *c == class).unwrap_or(2);
    if rank(a) >= rank(b) { a } else { b }
}

/// The row's origin note — the section's whole point: the effective
/// class first, then where it comes from. The two override states the
/// API distinguishes — a listed-with-override tool and one the server
/// stopped listing — say different things; an override whose live state
/// is unknown (the server is down) claims only that it's yours.
pub(super) fn tool_class_origin_note(row: &McpToolClassRow) -> String {
    if let Some(class) = row.override_class.as_deref() {
        // SME-295: the server lists a different tool under this name now,
        // so the override can only narrow it: the stricter of the two.
        if row.tool_changed {
            let server = row.server_class.as_deref().unwrap_or("destructive");
            let applied = stricter(class, server);
            return format!(
                "{applied} \u{2014} your override ({class}) was set for a different tool than the one \
                 listed under this name now, so the stricter of it and the server's class ({server}) \
                 applies until you apply it to this tool"
            );
        }
        // SME-386 T3: set for a tool now gone, a call under the name is
        // judged by the class the GET says (`applied_class`), not the
        // override's own, until the override is cleared and set again.
        return if row.listed == Some(false) {
            match row.applied_class.as_deref().filter(|applied| *applied != class) {
                Some(applied) => format!(
                    "{applied} \u{2014} your override ({class}) was set for a tool the server no longer lists, \
                     so a call under this name is judged {applied} until you clear it and set it again"
                ),
                None => format!("{class} \u{2014} your override (the server no longer lists this tool)"),
            }
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
/// its edit form. Its own resource, reloaded by its own saves and by the
/// page's `status_reload` (see the module note); a failed save shows
/// here, not on the form. When the server is unreachable or not
/// connected yet, the GET answers with just the saved overrides and a
/// note — the rows stay editable, so an override can be cleared while
/// the server is down (the plan's states table).
#[component]
pub fn McpToolClassesSection(id: i64, status_reload: Signal<u32>) -> Element {
    let mut classes_reload: Signal<u32> = use_signal(|| 0);
    let classes = use_resource(move || {
        let _ = classes_reload();
        // The same bump Status reloads on: a saved edit or an OAuth change
        // evicts the connection this section's live rows came from.
        let _ = status_reload();
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
                            let apply_name = row.tool_name.clone();
                            let shown = row.fingerprint.clone();
                            let apply_fingerprint = row.fingerprint.clone();
                            let apply_class = row.override_class.clone();
                            let changed = row.tool_changed;
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
                                            let shown = shown.clone();
                                            save_error.set(None);
                                            spawn(async move {
                                                match set_mcp_tool_class(id, tool_name, class, shown).await {
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
                                    // SME-295 review 1: re-choosing the shown class
                                    // fires no change, so a changed row says how.
                                    if changed {
                                        button {
                                            class: "mcp-tool-class-apply",
                                            r#type: "button",
                                            onclick: move |_| {
                                                let (tool_name, class, shown) =
                                                    (apply_name.clone(), apply_class.clone(), apply_fingerprint.clone());
                                                save_error.set(None);
                                                spawn(async move {
                                                    match set_mcp_tool_class(id, tool_name, class, shown).await {
                                                        Ok(()) => classes_reload.set(classes_reload() + 1),
                                                        Err(e) => save_error.set(Some(super::server_error_message(&e))),
                                                    }
                                                });
                                            },
                                            "Apply to this tool"
                                        }
                                    }
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
