use super::*;

fn thinking_block(text: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::Thinking {
        thinking: text.to_string(),
        signature: "sig".to_string(),
    }
}

/// SME-72: after a switch of provider or model, thinking signed for the
/// old one isn't replayed. A message that was only thinking keeps its
/// reasoning as plain text (the API rejects empty content, and a
/// made-up stand-in is text the model might imitate; review 3). Later
/// messages keep theirs.
#[test]
fn test_history_leaves_out_thinking_from_before_a_model_switch() {
    let text = |t: &str| anthropic::ContentBlock::Text { text: t.to_string() };
    let messages = vec![
        message_with_blocks(1, "user", vec![text("hi")]),
        message_with_blocks(2, "assistant", vec![thinking_block("old reasoning"), text("hello")]),
        message_with_blocks(3, "user", vec![text("go on")]),
        message_with_blocks(4, "assistant", vec![thinking_block("only thinking")]),
        message_with_blocks(5, "user", vec![text("and now?")]),
        message_with_blocks(6, "assistant", vec![thinking_block("new reasoning"), text("sure")]),
    ];
    let history = history_for_request(messages, Some(4)).expect("parses");
    let contents: Vec<Vec<anthropic::ContentBlock>> = history.into_iter().map(|m| m.content).collect();
    assert_eq!(
        contents,
        vec![
            vec![text("hi")],
            vec![text("hello")],
            vec![text("go on")],
            vec![text("only thinking")],
            vec![text("and now?")],
            vec![thinking_block("new reasoning"), text("sure")],
        ]
    );
}

#[test]
fn test_history_keeps_thinking_when_the_model_never_changed() {
    let messages = vec![message_with_blocks(2, "assistant", vec![thinking_block("r")])];
    let history = history_for_request(messages, None).expect("parses");
    assert_eq!(history[0].content, vec![thinking_block("r")]);
}

/// SME-51 B10: a request the user stopped isn't "unanswered": a
/// compaction mustn't hand it back to the model to answer.
#[test]
fn test_a_stopped_request_isnt_quoted_as_unanswered() {
    let messages = vec![
        text_message(1, "assistant", "ok"),
        text_message(2, "user", "Write a 400-word story"),
        text_message(3, "user", STOP_NOTICE),
        text_message(4, "user", "What is 2 + 2?"),
    ];
    assert_eq!(unanswered_user_text(&messages), vec!["What is 2 + 2?".to_string()]);
}

/// SME-51 B10: a huge pasted message quoted whole would leave the
/// compacted request as big as the one compaction was for.
#[test]
fn test_the_continuation_quotes_a_huge_message_only_in_part() {
    let huge = "x".repeat(100_000);
    let prompt = continuation_prompt(&[huge]);
    assert!(prompt.chars().count() < 10_000, "{} chars", prompt.chars().count());
    assert!(prompt.contains("cut"), "it should say the quote was cut");
}

fn text_message(id: i64, role: &str, text: &str) -> Message {
    message_with_blocks(id, role, vec![text_block(text)])
}

/// Messages 1-2 were already summarized by an earlier compaction
/// (3-5); only 6 is new since.
fn compacted_once() -> Vec<Message> {
    let mut messages = vec![
        text_message(1, "user", "ancient question"),
        text_message(2, "assistant", "ancient answer"),
    ];
    for (offset, (role, blocks)) in compaction_messages("the earlier summary".to_string(), 2, &[])
        .into_iter()
        .enumerate()
    {
        messages.push(message_with_blocks(3 + offset as i64, role, blocks));
    }
    messages.push(text_message(6, "user", "recent question"));
    messages
}

#[test]
fn test_compaction_transcript_starts_from_the_latest_summary() {
    let transcript = compaction_transcript(&compacted_once(), usize::MAX);
    assert!(transcript.contains("the earlier summary"), "got {transcript}");
    assert!(transcript.contains("recent question"), "got {transcript}");
    assert!(
        !transcript.contains("ancient"),
        "messages an earlier compaction already replaced came back: {transcript}"
    );
}

#[test]
fn test_compaction_transcript_keeps_the_most_recent_part_within_its_budget() {
    let mut messages: Vec<Message> = (1..=50)
        .map(|id| text_message(id, "user", &format!("message {id} {}", "x".repeat(1_000))))
        .collect();
    messages.push(text_message(51, "user", "the very latest"));
    let transcript = compaction_transcript(&messages, 5_000);
    assert!(transcript.chars().count() <= 5_000, "{} chars", transcript.chars().count());
    assert!(transcript.contains("the very latest"));
    assert!(transcript.contains("omitted"), "the cut should be marked");
    assert!(!transcript.contains("message 1 "), "the oldest part should be what's dropped");
}

/// SME-40 F3: compaction runs after the new user message is saved, so
/// the summary covers it and the model only saw "Continue based on the
/// summary above" — it answered from the summary's gist, or resumed
/// older work, instead of the request itself.
#[test]
fn test_the_continuation_quotes_what_the_model_hasnt_answered() {
    let messages = vec![
        text_message(1, "user", "old question"),
        text_message(2, "assistant", "old answer"),
        text_message(3, "user", "Terminal command abc finished: exit code 0."),
        message_with_blocks(
            4,
            "user",
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "a tool result".to_string(),
                is_error: None,
            }],
        ),
        text_message(5, "user", "Reply with just: ok"),
    ];
    let unanswered = unanswered_user_text(&messages);
    assert_eq!(
        unanswered,
        vec![
            "Terminal command abc finished: exit code 0.".to_string(),
            "Reply with just: ok".to_string()
        ]
    );

    let [_, _, (_, continuation)] = compaction_messages("the summary".to_string(), 5, &unanswered);
    let text = format!("{continuation:?}");
    assert!(text.contains("Reply with just: ok"), "the request isn't in the continuation: {text}");
    assert!(text.contains("Terminal command abc finished"), "{text}");
    assert!(!text.contains("old question"), "an answered message came back: {text}");
}

#[test]
fn test_compaction_messages_start_with_user_and_alternate_correctly() {
    let inserted = compaction_messages("the summary".to_string(), 42, &[]);
    let roles: Vec<&str> = inserted.iter().map(|(role, _)| *role).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user"],
        "Anthropic requires messages to start with user and strictly \
         alternate — a single summary message can't satisfy both \
         'starts with user' and 'ends with something to respond to'"
    );
    assert_eq!(
        inserted[1].1,
        vec![anthropic::ContentBlock::CompactionSummary {
            summary: "the summary".to_string(),
            covers_through_message_id: 42,
        }]
    );
}

fn tool_use(id: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolUse {
        id: id.to_string(),
        name: "add".to_string(),
        input: serde_json::json!({}),
    }
}

fn tool_result(id: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: "3".to_string(),
        is_error: None,
    }
}

fn unfinished(id: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: UNFINISHED_TOOL_CALL.to_string(),
        is_error: Some(true),
    }
}

/// A turn stopped (or a server restarted) between a tool call and its
/// result leaves the call unanswered, and a notice may have landed
/// after it. The request gets an error result for it, at the start of
/// the next user message.
#[test]
fn test_history_for_request_answers_a_tool_call_left_without_a_result() {
    let history = history_for_request_all(vec![
        text_message(1, "user", "go"),
        message_with_blocks(2, "assistant", vec![tool_use("t1")]),
        text_message(3, "user", "notice"),
    ])
    .expect("history");
    assert_eq!(history.len(), 3);
    assert_eq!(history[2].role, "user");
    assert_eq!(history[2].content, vec![unfinished("t1"), text_block("notice")]);
}

#[test]
fn test_history_for_request_answers_only_the_missing_calls_of_several() {
    let history = history_for_request_all(vec![
        text_message(1, "user", "go"),
        message_with_blocks(2, "assistant", vec![tool_use("t1"), tool_use("t2")]),
        message_with_blocks(3, "user", vec![tool_result("t1")]),
    ])
    .expect("history");
    assert_eq!(history[2].content, vec![unfinished("t2"), tool_result("t1")]);
}

/// An `ask_user` call is answered after notices saved while it waited
/// (SME-34): its result moves to the front of the message right after
/// the call, and a message left empty by the move is dropped.
#[test]
fn test_history_for_request_moves_a_late_result_to_just_after_its_call() {
    let history = history_for_request_all(vec![
        text_message(1, "user", "go"),
        message_with_blocks(2, "assistant", vec![tool_use("t1")]),
        text_message(3, "user", "notice"),
        message_with_blocks(4, "user", vec![tool_result("t1")]),
        message_with_blocks(5, "user", vec![tool_result("t9"), text_block("and also")]),
    ])
    .expect("history");
    assert_eq!(history.len(), 4, "{history:?}");
    assert_eq!(history[2].content, vec![tool_result("t1"), text_block("notice")]);
    assert_eq!(history[3].content, vec![tool_result("t9"), text_block("and also")]);
}

#[test]
fn test_history_for_request_joins_a_late_result_to_the_others_from_its_reply() {
    let history = history_for_request_all(vec![
        text_message(1, "user", "go"),
        message_with_blocks(2, "assistant", vec![tool_use("t1"), tool_use("t2")]),
        message_with_blocks(3, "user", vec![tool_result("t1")]),
        text_message(4, "user", "notice"),
        message_with_blocks(5, "user", vec![tool_result("t2"), text_block("answered")]),
    ])
    .expect("history");
    assert_eq!(history.len(), 5, "{history:?}");
    assert_eq!(history[2].content, vec![tool_result("t2"), tool_result("t1")]);
    assert_eq!(history[3].content, vec![text_block("notice")]);
    assert_eq!(history[4].content, vec![text_block("answered")]);
}

#[test]
fn test_history_for_request_adds_a_message_for_calls_left_at_the_end() {
    let history = history_for_request_all(vec![
        text_message(1, "user", "go"),
        message_with_blocks(2, "assistant", vec![text_block("on it"), tool_use("t1")]),
    ])
    .expect("history");
    assert_eq!(history.len(), 3);
    assert_eq!(history[2].role, "user");
    assert_eq!(history[2].content, vec![unfinished("t1")]);
}

#[test]
fn test_history_for_request_passes_everything_through_unchanged_with_no_compaction() {
    let messages = vec![
        message_with_blocks(1, "user", vec![text_block("hi")]),
        message_with_blocks(2, "assistant", vec![text_block("hello")]),
    ];
    let history = history_for_request(messages, None).expect("should parse");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, "user");
    assert_eq!(history[0].content, vec![text_block("hi")]);
    assert_eq!(history[1].role, "assistant");
    assert_eq!(history[1].content, vec![text_block("hello")]);
}

#[test]
fn test_history_for_request_skips_covered_messages_and_translates_the_summary() {
    let messages = vec![
        message_with_blocks(1, "user", vec![text_block("old message 1")]),
        message_with_blocks(2, "assistant", vec![text_block("old reply 1")]),
        message_with_blocks(
            3,
            "user",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary: "condensed: talked about X".to_string(),
                covers_through_message_id: 2,
            }],
        ),
        message_with_blocks(4, "user", vec![text_block("new message")]),
    ];
    let history = history_for_request(messages, None).expect("should parse");
    assert_eq!(
        history,
        vec![
            anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![text_block("condensed: talked about X")],
            },
            anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![text_block("new message")],
            },
        ]
    );
}

#[test]
fn test_history_for_request_translates_compaction_placeholder_to_text_too() {
    // Anthropic has no concept of either synthetic block type — an
    // untranslated CompactionPlaceholder forwarded as-is would be
    // rejected outright, same reasoning as CompactionSummary above.
    let messages = vec![message_with_blocks(
        1,
        "user",
        vec![anthropic::ContentBlock::CompactionPlaceholder {
            text: "Continue based on the summary above.".to_string(),
        }],
    )];
    let history = history_for_request(messages, None).expect("should parse");
    assert_eq!(
        history,
        vec![anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![text_block("Continue based on the summary above.")],
        }]
    );
}

#[test]
fn test_history_for_request_only_the_latest_of_two_compactions_applies() {
    let messages = vec![
        message_with_blocks(1, "user", vec![text_block("ancient")]),
        message_with_blocks(
            2,
            "user",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary: "first summary".to_string(),
                covers_through_message_id: 1,
            }],
        ),
        message_with_blocks(3, "user", vec![text_block("middle")]),
        message_with_blocks(
            4,
            "user",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary: "second summary, supersedes the first".to_string(),
                covers_through_message_id: 3,
            }],
        ),
        message_with_blocks(5, "user", vec![text_block("recent")]),
    ];
    let history = history_for_request(messages, None).expect("should parse");
    // Message 2 (the first compaction's own summary) has id 2 <= the
    // second compaction's boundary (3), so it's superseded and skipped
    // entirely too — only the second summary and what came after it
    // survive.
    assert_eq!(
        history,
        vec![
            anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![text_block("second summary, supersedes the first")],
            },
            anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![text_block("recent")],
            },
        ]
    );
}
