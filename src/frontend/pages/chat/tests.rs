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

/// SME-111: a cut-off reply's notice reads as a sentence naming its limit,
/// what bound it and what to change, for each limit.
#[test]
fn test_a_cut_off_notice_reads_as_a_sentence() {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    assert_eq!(
        system_notice(&cut_off_notice(131_072, ReplyLimit::HalfWindow, None), &HashMap::new()).as_deref(),
        Some("The reply was cut off at its limit of 131,072 tokens (half the context window). A reply can be at most half the context window: lower the model's effort or reasoning budget on its provider's page, or ask for less at once.")
    );
    for limit in ReplyLimit::ALL {
        let shown = system_notice(&cut_off_notice(16_384, limit, None), &HashMap::new()).expect("a notice");
        assert!(shown.contains("16,384") && shown.contains(limit.reason()) && shown.contains(limit.hint()), "{shown}");
    }
    assert_eq!(system_notice("Your last reply was cut off: it reached its limit of lots", &HashMap::new()), None);
}

/// SME-126: a notice for a reply cut off in the middle of a tool call
/// names the call, for the model and in the chat, and round-trips; a
/// notice saved before SME-126 still parses and reads as it did.
#[test]
fn test_a_cut_off_notice_names_the_call_it_was_cut_in() {
    use crate::api::chat::{ReplyLimit, cut_off_notice, parse_cut_off_notice};
    let notice = cut_off_notice(16_384, ReplyLimit::OutputCap, Some("write_file"));
    assert_eq!(
        notice,
        "Your last reply was cut off: it reached its limit of 16384 tokens (the model's maximum reply length) before it finished. It stopped in the middle of a call to `write_file`, which was dropped and not run."
    );
    assert_eq!(parse_cut_off_notice(&notice), Some((16_384, ReplyLimit::OutputCap, Some("write_file".to_string()))));
    assert_eq!(
        system_notice(&notice, &HashMap::new()).as_deref(),
        Some("The reply was cut off at its limit of 16,384 tokens (the model's maximum reply length) in the middle of a `write_file` call, which didn't run. Raise \u{201c}Max reply tokens\u{201d} for this model on its provider's page, or lower its effort or reasoning budget.")
    );

    let old = "Your last reply was cut off: it reached its limit of 8192 tokens (half the context window) before it finished.";
    assert_eq!(cut_off_notice(8_192, ReplyLimit::HalfWindow, None), old);
    assert_eq!(parse_cut_off_notice(old), Some((8_192, ReplyLimit::HalfWindow, None)));
    assert_eq!(parse_cut_off_notice(&format!("{old} It stopped in the middle of a call to ``, which was dropped and not run.")), None);
    assert_eq!(parse_cut_off_notice(&format!("{old} And more.")), None);
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

/// Two commands in terminal 10, `cmd-old` (with the given status and
/// code) then `cmd-new` (running).
fn two_commands(old_status: &str, old_code: Option<i32>) -> Vec<SandboxTerminalPanelEntry> {
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    let mut old = test_sandbox_command_entry("cmd-old", "make");
    old.status = old_status.to_string();
    old.exit_code = old_code;
    terminals[0].commands.push(old);
    terminals[0].commands.push(test_sandbox_command_entry("cmd-new", "sleep 30"));
    terminals
}

fn finish(terminals: &mut Vec<SandboxTerminalPanelEntry>, command_id: &str, status: &str, code: Option<i32>) {
    apply_sandbox_command_update(terminals, 10, command_id.into(), None, status.into(), code, None, None, None);
}

fn statuses(terminals: &[SandboxTerminalPanelEntry]) -> Vec<(String, String, Option<i32>)> {
    terminals[0].commands.iter().map(|c| (c.command_id.clone(), c.status.clone(), c.exit_code)).collect()
}

/// SME-144: a finish for an older command (published after the model
/// already started the next one) lands on that command, not on the
/// newest, which keeps running.
#[test]
fn test_a_finish_for_an_older_command_updates_only_that_command() {
    let mut terminals = two_commands("running", None);
    finish(&mut terminals, "cmd-old", "finished", Some(2));
    assert_eq!(
        statuses(&terminals),
        vec![
            ("cmd-old".to_string(), "finished".to_string(), Some(2)),
            ("cmd-new".to_string(), "running".to_string(), None),
        ]
    );
}

#[test]
fn test_a_finish_for_a_command_the_tab_never_saw_changes_nothing() {
    let mut terminals = two_commands("finished", Some(0));
    let before = statuses(&terminals);
    finish(&mut terminals, "cmd-unknown", "finished", Some(1));
    assert_eq!(statuses(&terminals), before);
}

#[test]
fn test_a_lost_update_marks_its_command_lost_with_no_code() {
    let mut terminals = two_commands("finished", Some(0));
    finish(&mut terminals, "cmd-new", "lost", None);
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "lost".to_string(), None));
}

#[test]
fn test_a_repeated_finish_is_idempotent() {
    let mut terminals = two_commands("finished", Some(0));
    finish(&mut terminals, "cmd-new", "finished", Some(4));
    finish(&mut terminals, "cmd-new", "finished", Some(4));
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "finished".to_string(), Some(4)));
}

/// A reconnect replays a line from before the snapshot after the snapshot
/// already says the command finished: the line mustn't flip it back to
/// running.
#[test]
fn test_a_buffered_line_for_a_finished_command_keeps_it_finished() {
    let mut terminals = two_commands("finished", Some(0));
    terminals[0].commands[1].status = "finished".into();
    terminals[0].commands[1].exit_code = Some(3);
    terminals[0].commands[1].output =
        vec![SandboxOutputLinePanelEntry { stream: "stdout".into(), data: "one".into(), seq: Some(1) }];
    apply_sandbox_command_update(&mut terminals, 10, "cmd-new".into(), None, "running".into(), None, Some("stdout".into()), Some("one".into()), Some(1));
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "finished".to_string(), Some(3)));
    assert_eq!(terminals[0].commands[1].output.len(), 1, "the replayed line was added twice");
}

/// A line for an older command goes to that command.
#[test]
fn test_a_line_for_an_older_command_goes_to_that_command() {
    let mut terminals = two_commands("running", None);
    apply_sandbox_command_update(&mut terminals, 10, "cmd-old".into(), None, "running".into(), None, Some("stdout".into()), Some("late".into()), Some(9));
    assert_eq!(terminals[0].commands[0].output.len(), 1);
    assert!(terminals[0].commands[1].output.is_empty(), "the old command's line went to the new one");
}

/// A replayed start for a command the snapshot already has finished
/// leaves it finished.
#[test]
fn test_a_replayed_start_keeps_the_snapshots_finished_status() {
    let mut terminals = two_commands("finished", Some(0));
    terminals[0].commands[1].status = "finished".into();
    terminals[0].commands[1].exit_code = Some(1);
    apply_sandbox_command_update(&mut terminals, 10, "cmd-new".into(), Some("sleep 30".into()), "running".into(), None, None, None, None);
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "finished".to_string(), Some(1)));
}

/// A tab that shows a command running takes the reconnect's word that it
/// finished.
#[test]
fn test_a_snapshot_finishes_a_command_the_tab_shows_running() {
    let mut pods = Vec::new();
    let mut terminals = two_commands("finished", Some(0));
    let snapshot = SandboxSnapshot {
        pods: vec![SandboxPodSummary {
            pod_id: 1,
            status: "Running".to_string(),
            terminals: vec![SandboxTerminalSummary {
                terminal_id: 10,
                pod_id: 1,
                status: "connected".to_string(),
                commands: vec![SandboxCommandSummary {
                    command_id: "cmd-new".to_string(),
                    command: "sleep 30".to_string(),
                    status: "finished".to_string(),
                    exit_code: Some(1),
                    output: Vec::new(),
                }],
            }],
            previews: Vec::new(),
        }],
    };
    merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);
    assert_eq!(statuses(&terminals), vec![("cmd-new".to_string(), "finished".to_string(), Some(1))]);
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

/// A conversation switch replaces the open conversation's state with
/// `ConversationState::default()` (SME-57), so every field has to start
/// empty, or the switch would carry it into the next conversation
/// (SME-51 B11). The pattern names every field: a new one doesn't compile
/// here until it's checked too.
#[test]
fn test_conversation_state_starts_empty() {
    let ConversationState {
        conversation,
        messages,
        load_error,
        streaming_reply,
        turn_running,
        turn_elapsed,
        notification_delivery_error,
        todos,
        pending_question,
        repo_url,
        repo_branch,
        repo_dir,
        repo_attaching,
        repo_attach_error,
        repo_action_error,
        repos,
        sandbox_pods,
        sandbox_terminals,
        pending_pod_stop,
        pod_stop_error,
        browsing_session_open,
        browsing_url,
        browsing_frame,
        address_draft,
        address_editing,
        address_pending,
        address_error,
        context_usage,
        context_detail,
        context_detail_open,
        layout_snap_pending,
        live,
    } = ConversationState::default();
    assert!(conversation.is_none(), "conversation");
    assert!(messages.is_empty(), "messages");
    assert!(load_error.is_none(), "load_error");
    assert!(streaming_reply.is_none(), "streaming_reply");
    assert!(!turn_running, "turn_running");
    assert_eq!(turn_elapsed, 0, "turn_elapsed");
    assert!(notification_delivery_error.is_none(), "notification_delivery_error");
    assert!(todos.is_empty(), "todos");
    assert!(pending_question.is_none(), "pending_question");
    assert!(repo_url.is_empty(), "repo_url");
    assert!(repo_branch.is_empty(), "repo_branch");
    assert!(repo_dir.is_empty(), "repo_dir");
    assert!(!repo_attaching, "repo_attaching");
    assert!(repo_attach_error.is_none(), "repo_attach_error");
    assert!(repo_action_error.is_none(), "repo_action_error");
    assert!(repos.is_empty(), "repos");
    assert!(sandbox_pods.is_empty(), "sandbox_pods");
    assert!(sandbox_terminals.is_empty(), "sandbox_terminals");
    assert!(pending_pod_stop.is_none(), "pending_pod_stop");
    assert!(pod_stop_error.is_none(), "pod_stop_error");
    assert!(!browsing_session_open, "browsing_session_open");
    assert!(browsing_url.is_none(), "browsing_url");
    assert!(browsing_frame.is_none(), "browsing_frame");
    assert!(address_draft.is_empty(), "address_draft");
    assert!(!address_editing, "address_editing");
    assert!(!address_pending, "address_pending");
    assert!(address_error.is_none(), "address_error");
    assert!(context_usage.is_none(), "context_usage");
    assert!(context_detail.is_none(), "context_detail");
    assert!(!context_detail_open, "context_detail_open");
    assert!(!layout_snap_pending, "layout_snap_pending");
    assert!(live.is_none(), "live");
}

/// Runs `f` with a fresh `ConversationState` store. A store, like a signal,
/// needs an owner, so it's created inside a `VirtualDom`'s root scope.
fn with_state(f: impl FnOnce(Store<ConversationState>)) {
    fn app() -> Element {
        rsx! {}
    }
    let mut dom = VirtualDom::new(app);
    dom.rebuild_in_place();
    dom.in_scope(ScopeId::APP, || f(Store::new(ConversationState::default())));
}

/// The pods and terminals are two fields of one store, which lends out one
/// write at a time: changing both at once has to go through
/// `change_sandbox_panel` (a reconnect's snapshot, a pod going away).
#[test]
fn test_change_sandbox_panel_changes_the_pods_and_terminals_together() {
    with_state(|state| {
        state.sandbox_terminals().set(vec![SandboxTerminalPanelEntry {
            terminal_id: 9,
            pod_id: 1,
            status: "open".to_string(),
            commands: Vec::new(),
        }]);
        let snapshot = SandboxSnapshot {
            pods: vec![SandboxPodSummary {
                pod_id: 2,
                status: "Running".to_string(),
                terminals: vec![SandboxTerminalSummary {
                    terminal_id: 20,
                    pod_id: 2,
                    status: "open".to_string(),
                    commands: Vec::new(),
                }],
                previews: Vec::new(),
            }],
        };
        change_sandbox_panel(state, |pods, terminals| merge_sandbox_snapshot(pods, terminals, snapshot));
        assert_eq!(state.sandbox_pods().peek().iter().map(|p| p.pod_id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(
            state.sandbox_terminals().peek().iter().map(|t| (t.terminal_id, t.pod_id)).collect::<Vec<_>>(),
            vec![(20, 2)],
            "the snapshot's terminals replace the old ones, and stay in the store"
        );
    });
}

fn text_message(id: i64, role: &str, text: &str) -> Message {
    message_with_blocks(id, role, vec![ContentBlock::Text { text: text.to_string() }])
}

fn repo(id: i64) -> RepoSummary {
    RepoSummary {
        id,
        url: format!("https://example.com/r{id}.git"),
        path: format!("/workspace/r{id}"),
        requested_branch: None,
        branch: None,
        commit: None,
        status: RepoStatus::Ready,
        error: None,
        agents_files: vec![],
        loaded_instructions: vec![],
        trust_requests: vec![],
    }
}

#[test]
fn test_apply_event_messages_appended_adds_them_once_and_refreshes_the_sidebar() {
    with_state(|state| {
        state.messages().set(vec![text_message(1, "user", "hi")]);
        let effect = apply_event(
            state,
            ConversationEvent::MessagesAppended { messages: vec![text_message(1, "user", "hi"), text_message(2, "user", "more")] },
        );
        assert_eq!(effect, EventEffect::ConversationsChanged);
        let ids: Vec<i64> = state.messages().peek().iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![1, 2], "the row already shown isn't added twice");
    });
}

#[test]
fn test_apply_event_a_saved_reply_replaces_its_streaming_copy() {
    with_state(|state| {
        state.streaming_reply().set(Some("partial".to_string()));
        apply_event(state, ConversationEvent::MessagesAppended { messages: vec![text_message(3, "user", "a user row")] });
        assert_eq!(state.streaming_reply().peek().as_deref(), Some("partial"), "a user row leaves the reply streaming");
        apply_event(state, ConversationEvent::MessagesAppended { messages: vec![text_message(4, "assistant", "partial and done")] });
        assert_eq!(*state.streaming_reply().peek(), None);
    });
}

#[test]
fn test_apply_event_sandbox_pod_update_adds_then_removes_the_pod() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::SandboxPodUpdate { pod_id: 7, status: "running".to_string(), terminated: false });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.sandbox_pods().peek().iter().map(|p| (p.pod_id, p.status.clone())).collect::<Vec<_>>(), vec![(7, "running".to_string())]);
        apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "open".to_string(), terminated: false });
        apply_event(state, ConversationEvent::SandboxPodUpdate { pod_id: 7, status: "gone".to_string(), terminated: true });
        assert!(state.sandbox_pods().peek().is_empty());
        assert!(state.sandbox_terminals().peek().is_empty(), "the pod's terminals go with it");
    });
}

#[test]
fn test_apply_event_sandbox_preview_update_sets_the_pods_previews() {
    with_state(|state| {
        apply_event(state, ConversationEvent::SandboxPodUpdate { pod_id: 7, status: "running".to_string(), terminated: false });
        let effect = apply_event(state, ConversationEvent::SandboxPreviewUpdate { pod_id: 7, previews: vec![preview(3000)] });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.sandbox_pods().peek()[0].previews, vec![preview(3000)]);
    });
}

#[test]
fn test_apply_event_sandbox_terminal_update_adds_then_removes_the_terminal() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "open".to_string(), terminated: false });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.sandbox_terminals().peek().iter().map(|t| (t.terminal_id, t.pod_id)).collect::<Vec<_>>(), vec![(70, 7)]);
        apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "closed".to_string(), terminated: true });
        assert!(state.sandbox_terminals().peek().is_empty());
    });
}

#[test]
fn test_apply_event_sandbox_command_update_starts_a_command_then_adds_its_output() {
    with_state(|state| {
        apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "open".to_string(), terminated: false });
        let effect = apply_event(
            state,
            ConversationEvent::SandboxCommandUpdate {
                terminal_id: 70,
                command_id: "c1".to_string(),
                command: Some("seq 1 2".to_string()),
                status: "running".to_string(),
                exit_code: None,
                stream: None,
                latest_output: None,
                position: None,
            },
        );
        assert_eq!(effect, EventEffect::None);
        apply_event(
            state,
            ConversationEvent::SandboxCommandUpdate {
                terminal_id: 70,
                command_id: "c1".to_string(),
                command: None,
                status: "running".to_string(),
                exit_code: None,
                stream: Some("stdout".to_string()),
                latest_output: Some("1".to_string()),
                position: Some(1),
            },
        );
        let terminals = state.sandbox_terminals();
        let terminals = terminals.peek();
        assert_eq!(terminals[0].commands.len(), 1);
        assert_eq!(terminals[0].commands[0].command, "seq 1 2");
        assert_eq!(terminals[0].commands[0].output.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(), vec!["1"]);
    });
}

#[test]
fn test_apply_event_notification_delivery_failed_shows_its_detail() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::NotificationDeliveryFailed { detail: "no model".to_string() });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.notification_delivery_error().peek().as_deref(), Some("no model"));
    });
}

#[test]
fn test_apply_event_context_usage_update_sets_the_meter() {
    with_state(|state| {
        let usage = TokenUsage { input_tokens: 1200, ..TokenUsage::default() };
        let effect = apply_event(state, ConversationEvent::ContextUsageUpdate { usage, context_window: 200_000 });
        assert_eq!(effect, EventEffect::None);
        let snapshot = state.context_usage();
        let snapshot = snapshot.peek();
        let snapshot = snapshot.as_ref().expect("a snapshot");
        assert_eq!(snapshot.usage, Some(usage));
        assert_eq!(snapshot.context_window, 200_000);
    });
}

#[test]
fn test_apply_event_todo_list_update_replaces_the_todos() {
    with_state(|state| {
        let items = vec![TodoItem { content: "split the page".to_string(), status: TodoStatus::InProgress }];
        let effect = apply_event(state, ConversationEvent::TodoListUpdate { items: items.clone() });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(*state.todos().peek(), items);
    });
}

#[test]
fn test_apply_event_browsing_session_update_opens_and_a_close_clears_the_frame_and_url() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::BrowsingSessionUpdate { open: true });
        assert_eq!(effect, EventEffect::None);
        assert!(*state.browsing_session_open().peek());
        state.browsing_frame().set(Some("frame".to_string()));
        state.browsing_url().set(Some("https://example.com/".to_string()));
        apply_event(state, ConversationEvent::BrowsingSessionUpdate { open: false });
        assert!(!*state.browsing_session_open().peek());
        assert_eq!(*state.browsing_frame().peek(), None);
        assert_eq!(*state.browsing_url().peek(), None);
    });
}

#[test]
fn test_apply_event_browsing_url_update_sets_the_url() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::BrowsingUrlUpdate { url: "https://example.com/".to_string() });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.browsing_url().peek().as_deref(), Some("https://example.com/"));
    });
}

#[test]
fn test_apply_event_repos_update_replaces_the_repos() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::ReposUpdate { repos: vec![repo(1), repo(2)] });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.repos().peek().iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2]);
    });
}

#[test]
fn test_apply_event_question_update_sets_and_clears_the_waiting_question() {
    with_state(|state| {
        let question = PendingQuestion { tool_use_id: "toolu_1".to_string(), questions: vec![] };
        let effect = apply_event(state, ConversationEvent::QuestionUpdate { question: Some(question.clone()) });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(*state.pending_question().peek(), Some(question));
        apply_event(state, ConversationEvent::QuestionUpdate { question: None });
        assert_eq!(*state.pending_question().peek(), None);
    });
}

#[test]
fn test_apply_event_turn_state_sets_running_and_an_ended_turn_drops_its_reply() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::TurnState { running: true });
        assert_eq!(effect, EventEffect::None);
        assert!(*state.turn_running().peek());
        state.streaming_reply().set(Some("half a reply".to_string()));
        apply_event(state, ConversationEvent::TurnState { running: false });
        assert!(!*state.turn_running().peek());
        assert_eq!(*state.streaming_reply().peek(), None);
    });
}

#[test]
fn test_apply_event_reply_reset_then_deltas_build_the_streaming_reply() {
    with_state(|state| {
        assert_eq!(apply_event(state, ConversationEvent::ReplyReset {}), EventEffect::None);
        assert_eq!(state.streaming_reply().peek().as_deref(), Some(""));
        apply_event(state, ConversationEvent::ReplyDelta { text: "Hel".to_string(), offset: 0 });
        assert_eq!(apply_event(state, ConversationEvent::ReplyDelta { text: "lo".to_string(), offset: 3 }), EventEffect::None);
        assert_eq!(state.streaming_reply().peek().as_deref(), Some("Hello"));
    });
}

#[test]
fn test_apply_event_turn_error_is_kept_but_a_stop_is_not() {
    with_state(|state| {
        assert_eq!(
            apply_event(state, ConversationEvent::TurnError { message: "overloaded".to_string() }),
            EventEffect::TurnError("overloaded".to_string())
        );
        assert_eq!(
            apply_event(state, ConversationEvent::TurnError { message: crate::api::chat::TURN_STOPPED.to_string() }),
            EventEffect::None,
            "a stop shows as its saved notice instead"
        );
    });
}

#[test]
fn test_apply_event_counters_and_the_stale_bundle_are_left_to_the_page() {
    with_state(|state| {
        assert_eq!(apply_event(state, ConversationEvent::PodsChanged {}), EventEffect::PodsChanged);
        assert_eq!(apply_event(state, ConversationEvent::TurnsChanged {}), EventEffect::TurnsChanged);
        assert_eq!(apply_event(state, ConversationEvent::QuestionsChanged {}), EventEffect::QuestionsChanged);
        assert_eq!(apply_event(state, ConversationEvent::ModelChanged {}), EventEffect::ModelChanged);
        assert_eq!(apply_event(state, ConversationEvent::ProvidersChanged {}), EventEffect::ModelChanged);
        assert_eq!(apply_event(state, ConversationEvent::Unknown), EventEffect::StaleBundle);
    });
}

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

fn terminal_with(commands: &[(&str, &str, Option<i32>)]) -> SandboxTerminalPanelEntry {
    let mut terminal = test_sandbox_terminal_entry(10, 1);
    for (i, (command, status, code)) in commands.iter().enumerate() {
        let mut entry = test_sandbox_command_entry(&format!("cmd-{i}"), command);
        entry.status = status.to_string();
        entry.exit_code = *code;
        terminal.commands.push(entry);
    }
    terminal
}

/// SME-144: the titlebar's pill reads the terminal's last command.
#[test]
fn test_latest_command_indicator_reads_the_last_command() {
    let cases: [(&[(&str, &str, Option<i32>)], CommandIndicator); 7] = [
        (&[], CommandIndicator::None),
        (&[("sleep 9", "running", None)], CommandIndicator::Running),
        (&[("true", "finished", Some(0))], CommandIndicator::Exited(0)),
        (&[("false", "finished", Some(3))], CommandIndicator::Exited(3)),
        (&[("make", "lost", None)], CommandIndicator::Lost),
        (&[("make", "finished", None)], CommandIndicator::Other("finished".into())),
        (&[("false", "finished", Some(1)), ("sleep 9", "running", None)], CommandIndicator::Running),
    ];
    for (commands, expected) in cases {
        assert_eq!(latest_command_indicator(&terminal_with(commands)), expected, "{commands:?}");
    }
}

#[test]
fn test_each_command_indicator_has_a_label_class_and_title() {
    let cases = [
        (CommandIndicator::Running, "running", "task-terminal-command task-terminal-command-running", "running"),
        (CommandIndicator::Exited(0), "exit 0", "task-terminal-command task-terminal-command-ok", "exited with status 0"),
        (CommandIndicator::Exited(130), "exit 130", "task-terminal-command task-terminal-command-failed", "exited with status 130"),
        (CommandIndicator::Lost, "lost", "task-terminal-command task-terminal-command-lost", "lost"),
        (CommandIndicator::Other("weird".into()), "weird", "task-terminal-command task-terminal-command-other", "weird"),
    ];
    for (indicator, label, class, says) in cases {
        assert_eq!(indicator.label(), label);
        assert_eq!(indicator.class(), class);
        assert!(!indicator.glyph().is_empty(), "{indicator:?} has no glyph");
        let title = indicator.title("cargo build --release");
        assert!(title.starts_with("Last command: cargo build --release"), "{title}");
        assert!(title.contains(says), "{title}");
    }
    assert!(CommandIndicator::Lost.title("x").contains("no exit status"));
}

/// SME-144 review 1: a long or multi-line command (a heredoc the model
/// wrote) isn't the whole tooltip and accessible name: its first line, cut
/// at 120 characters.
#[test]
fn test_a_command_indicators_title_shortens_a_long_command() {
    let long = format!("cat > big.txt <<'EOF'\n{}\nEOF", "x".repeat(5000));
    let title = CommandIndicator::Running.title(&long);
    assert_eq!(title, "Last command: cat > big.txt <<'EOF'\u{2026} \u{b7} running");
    let wide = "y".repeat(300);
    let title = CommandIndicator::Exited(1).title(&wide);
    assert_eq!(title, format!("Last command: {}\u{2026} \u{b7} exited with status 1", "y".repeat(120)));
    assert_eq!(CommandIndicator::Exited(0).title("true"), "Last command: true \u{b7} exited with status 0");
    assert_eq!(CommandIndicator::Exited(0).title("true\n"), "Last command: true \u{b7} exited with status 0");
}
