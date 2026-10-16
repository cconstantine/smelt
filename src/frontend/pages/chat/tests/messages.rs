use super::*;

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

/// SME-185 B36: a send that failed takes its copy back, so sending it
/// again shows the message once, not twice.
#[test]
fn test_a_resend_after_a_failed_send_shows_the_message_once() {
    let mut shown = vec![user_text(1, "earlier"), user_text(-1, "hello once")];
    withdraw_unsent(&mut shown, -1);
    shown.push(user_text(-2, "hello once"));
    accept_saved_messages(&mut shown, vec![user_text(5, "hello once")]);
    assert_eq!(ids(&shown), vec![1, 5], "one copy of the resent message, the saved one");
}

/// SME-185 B36: the response was lost after the server saved the message,
/// and the saved copy has already replaced the optimistic one. The
/// withdraw goes by id, so it leaves the saved copy alone.
#[test]
fn test_withdrawing_a_send_leaves_its_saved_copy() {
    let mut shown = vec![user_text(-1, "hi"), user_text(-2, "hi")];
    accept_saved_messages(&mut shown, vec![user_text(5, "hi")]);
    assert_eq!(ids(&shown), vec![-2, 5]);
    withdraw_unsent(&mut shown, -1);
    assert_eq!(ids(&shown), vec![-2, 5], "nothing with id -1 left; the in-flight -2 and the saved 5 stay");
}

/// SME-185 B36: the sent text goes back into an empty box, never over
/// something typed since.
#[test]
fn test_a_failed_send_gives_its_text_back_only_to_an_empty_box() {
    assert_eq!(draft_after_failed_send("", "hi").as_deref(), Some("hi"));
    assert_eq!(draft_after_failed_send("  \n", "hi").as_deref(), Some("hi"));
    assert_eq!(draft_after_failed_send("new text", "hi"), None);
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

/// Code review 1 (SME-34): a refused `ask_user` call shows its error
/// as a failed tool row, not as "you wrote a message instead".
#[test]
fn test_question_call_tells_waiting_answered_and_refused_apart() {
    assert_eq!(question_call(None), QuestionCall::Waiting);
    let answered = ("The user answered:\n1. Delete: No".to_string(), false);
    assert_eq!(question_call(Some(&answered)), QuestionCall::Answered(vec!["1. Delete: No".to_string()]));
    let dismissed = ("The user didn't answer these questions. Their message follows.".to_string(), false);
    assert_eq!(
        question_call(Some(&dismissed)),
        QuestionCall::Answered(vec!["Not answered: you wrote a message instead.".to_string()])
    );
    let refused = ("ask 1 to 4 questions in one call (got 0)".to_string(), true);
    assert_eq!(question_call(Some(&refused)), QuestionCall::Refused);
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

fn loaded(id: i64, ok: bool) -> (i64, Result<Vec<Message>, ServerFnError>) {
    (id, if ok { Ok(Vec::new()) } else { Err(ServerFnError::new("boom")) })
}

/// SME-185 B38: switching from 1 to 2, the messages resource still holds
/// 1's load until 2's lands. That's no reason for 2's start screen.
#[test]
fn test_the_start_screen_waits_for_the_open_conversations_own_load() {
    assert!(
        !shows_start_screen(Some(2), Some(&loaded(1, true)), true, false, false),
        "the conversation left's load showed the start screen"
    );
    assert!(!shows_start_screen(Some(2), None, true, false, false), "nothing loaded yet");
}

/// SME-185 B38: the open conversation's own load has landed, but the
/// effect that copies it into the transcript runs after this render. A
/// load with messages in it is no empty conversation, even for that frame.
#[test]
fn test_the_start_screen_waits_for_a_loaded_list_to_reach_the_transcript() {
    let with_a_message = (2, Ok(vec![user_text(5, "hello")]));
    assert!(
        !shows_start_screen(Some(2), Some(&with_a_message), true, false, false),
        "a load holding a message showed the start screen before the transcript had it"
    );
}

#[test]
fn test_the_start_screen_shows_only_for_an_empty_idle_loaded_conversation() {
    assert!(shows_start_screen(Some(2), Some(&loaded(2, true)), true, false, false));
    assert!(!shows_start_screen(Some(2), Some(&loaded(2, true)), false, false, false), "it has messages");
    assert!(!shows_start_screen(Some(2), Some(&loaded(2, true)), true, true, false), "a turn is running");
    assert!(!shows_start_screen(Some(2), Some(&loaded(2, true)), true, false, true), "it's missing");
    assert!(!shows_start_screen(Some(2), Some(&loaded(2, false)), true, false, false), "its load failed");
}
