//! Rendering the transcript: content blocks, tool calls and results, diffs, notices.

use super::*;
use crate::markdown::Markdown;

/// Pretty-prints a `ToolUse` block's `input` for display. Falls back to the
/// compact form on the (practically impossible, since `Value` always
/// serializes) chance pretty-printing fails.
pub(super) fn format_tool_input(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// One rendered line of an `edit_file` diff — `content` has its trailing
/// newline already stripped (`similar`'s line-based `Change::as_str`
/// includes it, since it's diffing whole lines).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DiffLine {
    pub(super) kind: DiffLineKind,
    pub(super) content: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum DiffLineKind {
    Equal,
    Removed,
    Added,
}

/// A real line-level diff between `edit_file`'s `old_string`/`new_string`,
/// via `similar::TextDiff::from_lines` — not a naive "all of old removed,
/// all of new added." Pure and testable the same way
/// `format_tool_input`/`tool_result_label` are, no DOM involved. See
/// SME-11's "Diff rendering."
pub(super) fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
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
pub(super) fn tool_summary(name: &str, input: &serde_json::Value) -> String {
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

pub(super) fn tool_result_label(is_error: bool) -> &'static str {
    if is_error {
        "Tool error"
    } else {
        "Tool result"
    }
}

/// Each tool call's result, by the call's id: its content and whether it
/// failed. A call's row shows its result folded in (SME-41 D2).
pub(super) fn tool_results_by_id(messages: &[Message]) -> HashMap<String, (String, bool)> {
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
pub(super) fn tool_use_names_by_id(messages: &[Message]) -> HashMap<String, String> {
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
pub(super) fn format_timestamp(created_at: chrono::NaiveDateTime, tz_offset_minutes: i32) -> String {
    let local = created_at + chrono::Duration::minutes(tz_offset_minutes as i64);
    local.format("%-I:%M %p").to_string()
}

/// Renders one content block, keyed by `{message_id}-{index}` for the
/// enclosing `for` loop. `Text` renders as an ordinary chat bubble, same as
/// always. `ToolUse`/
/// `ToolResult` render as their own centered cards, distinct from both the
/// Whether an assistant message is thinking and nothing else: no text,
/// no tool call. A model sometimes puts its whole answer in its thinking;
/// collapsed, that reply looked empty (SME-40 F14), so such a message's
/// thinking is shown open.
pub(super) fn reply_is_only_thinking(role: &str, blocks: &[ContentBlock]) -> bool {
    role == "assistant"
        && !blocks.is_empty()
        && blocks.iter().all(|b| matches!(b, ContentBlock::Thinking { .. }))
}

/// A notice smelt saved into the conversation for the model (a command
/// finishing, the sandbox stopping), as the
/// short sentence the chat shows in place of a user bubble, without
/// internal ids. `None` for anything the user actually wrote. They used
/// to look like the user talking (SME-41 D3). `commands` maps a terminal
/// command's id to the command line, from `terminal_commands_by_id`.
pub(super) fn system_notice(text: &str, commands: &HashMap<String, String>) -> Option<String> {
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
pub(super) fn terminal_commands_by_id(messages: &[Message]) -> HashMap<String, String> {
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
pub(super) fn display_text(text: &str) -> String {
    task_notice_sentence(text).unwrap_or_else(|| text.to_string())
}

pub(super) fn task_notice_sentence(text: &str) -> Option<String> {
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

/// How an `ask_user` call shows in the transcript (SME-34).
#[derive(Debug, PartialEq)]
pub(super) enum QuestionCall {
    /// No result yet: the card below the transcript asks it.
    Waiting,
    /// The answer lines the model got.
    Answered(Vec<String>),
    /// Refused (bad input, a second call in one reply): shown as the
    /// ordinary failed tool row, with its error.
    Refused,
}

pub(super) fn question_call(result: Option<&(String, bool)>) -> QuestionCall {
    match result {
        None => QuestionCall::Waiting,
        Some((_, true)) => QuestionCall::Refused,
        Some((content, false)) => QuestionCall::Answered(answered_lines(content)),
    }
}

/// The answer lines of an `ask_user` result, as the model got them:
/// "1. Delete: Yes", or what it was told when the user wrote instead.
pub(super) fn answered_lines(result: &str) -> Vec<String> {
    match result.strip_prefix("The user answered:") {
        Some(rest) => rest.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
        None => vec!["Not answered: you wrote a message instead.".to_string()],
    }
}

/// user- and assistant-aligned bubbles, so a tool call/result reads as
/// "the agent doing something" rather than "someone said something."
pub(super) fn render_block_element(
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
    on_media_load: EventHandler<()>,
) -> Element {
    let key = format!("{message_id}-{index}");
    let timestamp = format_timestamp(created_at, tz_offset_minutes);
    match block {
        ContentBlock::Text { text } if role == "user" && system_notice(text, commands).is_some() => {
            let notice = system_notice(text, commands).unwrap_or_default();
            rsx! {
                div { key: "{key}", class: "system-notice",
                    span { "{notice}" }
                    span { class: "timestamp", "{timestamp}" }
                }
            }
        }
        // The model's replies render as markdown (SME-30); everything
        // else stays plain text.
        ContentBlock::Text { text } if role == "assistant" => rsx! {
            div { key: "{key}", class: "message message-{role}",
                div { class: "message-text message-markdown",
                    Markdown { source: text.clone(), on_media_load }
                }
                span { class: "timestamp", "{timestamp}" }
            }
        },
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
        ContentBlock::ToolUse { id, name, input }
            if name == ASK_USER && question_call(tool_results.get(id)) != QuestionCall::Refused =>
        {
            match question_call(tool_results.get(id)) {
                QuestionCall::Answered(answer) => {
                    let questions: Vec<String> = input
                        .get("questions")
                        .and_then(|q| q.as_array())
                        .map(|qs| qs.iter().filter_map(|q| q.get("question").and_then(|t| t.as_str()).map(str::to_string)).collect())
                        .unwrap_or_default();
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
                QuestionCall::Waiting | QuestionCall::Refused => rsx! {},
            }
        }
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

/// One message's blocks. Keyed by the message's id in the transcript, and
/// re-rendered only when its own message or one of the conversation's tool
/// maps changes: a streamed delta, a tick, or another message arriving
/// leaves it alone (SME-57).
#[component]
pub(super) fn MessageView(
    message: Message,
    tz_offset_minutes: i32,
    tool_names: Memo<HashMap<String, String>>,
    tool_results: Memo<HashMap<String, (String, bool)>>,
    commands: Memo<HashMap<String, String>>,
    on_media_load: EventHandler<()>,
) -> Element {
    match message.blocks() {
        Ok(blocks) => {
            let thinking_open = reply_is_only_thinking(&message.role, &blocks);
            let tool_names = tool_names.read();
            let tool_results = tool_results.read();
            let commands = commands.read();
            rsx! {
                for (i , block) in blocks.iter().enumerate() {
                    {render_block_element(message.id, i, &message.role, message.created_at, tz_offset_minutes, block, &tool_names, &tool_results, &commands, thinking_open, on_media_load)}
                }
            }
        }
        Err(e) => rsx! {
            div { class: "message message-{message.role} message-error", "Error rendering message: {e}" }
        },
    }
}

/// The transcript: the conversation's messages, a new conversation's
/// first steps, the reply as it streams, the working line, errors, trust
/// cards and the model's waiting question. Scrolling it keeps the panel's stick-to-bottom and
/// pointer-hold state (SME-83, SME-75) current.
#[component]
pub(super) fn Transcript(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    initial_messages: Resource<Option<Result<Vec<Message>, ServerFnError>>>,
    tz_offset_minutes: Signal<i32>,
    mut input: Signal<String>,
    on_attach: EventHandler<()>,
    stream_errors: Signal<HashMap<i64, String>>,
    scroll: StickyBottom,
    mut pointer_over_transcript: Signal<bool>,
    mut media_loaded: Signal<u64>,
) -> Element {
    let messages = state.messages();
    let load_error = state.load_error();
    let turn_running = state.turn_running();
    let pending_question = state.pending_question();
    let mut layout_snap_pending = state.layout_snap_pending();
    // An image in a reply loaded: a layout change for the sticky scroll.
    let on_media_load = use_callback(move |()| *media_loaded.write() += 1);
    let conversation_missing = move || load_error().as_deref() == Some("conversation not found");
    // Worked out once per change to the messages, not on every render.
    let tool_names = use_memo(move || tool_use_names_by_id(&messages.read()));
    let tool_results = use_memo(move || tool_results_by_id(&messages.read()));
    let commands = use_memo(move || terminal_commands_by_id(&messages.read()));
    rsx! {
        div {
            class: "messages",
            onmounted: move |evt| {
                scroll.mounted(evt);
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
                    if scroll.is_stuck()
                        && let Some(el) = scroll.el_untracked()
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
                scroll.scrolled(&evt.data());
            },
            if conversation_missing() {
                p { class: "conversation-missing",
                    "This conversation doesn't exist. It may have been deleted."
                }
            } else {
                ErrorText { message: load_error().map(|err| format!("Error loading messages: {err}")) }
            }
            for message in messages() {
                MessageView {
                    key: "{message.id}",
                    message,
                    tz_offset_minutes: tz_offset_minutes(),
                    tool_names,
                    tool_results,
                    commands,
                    on_media_load,
                }
            }
            // A new conversation says what smelt does and offers a
            // few asks to start from, instead of a blank screen
            // (SME-41 D12). Picking one fills the message box.
            if messages().is_empty() && !turn_running() && !conversation_missing() && matches!(initial_messages(), Some(Some(Ok(_)))) {
                div { class: "conversation-empty",
                    h2 { "What should smelt work on?" }
                    p { "It works in a sandbox of its own: it writes and runs code, uses a terminal, reads the web, and shows you each step." }
                    RepoAttach {
                        state,
                        on_attach,
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
            StreamingReply { state, on_media_load }
            if turn_running() {
                WorkingLine { state }
            }
            ModelNotes { selected, state, stream_errors }
            TrustCards { selected, state }
            // The model's question, waiting on the user (SME-34).
            if let (Some(id), Some(question)) = (selected(), pending_question()) {
                QuestionCard { key: "{question.tool_use_id}", conversation_id: id, question }
            }
        }
    }
}
