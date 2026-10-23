use super::ToolClass::{Destructive, ReadOnly, Writing};
use super::*;

const ANY_METHOD: &[ToolClass] = &[ReadOnly, Writing, Destructive];

/// Every native tool's class, grouped as the design's table groups them
/// (`docs/design/personas/tool-rules.md#smelts-own-tools`). A tool added
/// later without a row here, or with another class, fails
/// `test_every_native_tool_has_the_designs_class`: its ticket classes it.
const DESIGN: &[(&str, &[ToolClass])] = &[
    // The agent's own pod.
    ("create_pod", &[ReadOnly]),
    ("terminate_pod", &[ReadOnly]),
    ("list_pods", &[ReadOnly]),
    ("create_terminal", &[ReadOnly]),
    ("terminate_terminal", &[ReadOnly]),
    ("list_terminals", &[ReadOnly]),
    ("run_terminal_command", &[ReadOnly]),
    ("send_signal", &[ReadOnly]),
    ("terminal_command_status", &[ReadOnly]),
    ("read_terminal_output", &[ReadOnly]),
    ("list_commands", &[ReadOnly]),
    ("set_idle_notice", &[ReadOnly]),
    // Files in its own pod.
    ("read_file", &[ReadOnly]),
    ("edit_file", &[ReadOnly]),
    ("write_file", &[ReadOnly]),
    ("list_directory", &[ReadOnly]),
    ("glob", &[ReadOnly]),
    ("grep", &[ReadOnly]),
    // The conversation's own list.
    ("todowrite", &[ReadOnly]),
    ("todoread", &[ReadOnly]),
    // Its own checkout.
    ("clone_repo", &[ReadOnly]),
    ("load_instructions", &[ReadOnly]),
    ("list_personas", &[ReadOnly]),
    // The web.
    ("webfetch", &[ReadOnly]),
    ("sandbox_preview_url", &[ReadOnly]),
    ("http_request", ANY_METHOD),
    // Browsing sessions.
    ("open_browser_session", &[ReadOnly]),
    ("close_browser_session", &[ReadOnly]),
    ("browser_navigate", &[ReadOnly]),
    ("browser_back", &[ReadOnly]),
    ("browser_read", &[ReadOnly]),
    ("browser_click", &[Writing]),
    ("browser_fill", &[Writing]),
    // Language servers for its own checkout.
    ("lsp_servers", &[ReadOnly]),
    ("start_language_server", &[ReadOnly]),
    ("lsp", &[ReadOnly]),
    // Asks the user.
    ("ask_user", &[ReadOnly]),
    // Agents.
    ("list_agents", &[ReadOnly]),
    ("message_agent", &[ReadOnly]),
    ("ask_parent", &[ReadOnly]),
    ("finish", &[ReadOnly]),
    ("start_agent", &[Writing]),
    ("stop_agent", &[Writing]),
];

#[test]
fn test_every_native_tool_has_the_designs_class() {
    let mut registry: Vec<(&str, &[ToolClass])> = super::super::REGISTRY
        .iter()
        .map(|registered| (registered.tool.def.name.as_str(), registered.tool.class.can_be()))
        .collect();
    let mut design = DESIGN.to_vec();
    registry.sort();
    design.sort();
    let wrong: Vec<String> = registry
        .iter()
        .filter(|tool| !design.contains(tool))
        .map(|(name, classes)| format!("{name} is {classes:?}"))
        .collect();
    let missing: Vec<&str> = design
        .iter()
        .filter(|(name, _)| !registry.iter().any(|(tool, _)| tool == name))
        .map(|(name, _)| *name)
        .collect();
    assert!(
        wrong.is_empty() && missing.is_empty(),
        "classes differ from the design's: {wrong:?}; in the design but not the registry: {missing:?}"
    );
    assert_eq!(registry.len(), 43, "every tool that exists at SME-237");
}

fn http_request_class_of(input: serde_json::Value) -> ToolClass {
    let registered = super::super::REGISTRY
        .iter()
        .find(|registered| registered.tool.def.name == "http_request")
        .expect("http_request is registered");
    registered.tool.class.of_call(&input)
}

#[test]
fn test_http_request_is_classed_by_the_method_it_sends() {
    let url = "https://example.com/";
    let cases: &[(serde_json::Value, ToolClass)] = &[
        (serde_json::json!({"url": url}), ReadOnly),
        (serde_json::json!({"url": url, "method": "GET"}), ReadOnly),
        (serde_json::json!({"url": url, "method": "get"}), ReadOnly),
        (serde_json::json!({"url": url, "method": "HEAD"}), ReadOnly),
        (serde_json::json!({"url": url, "method": "options"}), ReadOnly),
        (serde_json::json!({"url": url, "method": "DELETE"}), Destructive),
        (serde_json::json!({"url": url, "method": "delete"}), Destructive),
        (serde_json::json!({"url": url, "method": "Delete"}), Destructive),
        (serde_json::json!({"url": url, "method": "POST"}), Writing),
        (serde_json::json!({"url": url, "method": "put"}), Writing),
        (serde_json::json!({"url": url, "method": "PATCH"}), Writing),
        (serde_json::json!({"url": url, "method": "PURGE"}), Writing),
        // Doesn't parse, so nothing is sent: the design's "any other".
        (serde_json::json!({"url": url, "method": "GET /x"}), Writing),
        // The tool sends a non-string or null method as GET.
        (serde_json::json!({"url": url, "method": 5}), ReadOnly),
        (serde_json::json!({"url": url, "method": null}), ReadOnly),
    ];
    for (input, expected) in cases {
        assert_eq!(http_request_class_of(input.clone()), *expected, "{input}");
    }
}

fn span_attribute<'a>(
    span: &'a opentelemetry_sdk::trace::SpanData,
    key: &str,
) -> Option<&'a opentelemetry::Value> {
    span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| &kv.value)
}

fn test_pool() -> sqlx::PgPool {
    // Never connected to: neither call below reaches the database.
    sqlx::PgPool::connect_lazy("postgres://unused:unused@localhost/unused")
        .expect("lazy pool construction never fails")
}

/// A call's class is on its `tool` span, and nothing of its input is:
/// this `DELETE` fails on its URL before anything is sent.
#[tokio::test]
async fn test_a_calls_class_is_on_its_tool_span() {
    let input = serde_json::json!({"url": "not a SECRET-URL", "method": "DELETE"});
    let (result, spans) = crate::telemetry::capture_spans(crate::anthropic::tools::execute(
        &test_pool(),
        1,
        "toolu_237",
        "http_request",
        &input,
    ))
    .await;
    assert!(result.is_err(), "the URL doesn't parse: {result:?}");
    let span = spans.iter().find(|span| span.name == "tool http_request").expect("a tool span");
    assert_eq!(span_attribute(span, "smelt.tool.class"), Some(&"destructive".into()));
    let exported = format!("{:?}", span.attributes);
    assert!(!exported.contains("DELETE") && !exported.contains("SECRET-URL"), "{exported}");
}

/// A tool that doesn't exist has no class to record.
#[tokio::test]
async fn test_an_unknown_tools_span_has_no_class() {
    let (result, spans) = crate::telemetry::capture_spans(crate::anthropic::tools::execute(
        &test_pool(),
        1,
        "toolu_237",
        "bogus",
        &serde_json::json!({}),
    ))
    .await;
    assert!(result.is_err(), "{result:?}");
    let span = spans.iter().find(|span| span.name == "tool bogus").expect("a tool span");
    assert_eq!(span_attribute(span, "smelt.tool.class"), None);
}
