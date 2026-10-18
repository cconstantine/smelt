//! What ChatPanel's buttons and forms do (SME-163: its own closures, moved
//! here whole). Each function returns the closure ChatPanel used to build
//! in place; ChatPanel still calls it, so each request is spawned in
//! ChatPanel's own scope and outlives the panel that started it.

use super::*;

/// The composer's Stop button.
pub(super) fn stop_action(selected: Memo<Option<i64>>, stream_errors: Signal<ChatErrors>) -> impl FnMut(MouseEvent) + 'static {
    let mut stream_errors = stream_errors;
    move |_: MouseEvent| {
        let Some(id) = selected() else { return };
        // "Stopped." comes from the notice the stop saves, in every tab and
        // after a reload (SME-51 B10); a stop that fails says so (B11).
        spawn(async move {
            if let Err(e) = crate::api::chat::stop_turn(id).await
                && selected() == Some(id)
            {
                stream_errors
                    .write()
                    .set_local_error(id, format!("Couldn't stop the turn: {}", server_error_message(&e)));
            }
        });
    }
}

/// Clones the repo typed into a new conversation's "Work on a repo".
/// Here, not in `RepoAttach`: the form goes as soon as the first message
/// shows, which would drop a clone request still in flight with its
/// reset and its error (SME-57 code review).
pub(super) fn attach_action(selected: Memo<Option<i64>>, state: Store<ConversationState>) -> impl FnMut() + 'static {
    let mut repo_url = state.repo_url();
    let mut repo_branch = state.repo_branch();
    let mut repo_dir = state.repo_dir();
    let mut repo_attaching = state.repo_attaching();
    let mut repo_attach_error = state.repo_attach_error();
    move || {
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
    }
}

/// The sandbox panel's Stop button: click once to arm, again to stop.
/// Here, not in `SandboxPanel`: the panel unmounts once the pod is gone
/// and nothing else shows, which would drop the stop's task before it
/// clears or sets the stop error (SME-57 code review).
pub(super) fn pod_stop_action(selected: Memo<Option<i64>>, state: Store<ConversationState>) -> impl FnMut(i64) + 'static {
    let mut pending_pod_stop = state.pending_pod_stop();
    let mut pod_stop_error = state.pod_stop_error();
    move |pod_id: i64| {
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
    }
}

/// Goes to the address typed in the browsing panel's address bar.
/// Spawned here, not in `BrowsingPanel`: the panel unmounts when the
/// session closes, which would drop a navigation still loading and
/// leave the address bar disabled (SME-57 code review).
pub(super) fn navigate_action(selected: Memo<Option<i64>>, state: Store<ConversationState>) -> impl FnMut() + 'static {
    let address_draft = state.address_draft();
    let mut address_editing = state.address_editing();
    let mut address_pending = state.address_pending();
    let mut address_error = state.address_error();
    move || {
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
    }
}

/// Sends the message box's text: it shows at once under a temporary id,
/// and the turn's messages arrive on the conversation's event stream.
pub(super) fn send_action(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    input: Signal<String>,
    next_temp_id: Signal<i64>,
    model_ready: Signal<bool>,
    stream_errors: Signal<ChatErrors>,
    conversations_changed: Signal<u64>,
) -> impl FnMut() + 'static {
    let mut messages = state.messages();
    let mut input = input;
    let mut next_temp_id = next_temp_id;
    let mut stream_errors = stream_errors;
    let mut conversations_changed = conversations_changed;
    move || {
        let Some(id) = selected() else { return };
        let content = input();
        if content.trim().is_empty() || !model_ready() {
            return;
        }
        input.set(String::new());

        let temp_id = next_temp_id();
        next_temp_id.set(temp_id - 1);
        #[expect(clippy::expect_used, reason = "a text ContentBlock always serializes")]
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
            stream_errors.write().clear(id);
            if let Err(e) = send_message(id, content.clone()).await {
                // Not sent (SME-185 B36): the copy goes, so a resend shows
                // the message once, and its text goes back in the box if
                // the viewer is still here and hasn't typed anything since
                // (`input` outlives a switch, so on another conversation it
                // would land in the wrong box). After a switch the store was
                // reset and holds no `temp_id`, so the withdraw is a no-op.
                withdraw_unsent(&mut messages.write(), temp_id);
                // `next_temp_id` only moves on a send, so this send is still
                // the latest exactly when the counter sits where it left it.
                // Otherwise the viewer sent again while this one was in
                // flight, its own send is what emptied the box, and restoring
                // would replace that message with this one (code review 1).
                let another_send_since = *next_temp_id.peek() != temp_id - 1;
                let restored = draft_after_failed_send(&input.peek(), &content, another_send_since);
                if *selected.peek() == Some(id)
                    && let Some(draft) = restored
                {
                    input.set(draft);
                }
                stream_errors
                    .write()
                    .set_local_error(id, format!("Couldn't send the message: {}", server_error_message(&e)));
            }
            // The first message titles a conversation, and any send moves
            // it up the list — refresh the sidebar even if the viewer has
            // switched away (this tab then isn't listening to `id`'s events).
            *conversations_changed.write() += 1;
        });
    }
}

/// Takes a failed send's optimistic copy back off the transcript (SME-185
/// B36), by its temporary id. By id, not by text: another copy with the
/// same text may be a send still in flight, and a saved copy that already
/// replaced this one (the response was lost after the server saved it)
/// stays.
pub(super) fn withdraw_unsent(messages: &mut Vec<Message>, temp_id: i64) {
    messages.retain(|m| m.id != temp_id);
}

/// What the message box should hold after a send of `sent` failed, given
/// what it holds now and whether anything was sent since (SME-185 B36): the
/// sent text back when the box is empty, and nothing when the viewer has
/// typed something new since, or sent again since — a second send clears the
/// box too, and restoring would then swap the older message in for the one
/// the viewer meant to send (code review 1).
pub(super) fn draft_after_failed_send(draft: &str, sent: &str, another_send_since: bool) -> Option<String> {
    (!another_send_since && draft.trim().is_empty()).then(|| sent.to_string())
}
