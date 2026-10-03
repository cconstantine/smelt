use std::collections::HashMap;

use dioxus::html::geometry::{PixelsVector2D, WheelDelta};
use dioxus::html::input_data::MouseButton;
#[cfg(feature = "web")]
use dioxus::prelude::dioxus_core::Task;
use dioxus::prelude::*;

use super::server_error_message;

use crate::anthropic::{ContentBlock, TokenUsage};
// `TodoItem`/`TodoStatus` are used unconditionally (the todo panel itself
// renders in the shared SSR body, not gated behind `web`).
use crate::anthropic::tools::{TodoItem, TodoStatus};
#[cfg(any(feature = "web", test))]
use crate::api::sandbox::SandboxSnapshot;
use crate::api::browsing::{navigate_browser, send_browser_input};
use crate::api::chat::{
    ContextDetailSnapshot, ContextUsageSnapshot, create_conversation,
    delete_conversation, get_context_detail, get_conversations, get_messages, send_message,
};
// Only called from the live event-subscription loops below, which are
// `web`-only (see their own cfg) — a native `server`-only build never
// reaches them.
#[cfg(feature = "web")]
use crate::api::chat::{
    get_context_usage, get_reply_in_progress, get_todos,
    get_turn_error, get_turn_state, subscribe_conversation_events,
};
#[cfg(feature = "web")]
use crate::api::sandbox::get_sandbox_state;
#[cfg(feature = "web")]
use crate::api::browsing::{get_browsing_state, subscribe_browser_frames};
#[cfg(feature = "web")]
use crate::api::git::list_conversation_repos;
use crate::api::git::{attach_repo, decide_repo_trust};
use crate::git::{RepoStatus, RepoSummary};
use crate::browsing::BrowserInputEvent;
// Only referenced by this module's own tests, which build their own
// `SandboxSnapshot`s by hand rather than through `get_sandbox_state`.
#[cfg(test)]
use crate::api::sandbox::{
    SandboxCommandSummary, SandboxOutputLine, SandboxPodSummary, SandboxTerminalSummary,
};
#[cfg(feature = "web")]
use crate::events::ConversationEvent;
use crate::questions::{PendingQuestion, QuestionAnswer, ASK_USER};
use crate::api::questions::{answer_question, get_waiting_conversations};
#[cfg(feature = "web")]
use crate::api::questions::get_pending_question;
use crate::events::SandboxPreview;
use crate::frontend::Route;
use crate::models::{Conversation, Message};

/// Appends every message in `incoming` whose id isn't already present in
/// `existing` — the same row can legitimately arrive twice (once via
/// `send_message`'s own `ChatEvent::Done`, once via the live
/// `MessagesAppended` broadcast, or via the one-shot reconciliation pull on
/// (re)connect), and a duplicate id must never render as two bubbles.
///
/// Only called from the `web`-only live event-subscription loop below;
/// exercised directly by this module's own tests otherwise.
/// The message list to show once `conversation_id`'s messages have loaded:
/// the loaded list, plus any saved message of that conversation already on
/// screen that the load doesn't have. Opening a conversation starts two
/// loads — this one, and the live subscription's own merge — and this one
/// can finish last with a snapshot taken before a just-saved message
/// existed; replacing outright would wipe that message until a reload.
/// Messages from any other conversation are dropped, and so are optimistic
/// placeholders (negative ids), which the loaded copy supersedes.
fn apply_loaded_messages(
    current: &[Message],
    mut loaded: Vec<Message>,
    conversation_id: i64,
) -> Vec<Message> {
    let newer: Vec<Message> = current
        .iter()
        .filter(|m| m.conversation_id == conversation_id && m.id > 0)
        .filter(|m| !loaded.iter().any(|l| l.id == m.id))
        .cloned()
        .collect();
    loaded.extend(newer);
    loaded
}

#[cfg(any(feature = "web", test))]
/// Messages the server just saved (`MessagesAppended`), into the list on
/// screen: each saved user message replaces one optimistic copy of itself
/// (a negative id with the same text, shown the moment it was sent), then
/// anything not already shown is added. Without the replacement the sender
/// sees their own message twice.
#[cfg(any(feature = "web", test))]
fn accept_saved_messages(existing: &mut Vec<Message>, incoming: Vec<Message>) {
    for saved in &incoming {
        if saved.role != "user" || existing.iter().any(|m| m.id == saved.id) {
            continue;
        }
        let sent = sent_part(saved);
        if let Some(copy) = existing
            .iter()
            .position(|m| m.id < 0 && m.role == saved.role && sent_part(m) == sent)
        {
            existing.remove(copy);
        }
    }
    merge_messages_by_id(existing, incoming);
}

/// What the user sent in `message`: its blocks without a tool result the
/// server put in front (a question's answer or dismissal, SME-34), which
/// the optimistic copy doesn't have. Unparseable content compares whole.
#[cfg(any(feature = "web", test))]
fn sent_part(message: &Message) -> Result<Vec<ContentBlock>, String> {
    match message.blocks() {
        Ok(blocks) => Ok(blocks
            .into_iter()
            .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
            .collect()),
        Err(_) => Err(message.content.clone()),
    }
}

#[cfg(any(feature = "web", test))]
fn merge_messages_by_id(existing: &mut Vec<Message>, incoming: Vec<Message>) {
    for message in incoming {
        if !existing.iter().any(|m| m.id == message.id) {
            existing.push(message);
        }
    }
}

/// CSS class suffix for a todo's status marker — a plain string mapping,
/// not a `Display` impl, since this is presentation-only and the panel is
/// the only caller.
fn todo_status_class(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in-progress",
        TodoStatus::Completed => "completed",
    }
}

/// Maps a live-panel keydown to a `BrowserInputEvent`, or `None` for a
/// key that isn't meaningful to forward (a bare modifier like Shift/Alt/
/// Control on its own, an unrecognized named key, ...). A printable
/// character forwards as `TypeText`; a named key forwards as `PressKey`
/// only if `browsing::server::named_key_event_fields` (checked
/// server-side too — this is just avoiding an obviously-doomed round
/// trip) recognizes it. Unconditional (not web-gated): called from the
/// main render body's `onkeydown` handler, part of the same shared rsx
/// tree the server target compiles too — same reason `todo_status_class`
/// is unconditional. A character typed with Ctrl/Cmd held is a shortcut,
/// not text (Ctrl+V must not type a "v"), so it isn't forwarded — the
/// caller then leaves it to the viewer's own browser. Ctrl+Alt is let
/// through: that's how AltGr reports itself on Windows, and AltGr is
/// ordinary typing on many layouts. A named key carries its modifiers, so
/// Shift+Tab or Ctrl+Backspace do what they would on a real keyboard.
fn browser_input_event_for_key(
    key: keyboard_types::Key,
    modifiers: keyboard_types::Modifiers,
) -> Option<BrowserInputEvent> {
    match key {
        keyboard_types::Key::Character(_)
            if (modifiers.ctrl() || modifiers.meta()) && !modifiers.alt() =>
        {
            None
        }
        keyboard_types::Key::Character(text) => Some(BrowserInputEvent::TypeText { text }),
        named => {
            let name = named.to_string();
            matches!(
                name.as_str(),
                "Enter"
                    | "Backspace"
                    | "Tab"
                    | "Escape"
                    | "Delete"
                    | "ArrowUp"
                    | "ArrowDown"
                    | "ArrowLeft"
                    | "ArrowRight"
            )
            .then(|| BrowserInputEvent::PressKey {
                key: name,
                modifiers: cdp_modifiers(modifiers),
            })
        }
    }
}

/// CDP's modifier bitmask for `Input.dispatchKeyEvent`.
fn cdp_modifiers(modifiers: keyboard_types::Modifiers) -> i64 {
    [
        (modifiers.alt(), 1),
        (modifiers.ctrl(), 2),
        (modifiers.meta(), 4),
        (modifiers.shift(), 8),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .map(|(_, bit)| bit)
    .sum()
}

/// The live panel address bar's text: the page's URL, except while the
/// viewer is typing, when an arriving URL change mustn't overwrite them.
fn address_bar_value(editing: bool, draft: &str, url: Option<&str>) -> String {
    if editing {
        draft.to_string()
    } else {
        url.unwrap_or_default().to_string()
    }
}

/// Chromium scrolls 40px per wheel "line".
const WHEEL_PIXELS_PER_LINE: f64 = 40.0;
/// A wheel "page" is one live-panel frame (`browsing::server`'s pinned
/// 1280x800 viewport).
const WHEEL_PIXELS_PER_PAGE: (f64, f64) = (1280.0, 800.0);

/// The frame's width in the remote page's pixels (`browsing::server`'s
/// pinned 1280x800 viewport).
const FRAME_WIDTH: f64 = 1280.0;

/// A point on the live-panel frame as shown (`shown_width` pixels wide,
/// scaled to fit the panel) in the remote page's own pixels.
fn frame_point(x: f64, y: f64, shown_width: f64) -> (f64, f64) {
    if shown_width <= 0.0 {
        return (x, y);
    }
    let scale = FRAME_WIDTH / shown_width;
    (x * scale, y * scale)
}

/// A wheel event's delta in pixels, whatever unit the viewer's browser
/// reported it in — Firefox reports lines, so passing the raw number
/// through would scroll ~3px per notch.
fn wheel_delta_pixels(delta: WheelDelta) -> (f64, f64) {
    match delta {
        WheelDelta::Pixels(v) => (v.x, v.y),
        WheelDelta::Lines(v) => (v.x * WHEEL_PIXELS_PER_LINE, v.y * WHEEL_PIXELS_PER_LINE),
        WheelDelta::Pages(v) => (v.x * WHEEL_PIXELS_PER_PAGE.0, v.y * WHEEL_PIXELS_PER_PAGE.1),
    }
}

/// Collapses each run of back-to-back mouse moves (for the same
/// conversation) down to its last one — only the pointer's latest position
/// matters, and forwarding every intermediate one would let a fast-moving
/// mouse queue up far more requests than the page needs. Anything else
/// keeps its place and order.
fn coalesce_mouse_moves(batch: Vec<(i64, BrowserInputEvent)>) -> Vec<(i64, BrowserInputEvent)> {
    let mut out: Vec<(i64, BrowserInputEvent)> = Vec::with_capacity(batch.len());
    for item in batch {
        let replaces_last = matches!(
            (out.last(), &item),
            (
                Some((last_id, BrowserInputEvent::MouseMove { .. })),
                (id, BrowserInputEvent::MouseMove { .. }),
            ) if last_id == id
        );
        if replaces_last {
            out.pop();
        }
        out.push(item);
    }
    out
}

/// Adds a streamed reply's `text`, which starts `offset` bytes in. A tab
/// that connected mid-reply fetched the text so far and then receives the
/// deltas published since it subscribed, some already in that text; only
/// the part past what it has is added (SME-51 B3).
#[cfg(any(feature = "web", test))]
fn apply_reply_delta(reply: &mut Option<String>, offset: usize, text: &str) {
    let current = reply.get_or_insert_with(String::new);
    let have = current.len();
    // At or past the end: new text. (Past it means something was missed;
    // adding it still beats dropping it.)
    if offset >= have {
        current.push_str(text);
        return;
    }
    let already = have - offset;
    if already < text.len() && text.is_char_boundary(already) {
        current.push_str(&text[already..]);
    }
}

/// One pod's widget state — just enough to group its terminals under a
/// header in the sandbox panel.
#[derive(Clone, Debug, PartialEq)]
struct SandboxPodPanelEntry {
    pod_id: i64,
    status: String,
    /// Preview links for the user's own browser (SME-42).
    previews: Vec<SandboxPreview>,
}

/// One output line's widget state — same shape as the wire `SandboxOutputLine`,
/// kept as its own type for the same reason every other panel entry mirrors
/// rather than reuses its wire counterpart (see `SandboxPodPanelEntry` vs.
/// `SandboxPodSummary`).
#[derive(Clone, Debug, PartialEq)]
struct SandboxOutputLinePanelEntry {
    stream: String,
    data: String,
    /// The agent's per-command `seq`, when known, so a reconnect doesn't
    /// add a line twice (SME-51 B3).
    seq: Option<i64>,
}

/// One command's widget state within a terminal's history. `output` is a
/// single sequence in true chronological order (each line tagged with which stream it came
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
/// and terminals — the snapshot is authoritative, flattened from its
/// pod→terminal nesting into the two separate flat lists the panel renders
/// from. A pod or terminal the snapshot leaves out is gone: it went away
/// while the tab wasn't listening (SME-43).
#[cfg(any(feature = "web", test))]
fn merge_sandbox_snapshot(
    pods: &mut Vec<SandboxPodPanelEntry>,
    terminals: &mut Vec<SandboxTerminalPanelEntry>,
    snapshot: SandboxSnapshot,
) {
    pods.retain(|p| snapshot.pods.iter().any(|s| s.pod_id == p.pod_id));
    terminals.retain(|t| {
        snapshot
            .pods
            .iter()
            .flat_map(|s| &s.terminals)
            .any(|s| s.terminal_id == t.terminal_id)
    });
    for pod in snapshot.pods {
        if let Some(entry) = pods.iter_mut().find(|p| p.pod_id == pod.pod_id) {
            entry.status = pod.status.clone();
            entry.previews = pod.previews.clone();
        } else {
            pods.push(SandboxPodPanelEntry {
                pod_id: pod.pod_id,
                status: pod.status.clone(),
                previews: pod.previews.clone(),
            });
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
                        .map(|line| SandboxOutputLinePanelEntry {
                            stream: line.stream,
                            data: line.data,
                            seq: Some(line.seq),
                        })
                        .collect(),
                })
                .collect();
            if let Some(entry) = terminals
                .iter_mut()
                .find(|t| t.terminal_id == terminal.terminal_id)
            {
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
/// panel here: a terminated pod is gone, not just relabeled — see
/// SME-10's "How."
#[cfg(any(feature = "web", test))]
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
        pods.push(SandboxPodPanelEntry { pod_id, status, previews: Vec::new() });
    }
}

/// Applies one live `SandboxPreviewUpdate`: the pod's preview list becomes
/// the one carried. A pod the panel doesn't know yet is skipped — the next
/// snapshot brings it, previews included.
#[cfg(any(feature = "web", test))]
fn apply_sandbox_preview_update(pods: &mut [SandboxPodPanelEntry], pod_id: i64, previews: Vec<SandboxPreview>) {
    if let Some(entry) = pods.iter_mut().find(|p| p.pod_id == pod_id) {
        entry.previews = previews;
    }
}

/// Applies one live `SandboxTerminalUpdate` — same upsert-or-remove shape
/// as `apply_sandbox_pod_update`.
#[cfg(any(feature = "web", test))]
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
#[cfg(any(feature = "web", test))]
fn apply_sandbox_command_update(
    terminals: &mut Vec<SandboxTerminalPanelEntry>,
    terminal_id: i64,
    command_id: String,
    command: Option<String>,
    status: String,
    exit_code: Option<i32>,
    stream: Option<String>,
    latest_output: Option<String>,
    position: Option<i64>,
) {
    let Some(entry) = terminals.iter_mut().find(|t| t.terminal_id == terminal_id) else {
        return;
    };

    if let Some(command) = command {
        // Already known from the snapshot (SME-51 B3).
        if entry.commands.iter().any(|c| c.command_id == command_id) {
            return;
        }
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
        // A line the snapshot already has (SME-51 B3).
        let last_seq = current.output.iter().filter_map(|l| l.seq).max();
        if let (Some(seq), Some(last)) = (position, last_seq)
            && seq <= last
        {
            return;
        }
        current
            .output
            .push(SandboxOutputLinePanelEntry { stream, data, seq: position });
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
/// SME-11's "Diff rendering."
fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    similar::TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|change| {
            let kind = match change.tag() {
                similar::ChangeTag::Equal => DiffLineKind::Equal,
                similar::ChangeTag::Delete => DiffLineKind::Removed,
                similar::ChangeTag::Insert => DiffLineKind::Added,
            };
            let content = change
                .as_str()
                .unwrap_or_default()
                .trim_end_matches('\n')
                .to_string();
            DiffLine { kind, content }
        })
        .collect()
}

/// Label for a `ToolResult` card's header, distinguishing a normal result
/// from an error at a glance without repeating "error"/"result" as raw text
/// the caller has to style around.
/// A tool call described the way a person would say it ("Wrote
/// /home/sandbox/primes.py", "Ran `python3 primes.py`"), for the compact
/// row that replaces a raw card per call and per result (SME-41 D2). The
/// raw input and result stay one click away.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| -> String {
        match input.get(key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    };
    if let Some(rest) = name.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or(("", rest));
        let query = field("query");
        if tool.contains("web_search") && !query.is_empty() {
            return format!("Searched the web for \"{query}\"");
        }
        return format!("Used {tool} ({server})");
    }
    match name {
        "create_pod" => "Started the sandbox".to_string(),
        "terminate_pod" => "Stopped the sandbox".to_string(),
        "list_pods" => "Checked the sandbox".to_string(),
        "create_terminal" => "Opened a terminal".to_string(),
        "terminate_terminal" => "Closed a terminal".to_string(),
        "list_terminals" => "Listed the terminals".to_string(),
        "run_terminal_command" => format!("Ran `{}`", field("command")),
        "send_signal" => format!("Sent {} to a command", field("signal")),
        "terminal_command_status" => "Checked a command".to_string(),
        "read_terminal_output" => "Read a command's output".to_string(),
        "list_commands" => "Listed recent commands".to_string(),
        "read_file" => format!("Read {}", field("path")),
        "write_file" => format!("Wrote {}", field("path")),
        "edit_file" => format!("Edited {}", field("path")),
        "list_directory" => format!("Listed {}", field("path")),
        "glob" => format!("Found files matching `{}`", field("pattern")),
        "grep" => format!("Searched files for `{}`", field("pattern")),
        "webfetch" => format!("Read {}", field("url")),
        "http_request" => {
            let method = field("method");
            let method = if method.is_empty() { "GET".to_string() } else { method.to_uppercase() };
            format!("Sent {method} {}", field("url"))
        }
        "open_browser_session" => "Opened a browser".to_string(),
        "close_browser_session" => "Closed the browser".to_string(),
        "browser_navigate" => format!("Opened {} in the browser", field("url")),
        "browser_click" => "Clicked in the browser".to_string(),
        "browser_fill" => "Typed into the browser".to_string(),
        "browser_back" => "Went back in the browser".to_string(),
        "browser_read" => "Read the browser page".to_string(),
        "sandbox_preview_url" => match field("host") {
            host if host.is_empty() => format!("Shared a preview of port {}", field("port")),
            host => format!("Shared a preview of {host}:{}", field("port")),
        },
        "todowrite" => "Updated the todo list".to_string(),
        "todoread" => "Checked the todo list".to_string(),
        "clone_repo" => format!("Cloned {}", field("url")),
        "lsp_servers" => "Listed the language servers".to_string(),
        "start_language_server" => format!("Started {}", field("name")),
        "lsp" => match field("operation").as_str() {
            "workspace_symbols" => format!("Searched symbols for `{}`", field("query")),
            "document_symbols" => format!("Listed the symbols in {}", field("path")),
            "diagnostics" => format!("Checked {} for problems", field("path")),
            "rename" => format!("Renamed a symbol to {}", field("new_name")),
            operation => format!("Looked up {} at {}:{}", operation.replace('_', " "), field("path"), field("line")),
        },
        other => format!("Used {other}"),
    }
}

/// Where a loaded `AGENTS.md` came from, for the context detail view.
fn instructions_source(doc: &crate::git::ProjectInstructions) -> String {
    let commit: String = doc.commit.as_deref().unwrap_or("").chars().take(7).collect();
    let origin = if commit.is_empty() {
        doc.repo_url.clone()
    } else {
        format!("{} at {commit}", doc.repo_url)
    };
    if doc.truncated {
        format!("{origin} \u{b7} {} bytes, only the first 32 KiB loaded", doc.file_bytes)
    } else {
        format!("{origin} \u{b7} {} bytes", doc.file_bytes)
    }
}

/// What the sandbox panel says about a repo's `AGENTS.md` files, if it
/// has any: which the model has loaded.
fn instructions_label(repo: &RepoSummary) -> Option<String> {
    if !repo.loaded_instructions.is_empty() {
        Some(format!("Loaded {}", repo.loaded_instructions.join(", ")))
    } else if !repo.agents_files.is_empty() {
        Some("AGENTS.md not loaded".to_string())
    } else {
        None
    }
}

fn repo_status_class(status: RepoStatus) -> &'static str {
    match status {
        RepoStatus::Cloning => "cloning",
        RepoStatus::Ready => "ready",
        RepoStatus::Failed => "failed",
    }
}

/// A repo's state under its path in the sandbox panel.
fn repo_detail(repo: &RepoSummary) -> String {
    match repo.status {
        RepoStatus::Cloning => match &repo.requested_branch {
            Some(branch) => format!("Cloning {branch}\u{2026}"),
            None => "Cloning\u{2026}".to_string(),
        },
        RepoStatus::Failed => "Clone failed".to_string(),
        RepoStatus::Ready => {
            let branch = repo.branch.clone().unwrap_or_default();
            let commit: String = repo.commit.clone().unwrap_or_default().chars().take(7).collect();
            format!("{branch} \u{b7} {commit}")
        }
    }
}

fn tool_result_label(is_error: bool) -> &'static str {
    if is_error {
        "Tool error"
    } else {
        "Tool result"
    }
}

/// Each tool call's result, by the call's id: its content and whether it
/// failed. A call's row shows its result folded in (SME-41 D2).
fn tool_results_by_id(messages: &[Message]) -> HashMap<String, (String, bool)> {
    messages
        .iter()
        .filter_map(|m| m.blocks().ok())
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                Some((tool_use_id, (content, is_error.unwrap_or(false))))
            }
            _ => None,
        })
        .collect()
}

/// Maps every `ToolUse` block's id to its tool name across every message in
/// the conversation. A `ToolResult` block only carries the id of the call
/// it answers, not the tool's name — this is how `render_block_element`
/// tells a result whose call is in the conversation (shown in the call's
/// row) from an orphaned one.
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

/// Focus the context view's first and last controls, for the sentinels
/// that keep Tab inside it.
const FOCUS_FIRST_IN_CONTEXT_DETAIL: &str = "document.querySelector('.context-detail-close')?.focus();";
const FOCUS_LAST_IN_CONTEXT_DETAIL: &str = "const p = document.querySelector('.context-detail-panel'); \
     if (p) { const f = p.querySelectorAll('button, summary, a[href], input, select, textarea, [tabindex=\"0\"]'); \
     (f[f.length - 1] || p).focus(); }";

/// Installed on the transcript once it mounts: remembers the element under
/// the pointer and where it was on screen, kept current as the pointer
/// moves and the transcript scrolls (SME-75).
const TRANSCRIPT_ANCHOR_SETUP: &str = "const m = document.querySelector('.messages'); \
    if (m && !m.__smeltAnchor) { m.__smeltAnchor = true; \
      const remember = (x, y) => { const e = document.elementFromPoint(x, y); \
        window.__smeltTranscriptAnchor = e && e !== m && m.contains(e) \
          ? { el: e, top: e.getBoundingClientRect().top, x, y } : null; }; \
      m.addEventListener('pointermove', ev => remember(ev.clientX, ev.clientY)); \
      m.addEventListener('scroll', () => { const a = window.__smeltTranscriptAnchor; if (a) remember(a.x, a.y); }); \
      m.addEventListener('pointerleave', () => { window.__smeltTranscriptAnchor = null; }); }";

/// After a layout change with the pointer over the transcript: scrolls it
/// by however far the remembered element moved, so what's under the
/// pointer stays where it was, whichever way the layout moved it. Says
/// `none` when there was no such element, `bottom` when the transcript is
/// at its bottom afterwards (nothing left to snap), else `kept`.
const TRANSCRIPT_KEEP_ANCHOR: &str = "const m = document.querySelector('.messages'); \
    const a = window.__smeltTranscriptAnchor; \
    if (!(m && a && a.el.isConnected)) { return 'none'; } \
    const d = a.el.getBoundingClientRect().top - a.top; \
    if (d) { m.scrollTop += d; } a.top = a.el.getBoundingClientRect().top; \
    return m.scrollHeight - m.scrollTop - m.clientHeight <= 1 ? 'bottom' : 'kept';";

/// A snap to the bottom for new content: the remembered element's position
/// is forgotten first, so a layout change handled in the same render
/// doesn't scroll back by the snap's own movement (SME-75 code review 2).
const TRANSCRIPT_FORGET_ANCHOR: &str = "window.__smeltTranscriptAnchor = null;";

/// After a layout change with the pointer over the transcript: keeps what's
/// under the pointer in place (see `TRANSCRIPT_KEEP_ANCHOR`), or, when
/// there was nothing under it (the messages had only just arrived), snaps
/// to the bottom as if the pointer weren't there. Either way the pending
/// snap is settled here or on the pointer leaving.
async fn keep_transcript_anchor(el: MountedEvent, mut pending: Signal<bool>) {
    let outcome = document::eval(TRANSCRIPT_KEEP_ANCHOR).await.ok();
    match outcome.as_ref().and_then(|v| v.as_str()) {
        Some("kept") => {}
        Some("bottom") => pending.set(false),
        _ => {
            pending.set(false);
            scroll_to_bottom(el).await;
        }
    }
}

/// The content effect's snap: forgets the anchor, then scrolls to the
/// bottom (see `TRANSCRIPT_FORGET_ANCHOR`).
async fn snap_transcript_for_content(el: MountedEvent) {
    let _ = document::eval(TRANSCRIPT_FORGET_ANCHOR).await;
    scroll_to_bottom(el).await;
}

/// Scrolls `el` to its bottom at once.
async fn scroll_to_bottom(el: MountedEvent) {
    if let Ok(size) = el.get_scroll_size().await {
        let _ = el
            .scroll(PixelsVector2D::new(0.0, size.height), ScrollBehavior::Instant)
            .await;
    }
}

/// How full the model's context window is, as a whole-number percent — the
/// always-visible indicator's own number. `None` if `usage` hasn't arrived
/// yet (a brand-new conversation). Clamped to 100 — a conversation caught
/// mid-compaction, or a `context_window` estimate that's simply wrong for
/// the configured model, shouldn't render a bar past full. See
/// SME-18.
fn context_usage_percent(snapshot: &ContextUsageSnapshot) -> Option<u32> {
    let usage = snapshot.usage.as_ref()?;
    if snapshot.context_window == 0 {
        return None;
    }
    let breakdown = context_usage_breakdown(usage, snapshot.context_window);
    let used = breakdown
        .context_window
        .saturating_sub(breakdown.free_tokens);
    let percent = used.saturating_mul(100) / breakdown.context_window;
    Some(percent.min(100) as u32)
}

/// One category's worth of a context-window breakdown — the detail view's
/// visual meter splits usage into exactly these segments. Cache tokens are
/// additive to `input_tokens` (Anthropic counts fresh vs. cached input
/// separately) but still occupy real context-window space, so all four
/// count toward `free_tokens`'s subtraction — the same total
/// `context_usage_percent` reports, just split by category here.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ContextUsageBreakdown {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    free_tokens: u64,
    context_window: u64,
}

fn context_usage_breakdown(usage: &TokenUsage, context_window: u32) -> ContextUsageBreakdown {
    let input_tokens = usage.input_tokens.max(0) as u64;
    let output_tokens = usage.output_tokens.max(0) as u64;
    let cache_creation_tokens = usage.cache_creation_input_tokens.max(0) as u64;
    let cache_read_tokens = usage.cache_read_input_tokens.max(0) as u64;
    let window = context_window as u64;
    let used = input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens;
    ContextUsageBreakdown {
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        free_tokens: window.saturating_sub(used),
        context_window: window,
    }
}

/// The detail view's visual context-usage meter — a horizontal stacked bar,
/// one segment per non-zero category in `breakdown`, sized proportionally
/// (`flex-grow` set to each segment's own token count, so the segments
/// plus the free remainder always sum to the full bar with no manual
/// percent math). Fixed category order (input, output, cache creation,
/// cache read, free), never reassigned by size — see
/// SME-18 and the dataviz skill's
/// categorical-color rule. A zero-valued category is skipped entirely
/// (not rendered at flex-grow: 0) so it can't leave a stray 2px gap next
/// to nothing.
fn render_context_meter(breakdown: &ContextUsageBreakdown) -> Element {
    let segment = |category: &'static str, tokens: u64, label: &str| {
        if tokens == 0 {
            return rsx! {};
        }
        let percent = if breakdown.context_window > 0 {
            tokens.saturating_mul(100) / breakdown.context_window
        } else {
            0
        };
        rsx! {
            div {
                key: "{category}",
                class: "context-meter-segment",
                "data-category": category,
                style: "flex-grow: {tokens}",
                tabindex: 0,
                role: "img",
                "aria-label": "{label}: {tokens} tokens, {percent}% of context",
                span { class: "context-meter-tooltip", "{label}: {tokens} ({percent}%)" }
            }
        }
    };

    rsx! {
        div { class: "context-meter",
            div { class: "context-meter-track",
                {segment("input", breakdown.input_tokens, "Input")}
                {segment("output", breakdown.output_tokens, "Output")}
                {segment("cache-creation", breakdown.cache_creation_tokens, "Cache creation")}
                {segment("cache-read", breakdown.cache_read_tokens, "Cache read")}
                {segment("free", breakdown.free_tokens, "Free")}
            }
            div { class: "context-meter-legend",
                if breakdown.input_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "input" }
                        "Input"
                    }
                }
                if breakdown.output_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "output" }
                        "Output"
                    }
                }
                if breakdown.cache_creation_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "cache-creation" }
                        "Cache creation"
                    }
                }
                if breakdown.cache_read_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "cache-read" }
                        "Cache read"
                    }
                }
                if breakdown.free_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "free" }
                        "Free"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod context_usage_tests {
    use super::*;

    #[test]
    fn test_context_usage_percent_none_when_usage_not_yet_known() {
        let snapshot = ContextUsageSnapshot {
            usage: None,
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), None);
    }

    #[test]
    fn test_context_usage_percent_computes_input_plus_output_over_window() {
        let snapshot = ContextUsageSnapshot {
            usage: Some(TokenUsage {
                input_tokens: 40_000,
                output_tokens: 10_000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), Some(25));
    }

    #[test]
    fn test_context_usage_percent_clamps_at_100() {
        let snapshot = ContextUsageSnapshot {
            usage: Some(TokenUsage {
                input_tokens: 500_000,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), Some(100));
    }

    #[test]
    fn test_context_usage_percent_counts_cache_tokens_too() {
        // Cache tokens are additive to input_tokens (Anthropic counts fresh
        // vs. cached input separately) — they still occupy real context
        // window space and must count toward "how full is it", not be
        // silently excluded.
        let snapshot = ContextUsageSnapshot {
            usage: Some(TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 25_000,
                cache_read_input_tokens: 25_000,
            }),
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), Some(25));
    }

    #[test]
    fn test_context_usage_breakdown_splits_every_category_and_computes_free() {
        let usage = TokenUsage {
            input_tokens: 40_000,
            output_tokens: 10_000,
            cache_creation_input_tokens: 5_000,
            cache_read_input_tokens: 5_000,
        };
        let breakdown = context_usage_breakdown(&usage, 200_000);
        assert_eq!(
            breakdown,
            ContextUsageBreakdown {
                input_tokens: 40_000,
                output_tokens: 10_000,
                cache_creation_tokens: 5_000,
                cache_read_tokens: 5_000,
                free_tokens: 140_000,
                context_window: 200_000,
            }
        );
    }

    #[test]
    fn test_context_usage_breakdown_free_never_negative_when_usage_exceeds_window() {
        let usage = TokenUsage {
            input_tokens: 150_000,
            output_tokens: 100_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let breakdown = context_usage_breakdown(&usage, 200_000);
        assert_eq!(breakdown.free_tokens, 0);
    }
}

/// Renders one content block, keyed by `{message_id}-{index}` for the
/// enclosing `for` loop. `Text` renders as an ordinary chat bubble, same as
/// always. `ToolUse`/
/// `ToolResult` render as their own centered cards, distinct from both the
/// Whether an assistant message is thinking and nothing else: no text,
/// no tool call. A model sometimes puts its whole answer in its thinking;
/// collapsed, that reply looked empty (SME-40 F14), so such a message's
/// thinking is shown open.
fn reply_is_only_thinking(role: &str, blocks: &[ContentBlock]) -> bool {
    role == "assistant"
        && !blocks.is_empty()
        && blocks.iter().all(|b| matches!(b, ContentBlock::Thinking { .. }))
}

/// Asks offered in an empty conversation (SME-41 D12): one each for code,
/// a repository and the web, the three things smelt is for.
const EXAMPLE_ASKS: [&str; 3] = [
    "Write a Python script that prints the first 20 primes, then run it",
    "Clone https://github.com/pallets/itsdangerous and run its tests",
    "Find the latest stable Rust release and summarize what's new",
];

/// How long ago a conversation was last active, as the sidebar shows it:
/// one unit, `now`, `5m`, `3h`, `2d`, `3w` (SME-41 D8).
fn short_age(seconds: i64) -> String {
    match seconds {
        s if s < 60 => "now".to_string(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}w", s / (7 * 86_400)),
    }
}

/// How long a turn has been running, as shown on its "Working…" line:
/// `8s`, `2m 05s`.
fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// A notice smelt saved into the conversation for the model (a command
/// finishing, the sandbox stopping), as the
/// short sentence the chat shows in place of a user bubble, without
/// internal ids. `None` for anything the user actually wrote. They used
/// to look like the user talking (SME-41 D3). `commands` maps a terminal
/// command's id to the command line, from `terminal_commands_by_id`.
fn system_notice(text: &str, commands: &HashMap<String, String>) -> Option<String> {
    let command = |id: &str| match commands.get(id) {
        Some(line) => format!("`{line}`"),
        None => "A command".to_string(),
    };
    if let Some(rest) = text.strip_prefix("Terminal command ") {
        if let Some((id, tail)) = rest.split_once(" finished: exit code ") {
            let code = tail.trim_end_matches('.');
            return Some(format!("{} finished (exit code {code})", command(id)));
        }
        if let Some((id, _)) = rest.split_once("'s outcome is unknown") {
            return Some(format!("{} stopped: its terminal became unreachable", command(id)));
        }
    }
    if let Some(rest) = text.strip_prefix("Sandbox pod ")
        && let Some((_, tail)) = rest.split_once(" stopped unexpectedly")
    {
        let reason = tail
            .strip_prefix(" (")
            .and_then(|r| r.split_once(')'))
            .map(|(reason, _)| format!(" ({reason})"))
            .unwrap_or_default();
        return Some(format!("The sandbox stopped unexpectedly{reason}; its terminals are gone"));
    }
    if text == crate::api::chat::STOP_NOTICE {
        return Some("Stopped.".to_string());
    }
    if text.starts_with("The user stopped sandbox pod ") {
        return Some(
            "You stopped the sandbox; its terminals are gone, and /workspace is kept".to_string(),
        );
    }
    task_notice_sentence(text)
}

/// Each terminal command's id to its command line, from the
/// `run_terminal_command` calls and their "command sent (id: ...)" results.
fn terminal_commands_by_id(messages: &[Message]) -> HashMap<String, String> {
    let blocks: Vec<ContentBlock> = messages.iter().filter_map(|m| m.blocks().ok()).flatten().collect();
    let command_lines: HashMap<&str, &str> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } if name == "run_terminal_command" => {
                Some((id.as_str(), input.get("command")?.as_str()?))
            }
            _ => None,
        })
        .collect();
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                let line = command_lines.get(tool_use_id.as_str())?;
                let id = content.strip_prefix("command sent (id: ")?.strip_suffix(')')?;
                Some((id.to_string(), line.to_string()))
            }
            _ => None,
        })
        .collect()
}

/// A message's text as shown in the chat. The removed background-task
/// suite (SME-54) saved its notices in a tagged form the model reads
/// (`<task-notification task_id=".." tool="count">finished: ..</task-notification>`);
/// shown raw, that's markup (SME-40 F13), so old conversations' notices
/// still read as a sentence. Everything else is shown as written.
fn display_text(text: &str) -> String {
    task_notice_sentence(text).unwrap_or_else(|| text.to_string())
}

fn task_notice_sentence(text: &str) -> Option<String> {
    let attr = |open: &str, name: &str| -> Option<String> {
        let start = open.find(&format!("{name}=\""))? + name.len() + 2;
        let len = open[start..].find('"')?;
        Some(open[start..start + len].to_string())
    };
    for tag in ["task-notification", "task-output"] {
        let closing = format!("</{tag}>");
        if !text.starts_with(&format!("<{tag} ")) || !text.ends_with(&closing) {
            continue;
        }
        let open_end = text.find('>')?;
        let (open, body) = (&text[..open_end], &text[open_end + 1..text.len() - closing.len()]);
        let (task_id, tool) = (attr(open, "task_id")?, attr(open, "tool")?);
        return Some(match attr(open, "stream") {
            Some(stream) => format!("Background task {tool} ({task_id}) {stream}: {body}"),
            None => format!("Background task {tool} ({task_id}) {body}"),
        });
    }
    None
}

/// The answer lines of an `ask_user` result, as the model got them:
/// "1. Delete: Yes", or what it was told when the user wrote instead.
fn answered_lines(result: &str) -> Vec<String> {
    match result.strip_prefix("The user answered:") {
        Some(rest) => rest.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
        None => vec!["Not answered: you wrote a message instead.".to_string()],
    }
}

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
    tool_results: &HashMap<String, (String, bool)>,
    commands: &HashMap<String, String>,
    thinking_open: bool,
) -> Element {
    let key = format!("{message_id}-{index}");
    let timestamp = format_timestamp(created_at, tz_offset_minutes);
    match block {
        ContentBlock::Text { text } if role == "user" && system_notice(text, commands).is_some() => {
            let notice = system_notice(text, commands).unwrap_or_default();
            rsx! {
                div { key: "{key}", class: "system-notice",
                    span { class: "system-notice-text", "{notice}" }
                    span { class: "timestamp", "{timestamp}" }
                }
            }
        }
        ContentBlock::Text { text } => rsx! {
            div { key: "{key}", class: "message message-{role}",
                div { class: "message-text", "{display_text(text)}" }
                span { class: "timestamp", "{timestamp}" }
            }
        },
        // Collapsed by default, a native <details> — the reasoning is
        // rarely what someone wants
        // to read on every turn, but shouldn't cost space (or a click
        // through some separate view) when they do.
        ContentBlock::Thinking { thinking, .. } => rsx! {
            details { key: "{key}", class: "thinking-block", open: thinking_open,
                summary { class: "thinking-summary",
                    span { class: "thinking-icon", "💭" }
                    span { "Thinking" }
                    span { class: "timestamp", "{timestamp}" }
                }
                div { class: "thinking-body", "{thinking}" }
            }
        },
        // `edit_file` renders as an actual line-level diff instead of a
        // generic tool-call card showing two raw JSON strings — its
        // `old_string`/`new_string` already carry everything a diff needs.
        // See SME-11's "Diff rendering."
        ContentBlock::ToolUse { id, name, input } if name == "edit_file" => {
            let failure = tool_results.get(id).filter(|(_, failed)| *failed).map(|(content, _)| content.clone());
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let old_string = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_string = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lines = diff_lines(old_string, new_string);
            rsx! {
                div { key: "{key}", class: "file-edit-diff",
                    div { class: "tool-call-header",
                        span { class: "tool-call-icon", "✏️" }
                        span { "Edited" }
                        code { class: "tool-call-name", "{path}" }
                    }
                    if let Some(failure) = failure {
                        div { class: "tool-row-failure", "Failed: {failure}" }
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
        // The model's question (SME-34): while it waits, the card below
        // the transcript asks it; once answered, a compact row with each
        // question and the answer the model got.
        ContentBlock::ToolUse { id, name, input } if name == ASK_USER => match tool_results.get(id) {
            Some((result, _)) => {
                let questions: Vec<String> = input
                    .get("questions")
                    .and_then(|q| q.as_array())
                    .map(|qs| qs.iter().filter_map(|q| q.get("question").and_then(|t| t.as_str()).map(str::to_string)).collect())
                    .unwrap_or_default();
                let answer = answered_lines(result);
                rsx! {
                    div { key: "{key}", class: "question-answered",
                        div { class: "question-answered-header",
                            span { class: "question-answered-icon", "?" }
                            span { "Asked" }
                            span { class: "timestamp", "{timestamp}" }
                        }
                        for (i , question) in questions.iter().enumerate() {
                            p { key: "q{i}", class: "question-answered-question", "{question}" }
                        }
                        for (i , line) in answer.iter().enumerate() {
                            p { key: "a{i}", class: "question-answered-answer", "{line}" }
                        }
                    }
                }
            }
            None => rsx! {},
        },
        // One compact line per call, saying what it did, with its result
        // folded in: the raw input and result are one click away, and a
        // failed call starts open (SME-41 D2).
        ContentBlock::ToolUse { id, name, input } => {
            let summary = tool_summary(name, input);
            let pretty_input = format_tool_input(input);
            let result = tool_results.get(id).cloned();
            let failed = result.as_ref().is_some_and(|(_, failed)| *failed);
            rsx! {
                details {
                    key: "{key}",
                    class: if failed { "tool-row tool-row-failed" } else { "tool-row" },
                    open: failed,
                    summary { class: "tool-row-summary",
                        span { class: "tool-row-marker" }
                        span { class: "tool-row-text", "{summary}" }
                        if failed {
                            span { class: "tool-row-status", "failed" }
                        }
                    }
                    div { class: "tool-row-detail",
                        div { class: "tool-row-label", "{name}" }
                        pre { class: "tool-row-input", "{pretty_input}" }
                        if let Some((content, _)) = result {
                            div { class: "tool-row-label", if failed { "Error" } else { "Result" } }
                            pre { class: "tool-row-result", "{content}" }
                        }
                    }
                }
            }
        }
        // A result whose call is in the conversation is shown in that
        // call's row. Only an orphaned result gets a card.
        ContentBlock::ToolResult { tool_use_id, .. } if tool_names.contains_key(tool_use_id) => {
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
        // Auto-compaction's own output — visible as something that
        // happened (per SME-18's "visible
        // to the user" decision), collapsed by default same as `Thinking`
        // above so it doesn't dominate the transcript on every reload.
        ContentBlock::CompactionSummary { summary, .. } => rsx! {
            details { key: "{key}", class: "compaction-summary-block",
                summary { class: "compaction-summary-header",
                    span { class: "compaction-summary-icon", "🗜️" }
                    span { "Conversation compacted" }
                    span { class: "timestamp", "{timestamp}" }
                }
                div { class: "compaction-summary-body", "{summary}" }
            }
        },
        // Purely structural (see its own doc comment) — nothing a human
        // typed or needs to see; the `CompactionSummary` divider above is
        // what actually marks "compaction happened" in the transcript.
        ContentBlock::CompactionPlaceholder { .. } => rsx! {},
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
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "a".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "b".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "c".to_string()
                },
            ]
        );
    }

    #[test]
    fn test_diff_lines_detects_a_changed_line_as_removed_plus_added() {
        let result = diff_lines("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(
            result,
            vec![
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "a".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Removed,
                    content: "b".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    content: "x".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "c".to_string()
                },
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
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "fn f() {".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Removed,
                    content: "    old_body();".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    content: "    new_body();".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Equal,
                    content: "}".to_string()
                },
            ]
        );
    }

    fn message_with_blocks(id: i64, role: &str, blocks: Vec<ContentBlock>) -> Message {
        Message {
            id,
            conversation_id: 1,
            role: role.to_string(),
            content: serde_json::to_string(&blocks).expect("ContentBlock always serializes"),
            created_at: chrono::Utc::now().naive_utc(),
        }
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
            tool_use_message(2, "call_1", "read_file"),
            tool_use_message(3, "call_2", "todoread"),
        ];
        let names = tool_use_names_by_id(&messages);
        assert_eq!(names.get("call_1").map(String::as_str), Some("read_file"));
        assert_eq!(names.get("call_2").map(String::as_str), Some("todoread"));
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

    fn message_in(conversation_id: i64, id: i64) -> Message {
        Message {
            conversation_id,
            ..test_message(id)
        }
    }

    fn ids(messages: &[Message]) -> Vec<i64> {
        messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn test_apply_loaded_messages_keeps_a_newer_message_a_stale_load_missed() {
        // The reply (3) arrived through a merge while an older load, fetched
        // before it was saved, was still in flight.
        let current = vec![message_in(7, 1), message_in(7, 2), message_in(7, 3)];
        let loaded = vec![message_in(7, 1), message_in(7, 2)];
        assert_eq!(ids(&apply_loaded_messages(&current, loaded, 7)), vec![1, 2, 3]);
    }

    #[test]
    fn test_apply_loaded_messages_drops_another_conversations_messages() {
        let current = vec![message_in(8, 10), message_in(8, 11)];
        let loaded = vec![message_in(7, 1)];
        assert_eq!(ids(&apply_loaded_messages(&current, loaded, 7)), vec![1]);
    }

    #[test]
    fn test_apply_loaded_messages_does_not_duplicate() {
        let current = vec![message_in(7, 1), message_in(7, 2)];
        let loaded = vec![message_in(7, 1), message_in(7, 2)];
        assert_eq!(ids(&apply_loaded_messages(&current, loaded, 7)), vec![1, 2]);
    }

    #[test]
    fn test_apply_loaded_messages_lets_the_load_replace_optimistic_placeholders() {
        // A placeholder (negative id) stands in for a sent message until the
        // real row arrives; keeping it alongside the loaded copy would show
        // the message twice.
        let current = vec![message_in(7, 1), message_in(7, -1)];
        let loaded = vec![message_in(7, 1), message_in(7, 2)];
        assert_eq!(ids(&apply_loaded_messages(&current, loaded, 7)), vec![1, 2]);
    }

    fn user_text(id: i64, text: &str) -> Message {
        Message {
            id,
            conversation_id: 1,
            role: "user".to_string(),
            content: serde_json::to_string(&[ContentBlock::Text { text: text.to_string() }])
                .expect("serializes"),
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    #[test]
    fn test_accept_saved_messages_replaces_the_optimistic_copy() {
        let mut existing = vec![user_text(1, "earlier"), user_text(-1, "hello"), user_text(-2, "again")];
        accept_saved_messages(&mut existing, vec![user_text(5, "hello")]);
        let ids: Vec<i64> = existing.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![1, -2, 5], "the saved message replaces its own copy, not another");
    }

    #[test]
    fn test_accept_saved_messages_replaces_one_copy_per_saved_message() {
        let mut existing = vec![user_text(-1, "yes"), user_text(-2, "yes")];
        accept_saved_messages(&mut existing, vec![user_text(7, "yes")]);
        let ids: Vec<i64> = existing.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![-2, 7], "two identical sends: one replaced so far");
        accept_saved_messages(&mut existing, vec![user_text(7, "yes")]);
        assert_eq!(existing.len(), 2, "a repeat of an already shown message changes nothing");
    }

    /// Code review 1 (SME-34): a message sent while a question waits is
    /// saved with the question's result in front of the user's text. Its
    /// optimistic copy (just the text) is still replaced.
    #[test]
    fn test_accept_saved_messages_matches_a_copy_saved_behind_a_tool_result() {
        let mut existing = vec![user_text(-1, "keep it")];
        let mut saved = user_text(9, "keep it");
        saved.content = serde_json::to_string(&[
            ContentBlock::ToolResult {
                tool_use_id: "toolu_ask".to_string(),
                content: "The user didn't answer these questions. Their message follows.".to_string(),
                is_error: None,
            },
            ContentBlock::Text { text: "keep it".to_string() },
        ])
        .expect("serializes");
        accept_saved_messages(&mut existing, vec![saved]);
        let ids: Vec<i64> = existing.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![9], "the copy is replaced, not shown twice");
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

    /// SME-51 B3: a tab that reconnects mid-reply fetches the text so far,
    /// then gets the deltas published since it subscribed. Some are
    /// already in that text and mustn't be added twice.
    #[test]
    fn test_a_reconnect_adds_only_reply_text_it_doesnt_have() {
        let mut reply = Some("one two ".to_string());
        for (offset, text) in [(0, "one "), (4, "two "), (8, "three ")] {
            apply_reply_delta(&mut reply, offset, text);
        }
        assert_eq!(reply.as_deref(), Some("one two three "));
        // A delta that straddles the end of the fetched text.
        let mut reply = Some("one tw".to_string());
        apply_reply_delta(&mut reply, 4, "two ");
        assert_eq!(reply.as_deref(), Some("one two "));
        let mut reply = None;
        apply_reply_delta(&mut reply, 0, "hi");
        assert_eq!(reply.as_deref(), Some("hi"));
    }

    #[test]
    fn test_a_reconnect_adds_only_commands_and_lines_it_doesnt_have() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0].commands.push(test_sandbox_command_entry("cmd-1", "echo"));
        terminals[0].commands[0].output = vec![
            SandboxOutputLinePanelEntry { stream: "stdout".into(), data: "one".into(), seq: Some(1) },
            SandboxOutputLinePanelEntry { stream: "stdout".into(), data: "two".into(), seq: Some(2) },
        ];
        // Replayed: the command starting, and line 2.
        apply_sandbox_command_update(&mut terminals, 10, "cmd-1".into(), Some("echo".into()), "running".into(), None, None, None, None);
        apply_sandbox_command_update(&mut terminals, 10, "cmd-1".into(), None, "running".into(), None, Some("stdout".into()), Some("two".into()), Some(2));
        apply_sandbox_command_update(&mut terminals, 10, "cmd-1".into(), None, "running".into(), None, Some("stdout".into()), Some("three".into()), Some(3));
        assert_eq!(terminals[0].commands.len(), 1, "the command was added twice");
        let data: Vec<_> = terminals[0].commands[0].output.iter().map(|l| l.data.as_str()).collect();
        assert_eq!(data, vec!["one", "two", "three"]);
    }

    fn no_mods() -> keyboard_types::Modifiers {
        keyboard_types::Modifiers::empty()
    }

    #[test]
    fn test_browser_input_event_for_key_leaves_ctrl_and_cmd_shortcuts_to_the_viewer() {
        for mods in [keyboard_types::Modifiers::CONTROL, keyboard_types::Modifiers::META] {
            assert_eq!(
                browser_input_event_for_key(keyboard_types::Key::Character("v".to_string()), mods),
                None,
                "{mods:?}+V should not type a v"
            );
        }
    }

    #[test]
    fn test_browser_input_event_for_key_still_types_altgr_characters() {
        // Windows reports AltGr as Ctrl+Alt.
        assert_eq!(
            browser_input_event_for_key(
                keyboard_types::Key::Character("@".to_string()),
                keyboard_types::Modifiers::CONTROL | keyboard_types::Modifiers::ALT,
            ),
            Some(BrowserInputEvent::TypeText { text: "@".to_string() })
        );
    }

    #[test]
    fn test_browser_input_event_for_key_types_shifted_characters() {
        assert_eq!(
            browser_input_event_for_key(
                keyboard_types::Key::Character("A".to_string()),
                keyboard_types::Modifiers::SHIFT,
            ),
            Some(BrowserInputEvent::TypeText { text: "A".to_string() })
        );
    }

    #[test]
    fn test_browser_input_event_for_key_carries_modifiers_on_named_keys() {
        assert_eq!(
            browser_input_event_for_key(keyboard_types::Key::Tab, keyboard_types::Modifiers::SHIFT),
            Some(BrowserInputEvent::PressKey { key: "Tab".to_string(), modifiers: 8 })
        );
        assert_eq!(
            browser_input_event_for_key(
                keyboard_types::Key::Backspace,
                keyboard_types::Modifiers::CONTROL
            ),
            Some(BrowserInputEvent::PressKey { key: "Backspace".to_string(), modifiers: 2 })
        );
    }

    #[test]
    fn test_cdp_modifiers_sets_one_bit_per_held_modifier() {
        use keyboard_types::Modifiers;
        assert_eq!(cdp_modifiers(Modifiers::empty()), 0);
        assert_eq!(cdp_modifiers(Modifiers::ALT), 1);
        assert_eq!(cdp_modifiers(Modifiers::CONTROL), 2);
        assert_eq!(cdp_modifiers(Modifiers::META), 4);
        assert_eq!(cdp_modifiers(Modifiers::SHIFT), 8);
        assert_eq!(cdp_modifiers(Modifiers::CONTROL | Modifiers::SHIFT), 10);
    }

    #[test]
    fn test_a_reply_that_is_only_thinking_is_recognized() {
        let thinking = ContentBlock::Thinking {
            thinking: "The answer is /tmp and bar.".to_string(),
            signature: String::new(),
        };
        let text = ContentBlock::Text { text: "done".to_string() };
        assert!(reply_is_only_thinking("assistant", std::slice::from_ref(&thinking)));
        assert!(!reply_is_only_thinking("assistant", &[thinking.clone(), text]));
        assert!(!reply_is_only_thinking("user", std::slice::from_ref(&thinking)));
        assert!(!reply_is_only_thinking("assistant", &[]));
    }

    #[test]
    fn test_display_text_reads_task_notices_as_sentences() {
        assert_eq!(
            display_text(r#"<task-notification task_id="lS9Y" tool="count">finished: Counted to 3</task-notification>"#),
            "Background task count (lS9Y) finished: Counted to 3"
        );
        assert_eq!(
            display_text(r#"<task-output task_id="lS9Y" tool="count" stream="stdout">count: 1/3</task-output>"#),
            "Background task count (lS9Y) stdout: count: 1/3"
        );
        assert_eq!(display_text("plain words <b>and a tag</b>"), "plain words <b>and a tag</b>");
    }

    #[test]
    fn test_short_age_uses_one_unit() {
        assert_eq!(short_age(-5), "now");
        assert_eq!(short_age(30), "now");
        assert_eq!(short_age(5 * 60 + 20), "5m");
        assert_eq!(short_age(3 * 3600 + 59 * 60), "3h");
        assert_eq!(short_age(2 * 86400 + 5), "2d");
        assert_eq!(short_age(15 * 86400), "2w");
    }

    #[test]
    fn test_format_elapsed_counts_seconds_then_minutes() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(42), "42s");
        assert_eq!(format_elapsed(125), "2m 05s");
        assert_eq!(format_elapsed(3600), "60m 00s");
    }

    #[test]
    fn test_a_stop_reads_as_stopped() {
        assert_eq!(
            system_notice(crate::api::chat::STOP_NOTICE, &HashMap::new()).as_deref(),
            Some("Stopped.")
        );
    }

    #[test]
    fn test_system_notices_read_as_short_sentences() {
        let commands = HashMap::from([("abc123".to_string(), "python3 primes.py".to_string())]);
        let notice = |t: &str| system_notice(t, &commands);
        assert_eq!(
            notice("Terminal command abc123 finished: exit code 0.").as_deref(),
            Some("`python3 primes.py` finished (exit code 0)")
        );
        assert_eq!(
            notice("Terminal command zzz finished: exit code 127.").as_deref(),
            Some("A command finished (exit code 127)")
        );
        assert_eq!(
            notice("Terminal command abc123's outcome is unknown — the terminal became unreachable while it was running.").as_deref(),
            Some("`python3 primes.py` stopped: its terminal became unreachable")
        );
        assert_eq!(
            notice("Sandbox pod 156 stopped unexpectedly (OOMKilled); every terminal running in it is no longer available.").as_deref(),
            Some("The sandbox stopped unexpectedly (OOMKilled); its terminals are gone")
        );
        assert_eq!(
            notice("Sandbox pod 19 stopped unexpectedly; every terminal running in it is no longer available.").as_deref(),
            Some("The sandbox stopped unexpectedly; its terminals are gone")
        );
        assert_eq!(
            notice("The user stopped sandbox pod 157. Its terminals, and any files outside /workspace and mounted volumes, are gone. Create a new pod if you need one.").as_deref(),
            Some("You stopped the sandbox; its terminals are gone, and /workspace is kept")
        );
        assert_eq!(
            notice(r#"<task-notification task_id="t1" tool="count">finished: Counted to 3</task-notification>"#).as_deref(),
            Some("Background task count (t1) finished: Counted to 3")
        );
        assert_eq!(notice("Please run the tests"), None);
    }

    #[test]
    fn test_terminal_commands_are_found_by_id() {
        let messages = vec![
            message_with_blocks(1, "assistant", vec![ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "run_terminal_command".to_string(),
                input: serde_json::json!({"command": "ls -la", "terminal_id": 1}),
            }]),
            message_with_blocks(2, "user", vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "command sent (id: XYZ)".to_string(),
                is_error: None,
            }]),
        ];
        assert_eq!(terminal_commands_by_id(&messages).get("XYZ").map(String::as_str), Some("ls -la"));
    }

    #[test]
    fn test_tool_summary_says_what_a_call_did() {
        assert_eq!(tool_summary("create_pod", &serde_json::json!({})), "Started the sandbox");
        assert_eq!(tool_summary("terminate_pod", &serde_json::json!({})), "Stopped the sandbox");
        assert_eq!(tool_summary("create_terminal", &serde_json::json!({})), "Opened a terminal");
        assert_eq!(
            tool_summary("run_terminal_command", &serde_json::json!({"command": "python3 primes.py", "terminal_id": 3})),
            "Ran `python3 primes.py`"
        );
        assert_eq!(tool_summary("read_terminal_output", &serde_json::json!({"command_id": "x"})), "Read a command's output");
        assert_eq!(tool_summary("write_file", &serde_json::json!({"path": "/home/sandbox/a.py", "content": "x"})), "Wrote /home/sandbox/a.py");
        assert_eq!(tool_summary("read_file", &serde_json::json!({"path": "/etc/hosts"})), "Read /etc/hosts");
        assert_eq!(tool_summary("list_directory", &serde_json::json!({"path": "/tmp"})), "Listed /tmp");
        assert_eq!(tool_summary("glob", &serde_json::json!({"pattern": "**/*.rs", "path": "/src"})), "Found files matching `**/*.rs`");
        assert_eq!(tool_summary("grep", &serde_json::json!({"pattern": "fn main"})), "Searched files for `fn main`");
        assert_eq!(tool_summary("webfetch", &serde_json::json!({"url": "https://example.com"})), "Read https://example.com");
        assert_eq!(tool_summary("http_request", &serde_json::json!({"url": "https://api.x/y", "method": "POST"})), "Sent POST https://api.x/y");
        assert_eq!(tool_summary("http_request", &serde_json::json!({"url": "https://api.x/y"})), "Sent GET https://api.x/y");
        assert_eq!(tool_summary("browser_navigate", &serde_json::json!({"url": "https://example.com"})), "Opened https://example.com in the browser");
        assert_eq!(tool_summary("todowrite", &serde_json::json!({"todos": []})), "Updated the todo list");
        assert_eq!(
            tool_summary("clone_repo", &serde_json::json!({"url": "git@github.com:o/r.git"})),
            "Cloned git@github.com:o/r.git"
        );
    }

    #[test]
    fn test_instructions_source_names_repo_commit_and_size() {
        let mut doc = crate::git::ProjectInstructions {
            repo_url: "git@github.com:o/r.git".to_string(),
            path: "/workspace/r".to_string(),
            commit: Some("43835b44f939".to_string()),
            content: "Run make test.\n".to_string(),
            file_bytes: 15,
            truncated: false,
        };
        assert_eq!(instructions_source(&doc), "git@github.com:o/r.git at 43835b4 \u{b7} 15 bytes");
        doc.file_bytes = 50_000;
        doc.truncated = true;
        assert_eq!(
            instructions_source(&doc),
            "git@github.com:o/r.git at 43835b4 \u{b7} 50000 bytes, only the first 32 KiB loaded"
        );
    }

    #[test]
    fn test_instructions_label_says_which_agents_md_files_are_loaded() {
        let mut repo = RepoSummary {
            id: 1,
            url: "u".to_string(),
            path: "/workspace/r".to_string(),
            requested_branch: None,
            branch: None,
            commit: None,
            status: RepoStatus::Ready,
            error: None,
            agents_files: vec![],
            loaded_instructions: vec![],
            trust_requests: vec![],
        };
        assert_eq!(instructions_label(&repo), None, "no AGENTS.md files");
        repo.agents_files = vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()];
        assert_eq!(instructions_label(&repo).as_deref(), Some("AGENTS.md not loaded"));
        repo.loaded_instructions = vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()];
        assert_eq!(instructions_label(&repo).as_deref(), Some("Loaded AGENTS.md, web/AGENTS.md"));
    }

    #[test]
    fn test_repo_detail_says_what_is_checked_out() {
        let mut repo = RepoSummary {
            id: 1,
            url: "git@github.com:o/r.git".to_string(),
            path: "/workspace/r".to_string(),
            requested_branch: Some("dev".to_string()),
            branch: None,
            commit: None,
            status: RepoStatus::Cloning,
            error: None,
            agents_files: vec![],
            loaded_instructions: vec![],
            trust_requests: vec![],
        };
        assert_eq!(repo_detail(&repo), "Cloning dev\u{2026}");
        repo.requested_branch = None;
        assert_eq!(repo_detail(&repo), "Cloning\u{2026}");
        repo.status = RepoStatus::Ready;
        repo.branch = Some("main".to_string());
        repo.commit = Some("43835b44f939c268b73b49292428911526a51508".to_string());
        assert_eq!(repo_detail(&repo), "main \u{b7} 43835b4");
        repo.status = RepoStatus::Failed;
        assert_eq!(repo_detail(&repo), "Clone failed");
        assert_eq!(tool_summary("sandbox_preview_url", &serde_json::json!({"port": 5173})), "Shared a preview of port 5173");
        assert_eq!(
            tool_summary("sandbox_preview_url", &serde_json::json!({"port": 3000, "host": "172.21.0.2"})),
            "Shared a preview of 172.21.0.2:3000"
        );
        assert_eq!(
            tool_summary("mcp__exa__web_search_exa", &serde_json::json!({"query": "rust 1.0 release"})),
            "Searched the web for \"rust 1.0 release\""
        );
        assert_eq!(tool_summary("mcp__github__create_issue", &serde_json::json!({})), "Used create_issue (github)");
        assert_eq!(tool_summary("something_new", &serde_json::json!({})), "Used something_new");
        // A tool that no longer exists still reads sensibly in old history.
        assert_eq!(tool_summary("run_async", &serde_json::json!({"tool": "count"})), "Used run_async");
        assert_eq!(tool_summary("lsp_servers", &serde_json::json!({})), "Listed the language servers");
        assert_eq!(tool_summary("start_language_server", &serde_json::json!({"name": "pyright"})), "Started pyright");
        let lsp = |input| tool_summary("lsp", &input);
        assert_eq!(
            lsp(serde_json::json!({"operation": "incoming_calls", "path": "/workspace/a.rs", "line": 9, "character": 4})),
            "Looked up incoming calls at /workspace/a.rs:9"
        );
        assert_eq!(lsp(serde_json::json!({"operation": "workspace_symbols", "query": "Square"})), "Searched symbols for `Square`");
        assert_eq!(lsp(serde_json::json!({"operation": "document_symbols", "path": "/workspace/a.rs"})), "Listed the symbols in /workspace/a.rs");
        assert_eq!(lsp(serde_json::json!({"operation": "diagnostics", "path": "/workspace/a.rs"})), "Checked /workspace/a.rs for problems");
        assert_eq!(
            lsp(serde_json::json!({"operation": "rename", "path": "/workspace/a.rs", "line": 4, "new_name": "sum_areas"})),
            "Renamed a symbol to sum_areas"
        );
    }

    /// SME-40 F2: the frame now scales to fit the panel, so a click on
    /// the shrunk frame is scaled back up to the page's own pixels.
    #[test]
    fn test_frame_point_scales_a_shrunk_frame_back_to_page_pixels() {
        assert_eq!(frame_point(100.0, 50.0, 1280.0), (100.0, 50.0));
        assert_eq!(frame_point(100.0, 50.0, 640.0), (200.0, 100.0));
        // Before the first resize event arrives, don't scale by nonsense.
        assert_eq!(frame_point(100.0, 50.0, 0.0), (100.0, 50.0));
    }

    #[test]
    fn test_wheel_delta_pixels_passes_pixels_through() {
        assert_eq!(wheel_delta_pixels(WheelDelta::pixels(0.0, 120.0, 0.0)), (0.0, 120.0));
    }

    #[test]
    fn test_wheel_delta_pixels_converts_lines_and_pages() {
        assert_eq!(
            wheel_delta_pixels(WheelDelta::from_web_attributes(1, 0.0, 3.0, 0.0)),
            (0.0, 120.0)
        );
        assert_eq!(
            wheel_delta_pixels(WheelDelta::from_web_attributes(2, 1.0, -1.0, 0.0)),
            (1280.0, -800.0)
        );
    }

    #[test]
    fn test_address_bar_value_follows_the_page_until_editing() {
        assert_eq!(address_bar_value(false, "typed", Some("https://a.example/")), "https://a.example/");
        assert_eq!(address_bar_value(false, "typed", None), "");
    }

    #[test]
    fn test_address_bar_value_keeps_what_is_being_typed() {
        assert_eq!(address_bar_value(true, "exam", Some("https://a.example/")), "exam");
    }

    fn mv(x: f64) -> BrowserInputEvent {
        BrowserInputEvent::MouseMove { x, y: 0.0, left_held: false }
    }

    #[test]
    fn test_coalesce_mouse_moves_keeps_only_the_last_of_a_run() {
        let down = BrowserInputEvent::MouseDown { x: 3.0, y: 0.0 };
        assert_eq!(
            coalesce_mouse_moves(vec![(1, mv(1.0)), (1, mv(2.0)), (1, down.clone()), (1, mv(4.0)), (1, mv(5.0))]),
            vec![(1, mv(2.0)), (1, down), (1, mv(5.0))]
        );
    }

    #[test]
    fn test_coalesce_mouse_moves_keeps_moves_for_different_conversations() {
        assert_eq!(
            coalesce_mouse_moves(vec![(1, mv(1.0)), (2, mv(2.0))]),
            vec![(1, mv(1.0)), (2, mv(2.0))]
        );
    }

    #[test]
    fn test_browser_input_event_for_key_maps_a_printable_character_to_type_text() {
        assert_eq!(
            browser_input_event_for_key(keyboard_types::Key::Character("a".to_string()), no_mods()),
            Some(BrowserInputEvent::TypeText {
                text: "a".to_string()
            })
        );
    }

    #[test]
    fn test_browser_input_event_for_key_maps_each_recognized_named_key_to_press_key() {
        let cases = [
            (keyboard_types::Key::Enter, "Enter"),
            (keyboard_types::Key::Backspace, "Backspace"),
            (keyboard_types::Key::Tab, "Tab"),
            (keyboard_types::Key::Escape, "Escape"),
            (keyboard_types::Key::Delete, "Delete"),
            (keyboard_types::Key::ArrowUp, "ArrowUp"),
            (keyboard_types::Key::ArrowDown, "ArrowDown"),
            (keyboard_types::Key::ArrowLeft, "ArrowLeft"),
            (keyboard_types::Key::ArrowRight, "ArrowRight"),
        ];
        for (key, expected_name) in cases {
            assert_eq!(
                browser_input_event_for_key(key, no_mods()),
                Some(BrowserInputEvent::PressKey {
                    key: expected_name.to_string(),
                    modifiers: 0,
                }),
                "expected {expected_name} to forward as a PressKey"
            );
        }
    }

    #[test]
    fn test_browser_input_event_for_key_drops_an_unrecognized_named_key() {
        // F1 isn't in the forwarded set — same reasoning
        // `browsing::server::named_key_event_fields` uses server-side: no
        // point round-tripping a key the server would reject anyway.
        assert_eq!(browser_input_event_for_key(keyboard_types::Key::F1, no_mods()), None);
    }

    #[test]
    fn test_browser_input_event_for_key_drops_a_bare_modifier() {
        assert_eq!(browser_input_event_for_key(keyboard_types::Key::Shift, no_mods()), None);
        assert_eq!(browser_input_event_for_key(keyboard_types::Key::Control, no_mods()), None);
        assert_eq!(browser_input_event_for_key(keyboard_types::Key::Alt, no_mods()), None);
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
        SandboxOutputLine {
            stream: stream.to_string(),
            data: data.to_string(),
            seq: 0,
        }
    }

    fn test_output_line_entry(stream: &str, data: &str) -> SandboxOutputLinePanelEntry {
        SandboxOutputLinePanelEntry {
            stream: stream.to_string(),
            data: data.to_string(),
            seq: None,
        }
    }

    fn preview(port: u16) -> SandboxPreview {
        SandboxPreview {
            port,
            host: None,
            url: format!("http://{port}-1.preview.localhost:8181"),
        }
    }

    #[test]
    fn test_merge_sandbox_snapshot_carries_each_pods_previews() {
        let mut pods = vec![SandboxPodPanelEntry {
            pod_id: 1,
            status: "Running".to_string(),
            previews: vec![preview(8080)],
        }];
        let mut terminals = Vec::new();
        let snapshot = SandboxSnapshot {
            pods: vec![
                SandboxPodSummary {
                    pod_id: 1,
                    status: "Running".to_string(),
                    terminals: Vec::new(),
                    previews: vec![preview(3000)],
                },
                SandboxPodSummary {
                    pod_id: 2,
                    status: "Running".to_string(),
                    terminals: Vec::new(),
                    previews: vec![preview(5173)],
                },
            ],
        };
        merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);
        assert_eq!(pods[0].previews, vec![preview(3000)], "the snapshot's list replaces the old one");
        assert_eq!(pods[1].previews, vec![preview(5173)]);
    }

    #[test]
    fn test_apply_sandbox_preview_update_replaces_only_its_pods_list() {
        let mut pods = vec![
            SandboxPodPanelEntry { pod_id: 1, status: "Running".to_string(), previews: Vec::new() },
            SandboxPodPanelEntry { pod_id: 2, status: "Running".to_string(), previews: vec![preview(9000)] },
        ];
        apply_sandbox_preview_update(&mut pods, 1, vec![preview(3000), preview(5173)]);
        assert_eq!(pods[0].previews, vec![preview(3000), preview(5173)]);
        assert_eq!(pods[1].previews, vec![preview(9000)], "another pod's previews are untouched");

        apply_sandbox_preview_update(&mut pods, 99, vec![preview(1)]);
        assert_eq!(pods.len(), 2, "an unknown pod isn't invented");
    }

    #[test]
    fn test_a_pod_status_update_keeps_its_previews() {
        let mut pods = vec![SandboxPodPanelEntry {
            pod_id: 1,
            status: "Pending".to_string(),
            previews: vec![preview(3000)],
        }];
        let mut terminals = Vec::new();
        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
        assert_eq!(pods[0].previews, vec![preview(3000)]);
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
                previews: Vec::new(),
            }],
        };

        merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].pod_id, 1);
        assert_eq!(pods[0].status, "Running");
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].terminal_id, 2);
        assert_eq!(
            terminals[0]
                .commands
                .iter()
                .map(|c| c.command.as_str())
                .collect::<Vec<_>>(),
            vec!["cd /tmp", "echo hi"],
            "history should preserve the snapshot's own (oldest-first) order"
        );
        assert_eq!(
            terminals[0].commands[1]
                .output
                .iter()
                .map(|l| (l.stream.as_str(), l.data.as_str()))
                .collect::<Vec<_>>(),
            vec![("stdout", "hi"), ("stderr", "uh oh"), ("stdout", "bye")],
            "output order must match the snapshot's, not get split by stream"
        );
    }

    #[test]
    fn test_merge_sandbox_snapshot_is_authoritative_over_existing_entries() {
        let mut pods = vec![SandboxPodPanelEntry {
            pod_id: 1,
            status: "Pending".to_string(),
            previews: Vec::new(),
        }];
        let mut terminals = Vec::new();
        let snapshot = SandboxSnapshot {
            pods: vec![SandboxPodSummary {
                pod_id: 1,
                status: "Running".to_string(),
                terminals: Vec::new(),
                previews: Vec::new(),
            }],
        };

        merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

        assert_eq!(
            pods.len(),
            1,
            "an existing pod should be updated, not duplicated"
        );
        assert_eq!(pods[0].status, "Running");
    }

    /// A pod or terminal that went away while the tab was disconnected
    /// never gets its live `terminated` event; the reconnect's snapshot
    /// leaving it out is the only sign (SME-43).
    #[test]
    fn test_merge_sandbox_snapshot_drops_pods_and_terminals_it_no_longer_lists() {
        let pod = |pod_id| SandboxPodPanelEntry { pod_id, status: "Running".to_string(), previews: Vec::new() };
        let terminal = |terminal_id, pod_id| SandboxTerminalPanelEntry {
            terminal_id,
            pod_id,
            status: "connected".to_string(),
            commands: Vec::new(),
        };
        let mut pods = vec![pod(1), pod(2)];
        let mut terminals = vec![terminal(10, 1), terminal(11, 1), terminal(20, 2)];
        let snapshot = SandboxSnapshot {
            pods: vec![SandboxPodSummary {
                pod_id: 1,
                status: "Running".to_string(),
                terminals: vec![SandboxTerminalSummary {
                    terminal_id: 10,
                    pod_id: 1,
                    status: "connected".to_string(),
                    commands: Vec::new(),
                }],
                previews: Vec::new(),
            }],
        };

        merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

        assert_eq!(pods.iter().map(|p| p.pod_id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(terminals.iter().map(|t| t.terminal_id).collect::<Vec<_>>(), vec![10]);
    }

    #[test]
    fn test_apply_sandbox_pod_update_upserts_when_not_terminated() {
        let mut pods = Vec::new();
        let mut terminals = Vec::new();
        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].status, "Running");

        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
        assert_eq!(
            pods.len(),
            1,
            "a repeat update for the same pod_id should update, not duplicate"
        );
    }

    #[test]
    fn test_apply_sandbox_pod_update_removes_pod_and_its_terminals_when_terminated() {
        let mut pods = vec![SandboxPodPanelEntry {
            pod_id: 1,
            status: "Running".to_string(),
            previews: Vec::new(),
        }];
        let mut terminals = vec![
            test_sandbox_terminal_entry(10, 1),
            test_sandbox_terminal_entry(20, 2),
        ];

        apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "terminated".to_string(), true);

        assert!(
            pods.is_empty(),
            "the terminated pod should be removed, not just relabeled"
        );
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
        let mut terminals = vec![
            test_sandbox_terminal_entry(10, 1),
            test_sandbox_terminal_entry(20, 1),
        ];

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
        terminals[0]
            .commands
            .push(test_sandbox_command_entry("cmd-old", "sleep 30"));
        terminals[0].commands[0].output = vec![test_output_line_entry(
            "stdout",
            "output from a previous command",
        )];

        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-new".to_string(),
            Some("echo hi".to_string()),
            "running".to_string(),
            None,
            None,
            None, None,
        );

        assert_eq!(
            terminals[0]
                .commands
                .iter()
                .map(|c| c.command_id.as_str())
                .collect::<Vec<_>>(),
            vec!["cmd-old", "cmd-new"],
            "a new command should be appended to the terminal's history, not replace it"
        );
        assert_eq!(
            terminals[0].commands[0].output,
            vec![test_output_line_entry(
                "stdout",
                "output from a previous command"
            )],
            "an earlier command's own output should be untouched by a later command starting"
        );
        assert!(terminals[0].commands[1].output.is_empty());
    }

    #[test]
    fn test_apply_sandbox_command_update_without_command_appends_a_line_to_the_most_recent_command()
    {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0]
            .commands
            .push(test_sandbox_command_entry("cmd-1", "echo hi"));

        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-1".to_string(),
            None,
            "running".to_string(),
            None,
            Some("stdout".to_string()),
            Some("hi".to_string()), None,
        );

        assert_eq!(
            terminals[0].commands[0].output,
            vec![test_output_line_entry("stdout", "hi")]
        );
        assert_eq!(
            terminals[0].commands[0].command, "echo hi",
            "an output-line update shouldn't touch the already-known command text"
        );
    }

    #[test]
    fn test_apply_sandbox_command_update_preserves_arrival_order_across_streams() {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0]
            .commands
            .push(test_sandbox_command_entry("cmd-1", "sh -c '...'"));

        for (stream, data) in [("stdout", "one"), ("stderr", "uh oh"), ("stdout", "two")] {
            apply_sandbox_command_update(
                &mut terminals,
                10,
                "cmd-1".to_string(),
                None,
                "running".to_string(),
                None,
                Some(stream.to_string()),
                Some(data.to_string()), None,
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
    fn test_apply_sandbox_command_update_finish_sets_status_and_exit_code_on_the_most_recent_command()
     {
        let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
        terminals[0]
            .commands
            .push(test_sandbox_command_entry("cmd-1", "echo hi"));

        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-1".to_string(),
            None,
            "finished".to_string(),
            Some(0),
            None,
            None, None,
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
            Some("hi".to_string()), None,
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
            None, None,
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
    let router = router();
    let selected: Memo<Option<i64>> = use_memo(move || match router.current::<Route>() {
        Route::Home {} => None,
        Route::ConversationRoute { id } => Some(id),
        // `Chat` is never actually rendered on these routes (see their own
        // components in `frontend/mod.rs`) — these arms only exist to
        // satisfy exhaustiveness.
        Route::McpServersRoute {} => None,
        Route::McpServerNewRoute {} => None,
        Route::McpServerEditRoute { .. } => None,
        Route::SandboxVolumesRoute {} => None,
        Route::PodsRoute {} => None,
        Route::GitRoute {} => None,
        Route::LanguageServersRoute {} => None,
        Route::LanguageServerNewRoute {} => None,
        Route::LanguageServerEditRoute { .. } => None,
        Route::SandboxVolumeNewRoute {} => None,
        Route::ProvidersRoute {} => None,
        Route::ProviderNewRoute {} => None,
        Route::ProviderEditRoute { .. } => None,
        Route::NotFound { .. } => None,
    });

    // Bumped by the chat panel whenever its conversation gets new
    // messages; the sidebar refetches its list when it changes, so a
    // conversation's title (set by its first message) and its place in the
    // list stay current without a reload.
    let conversations_changed = use_signal(|| 0u64);
    // Bumped by the chat panel whenever a pod is created or goes away in
    // any conversation (`ConversationEvent::PodsChanged`, relayed on the
    // open conversation's own stream rather than a second connection; see
    // that variant). With no conversation open there's no stream, so the
    // sidebar's pod dots only refresh on navigation.
    let pods_changed = use_signal(|| 0u64);
    // Bumped the same way when a turn starts or ends in any conversation
    // (`ConversationEvent::TurnsChanged`), for the sidebar's busy marks.
    let turns_changed = use_signal(|| 0u64);
    // Bumped when a conversation starts or stops waiting on an answer to
    // the model's question (`ConversationEvent::QuestionsChanged`, SME-34).
    let questions_changed = use_signal(|| 0u64);

    rsx! {
        div { class: "chat-layout",
            ConversationSidebar { selected, conversations_changed, pods_changed, turns_changed, questions_changed }
            ChatPanel { selected, conversations_changed, pods_changed, turns_changed, questions_changed }
        }
    }
}

#[component]
fn ConversationSidebar(
    selected: Memo<Option<i64>>,
    conversations_changed: Signal<u64>,
    pods_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
) -> Element {
    let navigator = use_navigator();
    // Which conversations have a live pod, for the dot next to their title.
    let live_pods = use_resource(move || {
        let _ = pods_changed();
        crate::api::pods::get_live_pod_conversations()
    });
    let has_live_pod = move |id: i64| {
        matches!(&*live_pods.read(), Some(Ok(ids)) if ids.contains(&id))
    };
    // Which conversations have a turn running, marked as working
    // (SME-41 D9).
    let busy = use_resource(move || {
        let _ = turns_changed();
        crate::api::chat::get_busy_conversations()
    });
    let is_busy = move |id: i64| matches!(&*busy.read(), Some(Ok(ids)) if ids.contains(&id));
    // Which conversations wait on the user's answer to the model's question
    // (SME-34).
    let waiting = use_resource(move || {
        let _ = questions_changed();
        get_waiting_conversations()
    });
    let is_waiting = move |id: i64| matches!(&*waiting.read(), Some(Ok(ids)) if ids.contains(&id));
    let initial_conversations = use_resource(move || {
        let _ = conversations_changed();
        get_conversations()
    });
    let mut conversations: Signal<Vec<Conversation>> = use_signal(Vec::new);
    let mut loaded = use_signal(|| false);
    // Why the list couldn't load, and why the last New conversation or
    // Delete failed. Each is cleared by its own next success: the list
    // reloads on every new message, which says nothing about a failed
    // Delete (SME-81).
    let mut list_error: Signal<Option<String>> = use_signal(|| None);
    let mut action_error: Signal<Option<String>> = use_signal(|| None);
    let mut pending_delete: Signal<Option<i64>> = use_signal(|| None);
    // Whether the conversation list is open on a phone, where it folds
    // behind a button (SME-41 D4). Ignored at wider widths.
    let mut open_on_phone = use_signal(|| false);
    // The browser's clock, refreshed every minute, for each row's age
    // (SME-41 D8). None until the first reading, when ages aren't shown.
    #[allow(unused_mut)]
    let mut now_utc: Signal<Option<chrono::NaiveDateTime>> = use_signal(|| None);
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(async move {
            loop {
                if let Ok(value) = document::eval("return Date.now();").await
                    && let Some(ms) = value.as_f64()
                {
                    now_utc.set(chrono::DateTime::from_timestamp_millis(ms as i64).map(|t| t.naive_utc()));
                }
                gloo_timers::future::TimeoutFuture::new(60_000).await;
            }
        });
    });

    use_effect(move || {
        if let Some(result) = initial_conversations() {
            match result {
                Ok(list) => {
                    conversations.set(list);
                    list_error.set(None);
                }
                Err(e) => list_error.set(Some(server_error_message(&e))),
            }
            loaded.set(true);
        }
    });

    let new_conversation = move |_: Event<MouseData>| {
        spawn(async move {
            match create_conversation().await {
                Ok(conversation) => {
                    action_error.set(None);
                    let id = conversation.id;
                    conversations.write().insert(0, conversation);
                    navigator.push(Route::ConversationRoute { id });
                }
                Err(e) => action_error.set(Some(server_error_message(&e))),
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
                        action_error.set(None);
                        conversations.write().retain(|c| c.id != id);
                        if selected() == Some(id) {
                            navigator.push(Route::Home {});
                        }
                    }
                    Err(e) => action_error.set(Some(server_error_message(&e))),
                }
            });
        } else {
            pending_delete.set(Some(id));
        }
    };

    rsx! {
        aside { class: if open_on_phone() { "sidebar sidebar-open" } else { "sidebar" },
            // Phone only (see chat.css): the list folds behind this, so the
            // conversation, not the list, fills the screen (SME-41 D4).
            button {
                class: "sidebar-toggle",
                r#type: "button",
                aria_expanded: "{open_on_phone()}",
                onclick: move |_| open_on_phone.toggle(),
                if open_on_phone() { "Close" } else { "Conversations" }
            }
            div { class: "sidebar-body",
            button { class: "new-conversation", onclick: move |e| { open_on_phone.set(false); new_conversation(e) }, "New conversation" }
            Link { to: Route::ProvidersRoute {}, class: "sandbox-volumes-link providers-link", "Model providers" }
            Link { to: Route::McpServersRoute {}, class: "mcp-servers-link", "MCP servers" }
            Link { to: Route::PodsRoute {}, class: "pods-link", "Sandboxes" }
            Link { to: Route::SandboxVolumesRoute {}, class: "sandbox-volumes-link", "Sandbox volumes" }
            Link { to: Route::GitRoute {}, class: "sandbox-volumes-link git-link", "Git" }
            Link { to: Route::LanguageServersRoute {}, class: "sandbox-volumes-link language-servers-link", "Language servers" }
            if let Some(err) = list_error() {
                p { class: "error", "{err}" }
            }
            if let Some(err) = action_error() {
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
                            "data-conversation-id": "{conversation.id}",
                            class: if selected() == Some(conversation.id) { "conversation-item active" } else { "conversation-item" },
                            onclick: move |_| {
                                pending_delete.set(None);
                                open_on_phone.set(false);
                                navigator.push(Route::ConversationRoute { id: conversation.id });
                            },
                            span { class: "conversation-title", "{conversation.title}" }
                            if is_busy(conversation.id) {
                                span { class: "conversation-busy", title: "Working" }
                            }
                            if is_waiting(conversation.id) {
                                span { class: "conversation-waiting", title: "Waiting for your answer", "?" }
                            }
                            if has_live_pod(conversation.id) {
                                span { class: "live-pod-dot", title: "sandbox pod running" }
                            }
                            if let Some(now) = now_utc() {
                                span { class: "conversation-age", {short_age((now - conversation.updated_at).num_seconds())} }
                            }
                            button {
                                class: if pending_delete() == Some(conversation.id) { "delete-conversation confirm" } else { "delete-conversation" },
                                onclick: move |evt: Event<MouseData>| {
                                    evt.stop_propagation();
                                    request_delete(conversation.id);
                                },
                                super::TwoStepLabel { armed: pending_delete() == Some(conversation.id), idle: "Delete", confirm: "Confirm?" }
                            }
                        }
                    }
                }
            }
            }
        }
    }
}

/// The card for the question conversation `conversation_id` waits on
/// (SME-34): each question with its options (buttons, toggles when
/// multi-select) and an "Other" field, then Submit. A single question with
/// single-choice options answers on the click. The card goes when every tab
/// hears the question is answered (`QuestionUpdate { question: None }`);
/// another tab's answer first makes this one's fail with "already answered".
#[component]
fn QuestionCard(conversation_id: i64, question: PendingQuestion) -> Element {
    let count = question.questions.len();
    let mut chosen: Signal<Vec<Vec<String>>> = use_signal(|| vec![Vec::new(); count]);
    let mut others: Signal<Vec<String>> = use_signal(|| vec![String::new(); count]);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut sending = use_signal(|| false);
    let answers = move || -> Vec<QuestionAnswer> {
        chosen()
            .into_iter()
            .zip(others())
            .map(|(selected, other)| QuestionAnswer {
                selected,
                other: Some(other.trim().to_string()).filter(|t| !t.is_empty()),
            })
            .collect()
    };
    let complete = move || answers().iter().all(|a| !a.selected.is_empty() || a.other.is_some());
    let tool_use_id = question.tool_use_id.clone();
    let submit = use_callback(move |answers: Vec<QuestionAnswer>| {
        let tool_use_id = tool_use_id.clone();
        sending.set(true);
        error.set(None);
        spawn(async move {
            if let Err(e) = answer_question(conversation_id, tool_use_id, answers).await {
                error.set(Some(server_error_message(&e)));
                sending.set(false);
            }
        });
    });
    let one_click = count == 1 && !question.questions[0].multi_select && !question.questions[0].options.is_empty();
    rsx! {
        div { class: "question-card", role: "group", aria_label: "The model asks",
            for (qi , q) in question.questions.iter().cloned().enumerate() {
                fieldset { key: "{qi}", class: "question-card-question",
                    legend { class: "question-card-header", "{q.header}" }
                    p { class: "question-card-text", "{q.question}" }
                    if !q.options.is_empty() {
                        div { class: "question-card-options",
                            for (oi , option) in q.options.iter().cloned().enumerate() {
                                button {
                                    key: "{oi}",
                                    r#type: "button",
                                    class: if chosen()[qi].contains(&option.label) { "question-option chosen" } else { "question-option" },
                                    aria_pressed: "{chosen()[qi].contains(&option.label)}",
                                    disabled: sending(),
                                    onclick: {
                                        let label = option.label.clone();
                                        let multi = q.multi_select;
                                        move |_| {
                                            {
                                                let mut all = chosen.write();
                                                let picked = &mut all[qi];
                                                if !multi {
                                                    *picked = vec![label.clone()];
                                                } else if let Some(at) = picked.iter().position(|l| *l == label) {
                                                    picked.remove(at);
                                                } else {
                                                    picked.push(label.clone());
                                                }
                                            }
                                            if one_click {
                                                submit(answers());
                                            }
                                        }
                                    },
                                    span { class: "question-option-label", "{option.label}" }
                                    if let Some(description) = option.description.clone() {
                                        span { class: "question-option-description", "{description}" }
                                    }
                                }
                            }
                        }
                    }
                    input {
                        r#type: "text",
                        class: "question-card-other",
                        aria_label: "Your own answer to {q.header}",
                        placeholder: if q.options.is_empty() { "Your answer" } else { "Or write your own answer" },
                        value: "{others()[qi]}",
                        disabled: sending(),
                        oninput: move |e| others.write()[qi] = e.value(),
                    }
                }
            }
            if let Some(message) = error() {
                p { class: "error", role: "alert", "{message}" }
            }
            button {
                r#type: "button",
                class: "question-card-submit",
                disabled: sending() || !complete(),
                onclick: move |_| submit(answers()),
                if sending() { "Sending…" } else { "Submit" }
            }
        }
    }
}

#[component]
fn ChatPanel(
    selected: Memo<Option<i64>>,
    conversations_changed: Signal<u64>,
    pods_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
) -> Element {
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
    // The server's own "not found", distinct from a failed load: the page
    // says so and offers no message box, instead of a send that can only
    // fail.
    let conversation_missing = move || load_error().as_deref() == Some("conversation not found");
    // The open conversation's reply as it streams, from its event stream:
    // `ReplyReset` starts it, `ReplyDelta` adds to it, and the reply being
    // saved (`MessagesAppended`) or the turn ending clears it. Reset on a
    // switch, and restored on (re)connect from `get_reply_in_progress`.
    // Every tab watching the conversation shows it, whoever started the turn.
    #[allow(unused_mut)]
    let mut streaming_reply: Signal<Option<String>> = use_signal(|| None);
    let streaming_text = move || streaming_reply().filter(|text| !text.is_empty());
    // The last turn error, by conversation (`TurnError`, or a send the
    // server refused).
    let mut stream_errors: Signal<HashMap<i64, String>> = use_signal(HashMap::new);
    let stream_error = move || selected().and_then(|id| stream_errors.read().get(&id).cloned());
    // Whether the server has a turn running (or queued) in the selected
    // conversation, from `ConversationEvent::TurnState`: covers turns this
    // tab didn't start (another tab, a finished command waking the model).
    #[allow(unused_mut)]
    let mut turn_running = use_signal(|| false);
    // Bumped when the conversation's model or the providers change, so the
    // model picker refetches (SME-72).
    #[allow(unused_mut)]
    let mut model_changed = use_signal(|| 0u64);
    // Whether the conversation has a model to send to, from the picker:
    // Send waits for one (SME-72). True until the picker knows otherwise.
    let model_ready = use_signal(|| true);
    // The message box waits while the model works in this conversation,
    // whoever started the turn; Stop is offered instead.
    let is_streaming = move || turn_running();
    let can_stop = move || turn_running();
    // Seconds this tab has seen the current turn running, for the
    // "Working…" line (SME-41 D1). Ticks once a second while a turn runs,
    // and resets when it ends. Web only.
    #[allow(unused_mut)]
    let mut turn_elapsed = use_signal(|| 0u64);
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(1000).await;
                if *turn_running.peek() {
                    *turn_elapsed.write() += 1;
                } else if *turn_elapsed.peek() != 0 {
                    turn_elapsed.set(0);
                }
            }
        });
    });
    let stop = move |_| {
        let Some(id) = selected() else { return };
        // "Stopped." comes from the notice the stop saves, in every tab and
        // after a reload (SME-51 B10); a stop that fails says so (B11).
        spawn(async move {
            if let Err(e) = crate::api::chat::stop_turn(id).await
                && selected() == Some(id)
            {
                stream_errors
                    .write()
                    .insert(id, format!("Couldn't stop the turn: {}", server_error_message(&e)));
            }
        });
    };
    // Set when a background wake-up (a terminal command finishing with no
    // `send_message` call in flight) fails to actually reach the model —
    // see `ConversationEvent::NotificationDeliveryFailed`. Separate from
    // `stream_errors` since that one's reset at the start of every `send()`
    // call; this can arrive at any time, not tied to a live send.
    //
    // `mut` is only exercised by the `web`-only live-subscription loop and
    // timezone effect below (`.set()`/`.write()`); a `server`-only build
    // never mutates these, so `allow(unused_mut)` there.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut notification_delivery_error: Signal<Option<String>> = use_signal(|| None);
    let mut input = use_signal(String::new);
    let mut next_temp_id = use_signal(|| -1i64);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut todos: Signal<Vec<TodoItem>> = use_signal(Vec::new);
    // The question this conversation waits on, for its card (SME-34).
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut pending_question: Signal<Option<PendingQuestion>> = use_signal(|| None);
    // "Work on a repo" in a new conversation.
    let mut repo_url = use_signal(String::new);
    let mut repo_branch = use_signal(String::new);
    let mut repo_dir = use_signal(String::new);
    let mut repo_attaching = use_signal(|| false);
    let mut repo_attach_error: Signal<Option<String>> = use_signal(|| None);
    // The last Trust / Don't trust / Reload failure.
    let mut repo_action_error: Signal<Option<String>> = use_signal(|| None);
    // The conversation's git repos (SME-32), from `ReposUpdate`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repos: Signal<Vec<RepoSummary>> = use_signal(Vec::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut sandbox_pods: Signal<Vec<SandboxPodPanelEntry>> = use_signal(Vec::new);
    // The panel's Stop button: the pod armed for stopping (click once to
    // arm, again to confirm), and the last stop's error. The pod itself
    // disappears through the usual `SandboxPodUpdate` event.
    let mut pending_pod_stop: Signal<Option<i64>> = use_signal(|| None);
    let mut pod_stop_error: Signal<Option<String>> = use_signal(|| None);
    let mut request_pod_stop = move |pod_id: i64| {
        if pending_pod_stop() == Some(pod_id) {
            pending_pod_stop.set(None);
            let conversation = selected();
            spawn(async move {
                let result = crate::api::pods::stop_pod(pod_id).await;
                // Not onto another conversation's panel (SME-51 B11).
                if selected() != conversation {
                    return;
                }
                match result {
                    Ok(()) => pod_stop_error.set(None),
                    Err(e) => pod_stop_error.set(Some(server_error_message(&e))),
                }
            });
        } else {
            pending_pod_stop.set(Some(pod_id));
        }
    };
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut sandbox_terminals: Signal<Vec<SandboxTerminalPanelEntry>> = use_signal(Vec::new);
    // Whether the model currently has a browsing session open — drives
    // both the panel's visibility and (via the separate effect below)
    // the live frame subscription. Updated by the initial
    // `get_browsing_state` pull and by `ConversationEvent::BrowsingSessionUpdate`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_session_open: Signal<bool> = use_signal(|| false);
    // The session page's current URL, from `get_browsing_state` and
    // `ConversationEvent::BrowsingUrlUpdate`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_url: Signal<Option<String>> = use_signal(|| None);
    // Set while this tab is live on the conversation: subscribed to its
    // events and done with the snapshot pull that follows. The id, and how
    // many times this page has connected and pulled (more than one means a
    // reconnect). Shown as `data-live`/`data-live-pulls` on the panel, which
    // the browser tests wait for (SME-59).
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut live: Signal<Option<(i64, u32)>> = use_signal(|| None);
    // The address bar's own state: what's typed, whether the viewer is
    // typing (so incoming URL changes don't clobber it), an in-flight
    // navigation, and the last navigation error.
    let mut address_draft = use_signal(String::new);
    let mut address_editing = use_signal(|| false);
    // The live frame's width as shown; it scales to fit the panel, and
    // clicks on it are scaled back up to the page's own pixels.
    let mut frame_shown_width = use_signal(|| FRAME_WIDTH);
    let mut address_pending = use_signal(|| false);
    let mut address_error: Signal<Option<String>> = use_signal(|| None);
    // Forwards the live panel's input one request at a time, in the order
    // it happened — separately spawned requests can overtake each other (a
    // mouse-up reaching the page before its mouse-down). Moves that queued
    // up while a request was in flight collapse to the latest one.
    let browser_input = use_coroutine(
        move |mut queue: UnboundedReceiver<(i64, BrowserInputEvent)>| async move {
            while let Ok(first) = queue.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = queue.try_recv() {
                    batch.push(next);
                }
                for (id, event) in coalesce_mouse_moves(batch) {
                    let _ = send_browser_input(id, event).await;
                }
            }
        },
    );
    // The latest live-panel frame (base64 JPEG), `None` until the first
    // one arrives after subscribing.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_frame: Signal<Option<String>> = use_signal(|| None);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut tz_offset_minutes: Signal<i32> = use_signal(|| 0);
    // `None` until the first `ContextUsageUpdate`/`get_context_usage` pull
    // — a brand-new conversation has no turn yet to report usage for. See
    // SME-18.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut context_usage: Signal<Option<ContextUsageSnapshot>> = use_signal(|| None);
    // The click-through detail view: closed by default, fetched on demand
    // (not kept live) the moment it's opened — see `open_context_detail`.
    let mut context_detail: Signal<Option<ContextDetailSnapshot>> = use_signal(|| None);
    let mut context_detail_open = use_signal(|| false);
    // The context bar, so closing its detail view can give it focus back.
    let mut context_bar_el: Signal<Option<MountedEvent>> = use_signal(|| None);

    // Sticky-bottom auto-scroll state for the message transcript: the
    // mounted `.messages` element (so an effect can query/set its scroll
    // position) and whether it was at the bottom the last time the user
    // scrolled it — read, not written, by the auto-scroll effect below;
    // written only by the `onscroll` handler on the element itself, so it
    // always reflects a real user (or auto-scroll-induced) scroll position
    // rather than the reactive-render cycle.
    let mut messages_el: Signal<Option<MountedEvent>> = use_signal(|| None);
    let mut messages_stuck_to_bottom = use_signal(|| true);
    // Whether the pointer is over the transcript, and whether a snap to
    // the bottom that a layout change asked for is waiting for it to
    // leave (SME-75): a side panel appearing mustn't move what the user is
    // about to click. While it waits, the transcript is scrolled to keep
    // the element under the pointer where it was.
    let mut pointer_over_transcript = use_signal(|| false);
    let mut layout_snap_pending = use_signal(|| false);
    // What the transcript held when the effect below last ran: the message
    // count, the last message's id and the streaming reply's length. Only
    // a change in it is new content; a write that changes nothing (the
    // connect-time pull sets the messages again) is not.
    let mut last_content: Signal<Option<(usize, Option<i64>, Option<usize>)>> = use_signal(|| None);

    // Same idea, per sandbox terminal — each terminal's own
    // `.task-terminal-body` scrolls independently, like `tail -f` on its own
    // log, so each needs its own mounted handle and stuck flag.
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
            if let Some(id) = *selected.peek() {
                let merged = apply_loaded_messages(&messages.peek(), list, id);
                messages.set(merged);
            }
            load_error.set(None);
            // A freshly loaded conversation should open scrolled to its
            // latest message, regardless of where a previous conversation
            // was left scrolled.
            messages_stuck_to_bottom.set(true);
        }
        Some(Some(Err(e))) => load_error.set(Some(server_error_message(&e))),
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
            live.set(None);
            let Some(id) = selected() else { return };
            todos.set(Vec::new());
            repos.set(Vec::new());
            // "Work on a repo" and the trust cards belong to the conversation
            // they were used in (SME-32 code review 8).
            repo_url.set(String::new());
            repo_branch.set(String::new());
            repo_dir.set(String::new());
            repo_attaching.set(false);
            repo_attach_error.set(None);
            repo_action_error.set(None);
            layout_snap_pending.set(false);
            sandbox_pods.set(Vec::new());
            sandbox_terminals.set(Vec::new());
            terminal_body_els.write().clear();
            terminal_body_stuck.write().clear();
            browsing_session_open.set(false);
            browsing_frame.set(None);
            browsing_url.set(None);
            address_editing.set(false);
            address_error.set(None);
            notification_delivery_error.set(None);
            turn_running.set(false);
            streaming_reply.set(None);
            context_usage.set(None);
            // The rest of what belonged to the conversation left (SME-51 B11).
            messages.set(Vec::new());
            load_error.set(None);
            turn_elapsed.set(0);
            pending_pod_stop.set(None);
            pod_stop_error.set(None);
            context_detail.set(None);
            context_detail_open.set(false);
            pending_question.set(None);
            address_pending.set(false);
            address_draft.set(String::new());

            let handle = spawn(async move {
                let mut pulls = 0u32;
                loop {
                    let subscribed = subscribe_conversation_events(id).await;
                    // Deleted (from another tab, say): the server refuses
                    // to stream it, so say so instead of retrying for good
                    // (SME-91). A refused stream doesn't carry the
                    // server's reason, so ask the way a page load does.
                    if subscribed.is_err()
                        && let Err(e) = get_messages(id).await
                        && server_error_message(&e) == "conversation not found"
                    {
                        load_error.set(Some(server_error_message(&e)));
                        break;
                    }
                    if let Ok(mut events) = subscribed {
                        // One-shot reconciliation pull: a `broadcast`
                        // channel has no replay, so anything published
                        // before this subscription connected would
                        // otherwise be missed. This runs once per
                        // connection (initial load or reconnect), not on a
                        // timer — not the polling loop this replaces.
                        if let Ok(list) = get_messages(id).await {
                            accept_saved_messages(&mut messages.write(), list);
                        }
                        if let Ok(snapshot) = get_sandbox_state(id).await {
                            merge_sandbox_snapshot(
                                &mut sandbox_pods.write(),
                                &mut sandbox_terminals.write(),
                                snapshot,
                            );
                        }
                        if let Ok(snapshot) = get_context_usage(id).await {
                            context_usage.set(Some(snapshot));
                        }
                        if let Ok(snapshot) = get_todos(id).await {
                            todos.set(snapshot);
                        }
                        if let Ok(snapshot) = get_pending_question(id).await {
                            pending_question.set(snapshot);
                        }
                        if let Ok(snapshot) = list_conversation_repos(id).await {
                            repos.set(snapshot);
                        }
                        if let Ok(running) = get_turn_state(id).await {
                            turn_running.set(running);
                        }
                        if let Ok(reply) = get_reply_in_progress(id).await {
                            streaming_reply.set(reply);
                        }
                        // A turn that failed before this tab connected
                        // (SME-51 B11).
                        if let Ok(Some(error)) = get_turn_error(id).await {
                            stream_errors.write().insert(id, error);
                        }
                        crate::frontend::check_build_id().await;
                        if let Ok(state) = get_browsing_state(id).await {
                            browsing_session_open.set(state.session_open);
                            browsing_url.set(state.url);
                        }
                        pulls += 1;
                        live.set(Some((id, pulls)));

                        loop {
                            match events.recv().await {
                                Some(Ok(ConversationEvent::MessagesAppended { messages: rows })) => {
                                    *conversations_changed.write() += 1;
                                    // A saved reply replaces its streaming copy.
                                    if rows.iter().any(|m| m.role == "assistant") {
                                        streaming_reply.set(None);
                                    }
                                    accept_saved_messages(&mut messages.write(), rows);
                                }
                                Some(Ok(ConversationEvent::SandboxPodUpdate {
                                    pod_id,
                                    status,
                                    terminated,
                                })) => {
                                    apply_sandbox_pod_update(
                                        &mut sandbox_pods.write(),
                                        &mut sandbox_terminals.write(),
                                        pod_id,
                                        status,
                                        terminated,
                                    );
                                }
                                Some(Ok(ConversationEvent::SandboxPreviewUpdate { pod_id, previews })) => {
                                    apply_sandbox_preview_update(&mut sandbox_pods.write(), pod_id, previews);
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
                                    position,
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
                                        position,
                                    );
                                }
                                Some(Ok(ConversationEvent::NotificationDeliveryFailed {
                                    detail,
                                })) => {
                                    notification_delivery_error.set(Some(detail));
                                }
                                Some(Ok(ConversationEvent::ContextUsageUpdate {
                                    usage,
                                    context_window,
                                })) => {
                                    context_usage.set(Some(ContextUsageSnapshot {
                                        usage: Some(usage),
                                        context_window,
                                    }));
                                }
                                Some(Ok(ConversationEvent::TodoListUpdate { items })) => {
                                    todos.set(items);
                                }
                                Some(Ok(ConversationEvent::BrowsingSessionUpdate { open })) => {
                                    browsing_session_open.set(open);
                                    if !open {
                                        browsing_frame.set(None);
                                        browsing_url.set(None);
                                    }
                                }
                                Some(Ok(ConversationEvent::BrowsingUrlUpdate { url })) => {
                                    browsing_url.set(Some(url));
                                }
                                Some(Ok(ConversationEvent::PodsChanged {})) => {
                                    *pods_changed.write() += 1;
                                }
                                Some(Ok(ConversationEvent::TurnsChanged {})) => {
                                    *turns_changed.write() += 1;
                                }
                                Some(Ok(ConversationEvent::QuestionUpdate { question })) => {
                                    pending_question.set(question);
                                }
                                Some(Ok(ConversationEvent::QuestionsChanged {})) => {
                                    *questions_changed.write() += 1;
                                }
                                Some(Ok(ConversationEvent::ModelChanged {} | ConversationEvent::ProvidersChanged {})) => {
                                    *model_changed.write() += 1;
                                    // Another model can mean another context
                                    // window: the meter measures against it.
                                    spawn(async move {
                                        if let Ok(snapshot) = get_context_usage(id).await
                                            && selected() == Some(id)
                                        {
                                            context_usage.set(Some(snapshot));
                                        }
                                    });
                                }
                                Some(Ok(ConversationEvent::TurnState { running })) => {
                                    turn_running.set(running);
                                    if !running {
                                        streaming_reply.set(None);
                                    }
                                }
                                Some(Ok(ConversationEvent::ReplyReset {})) => {
                                    streaming_reply.set(Some(String::new()));
                                }
                                Some(Ok(ConversationEvent::ReplyDelta { text, offset })) => {
                                    apply_reply_delta(&mut streaming_reply.write(), offset, &text);
                                }
                                // A stop shows as its saved notice instead.
                                Some(Ok(ConversationEvent::TurnError { message }))
                                    if message != crate::api::chat::TURN_STOPPED =>
                                {
                                    stream_errors.write().insert(id, message);
                                }
                                Some(Ok(ConversationEvent::TurnError { .. })) => {}
                                Some(Ok(ConversationEvent::ReposUpdate { repos: list })) => {
                                    repos.set(list);
                                }
                                // A type added since this page loaded: the
                                // server is newer than this bundle.
                                Some(Ok(ConversationEvent::Unknown)) => {
                                    *crate::frontend::STALE_BUNDLE.write() = true;
                                }
                                Some(Err(_)) | None => break,
                            }
                        }
                        live.set(None);
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

    // A second, independent live subscription — the frame stream isn't
    // part of `ConversationEvent` (see `events::ConversationEvent`'s own
    // doc comment on why frames get a separate channel), and only needs
    // to run while a session is actually open, not for the conversation's
    // whole lifetime the way the main event subscription does. Reacts to
    // both `selected()` (conversation switch) and `browsing_session_open()`
    // (the model opening/closing a session) — either changing tears down
    // any existing subscription and, if a session is now open, starts a
    // fresh one.
    #[cfg(feature = "web")]
    {
        let mut frame_task: Signal<Option<Task>> = use_signal(|| None);
        use_effect(move || {
            if let Some(task) = frame_task.write().take() {
                task.cancel();
            }
            let Some(id) = selected() else { return };
            if !browsing_session_open() {
                return;
            }
            let handle = spawn(async move {
                loop {
                    if let Ok(mut frames) = subscribe_browser_frames(id).await {
                        while let Some(Ok(frame)) = frames.recv().await {
                            browsing_frame.set(Some(frame.data));
                        }
                    }
                    // The stream ended or never opened — a network blip, a
                    // server restart, or the session closing. Reconnect,
                    // unless the session really is gone (this effect then
                    // re-runs and hides the panel).
                    gloo_timers::future::TimeoutFuture::new(1500).await;
                    if let Ok(state) = get_browsing_state(id).await
                        && !state.session_open
                    {
                        browsing_session_open.set(false);
                        return;
                    }
                }
            });
            frame_task.set(Some(handle));
        });
    }

    // Closes the detail view from inside it (its ×, Escape, a click
    // outside) and puts focus back on the bar that opened it.
    let mut close_context_detail = move || {
        context_detail_open.set(false);
        if let Some(bar) = context_bar_el.peek().clone() {
            spawn(async move {
                let _ = bar.set_focus(true).await;
            });
        }
    };

    // Fetches a fresh detail snapshot every time it's opened, rather than
    // caching — matches the idea's "most recently sent request" decision;
    // reopening after a new turn should show that turn's numbers, not a
    // stale first-open snapshot.
    let mut open_context_detail = move || {
        let Some(id) = selected() else { return };
        context_detail_open.set(true);
        // Not the last snapshot, possibly another conversation's (SME-51 B11).
        context_detail.set(None);
        spawn(async move {
            if let Ok(detail) = get_context_detail(id).await
                && selected() == Some(id)
            {
                context_detail.set(Some(detail));
            }
        });
    };

    let mut send = move || {
        let Some(id) = selected() else { return };
        let content = input();
        if content.trim().is_empty() || !model_ready() {
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

        // The reply, and the turn's other messages, arrive on the
        // conversation's own event stream; this request only starts the
        // turn. Keyed by `id`: the viewer may switch away meanwhile.
        spawn(async move {
            stream_errors.write().remove(&id);
            if let Err(e) = send_message(id, content).await {
                stream_errors.write().insert(id, server_error_message(&e));
            }
            // The first message titles a conversation, and any send moves
            // it up the list — refresh the sidebar even if the viewer has
            // switched away (this tab then isn't listening to `id`'s events).
            *conversations_changed.write() += 1;
        });
    };

    // Auto-scroll the transcript to its new bottom whenever a message is
    // added or streaming text grows — but only if the user was already at
    // the bottom (`messages_stuck_to_bottom`, kept current by the
    // `.messages` div's own `onscroll` handler below). Reads `messages()`
    // and `streaming_reply()` so it reruns on both a persisted message and
    // an in-flight delta. The stuck flag itself is read with `peek()`:
    // `onscroll` sets it on every scroll event (a `set` notifies even when
    // the value is unchanged), so reading it reactively reran this on each
    // one and pulled a scroll that stayed within the slack back to the
    // bottom (SME-83).
    use_effect(move || {
        let content = {
            let list = messages();
            let reply = streaming_reply();
            (list.len(), list.last().map(|m| m.id), reply.as_ref().map(String::len))
        };
        let changed = *last_content.peek() != Some(content);
        last_content.set(Some(content));
        if !*messages_stuck_to_bottom.peek() {
            return;
        }
        let Some(el) = messages_el() else { return };
        // Rerun with nothing new (an unchanged write): only a layout change
        // can have moved the bottom, so it waits on the pointer like one.
        if !changed && *pointer_over_transcript.peek() {
            layout_snap_pending.set(true);
            spawn(keep_transcript_anchor(el, layout_snap_pending));
            return;
        }
        // This reaches the bottom too, so a snap waiting on the pointer
        // has nothing left to do.
        layout_snap_pending.set(false);
        spawn(snap_transcript_for_content(el));
    });

    // The side panels (the sandbox, repos, todos, the browser)
    // render below the transcript in `.side-panels-row`, and appearing or
    // growing shrinks `.chat-main`, after the snap above already ran —
    // leaving `.messages` scrolled to what used to be its bottom. This
    // re-snaps to the new bottom, except while the pointer is over the
    // transcript: there the shift would move whatever the user is about to
    // click (SME-75), so the transcript is scrolled to keep the element
    // under the pointer still, and the snap waits until the pointer leaves
    // (see `onpointerleave` below). Panels beside the chat (a wide window)
    // narrow it; panels above it (a narrow one) push its top edge down, so
    // neither "stay put" nor "snap" alone would keep that element still.
    // New content still follows the bottom with the pointer over it; a
    // growing reply is expected to move.
    use_effect(move || {
        let _ = sandbox_pods();
        let _ = sandbox_terminals();
        let _ = repos();
        let _ = todos();
        let _ = browsing_session_open();
        // The context bar above the transcript appears with the first usage.
        let _ = context_usage();
        if !*messages_stuck_to_bottom.peek() {
            return;
        }
        let Some(el) = messages_el.peek().clone() else { return };
        if *pointer_over_transcript.peek() {
            layout_snap_pending.set(true);
            spawn(keep_transcript_anchor(el, layout_snap_pending));
            return;
        }
        spawn(scroll_to_bottom(el));
    });

    // Same sticky-bottom behavior, per sandbox terminal — each terminal's
    // body scrolls independently as its own output grows. A terminal with
    // no recorded stuck state yet (just appeared) defaults to stuck, same as
    // the transcript on first load. The stuck map is peeked, as above, so a
    // scroll doesn't rerun this (SME-83).
    use_effect(move || {
        let current_terminals = sandbox_terminals();
        let els = terminal_body_els();
        let stuck = terminal_body_stuck.peek().clone();
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
        section {
            class: "chat-panel",
            "data-live": live().map(|(id, _)| id.to_string()),
            "data-live-pulls": live().map(|(_, pulls)| pulls.to_string()),
            match selected() {
                None => rsx! {
                    div { class: "empty-state", "Select or start a conversation" }
                },
                Some(_) => {
                    let tool_names = tool_use_names_by_id(&messages());
                    let tool_results = tool_results_by_id(&messages());
                    let commands = terminal_commands_by_id(&messages());
                    rsx! {
                    if !sandbox_pods().is_empty() || !repos().is_empty() || !todos().is_empty() || browsing_session_open() {
                        div { class: "side-panels-row",
                            if browsing_session_open() {
                                aside { class: "browsing-panel",
                                    h3 { "Live Browser" }
                                    form {
                                        class: "browsing-address-bar",
                                        onsubmit: move |event| {
                                            event.prevent_default();
                                            let Some(id) = selected() else { return };
                                            if address_pending() {
                                                return;
                                            }
                                            let address = address_draft();
                                            address_pending.set(true);
                                            address_error.set(None);
                                            spawn(async move {
                                                let result = navigate_browser(id, address).await;
                                                // Not onto another conversation's bar (SME-51 B11).
                                                if selected() != Some(id) {
                                                    return;
                                                }
                                                match result {
                                                    Ok(()) => address_editing.set(false),
                                                    Err(e) => address_error.set(Some(server_error_message(&e))),
                                                }
                                                address_pending.set(false);
                                            });
                                        },
                                        input {
                                            class: "browsing-address-input",
                                            r#type: "text",
                                            spellcheck: "false",
                                            autocomplete: "off",
                                            aria_label: "Address",
                                            placeholder: "Enter an address",
                                            disabled: address_pending(),
                                            value: address_bar_value(
                                                address_editing(),
                                                &address_draft(),
                                                browsing_url().as_deref(),
                                            ),
                                            onfocus: move |_| {
                                                if !address_editing() {
                                                    address_draft.set(browsing_url().unwrap_or_default());
                                                    address_editing.set(true);
                                                }
                                            },
                                            oninput: move |e| {
                                                address_draft.set(e.value());
                                                address_editing.set(true);
                                            },
                                            onblur: move |_| {
                                                if !address_pending() {
                                                    address_editing.set(false);
                                                }
                                            },
                                            onkeydown: move |e: Event<KeyboardData>| {
                                                if e.data().key() == keyboard_types::Key::Escape {
                                                    address_editing.set(false);
                                                    address_error.set(None);
                                                }
                                            },
                                        }
                                    }
                                    if let Some(error) = address_error() {
                                        p { class: "browsing-address-error", role: "alert", "{error}" }
                                    }
                                    div {
                                        class: "browsing-panel-frame-wrap",
                                        tabindex: "0",
                                        oncontextmenu: move |evt| evt.prevent_default(),
                                        onresize: move |evt: Event<ResizeData>| {
                                            if let Ok(size) = evt.data().get_content_box_size() {
                                                frame_shown_width.set(size.width);
                                            }
                                        },
                                        onmousemove: move |evt: Event<MouseData>| {
                                            let Some(id) = selected() else { return };
                                            let p = evt.data().element_coordinates();
                                            let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                                            let left_held = evt.data().held_buttons().contains(MouseButton::Primary);
                                            browser_input.send((id, BrowserInputEvent::MouseMove { x, y, left_held }));
                                        },
                                        onmousedown: move |evt: Event<MouseData>| {
                                            if evt.data().trigger_button() != Some(MouseButton::Primary) {
                                                return;
                                            }
                                            let Some(id) = selected() else { return };
                                            let p = evt.data().element_coordinates();
                                            let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                                            browser_input.send((id, BrowserInputEvent::MouseDown { x, y }));
                                        },
                                        onmouseup: move |evt: Event<MouseData>| {
                                            if evt.data().trigger_button() != Some(MouseButton::Primary) {
                                                return;
                                            }
                                            let Some(id) = selected() else { return };
                                            let p = evt.data().element_coordinates();
                                            let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                                            browser_input.send((id, BrowserInputEvent::MouseUp { x, y }));
                                        },
                                        onwheel: move |evt: Event<WheelData>| {
                                            let Some(id) = selected() else { return };
                                            let p = evt.data().element_coordinates();
                                            let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                                            let (delta_x, delta_y) = wheel_delta_pixels(evt.data().delta());
                                            browser_input.send((
                                                id,
                                                BrowserInputEvent::Wheel {
                                                    x,
                                                    y,
                                                    delta_x,
                                                    delta_y,
                                                },
                                            ));
                                        },
                                        onkeydown: move |evt: Event<KeyboardData>| {
                                            let Some(id) = selected() else { return };
                                            let Some(input_event) = browser_input_event_for_key(
                                                evt.data().key(),
                                                evt.data().modifiers(),
                                            ) else {
                                                // Not ours to handle (a shortcut, a bare
                                                // modifier) — leave it to the viewer's browser.
                                                return;
                                            };
                                            evt.prevent_default();
                                            browser_input.send((id, input_event));
                                        },
                                        if let Some(data) = browsing_frame() {
                                            img {
                                                class: "browsing-panel-frame",
                                                src: "data:image/jpeg;base64,{data}",
                                                alt: "Live browsing session",
                                            }
                                        } else {
                                            div { class: "browsing-panel-empty", "Waiting for the first frame…" }
                                        }
                                    }
                                }
                            }
                            if !todos().is_empty() {
                                aside { class: "todo-panel",
                                    h3 { "Todos" }
                                    ul { class: "todo-list",
                                        for (i , todo) in todos().into_iter().enumerate() {
                                            li {
                                                key: "{i}",
                                                class: "todo-item todo-item-{todo_status_class(todo.status)}",
                                                span { class: "todo-item-marker" }
                                                span { class: "todo-item-content", "{todo.content}" }
                                            }
                                        }
                                    }
                                }
                            }
                            if !sandbox_pods().is_empty() || !repos().is_empty() {
                                aside { class: "sandbox-panel",
                                    h3 { "Sandbox" }
                                    // The conversation's repos, pod or not: /workspace and
                                    // its checkouts outlive the pod, and a clone that
                                    // failed to start a sandbox must still say so.
                                            if !repos().is_empty() {
                                                div { class: "sandbox-repos",
                                                    for repo in repos() {
                                                        div {
                                                            key: "{repo.id}",
                                                            class: "sandbox-repo sandbox-repo-{repo_status_class(repo.status)}",
                                                            title: "{repo.url}",
                                                            code { class: "sandbox-repo-path", "{repo.path}" }
                                                            span { class: "sandbox-repo-detail", "{repo_detail(&repo)}" }
                                                            if let Some(label) = instructions_label(&repo) {
                                                                span {
                                                                    class: "sandbox-repo-instructions",
                                                                    title: "The model loads a repo's AGENTS.md files with load_instructions. Loaded ones are in its context on every turn; see the context view.",
                                                                    "{label}"
                                                                }
                                                            }
                                                            if let Some(err) = repo.error.clone() {
                                                                pre { class: "sandbox-repo-error", "{err}" }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                    // A conversation has at most one live pod (see
                                    // SME-11's "One pod
                                    // per conversation") — straight through, no tab
                                    // bar needed to pick between pods anymore.
                                    for pod in sandbox_pods() {
                                        div { key: "{pod.pod_id}", class: "sandbox-pod",
                                            div { class: "sandbox-pod-header",
                                                span { class: "sandbox-pod-status", "{pod.status}" }
                                                button {
                                                    class: if pending_pod_stop() == Some(pod.pod_id) { "pod-stop confirm" } else { "pod-stop" },
                                                    r#type: "button",
                                                    title: "Stop this pod. Its terminals and any files outside /workspace and mounted volumes are lost.",
                                                    onclick: move |_| request_pod_stop(pod.pod_id),
                                                    super::TwoStepLabel { armed: pending_pod_stop() == Some(pod.pod_id), idle: "Stop sandbox", confirm: "Confirm stop?" }
                                                }
                                            }
                                            if let Some(err) = pod_stop_error() {
                                                p { class: "error", "{err}" }
                                            }
                                            if !pod.previews.is_empty() {
                                                div { class: "sandbox-previews",
                                                    for preview in pod.previews.clone() {
                                                        a {
                                                            key: "{preview.url}",
                                                            class: "sandbox-preview",
                                                            href: "{preview.url}",
                                                            target: "_blank",
                                                            rel: "noopener noreferrer",
                                                            title: "Open what's running on {preview.label()} in the sandbox, in a new tab",
                                                            "Open preview · {preview.label()}"
                                                        }
                                                    }
                                                }
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
                        if let Some(snapshot) = context_usage() {
                            button {
                                r#type: "button",
                                class: "context-usage-bar",
                                onmounted: move |evt| context_bar_el.set(Some(evt)),
                                onclick: move |_| open_context_detail(),
                                if let Some(percent) = context_usage_percent(&snapshot) {
                                    div { class: "context-usage-track",
                                        div {
                                            class: "context-usage-fill",
                                            style: "width: {percent}%",
                                        }
                                    }
                                    span { class: "context-usage-label", "{percent}% of context" }
                                } else {
                                    span { class: "context-usage-label", "No usage yet" }
                                }
                            }
                        } else {
                            // The bar's space, kept until the usage arrives: it
                            // always shows once it has, and appearing above the
                            // transcript would push it down under the pointer
                            // (SME-75).
                            div { class: "context-usage-bar context-usage-bar-pending", aria_hidden: "true",
                                span { class: "context-usage-label", "\u{a0}" }
                            }
                        }
                        if context_detail_open() {
                            div { class: "context-detail-overlay",
                                onclick: move |_| close_context_detail(),
                                onkeydown: move |e: Event<KeyboardData>| {
                                    if e.data().key() == keyboard_types::Key::Escape {
                                        close_context_detail();
                                    }
                                },
                                // The view is modal, so Tab cycles inside it:
                                // focus reaching either sentinel wraps around
                                // to the view's other end.
                                div {
                                    class: "focus-sentinel",
                                    tabindex: "0",
                                    onfocus: move |_| {
                                        spawn(async move {
                                            let _ = document::eval(FOCUS_LAST_IN_CONTEXT_DETAIL).await;
                                        });
                                    },
                                }
                                div {
                                    class: "context-detail-panel",
                                    role: "dialog",
                                    aria_modal: "true",
                                    aria_label: "Context",
                                    // Focusable (not in the tab order), so a
                                    // click on the view's text keeps focus in
                                    // it and Escape still reaches the overlay.
                                    tabindex: "-1",
                                    onclick: move |evt| evt.stop_propagation(),
                                    button {
                                        r#type: "button",
                                        class: "context-detail-close",
                                        aria_label: "Close",
                                        onmounted: move |evt: MountedEvent| async move {
                                            let _ = evt.set_focus(true).await;
                                        },
                                        onclick: move |_| close_context_detail(),
                                        "×"
                                    }
                                    match context_detail() {
                                        None => rsx! { p { "Loading…" } },
                                        Some(detail) => rsx! {
                                            h3 { "Context" }
                                            if !detail.instructions.is_empty() {
                                                h4 { class: "context-detail-heading", "Project instructions ({detail.instructions.len()})" }
                                                p { class: "muted", "AGENTS.md files from this conversation's repos, sent with every turn as part of the system prompt below." }
                                                for doc in &detail.instructions {
                                                    details { class: "context-detail-instructions",
                                                        summary {
                                                            code { "{doc.path}" }
                                                            span { class: "muted", " {instructions_source(doc)}" }
                                                        }
                                                        pre { class: "context-detail-prompt", "{doc.content}" }
                                                    }
                                                }
                                            }
                                            h4 { class: "context-detail-heading", "System prompt" }
                                            if let Some(system) = &detail.system {
                                                // Its own line breaks and headings, not one
                                                // run-together paragraph (SME-41 D7).
                                                pre { class: "context-detail-prompt", "{system}" }
                                            } else {
                                                p { class: "muted", "None set." }
                                            }
                                            p { "Messages: {detail.message_count}" }
                                            if let Some(usage) = &detail.usage {
                                                p {
                                                    "Tokens — input: {usage.input_tokens}, output: {usage.output_tokens}, "
                                                    "cache creation: {usage.cache_creation_input_tokens}, cache read: {usage.cache_read_input_tokens}"
                                                }
                                                {render_context_meter(&context_usage_breakdown(usage, detail.context_window))}
                                            }
                                            p { "Context window: {detail.context_window}" }
                                            h4 { "Tools ({detail.tools.len()})" }
                                            for tool in &detail.tools {
                                                div { class: "context-detail-tool",
                                                    strong { "{tool.name}" }
                                                    p { "{tool.description}" }
                                                    pre { "{tool.input_schema}" }
                                                }
                                            }
                                        },
                                    }
                                }
                                div {
                                    class: "focus-sentinel",
                                    tabindex: "0",
                                    onfocus: move |_| {
                                        spawn(async move {
                                            let _ = document::eval(FOCUS_FIRST_IN_CONTEXT_DETAIL).await;
                                        });
                                    },
                                }
                            }
                        }
                        div {
                            class: "messages",
                            onmounted: move |evt| {
                                messages_el.set(Some(evt));
                                spawn(async move {
                                    let _ = document::eval(TRANSCRIPT_ANCHOR_SETUP).await;
                                });
                            },
                            onpointerenter: move |_| pointer_over_transcript.set(true),
                            // Also on a move, in case the transcript appeared
                            // under a pointer that was already there.
                            onpointermove: move |_| {
                                if !*pointer_over_transcript.peek() {
                                    pointer_over_transcript.set(true);
                                }
                            },
                            onpointerleave: move |_| {
                                pointer_over_transcript.set(false);
                                if *layout_snap_pending.peek() {
                                    layout_snap_pending.set(false);
                                    if *messages_stuck_to_bottom.peek()
                                        && let Some(el) = messages_el.peek().clone()
                                    {
                                        spawn(scroll_to_bottom(el));
                                    }
                                }
                            },
                            // A scroll the user starts while a snap is pending
                            // gives up the snap, so the scroll that follows
                            // decides whether they're still at the bottom.
                            onwheel: move |_| layout_snap_pending.set(false),
                            // A press starts a scrollbar or selection drag,
                            // or focuses text the keys then scroll.
                            onpointerdown: move |_| layout_snap_pending.set(false),
                            ontouchmove: move |_| layout_snap_pending.set(false),
                            onkeydown: move |_| layout_snap_pending.set(false),
                            onscroll: move |evt: Event<ScrollData>| {
                                // While a snap is pending, the scrolls are the
                                // transcript keeping the text under the pointer
                                // still, not the user leaving the bottom: the
                                // snap still owes them the bottom (SME-75 code
                                // review).
                                if *layout_snap_pending.peek() {
                                    return;
                                }
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
                            if conversation_missing() {
                                p { class: "conversation-missing",
                                    "This conversation doesn't exist. It may have been deleted."
                                }
                            } else if let Some(err) = load_error() {
                                p { class: "error", "Error loading messages: {err}" }
                            }
                            for message in messages() {
                                match message.blocks() {
                                    Ok(blocks) => {
                                        let thinking_open = reply_is_only_thinking(&message.role, &blocks);
                                        rsx! {
                                            for (i , block) in blocks.iter().enumerate() {
                                                {render_block_element(message.id, i, &message.role, message.created_at, tz_offset_minutes(), block, &tool_names, &tool_results, &commands, thinking_open)}
                                            }
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
                            // A new conversation says what smelt does and offers a
                            // few asks to start from, instead of a blank screen
                            // (SME-41 D12). Picking one fills the message box.
                            if messages().is_empty() && !turn_running() && !conversation_missing() && matches!(initial_messages(), Some(Some(Ok(_)))) {
                                div { class: "conversation-empty",
                                    h2 { "What should smelt work on?" }
                                    p { "It works in a sandbox of its own: it writes and runs code, uses a terminal, reads the web, and shows you each step." }
                                    form {
                                        class: "repo-attach",
                                        onsubmit: move |event| {
                                            event.prevent_default();
                                            let Some(id) = selected() else { return };
                                            if repo_attaching() {
                                                return;
                                            }
                                            let url = repo_url();
                                            let branch = repo_branch();
                                            let dir = repo_dir();
                                            repo_attaching.set(true);
                                            repo_attach_error.set(None);
                                            spawn(async move {
                                                let result = attach_repo(id, url, branch, dir).await;
                                                // The user may have moved on to another conversation.
                                                if selected() != Some(id) {
                                                    return;
                                                }
                                                match result {
                                                    Ok(_) => {
                                                        repo_url.set(String::new());
                                                        repo_branch.set(String::new());
                                                        repo_dir.set(String::new());
                                                    }
                                                    Err(e) => repo_attach_error.set(Some(server_error_message(&e))),
                                                }
                                                repo_attaching.set(false);
                                            });
                                        },
                                        label { r#for: "repo-attach-url", "Work on a repo" }
                                        div { class: "repo-attach-fields",
                                            input {
                                                id: "repo-attach-url",
                                                r#type: "text",
                                                required: true,
                                                placeholder: "git@github.com:owner/repo.git",
                                                value: "{repo_url}",
                                                oninput: move |e| repo_url.set(e.value()),
                                            }
                                            input {
                                                class: "repo-attach-branch",
                                                r#type: "text",
                                                placeholder: "branch (optional)",
                                                aria_label: "Branch",
                                                value: "{repo_branch}",
                                                oninput: move |e| repo_branch.set(e.value()),
                                            }
                                            input {
                                                class: "repo-attach-branch",
                                                r#type: "text",
                                                placeholder: "directory (optional)",
                                                aria_label: "Directory under /workspace",
                                                value: "{repo_dir}",
                                                oninput: move |e| repo_dir.set(e.value()),
                                            }
                                            button {
                                                r#type: "submit",
                                                disabled: repo_attaching(),
                                                if repo_attaching() { "Cloning\u{2026}" } else { "Clone" }
                                            }
                                        }
                                        if repo_attaching() {
                                            p { class: "muted", "Starting the sandbox and cloning. You can write your first message meanwhile." }
                                        }
                                        if let Some(err) = repo_attach_error() {
                                            pre { class: "error repo-attach-error", "{err}" }
                                        }
                                    }
                                    div { class: "example-asks",
                                        for example in EXAMPLE_ASKS {
                                            button {
                                                class: "example-ask",
                                                r#type: "button",
                                                onclick: move |_| input.set(example.to_string()),
                                                "{example}"
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(reply) = streaming_text() {
                                div { class: "message message-assistant message-streaming", "{reply}" }
                            }
                            if turn_running() {
                                div { class: "turn-working", role: "status",
                                    span { class: "turn-working-dot" }
                                    span { "Working… {format_elapsed(turn_elapsed())}" }
                                }
                            }
                            if let Some(err) = stream_error() {
                                if err == crate::providers::NO_MODEL_CONFIGURED {
                                    p { class: "model-picker-setup", role: "status",
                                        "{err} "
                                        Link { to: Route::ProvidersRoute {}, "Model providers" }
                                    }
                                } else {
                                    p { class: "error", "{err}" }
                                }
                            }
                            if let Some(err) = notification_delivery_error() {
                                if err == crate::providers::NO_MODEL_CONFIGURED {
                                    p { class: "model-picker-setup", role: "status",
                                        "A finished command or task is waiting for the model, but there's no model to send it to. "
                                        Link { to: Route::ProvidersRoute {}, "Model providers" }
                                    }
                                } else {
                                    p { class: "error", "A background notification failed to reach the model: {err}" }
                                }
                            }
                            // A repo's AGENTS.md waits for the user's trust before it
                            // becomes instructions the model follows (SME-32).
                            for (repo, request) in repos().into_iter().flat_map(|r| r.trust_requests.clone().into_iter().map(move |q| (r.clone(), q))) {
                                div { key: "trust-{request.id}", class: "trust-card", role: "group", aria_label: "Trust {repo.url}?",
                                    p { class: "trust-card-question",
                                        "Trust "
                                        code { "{repo.url}" }
                                        "?"
                                    }
                                    p { class: "muted",
                                        "The model wants to load this AGENTS.md as instructions it follows on every turn. Trust loads exactly the file below, and later ones from this repo load without asking. Only trust repos whose instructions you're happy for the model to follow."
                                    }
                                    details { class: "trust-card-preview", open: true,
                                        summary { "{request.path}" }
                                        pre { "{request.content}" }
                                    }
                                    div { class: "trust-card-buttons",
                                        button {
                                            class: "trust-card-trust",
                                            r#type: "button",
                                            onclick: {
                                                let request_id = request.id;
                                                let shown_hash = request.hash.clone();
                                                move |_| {
                                                    let Some(id) = selected() else { return };
                                                    let shown_hash = shown_hash.clone();
                                                    repo_action_error.set(None);
                                                    spawn(async move {
                                                        if let Err(e) = decide_repo_trust(id, request_id, shown_hash, true).await
                                                            // Not if the user has moved on to another conversation.
                                                            && selected() == Some(id)
                                                        {
                                                            repo_action_error.set(Some(server_error_message(&e)));
                                                        }
                                                    });
                                                }
                                            },
                                            "Trust"
                                        }
                                        button {
                                            class: "trust-card-decline",
                                            r#type: "button",
                                            onclick: {
                                                let request_id = request.id;
                                                let shown_hash = request.hash.clone();
                                                move |_| {
                                                    let Some(id) = selected() else { return };
                                                    let shown_hash = shown_hash.clone();
                                                    repo_action_error.set(None);
                                                    spawn(async move {
                                                        if let Err(e) = decide_repo_trust(id, request_id, shown_hash, false).await
                                                            // Not if the user has moved on to another conversation.
                                                            && selected() == Some(id)
                                                        {
                                                            repo_action_error.set(Some(server_error_message(&e)));
                                                        }
                                                    });
                                                }
                                            },
                                            "Don't trust"
                                        }
                                    }
                                }
                            }
                            if let Some(err) = repo_action_error() {
                                p { class: "error", "{err}" }
                            }
                            // The model's question, waiting on the user (SME-34).
                            if let (Some(id), Some(question)) = (selected(), pending_question()) {
                                QuestionCard { key: "{question.tool_use_id}", conversation_id: id, question }
                            }
                        }
                        if !conversation_missing() {
                        if let Some(id) = selected() {
                            super::ModelPicker { key: "{id}", conversation_id: id, refresh: model_changed, ready: model_ready }
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
                                placeholder: if pending_question().is_some() { "Answer the question above, or write a reply instead" } else { "Type a message..." },
                                oninput: move |e| input.set(e.value()),
                            }
                            button {
                                r#type: "submit",
                                disabled: is_streaming() || !model_ready(),
                                title: if model_ready() { "" } else { "Choose a model first" },
                                "Send"
                            }
                            if can_stop() {
                                button {
                                    r#type: "button",
                                    class: "stop-turn",
                                    title: "Stop the model's current turn. Its sandbox, terminals and running commands keep going.",
                                    onclick: stop,
                                    "Stop"
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
