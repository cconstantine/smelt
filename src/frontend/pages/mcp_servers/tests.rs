//! Pure-helper tests for the Tool-classes section (SME-240) — there is
//! no SSR component harness (docs/testing.md), so the two helpers the
//! section renders per row carry the logic that can be tested: the
//! select's value and the origin note.

use super::tool_classes::{SERVER_ANSWER, class_select_value, tool_class_origin_note};
use crate::api::mcp::McpToolClassRow;

fn row(
    server_class: Option<&str>,
    annotated: Option<bool>,
    override_class: Option<&str>,
    listed: Option<bool>,
) -> McpToolClassRow {
    McpToolClassRow {
        tool_name: "some_tool".to_string(),
        server_class: server_class.map(str::to_string),
        annotated,
        override_class: override_class.map(str::to_string),
        listed,
    }
}

#[test]
fn test_the_select_shows_the_override_or_servers_answer() {
    assert_eq!(class_select_value(&row(Some("read-only"), Some(true), None, Some(true))), SERVER_ANSWER);
    assert_eq!(
        class_select_value(&row(Some("read-only"), Some(true), Some("writing"), Some(true))),
        "writing"
    );
    // A server that's down: the saved override still drives the select.
    assert_eq!(class_select_value(&row(None, None, Some("destructive"), None)), "destructive");
}

/// The note is the Done-when's "shows … where it came from": annotation,
/// default, override, and override-on-a-gone-tool each say differently.
#[test]
fn test_the_origin_note_names_the_class_and_where_it_came_from() {
    assert_eq!(
        tool_class_origin_note(&row(Some("read-only"), Some(true), None, Some(true))),
        "read-only \u{2014} from the server"
    );
    assert_eq!(
        tool_class_origin_note(&row(Some("destructive"), Some(false), None, Some(true))),
        "destructive \u{2014} the server doesn't annotate it (the default)"
    );
    assert_eq!(
        tool_class_origin_note(&row(Some("read-only"), Some(true), Some("writing"), Some(true))),
        "writing \u{2014} your override"
    );
    assert_eq!(
        tool_class_origin_note(&row(None, None, Some("writing"), Some(false))),
        "writing \u{2014} your override (the server no longer lists this tool)"
    );
    // Override whose live state is unknown (server unreachable): the
    // override wording, without the "no longer lists" claim.
    assert_eq!(
        tool_class_origin_note(&row(None, None, Some("destructive"), None)),
        "destructive \u{2014} your override"
    );
    // No class at all to show (a state the GET never sends): the note is
    // empty rather than a wrong origin.
    assert_eq!(tool_class_origin_note(&row(None, None, None, None)), "");
}
