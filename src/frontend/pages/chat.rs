use std::collections::HashMap;

use dioxus::html::geometry::PixelsVector2D;
#[cfg(feature = "web")]
use dioxus::prelude::dioxus_core::Task;
use dioxus::prelude::*;

use crate::anthropic::ContentBlock;
use crate::anthropic::tools::TaskSummary;
use crate::api::chat::{
    ChatEvent, create_conversation, delete_conversation, get_conversations, get_messages,
    get_tasks, send_message, subscribe_conversation_events,
};
use crate::events::ConversationEvent;
use crate::models::{Conversation, Message};

/// Appends every message in `incoming` whose id isn't already present in
/// `existing` — the same row can legitimately arrive twice (once via
/// `send_message`'s own `ChatEvent::Done`, once via the live
/// `MessagesAppended` broadcast, or via the one-shot reconciliation pull on
/// (re)connect), and a duplicate id must never render as two bubbles.
fn merge_messages_by_id(existing: &mut Vec<Message>, incoming: Vec<Message>) {
    for message in incoming {
        if !existing.iter().any(|m| m.id == message.id) {
            existing.push(message);
        }
    }
}

/// One "terminal" widget's worth of state for a background task — task id,
/// tool, status, and the full accumulated stdout/stderr scrollback (kept as
/// two separate logs, mirroring a process's own two output streams, same
/// split `anthropic::tools::TaskSummary` and the server-side `Task` registry
/// use). Unlike the single-line version this replaced, every line is kept
/// so the widget can render like a real terminal's history rather than a
/// one-line status row — this is deliberately shaped to grow into a real
/// shell session later, not just a log viewer.
#[derive(Clone, Debug, PartialEq)]
struct TaskPanelEntry {
    task_id: String,
    tool: String,
    status: String,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

/// Applies one `get_tasks` snapshot onto the panel's current entries:
/// updates tool/status/full scrollback for tasks already known (the
/// snapshot's `stdout`/`stderr` are authoritative — the server's own
/// accumulated log — so they replace rather than merge with whatever the
/// panel already had), adds any that are new. Never removes an entry (a
/// finished/cancelled task should stay visible with its last known output,
/// not vanish from the panel).
fn merge_task_snapshot(existing: &mut Vec<TaskPanelEntry>, snapshot: Vec<TaskSummary>) {
    for task in snapshot {
        if let Some(entry) = existing.iter_mut().find(|e| e.task_id == task.task_id) {
            entry.tool = task.tool;
            entry.status = task.status;
            entry.stdout = task.stdout;
            entry.stderr = task.stderr;
        } else {
            existing.push(TaskPanelEntry {
                task_id: task.task_id,
                tool: task.tool,
                status: task.status,
                stdout: task.stdout,
                stderr: task.stderr,
            });
        }
    }
}

/// Applies one live `TaskUpdate` event onto the panel's current entries —
/// same upsert shape as `merge_task_snapshot`, but appends a single new
/// line rather than replacing the whole scrollback. A "just started"/
/// terminal event carries `stream: None` (a pure status transition, no line
/// to append) and only updates `tool`/`status`.
fn apply_task_update(
    existing: &mut Vec<TaskPanelEntry>,
    task_id: String,
    tool: String,
    status: String,
    stream: Option<String>,
    latest_output: Option<String>,
) {
    if let Some(entry) = existing.iter_mut().find(|e| e.task_id == task_id) {
        entry.tool = tool;
        entry.status = status;
        match (stream.as_deref(), latest_output) {
            (Some("stdout"), Some(line)) => entry.stdout.push(line),
            (Some("stderr"), Some(line)) => entry.stderr.push(line),
            _ => {}
        }
    } else {
        let (stdout, stderr) = match (stream.as_deref(), latest_output) {
            (Some("stdout"), Some(line)) => (vec![line], Vec::new()),
            (Some("stderr"), Some(line)) => (Vec::new(), vec![line]),
            _ => (Vec::new(), Vec::new()),
        };
        existing.push(TaskPanelEntry {
            task_id,
            tool,
            status,
            stdout,
            stderr,
        });
    }
}

/// Pretty-prints a `ToolUse` block's `input` for display. Falls back to the
/// compact form on the (practically impossible, since `Value` always
/// serializes) chance pretty-printing fails.
fn format_tool_input(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// Label for a `ToolResult` card's header, distinguishing a normal result
/// from an error at a glance without repeating "error"/"result" as raw text
/// the caller has to style around.
fn tool_result_label(is_error: bool) -> &'static str {
    if is_error {
        "Tool error"
    } else {
        "Tool result"
    }
}

/// The tool `run_async` was actually asked to start — its own `input.tool`
/// field, not to be confused with the enclosing `ToolUse` block's `name`
/// (always the literal `"run_async"`). Used so the compact inline summary
/// can say "Started count" rather than the uninformative "Started
/// run_async".
fn run_async_wrapped_tool(input: &serde_json::Value) -> Option<&str> {
    input.get("tool").and_then(|v| v.as_str())
}

/// Maps every `ToolUse` block's id to its tool name across every message in
/// the conversation. A `ToolResult` block only carries the id of the call
/// it answers, not the tool's name — this is how `render_block_element`
/// recognizes a `run_async` result (to fold it into the compact inline
/// summary instead of rendering its own card; the tasks sidebar already
/// shows what actually happened).
fn tool_use_names_by_id(messages: &[Message]) -> HashMap<String, String> {
    messages
        .iter()
        .filter_map(|m| m.blocks().ok())
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. } => Some((id, name)),
            _ => None,
        })
        .collect()
}

/// Formats a message's `created_at` for the small, subtle timestamp shown
/// on every rendered block, converted into the viewer's browser timezone.
/// `NaiveDateTime` carries no timezone of its own — it's stored as
/// whatever the server's `now()` produced, effectively UTC in this
/// single-user, single-deployment app — so the conversion needs an offset
/// from somewhere else. `tz_offset_minutes` is *minutes to add* to a UTC
/// time to get local time (the negation of JS's own
/// `Date.getTimezoneOffset()`, which returns UTC-minus-local — the opposite
/// sign), fetched once per page load via `document::eval` in `ChatPanel`
/// and passed in here rather than this function reaching for browser APIs
/// itself, so it stays pure and testable with a plain offset value.
fn format_timestamp(created_at: chrono::NaiveDateTime, tz_offset_minutes: i32) -> String {
    let local = created_at + chrono::Duration::minutes(tz_offset_minutes as i64);
    local.format("%-I:%M %p").to_string()
}

/// How close to a scrollable container's bottom edge still counts as "at
/// the bottom" for auto-scroll purposes — a little slack for sub-pixel
/// layout rounding, not a meaningful reading gesture.
const SCROLL_BOTTOM_SLACK_PX: f64 = 32.0;

/// Whether a scrollable container is close enough to its bottom edge that
/// new content should pull the view down with it. Both the message
/// transcript and each task's terminal body use this via their own
/// `onscroll` handler to decide, independently, whether the user has
/// scrolled up to read something (in which case new content must leave
/// their position alone) or is following along at the bottom (in which
/// case it should keep tracking new content, the way a real terminal
/// does).
fn is_scrolled_to_bottom(scroll_top: f64, scroll_height: f64, client_height: f64) -> bool {
    scroll_height - scroll_top - client_height <= SCROLL_BOTTOM_SLACK_PX
}

/// Renders one content block, keyed by `{message_id}-{index}` for the
/// enclosing `for` loop. `Text` renders as an ordinary chat bubble, same as
/// always (including synthetic pushed `<task-output>`/`<task-notification>`
/// -tagged messages a background task writes — still indistinguishable from
/// something a human typed at this stage, flagged as a known gap in the
/// tool-use-round-trip plan's retrospective, not solved here). `ToolUse`/
/// `ToolResult` render as their own centered cards, distinct from both the
/// user- and assistant-aligned bubbles, so a tool call/result reads as
/// "the agent doing something" rather than "someone said something."
fn render_block_element(
    message_id: i64,
    index: usize,
    role: &str,
    created_at: chrono::NaiveDateTime,
    tz_offset_minutes: i32,
    block: &ContentBlock,
    tool_names: &HashMap<String, String>,
) -> Element {
    let key = format!("{message_id}-{index}");
    let timestamp = format_timestamp(created_at, tz_offset_minutes);
    match block {
        ContentBlock::Text { text } => rsx! {
            div { key: "{key}", class: "message message-{role}",
                div { class: "message-text", "{text}" }
                span { class: "timestamp", "{timestamp}" }
            }
        },
        // `run_async` gets a much smaller, collapsed-by-default summary —
        // "Started <tool>" — instead of the full call card every other
        // tool gets: the tasks sidebar is the real place to watch what it's
        // doing, so this only needs to mark that it happened, with the raw
        // call available on demand via the native <details> disclosure.
        ContentBlock::ToolUse { name, input, .. } if name == "run_async" => {
            let pretty_input = format_tool_input(input);
            let wrapped_tool = run_async_wrapped_tool(input).unwrap_or("tool");
            rsx! {
                details { key: "{key}", class: "tool-async-start",
                    summary { class: "tool-async-start-summary",
                        span { class: "tool-async-start-icon", "🔧" }
                        span { "Started" }
                        code { class: "tool-async-start-tool", "{wrapped_tool}" }
                        span { class: "timestamp", "{timestamp}" }
                    }
                    pre { class: "tool-async-start-input", "{pretty_input}" }
                }
            }
        }
        ContentBlock::ToolUse { name, input, .. } => {
            let pretty_input = format_tool_input(input);
            rsx! {
                div { key: "{key}", class: "tool-call",
                    div { class: "tool-call-header",
                        span { class: "tool-call-icon", "🔧" }
                        span { "Called" }
                        code { class: "tool-call-name", "{name}" }
                        span { class: "timestamp", "{timestamp}" }
                    }
                    pre { class: "tool-call-input", "{pretty_input}" }
                }
            }
        }
        // The result of a `run_async` call is just the generic "task
        // started" boilerplate `anthropic::tools` always returns — the
        // compact summary above already conveys that, so render nothing
        // rather than a second, redundant card.
        ContentBlock::ToolResult { tool_use_id, .. }
            if tool_names.get(tool_use_id).map(String::as_str) == Some("run_async") =>
        {
            rsx! {}
        }
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            let is_error = is_error.unwrap_or(false);
            let card_class = if is_error {
                "tool-result tool-result-error"
            } else {
                "tool-result"
            };
            let label = tool_result_label(is_error);
            rsx! {
                div { key: "{key}", class: "{card_class}",
                    div { class: "tool-result-header",
                        span { class: "tool-result-icon", if is_error { "⚠️" } else { "✅" } }
                        span { "{label}" }
                        span { class: "timestamp", "{timestamp}" }
                    }
                    pre { class: "tool-result-content", "{content}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tool_input_pretty_prints_json_object() {
        let input = serde_json::json!({"a": 2, "b": 3});
        assert_eq!(format_tool_input(&input), "{\n  \"a\": 2,\n  \"b\": 3\n}");
    }

    #[test]
    fn test_tool_result_label_distinguishes_error_from_success() {
        assert_eq!(tool_result_label(false), "Tool result");
        assert_eq!(tool_result_label(true), "Tool error");
    }

    #[test]
    fn test_run_async_wrapped_tool_reads_the_tool_field() {
        let input = serde_json::json!({"tool": "count", "input": {"target": 8}});
        assert_eq!(run_async_wrapped_tool(&input), Some("count"));
    }

    #[test]
    fn test_run_async_wrapped_tool_missing_field_returns_none() {
        assert_eq!(run_async_wrapped_tool(&serde_json::json!({})), None);
    }

    fn tool_use_message(id: i64, tool_use_id: &str, name: &str) -> Message {
        Message {
            id,
            conversation_id: 1,
            role: "assistant".to_string(),
            content: serde_json::to_string(&[ContentBlock::ToolUse {
                id: tool_use_id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }])
            .expect("ContentBlock always serializes"),
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    #[test]
    fn test_tool_use_names_by_id_maps_every_tool_use_across_messages() {
        let messages = vec![
            test_message(1),
            tool_use_message(2, "call_1", "run_async"),
            tool_use_message(3, "call_2", "add"),
        ];
        let names = tool_use_names_by_id(&messages);
        assert_eq!(names.get("call_1").map(String::as_str), Some("run_async"));
        assert_eq!(names.get("call_2").map(String::as_str), Some("add"));
        assert_eq!(names.get("call_3"), None);
    }

    #[test]
    fn test_format_timestamp_uses_12_hour_clock_with_am_pm() {
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
            .unwrap()
            .and_hms_opt(14, 32, 0)
            .unwrap();
        assert_eq!(format_timestamp(dt, 0), "2:32 PM");
    }

    #[test]
    fn test_format_timestamp_midnight_and_noon() {
        let midnight = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
            .unwrap()
            .and_hms_opt(0, 5, 0)
            .unwrap();
        assert_eq!(format_timestamp(midnight, 0), "12:05 AM");

        let noon = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        assert_eq!(format_timestamp(noon, 0), "12:00 PM");
    }

    #[test]
    fn test_format_timestamp_applies_negative_offset_for_a_timezone_behind_utc() {
        // US Eastern Standard Time is UTC-5: 2:32 PM UTC -> 9:32 AM local.
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
            .unwrap()
            .and_hms_opt(14, 32, 0)
            .unwrap();
        assert_eq!(format_timestamp(dt, -5 * 60), "9:32 AM");
    }

    #[test]
    fn test_format_timestamp_applies_positive_offset_for_a_timezone_ahead_of_utc() {
        // Japan Standard Time is UTC+9: 2:32 PM UTC -> 11:32 PM local.
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
            .unwrap()
            .and_hms_opt(14, 32, 0)
            .unwrap();
        assert_eq!(format_timestamp(dt, 9 * 60), "11:32 PM");
    }

    #[test]
    fn test_format_timestamp_offset_crosses_a_day_boundary() {
        // 11:32 PM UTC, timezone ahead by 2 hours -> 1:32 AM the next day.
        // format_timestamp only ever shows a time, so the day rollover
        // itself isn't asserted here, just that the hour wraps correctly.
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
            .unwrap()
            .and_hms_opt(23, 32, 0)
            .unwrap();
        assert_eq!(format_timestamp(dt, 2 * 60), "1:32 AM");
    }

    #[test]
    fn test_is_scrolled_to_bottom_true_when_flush_with_bottom() {
        assert!(is_scrolled_to_bottom(500.0, 600.0, 100.0));
    }

    #[test]
    fn test_is_scrolled_to_bottom_true_within_slack() {
        // 20px short of the bottom — inside SCROLL_BOTTOM_SLACK_PX (32px).
        assert!(is_scrolled_to_bottom(480.0, 600.0, 100.0));
    }

    #[test]
    fn test_is_scrolled_to_bottom_false_when_scrolled_up() {
        // 400px short of the bottom — well past the slack.
        assert!(!is_scrolled_to_bottom(100.0, 600.0, 100.0));
    }

    fn test_message(id: i64) -> Message {
        Message {
            id,
            conversation_id: 1,
            role: "user".to_string(),
            content: r#"[{"type":"text","text":"hi"}]"#.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    #[test]
    fn test_merge_messages_by_id_skips_ids_already_present() {
        let mut existing = vec![test_message(1)];
        merge_messages_by_id(&mut existing, vec![test_message(1), test_message(2)]);
        let ids: Vec<i64> = existing.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![1, 2],
            "id 1 should not be duplicated, id 2 should be appended"
        );
    }

    #[test]
    fn test_merge_messages_by_id_on_empty_existing_appends_all() {
        let mut existing = Vec::new();
        merge_messages_by_id(&mut existing, vec![test_message(1), test_message(2)]);
        assert_eq!(existing.len(), 2);
    }

    fn test_task_summary(task_id: &str, status: &str) -> TaskSummary {
        TaskSummary {
            task_id: task_id.to_string(),
            tool: "count".to_string(),
            status: status.to_string(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn test_task_entry(task_id: &str, stdout: &[&str], stderr: &[&str]) -> TaskPanelEntry {
        TaskPanelEntry {
            task_id: task_id.to_string(),
            tool: "count".to_string(),
            status: "running".to_string(),
            stdout: stdout.iter().map(|s| s.to_string()).collect(),
            stderr: stderr.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_merge_task_snapshot_adds_new_and_updates_existing_status() {
        let mut existing = vec![test_task_entry("t1", &["count: 1/3"], &[])];
        let mut finished = test_task_summary("t1", "finished");
        finished.stdout = vec!["count: 1/3".to_string(), "count: 2/3".to_string()];
        merge_task_snapshot(
            &mut existing,
            vec![finished, test_task_summary("t2", "running")],
        );

        assert_eq!(existing.len(), 2);
        assert_eq!(existing[0].status, "finished");
        assert_eq!(
            existing[0].stdout,
            vec!["count: 1/3".to_string(), "count: 2/3".to_string()],
            "the snapshot's own scrollback is authoritative and should replace the panel's"
        );
        assert_eq!(existing[1].task_id, "t2");
    }

    #[test]
    fn test_apply_task_update_appends_to_stdout_when_stream_is_stdout() {
        let mut existing = Vec::new();
        apply_task_update(
            &mut existing,
            "t1".to_string(),
            "count".to_string(),
            "running".to_string(),
            Some("stdout".to_string()),
            Some("count: 1/3".to_string()),
        );
        apply_task_update(
            &mut existing,
            "t1".to_string(),
            "count".to_string(),
            "running".to_string(),
            Some("stdout".to_string()),
            Some("count: 2/3".to_string()),
        );
        assert_eq!(existing.len(), 1);
        assert_eq!(
            existing[0].stdout,
            vec!["count: 1/3".to_string(), "count: 2/3".to_string()],
            "each update should append a new line, not overwrite the last one"
        );
        assert!(existing[0].stderr.is_empty());
    }

    #[test]
    fn test_apply_task_update_appends_to_stderr_when_stream_is_stderr() {
        let mut existing = Vec::new();
        apply_task_update(
            &mut existing,
            "t1".to_string(),
            "echo".to_string(),
            "running".to_string(),
            Some("stderr".to_string()),
            Some("echo: received 5 byte(s) of input".to_string()),
        );
        assert_eq!(existing.len(), 1);
        assert!(existing[0].stdout.is_empty());
        assert_eq!(
            existing[0].stderr,
            vec!["echo: received 5 byte(s) of input".to_string()]
        );
    }

    #[test]
    fn test_apply_task_update_without_stream_does_not_erase_either_stream() {
        let mut existing = vec![test_task_entry("t1", &["count: 1/3"], &["a diagnostic"])];
        // A "just started" or terminal event carries stream: None.
        apply_task_update(
            &mut existing,
            "t1".to_string(),
            "count".to_string(),
            "finished".to_string(),
            None,
            None,
        );
        assert_eq!(existing[0].status, "finished");
        assert_eq!(existing[0].stdout, vec!["count: 1/3".to_string()]);
        assert_eq!(existing[0].stderr, vec!["a diagnostic".to_string()]);
    }
}

#[component]
pub fn Chat() -> Element {
    let selected: Signal<Option<i64>> = use_signal(|| None);

    rsx! {
        div { class: "chat-layout",
            ConversationSidebar { selected }
            ChatPanel { selected }
        }
    }
}

#[component]
fn ConversationSidebar(mut selected: Signal<Option<i64>>) -> Element {
    let initial_conversations = use_resource(get_conversations);
    let mut conversations: Signal<Vec<Conversation>> = use_signal(Vec::new);
    let mut loaded = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut pending_delete: Signal<Option<i64>> = use_signal(|| None);

    use_effect(move || {
        if let Some(result) = initial_conversations() {
            match result {
                Ok(list) => conversations.set(list),
                Err(e) => error.set(Some(e.to_string())),
            }
            loaded.set(true);
        }
    });

    let new_conversation = move |_| {
        spawn(async move {
            match create_conversation().await {
                Ok(conversation) => {
                    let id = conversation.id;
                    conversations.write().insert(0, conversation);
                    selected.set(Some(id));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // First click on a row's delete button arms it; a second click on the
    // same (still-armed) row confirms. Only one row is ever armed at a
    // time, so arming a different row implicitly cancels the last one.
    let mut request_delete = move |id: i64| {
        if pending_delete() == Some(id) {
            pending_delete.set(None);
            spawn(async move {
                match delete_conversation(id).await {
                    Ok(()) => {
                        conversations.write().retain(|c| c.id != id);
                        if selected() == Some(id) {
                            selected.set(None);
                        }
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }
            });
        } else {
            pending_delete.set(Some(id));
        }
    };

    rsx! {
        aside { class: "sidebar",
            button { class: "new-conversation", onclick: new_conversation, "New conversation" }
            if let Some(err) = error() {
                p { class: "error", "{err}" }
            }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if conversations().is_empty() {
                p { class: "muted", "No conversations yet" }
            } else {
                div { class: "conversation-list",
                    for conversation in conversations() {
                        div {
                            key: "{conversation.id}",
                            class: if selected() == Some(conversation.id) { "conversation-item active" } else { "conversation-item" },
                            onclick: move |_| {
                                pending_delete.set(None);
                                selected.set(Some(conversation.id));
                            },
                            span { class: "conversation-title", "{conversation.title}" }
                            button {
                                class: if pending_delete() == Some(conversation.id) { "delete-conversation confirm" } else { "delete-conversation" },
                                onclick: move |evt: Event<MouseData>| {
                                    evt.stop_propagation();
                                    request_delete(conversation.id);
                                },
                                if pending_delete() == Some(conversation.id) { "Confirm?" } else { "Delete" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ChatPanel(selected: Signal<Option<i64>>) -> Element {
    let initial_messages = use_resource(move || {
        let id = selected();
        async move {
            match id {
                Some(id) => Some(get_messages(id).await),
                None => None,
            }
        }
    });

    let mut messages: Signal<Vec<Message>> = use_signal(Vec::new);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);
    let mut streaming_text: Signal<String> = use_signal(String::new);
    let mut is_streaming = use_signal(|| false);
    let mut stream_error: Signal<Option<String>> = use_signal(|| None);
    let mut input = use_signal(String::new);
    let mut next_temp_id = use_signal(|| -1i64);
    let mut tasks: Signal<Vec<TaskPanelEntry>> = use_signal(Vec::new);
    let mut tz_offset_minutes: Signal<i32> = use_signal(|| 0);

    // Sticky-bottom auto-scroll state for the message transcript: the
    // mounted `.messages` element (so an effect can query/set its scroll
    // position) and whether it was at the bottom the last time the user
    // scrolled it — read, not written, by the auto-scroll effect below;
    // written only by the `onscroll` handler on the element itself, so it
    // always reflects a real user (or auto-scroll-induced) scroll position
    // rather than the reactive-render cycle.
    let mut messages_el: Signal<Option<MountedEvent>> = use_signal(|| None);
    let mut messages_stuck_to_bottom = use_signal(|| true);

    // Same idea, per background task — each task's own `.task-terminal-body`
    // scrolls independently, like `tail -f` on its own log, so each needs
    // its own mounted handle and stuck flag rather than one shared pair.
    let mut task_body_els: Signal<HashMap<String, MountedEvent>> = use_signal(HashMap::new);
    let mut task_body_stuck: Signal<HashMap<String, bool>> = use_signal(HashMap::new);

    // Fetched once per page load (this effect reads no reactive signal, so
    // it never re-runs), not per message — timestamps are stored as
    // effectively-UTC `NaiveDateTime`s with no timezone of their own, and
    // the browser's offset is the only place that information can come
    // from. Web-only: there's no browser `Date` during SSR, and 0 (UTC) is
    // a fine fallback for the pre-hydration render either way.
    #[cfg(feature = "web")]
    use_effect(move || {
        spawn(async move {
            if let Ok(value) = document::eval("return -new Date().getTimezoneOffset();").await {
                if let Some(offset) = value.as_i64() {
                    tz_offset_minutes.set(offset as i32);
                }
            }
        });
    });

    use_effect(move || match initial_messages() {
        Some(Some(Ok(list))) => {
            messages.set(list);
            load_error.set(None);
            // A freshly loaded conversation should open scrolled to its
            // latest message, regardless of where a previous conversation
            // was left scrolled.
            messages_stuck_to_bottom.set(true);
        }
        Some(Some(Err(e))) => load_error.set(Some(e.to_string())),
        Some(None) => {
            messages.set(Vec::new());
            messages_stuck_to_bottom.set(true);
        }
        None => {}
    });

    // Live event subscription: opens once per selected conversation and
    // keeps itself open for as long as that conversation stays selected —
    // independent of, and in addition to, whatever `send_message` calls are
    // in flight. Web-only: SSR has no live browser tab to keep a stream
    // open for, and the server-side executor has no reason to run a loop
    // that never terminates on its own. `event_task` holds the previous
    // subscription's handle so switching conversations cancels it outright
    // (`Task::cancel`) rather than relying on the loop to notice on its own
    // — it might be parked in `events.recv().await` with nothing arriving
    // to wake it back up to check.
    #[cfg(feature = "web")]
    {
        let mut event_task: Signal<Option<Task>> = use_signal(|| None);
        use_effect(move || {
            if let Some(task) = event_task.write().take() {
                task.cancel();
            }
            let Some(id) = selected() else { return };
            tasks.set(Vec::new());
            task_body_els.write().clear();
            task_body_stuck.write().clear();

            let handle = spawn(async move {
                loop {
                    if let Ok(mut events) = subscribe_conversation_events(id).await {
                        // One-shot reconciliation pull: a `broadcast`
                        // channel has no replay, so anything published
                        // before this subscription connected would
                        // otherwise be missed. This runs once per
                        // connection (initial load or reconnect), not on a
                        // timer — not the polling loop this replaces.
                        if let Ok(list) = get_messages(id).await {
                            merge_messages_by_id(&mut messages.write(), list);
                        }
                        if let Ok(snapshot) = get_tasks(id).await {
                            merge_task_snapshot(&mut tasks.write(), snapshot);
                        }

                        loop {
                            match events.recv().await {
                                Some(Ok(ConversationEvent::MessagesAppended(rows))) => {
                                    merge_messages_by_id(&mut messages.write(), rows);
                                }
                                Some(Ok(ConversationEvent::TaskUpdate {
                                    task_id,
                                    tool,
                                    status,
                                    stream,
                                    latest_output,
                                })) => {
                                    apply_task_update(
                                        &mut tasks.write(),
                                        task_id,
                                        tool,
                                        status,
                                        stream,
                                        latest_output,
                                    );
                                }
                                Some(Err(_)) | None => break,
                            }
                        }
                    }
                    // Stream ended or failed to open — reconnect after a
                    // short fixed delay (a guessed default, like `MAX_TURNS`
                    // and `count`'s own clamps elsewhere in this codebase;
                    // not meant to be a production backoff policy).
                    gloo_timers::future::TimeoutFuture::new(1500).await;
                }
            });
            event_task.set(Some(handle));
        });
    }

    let mut send = move || {
        let Some(id) = selected() else { return };
        let content = input();
        if content.trim().is_empty() {
            return;
        }
        input.set(String::new());

        let temp_id = next_temp_id();
        next_temp_id.set(temp_id - 1);
        messages.write().push(Message {
            id: temp_id,
            conversation_id: id,
            role: "user".to_string(),
            content: serde_json::to_string(&[ContentBlock::Text {
                text: content.clone(),
            }])
            .expect("ContentBlock always serializes"),
            created_at: chrono::Utc::now().naive_utc(),
        });

        spawn(async move {
            is_streaming.set(true);
            stream_error.set(None);
            streaming_text.set(String::new());

            match send_message(id, content).await {
                Ok(mut events) => {
                    while let Some(event) = events.recv().await {
                        match event {
                            Ok(ChatEvent::Delta { text }) => {
                                streaming_text.write().push_str(&text);
                            }
                            Ok(ChatEvent::Done {
                                message_id,
                                role,
                                content,
                            }) => {
                                messages.write().push(Message {
                                    id: message_id,
                                    conversation_id: id,
                                    role,
                                    content,
                                    created_at: chrono::Utc::now().naive_utc(),
                                });
                                streaming_text.set(String::new());
                            }
                            Ok(ChatEvent::Error { message }) => {
                                stream_error.set(Some(message));
                            }
                            Err(e) => stream_error.set(Some(e.to_string())),
                        }
                    }
                }
                Err(e) => stream_error.set(Some(e.to_string())),
            }

            is_streaming.set(false);
        });
    };

    // Auto-scroll the transcript to its new bottom whenever a message is
    // added or streaming text grows — but only if the user was already at
    // the bottom (`messages_stuck_to_bottom`, kept current by the
    // `.messages` div's own `onscroll` handler below). Reads `messages()`
    // and `streaming_text()` so it reruns on both a persisted message and
    // an in-flight delta.
    use_effect(move || {
        let _ = messages();
        let _ = streaming_text();
        if !messages_stuck_to_bottom() {
            return;
        }
        let Some(el) = messages_el() else { return };
        spawn(async move {
            if let Ok(size) = el.get_scroll_size().await {
                let _ = el
                    .scroll(
                        PixelsVector2D::new(0.0, size.height),
                        ScrollBehavior::Instant,
                    )
                    .await;
            }
        });
    });

    // Same sticky-bottom behavior, per background task — each task's
    // terminal body scrolls independently as its own output grows. A task
    // with no recorded stuck state yet (just appeared) defaults to stuck,
    // same as the transcript on first load.
    use_effect(move || {
        let current_tasks = tasks();
        let els = task_body_els();
        let stuck = task_body_stuck();
        for task in current_tasks {
            if !stuck.get(&task.task_id).copied().unwrap_or(true) {
                continue;
            }
            let Some(el) = els.get(&task.task_id).cloned() else {
                continue;
            };
            spawn(async move {
                if let Ok(size) = el.get_scroll_size().await {
                    let _ = el
                        .scroll(
                            PixelsVector2D::new(0.0, size.height),
                            ScrollBehavior::Instant,
                        )
                        .await;
                }
            });
        }
    });

    rsx! {
        section { class: "chat-panel",
            match selected() {
                None => rsx! {
                    div { class: "empty-state", "Select or start a conversation" }
                },
                Some(_) => {
                    let tool_names = tool_use_names_by_id(&messages());
                    rsx! {
                    div { class: "chat-main",
                        div {
                            class: "messages",
                            onmounted: move |evt| messages_el.set(Some(evt)),
                            onscroll: move |evt: Event<ScrollData>| {
                                let d = evt.data();
                                messages_stuck_to_bottom
                                    .set(
                                        is_scrolled_to_bottom(
                                            d.scroll_top(),
                                            d.scroll_height() as f64,
                                            d.client_height() as f64,
                                        ),
                                    );
                            },
                            if let Some(err) = load_error() {
                                p { class: "error", "Error loading messages: {err}" }
                            }
                            for message in messages() {
                                match message.blocks() {
                                    Ok(blocks) => rsx! {
                                        for (i , block) in blocks.iter().enumerate() {
                                            {render_block_element(message.id, i, &message.role, message.created_at, tz_offset_minutes(), block, &tool_names)}
                                        }
                                    },
                                    Err(e) => rsx! {
                                        div {
                                            key: "{message.id}",
                                            class: "message message-{message.role} message-error",
                                            "Error rendering message: {e}"
                                        }
                                    },
                                }
                            }
                            if is_streaming() {
                                div { class: "message message-assistant message-streaming", "{streaming_text}" }
                            }
                            if let Some(err) = stream_error() {
                                p { class: "error", "{err}" }
                            }
                        }
                        form {
                            class: "composer",
                            onsubmit: move |event| {
                                event.prevent_default();
                                send();
                            },
                            input {
                                r#type: "text",
                                value: "{input}",
                                disabled: is_streaming(),
                                placeholder: "Type a message...",
                                oninput: move |e| input.set(e.value()),
                            }
                            button { r#type: "submit", disabled: is_streaming(), "Send" }
                        }
                    }
                    if !tasks().is_empty() {
                        aside { class: "tasks-panel",
                            h3 { "Background tasks" }
                            div { class: "task-terminal-stack",
                                for task in tasks() {
                                    div {
                                        key: "{task.task_id}",
                                        class: "task-terminal task-terminal-status-{task.status}",
                                        div { class: "task-terminal-titlebar",
                                            span { class: "task-terminal-dots",
                                                span { class: "dot dot-red" }
                                                span { class: "dot dot-yellow" }
                                                span { class: "dot dot-green" }
                                            }
                                            code { class: "task-terminal-tool", "{task.tool}" }
                                            span { class: "task-terminal-id", "{task.task_id}" }
                                            span { class: "task-terminal-status", "{task.status}" }
                                        }
                                        div {
                                            class: "task-terminal-body",
                                            onmounted: {
                                                let task_id = task.task_id.clone();
                                                move |evt| {
                                                    task_body_els.write().insert(task_id.clone(), evt);
                                                }
                                            },
                                            onscroll: {
                                                let task_id = task.task_id.clone();
                                                move |evt: Event<ScrollData>| {
                                                    let d = evt.data();
                                                    task_body_stuck
                                                        .write()
                                                        .insert(
                                                            task_id.clone(),
                                                            is_scrolled_to_bottom(
                                                                d.scroll_top(),
                                                                d.scroll_height() as f64,
                                                                d.client_height() as f64,
                                                            ),
                                                        );
                                                }
                                            },
                                            if task.stdout.is_empty() && task.stderr.is_empty() {
                                                span { class: "task-terminal-empty", "no output yet" }
                                            }
                                            for (i , line) in task.stdout.iter().enumerate() {
                                                div { key: "out-{i}", class: "task-terminal-line", "{line}" }
                                            }
                                            for (i , line) in task.stderr.iter().enumerate() {
                                                div { key: "err-{i}", class: "task-terminal-line task-terminal-line-stderr", "{line}" }
                                            }
                                            if task.status == "running" {
                                                span { class: "task-terminal-cursor" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    }
                },
            }
        }
    }
}
