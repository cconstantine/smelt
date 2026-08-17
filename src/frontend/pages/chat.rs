use std::collections::HashMap;

use dioxus::html::geometry::PixelsVector2D;
#[cfg(feature = "web")]
use dioxus::prelude::dioxus_core::Task;
use dioxus::prelude::*;

use crate::anthropic::ContentBlock;
use crate::anthropic::tools::TaskSummary;
use crate::api::chat::{
    ChatEvent, SandboxCommandSummary, SandboxOutputLine, SandboxPodSummary, SandboxSnapshot,
    SandboxTerminalSummary, create_conversation, delete_conversation, get_conversations,
    get_messages, get_sandbox_state, get_tasks, send_message, subscribe_conversation_events,
};
use crate::events::ConversationEvent;
use crate::frontend::Route;
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

/// One pod's widget state — just enough to group its terminals under a
/// header in the sandbox panel.
#[derive(Clone, Debug, PartialEq)]
struct SandboxPodPanelEntry {
    pod_id: i64,
    status: String,
}

/// One output line's widget state — same shape as the wire `SandboxOutputLine`,
/// kept as its own type for the same reason every other panel entry mirrors
/// rather than reuses its wire counterpart (see `SandboxPodPanelEntry` vs.
/// `SandboxPodSummary`).
#[derive(Clone, Debug, PartialEq)]
struct SandboxOutputLinePanelEntry {
    stream: String,
    data: String,
}

/// One command's widget state within a terminal's history. Unlike
/// `TaskPanelEntry`'s stdout/stderr split, `output` is a single sequence in
/// true chronological order (each line tagged with which stream it came
/// from) — a real terminal interleaves the two as they happen, and a panel
/// that rendered them as two separate blocks would show "all stdout, then
/// all stderr" regardless of when anything was actually written.
#[derive(Clone, Debug, PartialEq)]
struct SandboxCommandPanelEntry {
    command_id: String,
    command: String,
    status: String,
    exit_code: Option<i32>,
    output: Vec<SandboxOutputLinePanelEntry>,
}

/// One terminal's widget state — a real terminal's scrollback, not just its
/// current command: every command run in it (bounded, oldest first — see
/// `SandboxTerminalSummary`), each with its own output, so the panel reads
/// like the terminal's actual history rather than only ever showing the
/// latest line.
#[derive(Clone, Debug, PartialEq)]
struct SandboxTerminalPanelEntry {
    terminal_id: i64,
    pod_id: i64,
    status: String,
    commands: Vec<SandboxCommandPanelEntry>,
}

/// Applies one `get_sandbox_state` snapshot onto the panel's current pods
/// and terminals — same "snapshot is authoritative" upsert semantics as
/// `merge_task_snapshot`, flattened from the snapshot's pod→terminal
/// nesting into the two separate flat lists the panel renders from.
fn merge_sandbox_snapshot(
    pods: &mut Vec<SandboxPodPanelEntry>,
    terminals: &mut Vec<SandboxTerminalPanelEntry>,
    snapshot: SandboxSnapshot,
) {
    for pod in snapshot.pods {
        if let Some(entry) = pods.iter_mut().find(|p| p.pod_id == pod.pod_id) {
            entry.status = pod.status.clone();
        } else {
            pods.push(SandboxPodPanelEntry { pod_id: pod.pod_id, status: pod.status.clone() });
        }

        for terminal in pod.terminals {
            let commands = terminal
                .commands
                .into_iter()
                .map(|cmd| SandboxCommandPanelEntry {
                    command_id: cmd.command_id,
                    command: cmd.command,
                    status: cmd.status,
                    exit_code: cmd.exit_code,
                    output: cmd
                        .output
                        .into_iter()
                        .map(|line| SandboxOutputLinePanelEntry { stream: line.stream, data: line.data })
                        .collect(),
                })
                .collect();
            if let Some(entry) = terminals.iter_mut().find(|t| t.terminal_id == terminal.terminal_id) {
                entry.pod_id = terminal.pod_id;
                entry.status = terminal.status;
                entry.commands = commands;
            } else {
                terminals.push(SandboxTerminalPanelEntry {
                    terminal_id: terminal.terminal_id,
                    pod_id: terminal.pod_id,
                    status: terminal.status,
                    commands,
                });
            }
        }
    }
}

/// Applies one live `SandboxPodUpdate` — upserts on `terminated: false`,
/// *removes* the pod (and, defensively, any of its terminals still present
/// locally) on `terminated: true`. Deliberately diverges from the task
/// panel here: a terminated pod is gone, not just relabeled — see the
/// plan's "How."
fn apply_sandbox_pod_update(
    pods: &mut Vec<SandboxPodPanelEntry>,
    terminals: &mut Vec<SandboxTerminalPanelEntry>,
    pod_id: i64,
    status: String,
    terminated: bool,
) {
    if terminated {
        pods.retain(|p| p.pod_id != pod_id);
        terminals.retain(|t| t.pod_id != pod_id);
        return;
    }
    if let Some(entry) = pods.iter_mut().find(|p| p.pod_id == pod_id) {
        entry.status = status;
    } else {
        pods.push(SandboxPodPanelEntry { pod_id, status });
    }
}

/// Applies one live `SandboxTerminalUpdate` — same upsert-or-remove shape
/// as `apply_sandbox_pod_update`.
fn apply_sandbox_terminal_update(
    terminals: &mut Vec<SandboxTerminalPanelEntry>,
    pod_id: i64,
    terminal_id: i64,
    status: String,
    terminated: bool,
) {
    if terminated {
        terminals.retain(|t| t.terminal_id != terminal_id);
        return;
    }
    if let Some(entry) = terminals.iter_mut().find(|t| t.terminal_id == terminal_id) {
        entry.pod_id = pod_id;
        entry.status = status;
    } else {
        terminals.push(SandboxTerminalPanelEntry {
            terminal_id,
            pod_id,
            status,
            commands: Vec::new(),
        });
    }
}

/// Applies one live `SandboxCommandUpdate` onto the owning terminal. `Some
/// (command)` means a *new* command just started in this terminal — pushed
/// onto the terminal's history as a new entry, rather than overwriting
/// anything (a real terminal's scrollback keeps growing, it doesn't erase
/// itself for the next command). `None` means this is continuing the
/// terminal's *most recent* command (an output line, or its completion) —
/// the single-command-in-flight-per-terminal guarantee is what makes "the
/// last entry in this terminal's history" an unambiguous target, no
/// `command_id` matching needed. A `terminal_id` with no matching entry is
/// a no-op (shouldn't happen: a command can't start before its terminal is
/// known to the panel).
fn apply_sandbox_command_update(
    terminals: &mut Vec<SandboxTerminalPanelEntry>,
    terminal_id: i64,
    command_id: String,
    command: Option<String>,
    status: String,
    exit_code: Option<i32>,
    stream: Option<String>,
    latest_output: Option<String>,
) {
    let Some(entry) = terminals.iter_mut().find(|t| t.terminal_id == terminal_id) else {
        return;
    };

    if let Some(command) = command {
        entry.commands.push(SandboxCommandPanelEntry {
            command_id,
            command,
            status,
            exit_code,
            output: Vec::new(),
        });
        return;
    }

    let Some(current) = entry.commands.last_mut() else {
        return;
    };
    current.status = status;
    current.exit_code = exit_code;
    if let (Some(stream), Some(data)) = (stream, latest_output) {
        current.output.push(SandboxOutputLinePanelEntry { stream, data });
    }
}

/// Pretty-prints a `ToolUse` block's `input` for display. Falls back to the
/// compact form on the (practically impossible, since `Value` always
/// serializes) chance pretty-printing fails.
fn format_tool_input(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// One rendered line of an `edit_file` diff — `content` has its trailing
/// newline already stripped (`similar`'s line-based `Change::as_str`
/// includes it, since it's diffing whole lines).
#[derive(Debug, Clone, PartialEq)]
struct DiffLine {
    kind: DiffLineKind,
    content: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DiffLineKind {
    Equal,
    Removed,
    Added,
}

/// A real line-level diff between `edit_file`'s `old_string`/`new_string`,
/// via `similar::TextDiff::from_lines` — not a naive "all of old removed,
/// all of new added." Pure and testable the same way
/// `format_tool_input`/`tool_result_label` are, no DOM involved. See
/// docs/projects/plans/file-tools.md's "Diff rendering."
fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    similar::TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|change| {
            let kind = match change.tag() {
                similar::ChangeTag::Equal => DiffLineKind::Equal,
                similar::ChangeTag::Delete => DiffLineKind::Removed,
                similar::ChangeTag::Insert => DiffLineKind::Added,
            };
            let content = change.as_str().unwrap_or_default().trim_end_matches('\n').to_string();
            DiffLine { kind, content }
        })
        .collect()
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
        // Collapsed by default, same native-<details> pattern as
        // `run_async` below — the reasoning is rarely what someone wants
        // to read on every turn, but shouldn't cost space (or a click
        // through some separate view) when they do.
        ContentBlock::Thinking { thinking, .. } => rsx! {
            details { key: "{key}", class: "thinking-block",
                summary { class: "thinking-summary",
                    span { class: "thinking-icon", "💭" }
                    span { "Thinking" }
                    span { class: "timestamp", "{timestamp}" }
                }
                div { class: "thinking-body", "{thinking}" }
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
        // `edit_file` renders as an actual line-level diff instead of a
        // generic tool-call card showing two raw JSON strings — its
        // `old_string`/`new_string` already carry everything a diff needs.
        // See docs/projects/plans/file-tools.md's "Diff rendering."
        ContentBlock::ToolUse { name, input, .. } if name == "edit_file" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let old_string = input.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
            let new_string = input.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
            let lines = diff_lines(old_string, new_string);
            rsx! {
                div { key: "{key}", class: "file-edit-diff",
                    div { class: "tool-call-header",
                        span { class: "tool-call-icon", "✏️" }
                        span { "Edited" }
                        code { class: "tool-call-name", "{path}" }
                        span { class: "timestamp", "{timestamp}" }
                    }
                    div { class: "file-edit-diff-body",
                        for (i , line) in lines.iter().enumerate() {
                            div {
                                key: "{i}",
                                class: if line.kind == DiffLineKind::Removed { "file-edit-diff-line file-edit-diff-line-removed" } else if line.kind == DiffLineKind::Added { "file-edit-diff-line file-edit-diff-line-added" } else { "file-edit-diff-line" },
                                "{line.content}"
                            }
                        }
                    }
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
    fn test_diff_lines_identical_content_is_all_equal() {
        let result = diff_lines("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(
            result,
            vec![
                DiffLine { kind: DiffLineKind::Equal, content: "a".to_string() },
                DiffLine { kind: DiffLineKind::Equal, content: "b".to_string() },
                DiffLine { kind: DiffLineKind::Equal, content: "c".to_string() },
            ]
        );
    }

    #[test]
    fn test_diff_lines_detects_a_changed_line_as_removed_plus_added() {
        let result = diff_lines("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(
            result,
            vec![
                DiffLine { kind: DiffLineKind::Equal, content: "a".to_string() },
                DiffLine { kind: DiffLineKind::Removed, content: "b".to_string() },
                DiffLine { kind: DiffLineKind::Added, content: "x".to_string() },
                DiffLine { kind: DiffLineKind::Equal, content: "c".to_string() },
            ]
        );
    }

    #[test]
    fn test_diff_lines_keeps_common_context_around_a_multiline_change() {
        // Proves this is a real diff, not "all of old removed, all of new
        // added" — only the differing middle line should be marked.
        let old = "fn f() {\n    old_body();\n}\n";
        let new = "fn f() {\n    new_body();\n}\n";
        let result = diff_lines(old, new);
        assert_eq!(
            result,
            vec![
                DiffLine { kind: DiffLineKind::Equal, content: "fn f() {".to_string() },
                DiffLine { kind: DiffLineKind::Removed, content: "    old_body();".to_string() },
                DiffLine { kind: DiffLineKind::Added, content: "    new_body();".to_string() },
                DiffLine { kind: DiffLineKind::Equal, content: "}".to_string() },
            ]
        );
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

    fn test_sandbox_terminal_entry(terminal_id: i64, pod_id: i64) -> SandboxTerminalPanelEntry {
        SandboxTerminalPanelEntry {
            terminal_id,
            pod_id,
            status: "connected".to_string(),
            commands: Vec::new(),
        }
    }

    fn test_sandbox_command_entry(command_id: &str, command: &str) -> SandboxCommandPanelEntry {
        SandboxCommandPanelEntry {
            command_id: command_id.to_string(),
            command: command.to_string(),
            status: "running".to_string(),
            exit_code: None,
            output: Vec::new(),
        }
    }

    fn test_output_line(stream: &str, data: &str) -> SandboxOutputLine {
        SandboxOutputLine { stream: stream.to_string(), data: data.to_string() }
    }

    fn test_output_line_entry(stream: &str, data: &str) -> SandboxOutputLinePanelEntry {
        SandboxOutputLinePanelEntry { stream: stream.to_string(), data: data.to_string() }
    }

    #[test]
    fn test_merge_sandbox_snapshot_flattens_pods_and_terminals_and_hydrates_command_history() {
        let mut pods = Vec::new();
        let mut terminals = Vec::new();
        let snapshot = SandboxSnapshot {
            pods: vec![SandboxPodSummary {
                pod_id: 1,
                status: "Running".to_string(),
                terminals: vec![SandboxTerminalSummary {
                    terminal_id: 2,
                    pod_id: 1,
                    status: "connected".to_string(),
                    commands: vec![
                        SandboxCommandSummary {
                            command_id: "cmd-1".to_string(),
                            command: "cd /tmp".to_string(),
                            status: "finished".to_string(),
                            exit_code: Some(0),
                            output: Vec::new(),
                        },
                        SandboxCommandSummary {
                            command_id: "cmd-2".to_string(),
                            command: "echo hi".to_string(),
                            status: "finished".to_string(),
                            exit_code: Some(0),
                            // Deliberately interleaved (stdout, stderr, stdout) —
                            // proves the snapshot's own order survives the merge,
                            // rather than getting bucketed into "all stdout, then
                            // all stderr".
                            output: vec![
                                test_output_line("stdout", "hi"),
                                test_output_line("stderr", "uh oh"),
                                test_output_line("stdout", "bye"),
                            ],
                        },
                    ],
                }],
            }],
        };

        merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].pod_id, 1);
        assert_eq!(pods[0].status, "Running");
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].terminal_id, 2);
        assert_eq!(
            terminals[0].commands.iter().map(|c| c.command.as_str()).collect::<Vec<_>>(),
            vec!["cd /tmp", "echo hi"],
            "history should preserve the snapshot's own (oldest-first) order"
        );
        assert_eq!(
            terminals[0].commands[1].output.iter().map(|l| (l.stream.as_str(), l.data.as_str())).collect::<Vec<_>>(),
            vec![("stdout", "hi"), ("stderr", "uh oh"), ("stdout", "bye")],
            "output order must match the snapshot's, not get split by stream"
        );
    }

    #[test]
    fn test_merge_sandbox_snapshot_is_authoritative_over_existing_entries() {
        let mut pods = vec![SandboxPodPanelEntry { pod_id: 1, status: "Pending".to_string() }];
        let mut terminals = Vec::new();
        let snapshot = SandboxSnapshot {
            pods: vec![SandboxPodSummary {
                pod_id: 1,
                status: "Running".to_string(),
                terminals: Vec::new(),
            }],
        };

        merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

        assert_eq!(pods.len(), 1, "an existing pod should be updated, not duplicated");
        assert_eq!(pods[0].status, "Running");
    }

    #[test]
    fn test_apply_sandbox_pod_update_upserts_when_not_terminated() {
        let mut pods = Vec::new();
        let mut terminals = Vec::new();
        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].status, "Running");

        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
        assert_eq!(pods.len(), 1, "a repeat update for the same pod_id should update, not duplicate");
    }

    #[test]
    fn test_apply_sandbox_pod_update_removes_pod_and_its_terminals_when_terminated() {
        let mut pods = vec![SandboxPodPanelEntry { pod_id: 1, status: "Running".to_string() }];
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1), test_sandbox_terminal_entry(20, 2)];

        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "terminated".to_string(), true);

        assert!(pods.is_empty(), "the terminated pod should be removed, not just relabeled");
        assert_eq!(
            terminals.iter().map(|t| t.terminal_id).collect::<Vec<_>>(),
            vec![20],
            "only terminal_id 10 (under the terminated pod) should be dropped; pod 2's terminal is untouched"
        );
    }

    #[test]
    fn test_apply_sandbox_terminal_update_upserts_when_not_terminated() {
        let mut terminals = Vec::new();
        apply_sandbox_terminal_update(&mut terminals, 1, 10, "connected".to_string(), false);
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].status, "connected");
    }

    #[test]
    fn test_apply_sandbox_terminal_update_removes_only_the_matching_terminal_when_terminated() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1), test_sandbox_terminal_entry(20, 1)];

        apply_sandbox_terminal_update(&mut terminals, 1, 10, "disconnected".to_string(), true);

        assert_eq!(
            terminals.iter().map(|t| t.terminal_id).collect::<Vec<_>>(),
            vec![20],
            "terminating one terminal should not affect its sibling in the same pod"
        );
    }

    #[test]
    fn test_apply_sandbox_command_update_with_command_appends_a_new_history_entry() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0].commands.push(test_sandbox_command_entry("cmd-old", "sleep 30"));
        terminals[0].commands[0].output = vec![test_output_line_entry("stdout", "output from a previous command")];

        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-new".to_string(),
            Some("echo hi".to_string()),
            "running".to_string(),
            None,
            None,
            None,
        );

        assert_eq!(
            terminals[0].commands.iter().map(|c| c.command_id.as_str()).collect::<Vec<_>>(),
            vec!["cmd-old", "cmd-new"],
            "a new command should be appended to the terminal's history, not replace it"
        );
        assert_eq!(
            terminals[0].commands[0].output,
            vec![test_output_line_entry("stdout", "output from a previous command")],
            "an earlier command's own output should be untouched by a later command starting"
        );
        assert!(terminals[0].commands[1].output.is_empty());
    }

    #[test]
    fn test_apply_sandbox_command_update_without_command_appends_a_line_to_the_most_recent_command() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0].commands.push(test_sandbox_command_entry("cmd-1", "echo hi"));

        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-1".to_string(),
            None,
            "running".to_string(),
            None,
            Some("stdout".to_string()),
            Some("hi".to_string()),
        );

        assert_eq!(terminals[0].commands[0].output, vec![test_output_line_entry("stdout", "hi")]);
        assert_eq!(
            terminals[0].commands[0].command,
            "echo hi",
            "an output-line update shouldn't touch the already-known command text"
        );
    }

    #[test]
    fn test_apply_sandbox_command_update_preserves_arrival_order_across_streams() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0].commands.push(test_sandbox_command_entry("cmd-1", "sh -c '...'"));

        for (stream, data) in [("stdout", "one"), ("stderr", "uh oh"), ("stdout", "two")] {
            apply_sandbox_command_update(
                &mut terminals,
                10,
                "cmd-1".to_string(),
                None,
                "running".to_string(),
                None,
                Some(stream.to_string()),
                Some(data.to_string()),
            );
        }

        assert_eq!(
            terminals[0].commands[0].output,
            vec![
                test_output_line_entry("stdout", "one"),
                test_output_line_entry("stderr", "uh oh"),
                test_output_line_entry("stdout", "two"),
            ],
            "live updates must interleave in arrival order, not group by stream"
        );
    }

    #[test]
    fn test_apply_sandbox_command_update_finish_sets_status_and_exit_code_on_the_most_recent_command() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0].commands.push(test_sandbox_command_entry("cmd-1", "echo hi"));

        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-1".to_string(),
            None,
            "finished".to_string(),
            Some(0),
            None,
            None,
        );

        assert_eq!(terminals[0].commands[0].status, "finished");
        assert_eq!(terminals[0].commands[0].exit_code, Some(0));
    }

    #[test]
    fn test_apply_sandbox_command_update_without_command_and_no_history_yet_is_a_no_op() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-1".to_string(),
            None,
            "running".to_string(),
            None,
            Some("stdout".to_string()),
            Some("hi".to_string()),
        );
        assert!(
            terminals[0].commands.is_empty(),
            "an output-line update with no prior 'started' event has nothing to attach to"
        );
    }

    #[test]
    fn test_apply_sandbox_command_update_for_unknown_terminal_is_a_no_op() {
        let mut terminals = Vec::new();
        apply_sandbox_command_update(
            &mut terminals,
            999,
            "cmd-1".to_string(),
            Some("echo hi".to_string()),
            "running".to_string(),
            None,
            None,
            None,
        );
        assert!(terminals.is_empty());
    }
}

/// The URL is the source of truth for which conversation is selected (so
/// a refresh lands back on the same one) — `selected` reads it straight
/// from the router via `use_memo`, rather than through a prop synced by a
/// `use_effect`. That first approach looked reasonable but silently
/// broke: `use_effect` only re-runs when it reads a tracked reactive
/// value, and a plain `Option<i64>` prop isn't one, so `selected` synced
/// once on mount and then never again — the URL and sidebar highlight
/// kept moving (they read the route/signal directly) but the messages,
/// tasks, and sandbox panel all froze on whatever conversation loaded
/// first. `router.current::<Route>()` performs a genuine tracked signal
/// read, so wrapping it in `use_memo` gives every descendant (including
/// hooks like `use_resource`, which only restarts for a tracked read
/// inside its own closure) a value that actually updates on navigation.
#[component]
pub fn Chat() -> Element {
    let router = use_router();
    let selected: Memo<Option<i64>> = use_memo(move || match router.current::<Route>() {
        Route::Home {} => None,
        Route::ConversationRoute { id } => Some(id),
        // `Chat` is never actually rendered on these routes (see their own
        // components in `frontend/mod.rs`) — these arms only exist to
        // satisfy exhaustiveness.
        Route::McpServersRoute {} => None,
        Route::McpServerNewRoute {} => None,
        Route::McpServerEditRoute { .. } => None,
    });

    rsx! {
        div { class: "chat-layout",
            ConversationSidebar { selected }
            ChatPanel { selected }
        }
    }
}

#[component]
fn ConversationSidebar(selected: Memo<Option<i64>>) -> Element {
    let navigator = use_navigator();
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
                    navigator.push(Route::ConversationRoute { id });
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
                            navigator.push(Route::Home {});
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
            Link { to: Route::McpServersRoute {}, class: "mcp-servers-link", "MCP servers" }
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
                                navigator.push(Route::ConversationRoute { id: conversation.id });
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
fn ChatPanel(selected: Memo<Option<i64>>) -> Element {
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
    // Set when a background wake-up (a terminal command finishing with no
    // `send_message` call in flight) fails to actually reach the model —
    // see `ConversationEvent::NotificationDeliveryFailed`. Separate from
    // `stream_error` since that one's reset at the start of every `send()`
    // call; this can arrive at any time, not tied to a live send.
    let mut notification_delivery_error: Signal<Option<String>> = use_signal(|| None);
    let mut input = use_signal(String::new);
    let mut next_temp_id = use_signal(|| -1i64);
    let mut tasks: Signal<Vec<TaskPanelEntry>> = use_signal(Vec::new);
    let mut sandbox_pods: Signal<Vec<SandboxPodPanelEntry>> = use_signal(Vec::new);
    let mut sandbox_terminals: Signal<Vec<SandboxTerminalPanelEntry>> = use_signal(Vec::new);
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

    // Same idea again, per sandbox terminal — keyed by `terminal_id` rather
    // than `task_id`, otherwise identical to `task_body_els`/`task_body_stuck`.
    let mut terminal_body_els: Signal<HashMap<i64, MountedEvent>> = use_signal(HashMap::new);
    let mut terminal_body_stuck: Signal<HashMap<i64, bool>> = use_signal(HashMap::new);

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
            sandbox_pods.set(Vec::new());
            sandbox_terminals.set(Vec::new());
            terminal_body_els.write().clear();
            terminal_body_stuck.write().clear();

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
                        if let Ok(snapshot) = get_sandbox_state(id).await {
                            merge_sandbox_snapshot(&mut sandbox_pods.write(), &mut sandbox_terminals.write(), snapshot);
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
                                Some(Ok(ConversationEvent::SandboxPodUpdate { pod_id, status, terminated })) => {
                                    apply_sandbox_pod_update(
                                        &mut sandbox_pods.write(),
                                        &mut sandbox_terminals.write(),
                                        pod_id,
                                        status,
                                        terminated,
                                    );
                                }
                                Some(Ok(ConversationEvent::SandboxTerminalUpdate {
                                    pod_id,
                                    terminal_id,
                                    status,
                                    terminated,
                                })) => {
                                    apply_sandbox_terminal_update(
                                        &mut sandbox_terminals.write(),
                                        pod_id,
                                        terminal_id,
                                        status,
                                        terminated,
                                    );
                                }
                                Some(Ok(ConversationEvent::SandboxCommandUpdate {
                                    terminal_id,
                                    command_id,
                                    command,
                                    status,
                                    exit_code,
                                    stream,
                                    latest_output,
                                })) => {
                                    apply_sandbox_command_update(
                                        &mut sandbox_terminals.write(),
                                        terminal_id,
                                        command_id,
                                        command,
                                        status,
                                        exit_code,
                                        stream,
                                        latest_output,
                                    );
                                }
                                Some(Ok(ConversationEvent::NotificationDeliveryFailed { detail })) => {
                                    notification_delivery_error.set(Some(detail));
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
    //
    // Also reads `tasks()`/`sandbox_pods()`/`sandbox_terminals()`: those
    // panels render below the transcript in `.side-panels-row`, which is
    // conditionally present at all — it only starts rendering once one of
    // them arrives (see the `if !tasks().is_empty() || !sandbox_pods()...`
    // gate further down). That first appearance shrinks `.chat-main` (they
    // split the column's height via flex), which happens *after* the
    // scroll-to-bottom already ran off of `messages()`/`streaming_text()`
    // alone — leaving `.messages` scrolled to what used to be the bottom
    // but, now that the container is shorter, isn't anymore. Re-running
    // this effect on their arrival re-snaps to the new true bottom.
    use_effect(move || {
        let _ = messages();
        let _ = streaming_text();
        let _ = tasks();
        let _ = sandbox_pods();
        let _ = sandbox_terminals();
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

    // Same sticky-bottom behavior again, per sandbox terminal — identical
    // to the background-task effect above, just keyed by `terminal_id`.
    use_effect(move || {
        let current_terminals = sandbox_terminals();
        let els = terminal_body_els();
        let stuck = terminal_body_stuck();
        for terminal in current_terminals {
            if !stuck.get(&terminal.terminal_id).copied().unwrap_or(true) {
                continue;
            }
            let Some(el) = els.get(&terminal.terminal_id).cloned() else {
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
                    if !tasks().is_empty() || !sandbox_pods().is_empty() {
                        div { class: "side-panels-row",
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
                            if !sandbox_pods().is_empty() {
                                aside { class: "sandbox-panel",
                                    h3 { "Sandbox" }
                                    // A conversation has at most one live pod (see
                                    // docs/projects/plans/file-tools.md's "One pod
                                    // per conversation") — straight through, no tab
                                    // bar needed to pick between pods anymore.
                                    for pod in sandbox_pods() {
                                        div { key: "{pod.pod_id}", class: "sandbox-pod",
                                            div { class: "sandbox-pod-header",
                                                span { class: "sandbox-pod-status", "{pod.status}" }
                                            }
                                            div { class: "task-terminal-stack",
                                                for terminal in sandbox_terminals().into_iter().filter(|t| t.pod_id == pod.pod_id) {
                                                    div {
                                                        key: "{terminal.terminal_id}",
                                                        class: "task-terminal",
                                                        div { class: "task-terminal-titlebar",
                                                            span { class: "task-terminal-dots",
                                                                span { class: "dot dot-red" }
                                                                span { class: "dot dot-yellow" }
                                                                span { class: "dot dot-green" }
                                                            }
                                                            span { class: "task-terminal-id", "terminal {terminal.terminal_id}" }
                                                            span { class: "task-terminal-status", "{terminal.status}" }
                                                        }
                                                        div {
                                                            class: "task-terminal-body",
                                                            onmounted: {
                                                                let terminal_id = terminal.terminal_id;
                                                                move |evt| {
                                                                    terminal_body_els.write().insert(terminal_id, evt);
                                                                }
                                                            },
                                                            onscroll: {
                                                                let terminal_id = terminal.terminal_id;
                                                                move |evt: Event<ScrollData>| {
                                                                    let d = evt.data();
                                                                    terminal_body_stuck
                                                                        .write()
                                                                        .insert(
                                                                            terminal_id,
                                                                            is_scrolled_to_bottom(
                                                                                d.scroll_top(),
                                                                                d.scroll_height() as f64,
                                                                                d.client_height() as f64,
                                                                            ),
                                                                        );
                                                                }
                                                            },
                                                            if terminal.commands.is_empty() {
                                                                span { class: "task-terminal-empty", "no commands yet" }
                                                            }
                                                            for (ci , command) in terminal.commands.iter().enumerate() {
                                                                div { key: "{command.command_id}", class: "sandbox-command-block",
                                                                    div { class: "sandbox-command-header",
                                                                        code { "{command.command}" }
                                                                        span { class: "sandbox-command-status",
                                                                            if let Some(code) = command.exit_code {
                                                                                "{command.status} ({code})"
                                                                            } else {
                                                                                "{command.status}"
                                                                            }
                                                                        }
                                                                    }
                                                                    for (i , line) in command.output.iter().enumerate() {
                                                                        div {
                                                                            key: "line-{i}",
                                                                            class: if line.stream == "stderr" { "task-terminal-line task-terminal-line-stderr" } else { "task-terminal-line" },
                                                                            "{line.data}"
                                                                        }
                                                                    }
                                                                    if ci == terminal.commands.len() - 1 && command.status == "running" {
                                                                        span { class: "task-terminal-cursor" }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
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
                            if let Some(err) = notification_delivery_error() {
                                p { class: "error", "A background notification failed to reach the model: {err}" }
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
                    }
                },
            }
        }
    }
}
