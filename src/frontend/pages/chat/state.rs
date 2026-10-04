//! The chat page's per-conversation state, and the pure merges onto it:
//! loaded and live messages, the streaming reply, and the sandbox panel's
//! entries.

use super::*;

/// Everything the chat panel shows that belongs to the open conversation.
/// Switching conversations replaces it with `ConversationState::default()`
/// in one write, so nothing from the conversation left behind can show in
/// the next one (SME-51 B11; SME-57). Each component reads only the fields
/// it shows, through the store's field lenses, so a write to one field
/// (a streamed delta, say) re-renders only what reads that field.
///
/// What isn't here outlives a switch on purpose: the message box's draft,
/// the turn errors (kept by conversation), the model picker's state, the
/// time zone, and the transcript's own scroll bookkeeping.
#[derive(Store, Default)]
pub(super) struct ConversationState {
    /// The conversation this state is about: set by the switch's reset,
    /// and `None` while no conversation is open.
    /// An effect that acts on a field for `selected` checks it first: on a
    /// switch, effects rerun in no set order, and one can run before the
    /// reset, while the fields still hold the previous conversation's.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(super) conversation: Option<i64>,
    pub(super) messages: Vec<Message>,
    /// The load's error; "conversation not found" means it was deleted.
    pub(super) load_error: Option<String>,
    /// The reply as it streams: `ReplyReset` starts it, `ReplyDelta` adds
    /// to it, and the reply being saved or the turn ending clears it.
    /// Restored on (re)connect from `get_reply_in_progress`; every tab
    /// watching the conversation shows it, whoever started the turn.
    pub(super) streaming_reply: Option<String>,
    /// Whether the server has a turn running (or queued) here, whoever
    /// started it (`ConversationEvent::TurnState`).
    pub(super) turn_running: bool,
    /// Seconds this tab has seen the current turn running, for the
    /// "Working…" line (SME-41 D1).
    pub(super) turn_elapsed: u64,
    /// A background wake-up that failed to reach the model
    /// (`ConversationEvent::NotificationDeliveryFailed`). Kept apart from
    /// the turn errors, which a send clears: this can arrive at any time.
    pub(super) notification_delivery_error: Option<String>,
    pub(super) todos: Vec<TodoItem>,
    /// The question this conversation waits on, for its card (SME-34).
    pub(super) pending_question: Option<PendingQuestion>,
    /// "Work on a repo" in a new conversation: the form, a clone in flight,
    /// and its error.
    pub(super) repo_url: String,
    pub(super) repo_branch: String,
    pub(super) repo_dir: String,
    pub(super) repo_attaching: bool,
    pub(super) repo_attach_error: Option<String>,
    /// The last Trust / Don't trust / Reload failure.
    pub(super) repo_action_error: Option<String>,
    /// The conversation's git repos (SME-32), from `ReposUpdate`.
    pub(super) repos: Vec<RepoSummary>,
    pub(super) sandbox_pods: Vec<SandboxPodPanelEntry>,
    pub(super) sandbox_terminals: Vec<SandboxTerminalPanelEntry>,
    /// The sandbox panel's Stop button: the pod armed for stopping (click
    /// once to arm, again to confirm), and the last stop's error.
    pub(super) pending_pod_stop: Option<i64>,
    pub(super) pod_stop_error: Option<String>,
    /// Whether the model has a browsing session open (it shows the panel
    /// and runs the frame subscription), the page's URL, and the latest
    /// frame (base64 JPEG, `None` until the first one arrives).
    pub(super) browsing_session_open: bool,
    pub(super) browsing_url: Option<String>,
    pub(super) browsing_frame: Option<String>,
    /// The address bar: what's typed, whether the viewer is typing (so an
    /// incoming URL doesn't clobber it), a navigation in flight, and the
    /// last navigation's error.
    pub(super) address_draft: String,
    pub(super) address_editing: bool,
    pub(super) address_pending: bool,
    pub(super) address_error: Option<String>,
    /// `None` until the first `ContextUsageUpdate`/`get_context_usage`
    /// pull: a new conversation has no turn yet to report on (SME-18).
    pub(super) context_usage: Option<ContextUsageSnapshot>,
    /// The context bar's detail view, fetched when it opens.
    pub(super) context_detail: Option<ContextDetailSnapshot>,
    pub(super) context_detail_open: bool,
    /// A snap to the bottom that a layout change asked for, waiting for the
    /// pointer to leave the transcript (SME-75).
    pub(super) layout_snap_pending: bool,
    /// Set while this tab is live on the conversation (subscribed, and done
    /// with the pull that follows): the id, and how many times it has
    /// connected and pulled, more than once meaning a reconnect. Shown as
    /// `data-live`/`data-live-pulls`, which the browser tests wait for
    /// (SME-59).
    pub(super) live: Option<(i64, u32)>,
}

/// Runs `change` on the sandbox panel's pods and terminals together. They're
/// two fields of one store, and the store lends out one write at a time
/// (a second, while the first is held, panics), so the terminals are taken
/// out while the pods are borrowed and put back after.
#[cfg(any(feature = "web", test))]
pub(super) fn change_sandbox_panel<R>(
    state: Store<ConversationState>,
    change: impl FnOnce(&mut Vec<SandboxPodPanelEntry>, &mut Vec<SandboxTerminalPanelEntry>) -> R,
) -> R {
    let mut terminals = std::mem::take(&mut *state.sandbox_terminals().write());
    let result = change(&mut state.sandbox_pods().write(), &mut terminals);
    state.sandbox_terminals().set(terminals);
    result
}

/// What an event asks of the page beyond the open conversation's state,
/// which `apply_event` has already updated: a counter for the sidebar or
/// the model picker, a turn error (kept by conversation, outside the
/// state), or the bundle being out of date.
#[cfg(any(feature = "web", test))]
#[derive(Debug, PartialEq)]
pub(super) enum EventEffect {
    None,
    ConversationsChanged,
    PodsChanged,
    TurnsChanged,
    QuestionsChanged,
    /// The conversation's model or the providers changed: the model picker
    /// refetches, and so does the context meter, which measures against
    /// the model's window.
    ModelChanged,
    TurnError(String),
    /// A type added since this page loaded: the server is newer than this
    /// bundle.
    StaleBundle,
}

/// Applies one live event from the open conversation's stream onto its
/// state, and says what else it asks for.
#[cfg(any(feature = "web", test))]
pub(super) fn apply_event(state: Store<ConversationState>, event: ConversationEvent) -> EventEffect {
    match event {
        ConversationEvent::MessagesAppended { messages: rows } => {
            // A saved reply replaces its streaming copy.
            if rows.iter().any(|m| m.role == "assistant") {
                state.streaming_reply().set(None);
            }
            accept_saved_messages(&mut state.messages().write(), rows);
            return EventEffect::ConversationsChanged;
        }
        ConversationEvent::SandboxPodUpdate { pod_id, status, terminated } => {
            change_sandbox_panel(state, |pods, terminals| {
                apply_sandbox_pod_update(pods, terminals, pod_id, status, terminated)
            });
        }
        ConversationEvent::SandboxPreviewUpdate { pod_id, previews } => {
            apply_sandbox_preview_update(&mut state.sandbox_pods().write(), pod_id, previews);
        }
        ConversationEvent::SandboxTerminalUpdate { pod_id, terminal_id, status, terminated } => {
            apply_sandbox_terminal_update(&mut state.sandbox_terminals().write(), pod_id, terminal_id, status, terminated);
        }
        ConversationEvent::SandboxCommandUpdate {
            terminal_id,
            command_id,
            command,
            status,
            exit_code,
            stream,
            latest_output,
            position,
        } => {
            apply_sandbox_command_update(
                &mut state.sandbox_terminals().write(),
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
        ConversationEvent::NotificationDeliveryFailed { detail } => {
            state.notification_delivery_error().set(Some(detail));
        }
        ConversationEvent::ContextUsageUpdate { usage, context_window } => {
            state.context_usage().set(Some(ContextUsageSnapshot { usage: Some(usage), context_window }));
        }
        ConversationEvent::TodoListUpdate { items } => state.todos().set(items),
        ConversationEvent::BrowsingSessionUpdate { open } => {
            state.browsing_session_open().set(open);
            if !open {
                state.browsing_frame().set(None);
                state.browsing_url().set(None);
            }
        }
        ConversationEvent::BrowsingUrlUpdate { url } => state.browsing_url().set(Some(url)),
        ConversationEvent::ReposUpdate { repos } => state.repos().set(repos),
        ConversationEvent::QuestionUpdate { question } => state.pending_question().set(question),
        ConversationEvent::TurnState { running } => {
            state.turn_running().set(running);
            if !running {
                state.streaming_reply().set(None);
            }
        }
        ConversationEvent::ReplyReset {} => state.streaming_reply().set(Some(String::new())),
        ConversationEvent::ReplyDelta { text, offset } => {
            apply_reply_delta(&mut state.streaming_reply().write(), offset, &text);
        }
        // A stop shows as its saved notice instead.
        ConversationEvent::TurnError { message } if message != crate::api::chat::TURN_STOPPED => {
            return EventEffect::TurnError(message);
        }
        ConversationEvent::TurnError { .. } => {}
        ConversationEvent::PodsChanged {} => return EventEffect::PodsChanged,
        ConversationEvent::TurnsChanged {} => return EventEffect::TurnsChanged,
        ConversationEvent::QuestionsChanged {} => return EventEffect::QuestionsChanged,
        ConversationEvent::ModelChanged {} | ConversationEvent::ProvidersChanged {} => {
            return EventEffect::ModelChanged;
        }
        ConversationEvent::Unknown => return EventEffect::StaleBundle,
        // Only the browser tier's server sends it, to a page that should
        // take it for a type it doesn't know.
        #[cfg(feature = "browser-test")]
        ConversationEvent::BrowserTestAddedLater {} => return EventEffect::StaleBundle,
    }
    EventEffect::None
}

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
pub(super) fn apply_loaded_messages(
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
pub(super) fn accept_saved_messages(existing: &mut Vec<Message>, incoming: Vec<Message>) {
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
pub(super) fn sent_part(message: &Message) -> Result<Vec<ContentBlock>, String> {
    match message.blocks() {
        Ok(blocks) => Ok(blocks
            .into_iter()
            .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
            .collect()),
        Err(_) => Err(message.content.clone()),
    }
}

#[cfg(any(feature = "web", test))]
pub(super) fn merge_messages_by_id(existing: &mut Vec<Message>, incoming: Vec<Message>) {
    for message in incoming {
        if !existing.iter().any(|m| m.id == message.id) {
            existing.push(message);
        }
    }
}

/// Adds a streamed reply's `text`, which starts `offset` bytes in. A tab
/// that connected mid-reply fetched the text so far and then receives the
/// deltas published since it subscribed, some already in that text; only
/// the part past what it has is added (SME-51 B3).
#[cfg(any(feature = "web", test))]
pub(super) fn apply_reply_delta(reply: &mut Option<String>, offset: usize, text: &str) {
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
pub(super) struct SandboxPodPanelEntry {
    pub(super) pod_id: i64,
    pub(super) status: String,
    /// Preview links for the user's own browser (SME-42).
    pub(super) previews: Vec<SandboxPreview>,
}

/// One output line's widget state — same shape as the wire `SandboxOutputLine`,
/// kept as its own type for the same reason every other panel entry mirrors
/// rather than reuses its wire counterpart (see `SandboxPodPanelEntry` vs.
/// `SandboxPodSummary`).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SandboxOutputLinePanelEntry {
    pub(super) stream: String,
    pub(super) data: String,
    /// The agent's per-command `seq`, when known, so a reconnect doesn't
    /// add a line twice (SME-51 B3).
    pub(super) seq: Option<i64>,
}

/// One command's widget state within a terminal's history. `output` is a
/// single sequence in true chronological order (each line tagged with which stream it came
/// from) — a real terminal interleaves the two as they happen, and a panel
/// that rendered them as two separate blocks would show "all stdout, then
/// all stderr" regardless of when anything was actually written.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SandboxCommandPanelEntry {
    pub(super) command_id: String,
    pub(super) command: String,
    pub(super) status: String,
    pub(super) exit_code: Option<i32>,
    pub(super) output: Vec<SandboxOutputLinePanelEntry>,
}

/// One terminal's widget state — a real terminal's scrollback, not just its
/// current command: every command run in it (bounded, oldest first — see
/// `SandboxTerminalSummary`), each with its own output, so the panel reads
/// like the terminal's actual history rather than only ever showing the
/// latest line.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SandboxTerminalPanelEntry {
    pub(super) terminal_id: i64,
    pub(super) pod_id: i64,
    pub(super) status: String,
    pub(super) commands: Vec<SandboxCommandPanelEntry>,
}

/// Applies one `get_sandbox_state` snapshot onto the panel's current pods
/// and terminals — the snapshot is authoritative, flattened from its
/// pod→terminal nesting into the two separate flat lists the panel renders
/// from. A pod or terminal the snapshot leaves out is gone: it went away
/// while the tab wasn't listening (SME-43).
#[cfg(any(feature = "web", test))]
pub(super) fn merge_sandbox_snapshot(
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
pub(super) fn apply_sandbox_pod_update(
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
pub(super) fn apply_sandbox_preview_update(pods: &mut [SandboxPodPanelEntry], pod_id: i64, previews: Vec<SandboxPreview>) {
    if let Some(entry) = pods.iter_mut().find(|p| p.pod_id == pod_id) {
        entry.previews = previews;
    }
}

/// Applies one live `SandboxTerminalUpdate` — same upsert-or-remove shape
/// as `apply_sandbox_pod_update`.
#[cfg(any(feature = "web", test))]
pub(super) fn apply_sandbox_terminal_update(
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
pub(super) fn apply_sandbox_command_update(
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
