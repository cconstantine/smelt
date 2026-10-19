//! Rendering the transcript: content blocks, tool calls and results, diffs, notices.

use super::*;
use crate::frontend::clipboard::CopyButton;
use crate::markdown::Markdown;

/// Outlines the bubbles of the reply whose Copy reply button is pointed
/// at, focused or showing its feedback, so the user sees how much it
/// copies. A rule of its own rather than a prop, so only this re-renders
/// when the pointer moves on or off a button.
#[component]
fn ReplyOutline(lit_reply: Signal<Option<i64>>) -> Element {
    match lit_reply() {
        Some(reply) => rsx! {
            style { ".message[data-reply=\"{reply}\"] {{ outline: 2px solid var(--accent); outline-offset: 2px; }}" }
        },
        None => rsx! {},
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
    reply_part: Option<&ReplyPart>,
    mut lit_reply: Signal<Option<i64>>,
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
        // A finished reply's last text holds its Copy reply button, and
        // every text the copy includes is marked with the reply, for the
        // outline the button shows (SME-105).
        ContentBlock::Text { text } if role == "assistant" => {
            let reply_attr = reply_part.filter(|part| part.blocks.contains(&index)).map(|part| part.reply.to_string());
            let copy = reply_part
                .and_then(|part| part.copy.clone().filter(|copy| copy.block_index == index).map(|copy| (part.reply, copy)));
            rsx! {
                div { key: "{key}", class: "message message-{role}", "data-reply": reply_attr,
                    div { class: "message-text message-markdown",
                        Markdown { source: text.clone(), on_media_load }
                    }
                    if let Some((reply, copy)) = copy {
                        div { class: "message-footer",
                            CopyButton {
                                label: reply_copy_label(copy.parts),
                                title: reply_copy_title(copy.parts),
                                text: copy.markdown,
                                class: "reply-copy",
                                on_highlight: move |on: bool| {
                                    if on {
                                        lit_reply.set(Some(reply));
                                    } else if *lit_reply.peek() == Some(reply) {
                                        lit_reply.set(None);
                                    }
                                },
                            }
                            span { class: "timestamp", "{timestamp}" }
                        }
                    } else {
                        span { class: "timestamp", "{timestamp}" }
                    }
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
    reply_part: Option<ReplyPart>,
    lit_reply: Signal<Option<i64>>,
) -> Element {
    match message.blocks() {
        Ok(blocks) => {
            let thinking_open = reply_is_only_thinking(&message.role, &blocks);
            let tool_names = tool_names.read();
            let tool_results = tool_results.read();
            let commands = commands.read();
            rsx! {
                for (i , block) in blocks.iter().enumerate() {
                    {render_block_element(message.id, i, &message.role, message.created_at, tz_offset_minutes, block, &tool_names, &tool_results, &commands, thinking_open, on_media_load, reply_part.as_ref(), lit_reply)}
                }
            }
        }
        Err(e) => rsx! {
            div { class: "message message-{message.role} message-error", "Error rendering message: {e}" }
        },
    }
}

/// Whether the transcript shows a new conversation's start screen (SME-41
/// D12): no messages, no turn running, not missing, nothing of the
/// conversation's own showing in its place, and its messages loaded with
/// none in them. Two frames used to flash it on a switch (SME-185 B38): the
/// messages resource still holding the conversation left until the new load
/// lands (so the load must be `selected`'s), and the new load landing one
/// render before the effect that copies it into the transcript (so the load
/// itself must be empty). It stays down while the conversation has an error
/// line (code review 1): a failed first send leaves it empty, idle and
/// loaded, and the screen — with its repo form and the example asks that
/// overwrite whatever is in the message box — doesn't belong under the line
/// saying the send didn't go.
pub(super) fn shows_start_screen(
    selected: Option<i64>,
    loaded: Option<&(i64, Result<Vec<Message>, ServerFnError>)>,
    no_messages: bool,
    turn_running: bool,
    missing: bool,
    error_showing: bool,
) -> bool {
    no_messages
        && !turn_running
        && !missing
        && !error_showing
        && matches!(loaded, Some((id, Ok(list))) if Some(*id) == selected && list.is_empty())
}

/// The transcript: the conversation's messages, a new conversation's
/// first steps, the reply as it streams, the working line, errors, trust
/// cards and the model's waiting question. Scrolling it keeps the panel's stick-to-bottom and
/// pointer-hold state (SME-83, SME-75) current.
#[component]
pub(super) fn Transcript(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    initial_messages: Resource<Option<(i64, Result<Vec<Message>, ServerFnError>)>>,
    tz_offset_minutes: Signal<i32>,
    mut input: Signal<String>,
    on_attach: EventHandler<()>,
    stream_errors: Signal<super::state::ChatErrors>,
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
    // Recomputed when a message arrives or the turn starts or ends, not per
    // streamed delta. Each message gets only its own part, so a moved
    // button re-renders just the messages it moved between (SME-105).
    let copies = use_memo(move || reply_parts(&messages.read(), turn_running()));
    // The reply whose Copy reply button is pointed at, focused or showing
    // its feedback: its bubbles are outlined.
    let mut lit_reply = use_signal(|| None::<i64>);
    // A lit reply whose button is gone (another conversation opened, or the
    // turn started again) isn't lit any more: the button can't say so
    // itself once it's unmounted (code review 1).
    use_effect(move || {
        let lit = *lit_reply.peek();
        let has_button = |reply: i64| copies.read().get(&reply).is_some_and(|part| part.copy.is_some());
        // Subscribe to the parts even when nothing is lit.
        let _ = copies.read();
        if let Some(reply) = lit
            && !has_button(reply)
        {
            lit_reply.set(None);
        }
    });
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
                    reply_part: copies.read().get(&message.id).cloned(),
                    message,
                    tz_offset_minutes: tz_offset_minutes(),
                    tool_names,
                    tool_results,
                    commands,
                    on_media_load,
                    lit_reply,
                }
            }
            ReplyOutline { lit_reply }
            // A new conversation says what smelt does and offers a
            // few asks to start from, instead of a blank screen
            // (SME-41 D12). Picking one fills the message box.
            if shows_start_screen(
                selected(),
                initial_messages.read().as_ref().and_then(|loaded| loaded.as_ref()),
                messages().is_empty(),
                turn_running(),
                conversation_missing(),
                selected().is_some_and(|id| stream_errors.read().lines.contains_key(&id)),
            ) {
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

mod text;

pub(in crate::frontend::pages::chat) use text::*;
