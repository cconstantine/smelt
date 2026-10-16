//! The transcript's follow (SME-163: ChatPanel's own effects, moved here
//! whole): new content and layout changes keep it at its bottom while the
//! user is there, without moving what's under the pointer.

use super::*;

/// Snaps the transcript to its new bottom on new content and on a layout
/// change, while it follows; see each effect.
pub(super) fn use_transcript_follow(
    state: Store<ConversationState>,
    transcript_scroll: StickyBottom,
    pointer_over_transcript: Signal<bool>,
    media_loaded: Signal<u64>,
) {
    let messages = state.messages();
    let streaming_reply = state.streaming_reply();
    let mut layout_snap_pending = state.layout_snap_pending();
    let sandbox_pods = state.sandbox_pods();
    let sandbox_terminals = state.sandbox_terminals();
    let repos = state.repos();
    let todos = state.todos();
    let browsing_session_open = state.browsing_session_open();
    let context_usage = state.context_usage();
    // What the transcript held when the effect below last ran: the message
    // count, the last message's id and the streaming reply's length. Only
    // a change in it is new content; a write that changes nothing (the
    // connect-time pull sets the messages again) is not.
    let mut last_content: Signal<Option<(usize, Option<i64>, Option<usize>)>> = use_signal(|| None);

    // Auto-scroll the transcript to its new bottom whenever a message is
    // added or streaming text grows — but only if the user was already at
    // the bottom (`transcript_scroll`, kept current by the transcript's own
    // `onscroll` handler). Reads the messages and the streaming reply so it
    // reruns on both a persisted message and an in-flight delta; the stuck
    // flag isn't subscribed to (see `StickyBottom::is_stuck`).
    use_effect(move || {
        // Borrowed, not cloned: this reruns on every streamed delta.
        let content = {
            let list = messages.read();
            let reply_len = streaming_reply.read().as_ref().map(String::len);
            (list.len(), list.last().map(|m| m.id), reply_len)
        };
        let changed = *last_content.peek() != Some(content);
        last_content.set(Some(content));
        if !transcript_scroll.is_stuck() {
            return;
        }
        let Some(el) = transcript_scroll.el() else { return };
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
        // An image in a reply loaded and grew it (SME-30).
        let _ = media_loaded();
        if !transcript_scroll.is_stuck() {
            return;
        }
        let Some(el) = transcript_scroll.el_untracked() else { return };
        if *pointer_over_transcript.peek() {
            layout_snap_pending.set(true);
            spawn(keep_transcript_anchor(el, layout_snap_pending));
            return;
        }
        spawn(scroll_to_bottom(el));
    });
}
