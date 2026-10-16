use super::*;

// --- Copying a reply (SME-105) ---

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.to_string() }
}

fn call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolUse { id: id.to_string(), name: name.to_string(), input: serde_json::json!({"secret": "tool input"}) }
}

fn result(id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult { tool_use_id: id.to_string(), content: content.to_string(), is_error: None }
}

/// The reply's button: (message id, block index, markdown, parts).
fn buttons(parts: &HashMap<i64, ReplyPart>) -> Vec<(i64, usize, String, usize)> {
    let mut buttons: Vec<_> = parts
        .iter()
        .filter_map(|(id, part)| part.copy.as_ref().map(|c| (*id, c.block_index, c.markdown.to_string(), c.parts)))
        .collect();
    buttons.sort();
    buttons
}

#[test]
fn test_reply_parts_one_reply_copies_its_text_verbatim() {
    let messages = vec![
        user_text(1, "hi"),
        message_with_blocks(2, "assistant", vec![text("Hello **there**.\n")]),
    ];
    let parts = reply_parts(&messages, false);
    assert_eq!(buttons(&parts), vec![(2, 0, "Hello **there**.".to_string(), 1)]);
    assert_eq!(parts.get(&2).map(|p| (p.reply, p.blocks.clone())), Some((2, vec![0])));
}

#[test]
fn test_reply_parts_joins_the_text_around_tool_calls_and_leaves_the_calls_out() {
    let messages = vec![
        user_text(1, "check the todos"),
        message_with_blocks(2, "assistant", vec![text("Let me check."), call("t1", "todoread")]),
        message_with_blocks(3, "user", vec![result("t1", "secret tool result")]),
        message_with_blocks(4, "assistant", vec![text("## Done\n\nAll clear.")]),
    ];
    let parts = reply_parts(&messages, false);
    assert_eq!(buttons(&parts), vec![(4, 0, "Let me check.\n\n## Done\n\nAll clear.".to_string(), 2)]);
    assert_eq!(parts.get(&2).map(|p| (p.reply, p.blocks.clone(), p.copy.is_none())), Some((4, vec![0], true)));
    assert!(!parts.contains_key(&3), "the tool result isn't part of the copy");
}

#[test]
fn test_reply_parts_leave_out_thinking_and_compaction_which_does_not_split_a_reply() {
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![
            ContentBlock::Thinking { thinking: "secret thought".into(), signature: "s".into() },
            text("First."),
            call("t1", "read_file"),
        ]),
        message_with_blocks(3, "user", vec![result("t1", "ok")]),
        message_with_blocks(4, "user", vec![ContentBlock::CompactionPlaceholder { text: "placeholder".into() }]),
        message_with_blocks(5, "assistant", vec![ContentBlock::CompactionSummary { summary: "secret summary".into(), covers_through_message_id: 3 }]),
        message_with_blocks(6, "user", vec![ContentBlock::CompactionPlaceholder { text: "continue".into() }]),
        message_with_blocks(7, "assistant", vec![text("Second.")]),
    ];
    let parts = reply_parts(&messages, false);
    assert_eq!(buttons(&parts), vec![(7, 0, "First.\n\nSecond.".to_string(), 2)]);
    assert_eq!(parts.get(&2).map(|p| p.blocks.clone()), Some(vec![1]), "only the text block, not the thinking");
}

#[test]
fn test_reply_parts_a_notice_or_an_ask_user_answer_starts_a_new_reply() {
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("One."), call("q1", ASK_USER)]),
        message_with_blocks(3, "user", vec![result("q1", "The user answered:\n1. Yes")]),
        message_with_blocks(4, "assistant", vec![text("Two.")]),
        user_text(5, crate::api::chat::STOP_NOTICE),
        message_with_blocks(6, "assistant", vec![text("Three.")]),
    ];
    let parts = reply_parts(&messages, false);
    assert_eq!(
        buttons(&parts),
        vec![(2, 0, "One.".to_string(), 1), (4, 0, "Two.".to_string(), 1), (6, 0, "Three.".to_string(), 1)]
    );
}

#[test]
fn test_reply_parts_a_reply_without_text_has_no_button() {
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![ContentBlock::Thinking { thinking: "hm".into(), signature: "s".into() }]),
        user_text(3, "and?"),
        message_with_blocks(4, "assistant", vec![text("  \n\n "), call("t1", "todoread")]),
        message_with_blocks(5, "user", vec![result("t1", "ok")]),
    ];
    assert!(reply_parts(&messages, false).is_empty());
}

#[test]
fn test_reply_parts_the_button_sits_on_the_text_before_a_trailing_tool_call() {
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("Before the call."), call("t1", "todoread")]),
    ];
    assert_eq!(buttons(&reply_parts(&messages, false)), vec![(2, 0, "Before the call.".to_string(), 1)]);
}

#[test]
fn test_reply_parts_skip_whitespace_only_blocks_in_a_reply() {
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("A."), text("\n \n"), text("B.")]),
    ];
    let parts = reply_parts(&messages, false);
    assert_eq!(buttons(&parts), vec![(2, 2, "A.\n\nB.".to_string(), 2)]);
    assert_eq!(parts.get(&2).map(|p| p.blocks.clone()), Some(vec![0, 2]));
}

#[test]
fn test_reply_parts_while_the_turn_runs_only_the_newest_reply_has_none() {
    let messages = vec![
        user_text(1, "first"),
        message_with_blocks(2, "assistant", vec![text("Answer one.")]),
        user_text(3, "second"),
        message_with_blocks(4, "assistant", vec![text("Partway"), call("t1", "todoread")]),
        message_with_blocks(5, "user", vec![result("t1", "ok")]),
    ];
    let parts = reply_parts(&messages, true);
    assert_eq!(buttons(&parts), vec![(2, 0, "Answer one.".to_string(), 1)]);
    assert!(!parts.contains_key(&4), "the running reply isn't outlined either");
    // An optimistic message (negative id) is the user's: the reply before
    // it is finished, and the turn it starts has no text yet.
    let mut sent = messages.clone();
    sent.truncate(2);
    sent.push(user_text(-1, "another"));
    assert_eq!(buttons(&reply_parts(&sent, true)), vec![(2, 0, "Answer one.".to_string(), 1)]);
}

#[test]
fn test_reply_parts_skip_an_unreadable_message() {
    let mut broken = test_message(3);
    broken.role = "assistant".into();
    broken.content = "not json".into();
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("Fine.")]),
        broken,
    ];
    assert_eq!(buttons(&reply_parts(&messages, false)), vec![(2, 0, "Fine.".to_string(), 1)]);
}

#[test]
fn test_reply_parts_copy_markdown_byte_for_byte() {
    let reply = "\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n\n<img src=x onerror=\"y\">  trailing spaces  \n\n";
    let messages = vec![user_text(1, "go"), message_with_blocks(2, "assistant", vec![text(reply)])];
    assert_eq!(
        buttons(&reply_parts(&messages, false)),
        vec![(2, 0, reply.trim_matches('\n').to_string(), 1)]
    );
}

#[test]
fn test_reply_parts_a_command_notice_inside_a_running_turn_does_not_split_its_reply() {
    // A command that finishes while the turn is still in its tool loop is
    // saved as a notice right after the tool results, and the turn goes on
    // (`drain_unnotified_terminal_commands`). Code review 1, M1.
    let messages = vec![
        user_text(1, "run the tests"),
        message_with_blocks(2, "assistant", vec![text("Running the tests."), call("t1", "run_terminal_command")]),
        message_with_blocks(3, "user", vec![result("t1", "command sent (id: c1)")]),
        user_text(4, "Terminal command c1 finished: exit code 0."),
        message_with_blocks(5, "assistant", vec![text("All tests pass.")]),
    ];
    assert_eq!(
        buttons(&reply_parts(&messages, false)),
        vec![(5, 0, "Running the tests.\n\nAll tests pass.".to_string(), 2)]
    );
    assert!(reply_parts(&messages[..4], true).is_empty(), "no button on a reply whose turn still runs");
}

#[test]
fn test_reply_parts_a_notice_that_starts_a_turn_still_starts_a_reply() {
    let messages = vec![
        user_text(1, "start the build"),
        message_with_blocks(2, "assistant", vec![text("Started it.")]),
        user_text(3, "Terminal command c1 finished: exit code 0."),
        message_with_blocks(4, "assistant", vec![text("The build finished.")]),
    ];
    assert_eq!(
        buttons(&reply_parts(&messages, false)),
        vec![(2, 0, "Started it.".to_string(), 1), (4, 0, "The build finished.".to_string(), 1)]
    );
}

#[test]
fn test_reply_parts_the_users_own_message_inside_a_tool_loop_still_starts_a_reply() {
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("Working."), call("t1", "todoread")]),
        message_with_blocks(3, "user", vec![result("t1", "[]")]),
        user_text(4, "also check the docs"),
        message_with_blocks(5, "assistant", vec![text("Checked them.")]),
    ];
    assert_eq!(
        buttons(&reply_parts(&messages, false)),
        vec![(2, 0, "Working.".to_string(), 1), (5, 0, "Checked them.".to_string(), 1)]
    );
}

#[test]
fn test_reply_parts_a_refused_ask_user_call_does_not_split_a_reply() {
    // A malformed or second `ask_user` gets an error result in the
    // ordinary tool results, and the turn goes on. Code review 1, L1.
    let refused = ContentBlock::ToolResult { tool_use_id: "q1".into(), content: "bad input".into(), is_error: Some(true) };
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("Let me ask."), call("q1", ASK_USER)]),
        message_with_blocks(3, "user", vec![refused]),
        message_with_blocks(4, "assistant", vec![text("Retrying.")]),
    ];
    assert_eq!(buttons(&reply_parts(&messages, false)), vec![(4, 0, "Let me ask.\n\nRetrying.".to_string(), 2)]);
}

#[test]
fn test_reply_parts_a_cut_off_or_stop_after_tool_results_ends_the_reply() {
    // A turn cut off mid-call saves the call's "not run" results, then the
    // cut-off notice, and ends; a later turn (a command finishing wakes
    // the model) is a new reply. Code review 2, L3.
    let cut_off = crate::api::chat::cut_off_notice(1024, crate::api::chat::ReplyLimit::ALL[0], Some("todowrite"));
    let messages = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("Partway."), call("t1", "todowrite")]),
        message_with_blocks(3, "user", vec![result("t1", "not run")]),
        user_text(4, &cut_off),
        user_text(5, "Terminal command c1 finished: exit code 0."),
        message_with_blocks(6, "assistant", vec![text("Woken reply.")]),
    ];
    assert_eq!(
        buttons(&reply_parts(&messages, false)),
        vec![(2, 0, "Partway.".to_string(), 1), (6, 0, "Woken reply.".to_string(), 1)]
    );
    let stopped = vec![
        user_text(1, "go"),
        message_with_blocks(2, "assistant", vec![text("Working."), call("t1", "todoread")]),
        message_with_blocks(3, "user", vec![result("t1", "[]")]),
        user_text(4, crate::api::chat::STOP_NOTICE),
        user_text(5, "Terminal command c1 finished: exit code 0."),
        message_with_blocks(6, "assistant", vec![text("After the stop.")]),
    ];
    assert_eq!(
        buttons(&reply_parts(&stopped, false)),
        vec![(2, 0, "Working.".to_string(), 1), (6, 0, "After the stop.".to_string(), 1)]
    );
}
