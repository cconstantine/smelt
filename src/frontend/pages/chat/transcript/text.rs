use super::*;

/// Pretty-prints a `ToolUse` block's `input` for display. Falls back to the
/// compact form on the (practically impossible, since `Value` always
/// serializes) chance pretty-printing fails.
pub(in crate::frontend::pages::chat) fn format_tool_input(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// One rendered line of an `edit_file` diff — `content` has its trailing
/// newline already stripped (`similar`'s line-based `Change::as_str`
/// includes it, since it's diffing whole lines).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::frontend::pages::chat) struct DiffLine {
    pub(in crate::frontend::pages::chat) kind: DiffLineKind,
    pub(in crate::frontend::pages::chat) content: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::frontend::pages::chat) enum DiffLineKind {
    Equal,
    Removed,
    Added,
}

/// A real line-level diff between `edit_file`'s `old_string`/`new_string`,
/// via `similar::TextDiff::from_lines` — not a naive "all of old removed,
/// all of new added." Pure and testable the same way
/// `format_tool_input`/`tool_result_label` are, no DOM involved. See
/// SME-11's "Diff rendering."
pub(in crate::frontend::pages::chat) fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
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
pub(in crate::frontend::pages::chat) fn tool_summary(name: &str, input: &serde_json::Value) -> String {
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
        "set_idle_notice" => "Re-timed a command's idle notice".to_string(),
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

pub(in crate::frontend::pages::chat) fn tool_result_label(is_error: bool) -> &'static str {
    if is_error {
        "Tool error"
    } else {
        "Tool result"
    }
}

/// Each tool call's result, by the call's id: its content and whether it
/// failed. A call's row shows its result folded in (SME-41 D2).
pub(in crate::frontend::pages::chat) fn tool_results_by_id(messages: &[Message]) -> HashMap<String, (String, bool)> {
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
pub(in crate::frontend::pages::chat) fn tool_use_names_by_id(messages: &[Message]) -> HashMap<String, String> {
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

/// One message's share of a finished reply the user can copy (SME-105).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::frontend::pages::chat) struct ReplyPart {
    /// The reply it belongs to: the id of the message holding the reply's
    /// last text, where its button is.
    pub(in crate::frontend::pages::chat) reply: i64,
    /// The blocks of this message the copy includes, which the button
    /// outlines while it's pointed at.
    pub(in crate::frontend::pages::chat) blocks: Vec<usize>,
    /// On the message holding the reply's last text: the button.
    pub(in crate::frontend::pages::chat) copy: Option<ReplyCopy>,
}

/// A reply's button: where it sits and what it copies.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::frontend::pages::chat) struct ReplyCopy {
    /// The block whose bubble holds the button: the reply's last text.
    pub(in crate::frontend::pages::chat) block_index: usize,
    /// The reply's text blocks as the model wrote them, joined by a blank
    /// line. Shared, so handing each message its part doesn't copy it.
    pub(in crate::frontend::pages::chat) markdown: std::rc::Rc<str>,
    /// How many text bubbles that is.
    pub(in crate::frontend::pages::chat) parts: usize,
}

/// Each finished reply's parts, by message id (SME-105). A reply is every
/// assistant text block from one thing the user did to the next: a
/// user-role message holding text (their message, or a notice such as
/// "Stopped.") or answering an `ask_user` call (not refusing it) starts a new one; ordinary
/// tool results and compaction's placeholders don't, nor does a terminal
/// command's notice saved right after tool results, inside a turn's tool
/// loop (a command that finished while the turn went on). Thinking, tool calls,
/// results and compaction summaries aren't part of the copy, nor is a text
/// block that is only whitespace. While the turn runs, the newest reply
/// (the one after the last such message) has none: it may still grow.
pub(in crate::frontend::pages::chat) fn reply_parts(messages: &[Message], turn_running: bool) -> HashMap<i64, ReplyPart> {
    // Each message parsed once, for the tool names and the walk below.
    let readable: Vec<(&Message, Vec<ContentBlock>)> =
        messages.iter().filter_map(|m| m.blocks().ok().map(|blocks| (m, blocks))).collect();
    let tool_names: HashMap<&str, &str> = readable
        .iter()
        .flat_map(|(_, blocks)| blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    let mut parts = HashMap::new();
    // The reply being gathered: (message id, block index, text) per part.
    let mut reply: Vec<(i64, usize, &str)> = Vec::new();
    // Whether the turn is inside its tool loop: the last message was tool
    // results going back to the model. A terminal command's notice saved
    // there (drained at the top of the loop) is part of the same turn, not
    // a new one (code review 1). Only those are drained there: a cut-off
    // notice or "Stopped." after tool results ends the turn (code review
    // 2). A turn that failed right after its tool results looks the same
    // as one still in its loop, so a command finishing after it joins the
    // failed turn's reply.
    let mut in_tool_loop = false;
    for (message, blocks) in &readable {
        if message.role == "user" {
            let mut starts_reply = false;
            let mut tool_results = false;
            for block in blocks {
                match block {
                    ContentBlock::Text { text } => {
                        let mid_turn_notice = in_tool_loop && text.starts_with("Terminal command ");
                        starts_reply |= !mid_turn_notice;
                    }
                    // An answer to `ask_user` (or the user writing
                    // instead); a refused call's error result is an
                    // ordinary tool result and the turn goes on.
                    ContentBlock::ToolResult { tool_use_id, is_error, .. } => {
                        tool_results = true;
                        starts_reply |= *is_error != Some(true)
                            && tool_names.get(tool_use_id.as_str()).is_some_and(|name| *name == ASK_USER);
                    }
                    _ => {}
                }
            }
            if starts_reply {
                finish_reply(&mut parts, std::mem::take(&mut reply));
                in_tool_loop = false;
            } else if tool_results {
                in_tool_loop = true;
            }
        } else if message.role == "assistant" {
            in_tool_loop = false;
            for (index, block) in blocks.iter().enumerate() {
                if let ContentBlock::Text { text } = block
                    && !text.trim().is_empty()
                {
                    reply.push((message.id, index, text));
                }
            }
        }
    }
    if !turn_running {
        finish_reply(&mut parts, reply);
    }
    parts
}

/// Adds one finished reply's parts to `parts`, unless it has no text.
fn finish_reply(parts: &mut HashMap<i64, ReplyPart>, reply: Vec<(i64, usize, &str)>) {
    let Some(&(last_message, last_block, _)) = reply.last() else {
        return;
    };
    let markdown = reply
        .iter()
        .map(|(_, _, text)| text.trim_matches(|c| c == '\n' || c == '\r'))
        .collect::<Vec<_>>()
        .join("\n\n");
    for &(message, block, _) in &reply {
        parts
            .entry(message)
            .or_insert_with(|| ReplyPart { reply: last_message, blocks: Vec::new(), copy: None })
            .blocks
            .push(block);
    }
    if let Some(part) = parts.get_mut(&last_message) {
        part.copy = Some(ReplyCopy { block_index: last_block, markdown: markdown.into(), parts: reply.len() });
    }
}

/// A reply's Copy button label: how many bubbles it copies, when that's
/// more than the one it sits in.
pub(in crate::frontend::pages::chat) fn reply_copy_label(parts: usize) -> String {
    if parts > 1 { format!("Copy reply ({parts} parts)") } else { "Copy reply".to_string() }
}

pub(in crate::frontend::pages::chat) fn reply_copy_title(parts: usize) -> String {
    if parts > 1 {
        format!("Copy this reply's {parts} outlined parts as Markdown")
    } else {
        "Copy this reply as Markdown".to_string()
    }
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
pub(in crate::frontend::pages::chat) fn format_timestamp(created_at: chrono::NaiveDateTime, tz_offset_minutes: i32) -> String {
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
pub(in crate::frontend::pages::chat) fn reply_is_only_thinking(role: &str, blocks: &[ContentBlock]) -> bool {
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
pub(in crate::frontend::pages::chat) fn system_notice(text: &str, commands: &HashMap<String, String>) -> Option<String> {
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
        // The idle notice (SME-79): smelt woke the model because a running
        // command went untouched — the sentence, without the tool advice
        // written for the model.
        if let Some((head, tail)) = rest.split_once(" is still running and hasn't been touched in ")
            && let Some((id, _)) = head.split_once(" (\"")
            && let Some((quiet, _)) = tail.split_once(':')
        {
            return Some(format!("{} is still running, untouched for {quiet}", command(id)));
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
    if let Some((tokens, limit, cut_call)) = crate::api::chat::parse_cut_off_notice(text) {
        let call = cut_call.map(|name| format!(" in the middle of a `{name}` call, which didn't run")).unwrap_or_default();
        return Some(format!(
            "The reply was cut off at its limit of {} tokens ({}){call}. {}",
            crate::frontend::pages::providers::group_digits(tokens),
            limit.reason(),
            limit.hint()
        ));
    }
    if let Some(cut_call) = crate::api::chat::parse_refusal_notice(text) {
        let call = cut_call.map(|name| format!(" in the middle of a `{name}` call, which didn't run")).unwrap_or_default();
        return Some(format!("The model's provider stopped this reply (a refusal){call}."));
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
pub(in crate::frontend::pages::chat) fn terminal_commands_by_id(messages: &[Message]) -> HashMap<String, String> {
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
pub(in crate::frontend::pages::chat) fn display_text(text: &str) -> String {
    task_notice_sentence(text).unwrap_or_else(|| text.to_string())
}

pub(in crate::frontend::pages::chat) fn task_notice_sentence(text: &str) -> Option<String> {
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
pub(in crate::frontend::pages::chat) enum QuestionCall {
    /// No result yet: the card below the transcript asks it.
    Waiting,
    /// The answer lines the model got.
    Answered(Vec<String>),
    /// Refused (bad input, a second call in one reply): shown as the
    /// ordinary failed tool row, with its error.
    Refused,
}

pub(in crate::frontend::pages::chat) fn question_call(result: Option<&(String, bool)>) -> QuestionCall {
    match result {
        None => QuestionCall::Waiting,
        Some((_, true)) => QuestionCall::Refused,
        Some((content, false)) => QuestionCall::Answered(answered_lines(content)),
    }
}

/// The answer lines of an `ask_user` result, as the model got them:
/// "1. Delete: Yes", or what it was told when the user wrote instead.
pub(in crate::frontend::pages::chat) fn answered_lines(result: &str) -> Vec<String> {
    match result.strip_prefix("The user answered:") {
        Some(rest) => rest.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
        None => vec!["Not answered: you wrote a message instead.".to_string()],
    }
}
