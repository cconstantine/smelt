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
    let prompt = continuation_prompt(&Typed::default(), &[huge]);
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
    for (offset, (role, blocks)) in compaction_messages("the earlier summary".to_string(), 2, &Typed::default(), &[], None)
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
        !transcript.contains("ancient answer"),
        "messages an earlier compaction already replaced came back: {transcript}"
    );
    assert!(transcript.contains("ancient question"), "only the user's own words stay (SME-386 F11): {transcript}");
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
    assert!(!transcript.contains("message 2 "), "the oldest part should be what's dropped");
    assert!(transcript.contains("message 1 "), "after the user's first ask, which stays (SME-386 F11)");
}

/// SME-40 F3: compaction runs after the new user message is saved, so
/// the summary covers it and the model only saw the continuation
/// nudge — it answered from the summary's gist, or resumed older work,
/// instead of the request itself.
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

    let [_, _, (_, continuation)] = compaction_messages("the summary".to_string(), 5, &Typed::default(), &unanswered, None);
    let text = format!("{continuation:?}");
    assert!(text.contains("Reply with just: ok"), "the request isn't in the continuation: {text}");
    assert!(text.contains("Terminal command abc finished"), "{text}");
    assert!(!text.contains("old question"), "an answered message came back: {text}");
}

#[test]
fn test_compaction_messages_start_with_user_and_alternate_correctly() {
    let inserted = compaction_messages("the summary".to_string(), 42, &Typed::default(), &[], None);
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
            refused: None,
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
                refused: None,
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

/// SME-73: the refusal reason lives on the block for the transcript
/// divider only. Replayed, the block is its summary text and nothing
/// else — the model never learns (or imitates) the divider's words.
#[test]
fn test_history_replay_never_sends_the_refusal_reason() {
    let messages = vec![
        message_with_blocks(1, "user", vec![text_block("old")]),
        message_with_blocks(
            2,
            "user",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary: "what the earlier part said".to_string(),
                covers_through_message_id: 1,
                refused: Some(
                    "the model's server refused a request that didn't fit its 32,768-token window".to_string(),
                ),
            }],
        ),
        message_with_blocks(3, "user", vec![text_block("new")]),
    ];
    let history = history_for_request(messages, None).expect("should parse");
    let sent = serde_json::to_string(&history).expect("serialize");
    assert!(!sent.contains("refused"), "the reason leaked to the model: {sent}");
    assert!(!sent.contains("32,768"), "the reason leaked to the model: {sent}");
    assert_eq!(history[0].content, vec![text_block("what the earlier part said")]);
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
                refused: None,
            }],
        ),
        message_with_blocks(3, "user", vec![text_block("middle")]),
        message_with_blocks(
            4,
            "user",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary: "second summary, supersedes the first".to_string(),
                covers_through_message_id: 3,
                refused: None,
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

/// A call stored under a pre-SME-365 name (written while a local server
/// took any name) goes out under the safe name offered now, so a
/// conversation switched to Anthropic doesn't send a name it refuses; a
/// safe name, and a native tool's, go out as they were.
#[test]
fn test_history_for_request_sends_a_stored_mcp_calls_name_made_safe() {
    let call = |id: &str, name: &str| anthropic::ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({}),
    };
    let history = history_for_request_all(vec![
        text_message(1, "user", "go"),
        message_with_blocks(
            2,
            "assistant",
            vec![call("t1", "mcp__linear.app__search.web"), call("t2", "mcp__github__list_issues"), call("t3", "read_file")],
        ),
        message_with_blocks(3, "user", vec![tool_result("t1"), tool_result("t2"), tool_result("t3")]),
    ])
    .expect("history");
    let names: Vec<&str> = history[1]
        .content
        .iter()
        .filter_map(|block| match block {
            anthropic::ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let safe = crate::mcp::namespaced_tool_name("linear.app", "search.web");
    assert_eq!(names, vec![safe.as_str(), "mcp__github__list_issues", "read_file"]);
}

/// SME-367: the user's and the parent's own words are quoted through a
/// compaction, word for word: a mid-run correction too, not only the
/// unanswered tail. Notices, tool results and other agents' texts aren't —
/// the notices in their saved, marked form (SME-357); wording that only
/// looks like one is the user's own, and quoted by
/// `test_words_starting_like_a_notice_are_quoted`.
#[test]
fn test_typed_words_are_quoted_and_notices_arent() {
    let correction = "you should be using the rust available in the smelt service from the docker-compose.yml";
    let messages = vec![
        text_message(1, "user", "Drive SME-73."),
        text_message(2, "assistant", "ok"),
        text_message(3, "user", &mark_notice("Terminal command abc finished: exit code 0.")),
        text_message(4, "user", correction),
        text_message(5, "user", "Report from agent #9 (smelt:developer):\n\nDone."),
        text_message(6, "user", &crate::api::chat::failure_notice("boom")),
        text_message(7, "user", "Message from your parent:\n\nUse the bare persona name."),
        message_with_blocks(
            8,
            "user",
            vec![anthropic::ContentBlock::ToolResult { tool_use_id: "t".to_string(), content: "out".to_string(), is_error: None }],
        ),
        text_message(9, "assistant", "on it"),
        text_message(10, "user", "And the next thing?"),
    ];
    let typed = typed_user_text(&messages, 200_000);
    assert_eq!(
        typed,
        Typed {
            quotes: vec![
                "Drive SME-73.".to_string(),
                correction.to_string(),
                "Message from your parent:\n\nUse the bare persona name.".to_string(),
            ],
            omitted: 0,
        },
        "the unanswered tail is quoted as such, not here"
    );
    let prompt = continuation_prompt(&typed, &unanswered_user_text(&messages));
    assert!(prompt.contains(correction), "{prompt}");
    assert!(prompt.contains("And the next thing?"), "{prompt}");
    assert!(!prompt.contains("exit code 0") && !prompt.contains("Report from"), "{prompt}");
}

/// SME-367: over the budget, the first message (the original ask) and
/// the newest are kept, and the continuation says how many it left out.
#[test]
fn test_typed_words_over_the_budget_keep_the_first_and_the_newest() {
    let mut messages = vec![text_message(1, "user", "The original ask.")];
    for i in 0..40 {
        messages.push(text_message(2 + 2 * i, "assistant", "ok"));
        messages.push(text_message(3 + 2 * i, "user", &format!("correction {i}: {}", "y".repeat(990))));
    }
    messages.push(text_message(200, "assistant", "ok"));
    let typed = typed_user_text(&messages, 200_000);
    assert_eq!(typed.quotes.first().map(String::as_str), Some("The original ask."));
    assert!(typed.quotes.last().is_some_and(|q| q.starts_with("correction 39:")), "the newest is kept");
    assert!(typed.omitted > 0 && typed.quotes.len() + typed.omitted == 41, "{} kept, {} left out", typed.quotes.len(), typed.omitted);
    let size: usize = typed.quotes.iter().map(|q| q.chars().count()).sum();
    assert!(size <= CONTINUATION_TYPED_MAX_CHARS, "{size}");
    let prompt = continuation_prompt(&typed, &[]);
    assert!(prompt.contains(&format!("({} more they wrote in between are left out here", typed.omitted)), "{prompt}");
}

/// SME-367 review 1: a request the user stopped isn't quoted as standing;
/// what they asked next is. The user's answer to the model's question is
/// quoted; notices worded as news about the user aren't.
#[test]
fn test_typed_words_drop_a_stopped_request_and_keep_answers() {
    let ask = anthropic::ContentBlock::ToolUse {
        id: "toolu_q".to_string(),
        name: crate::questions::ASK_USER.to_string(),
        input: serde_json::json!({}),
    };
    let answer = anthropic::ContentBlock::ToolResult {
        tool_use_id: "toolu_q".to_string(),
        content: "Which branch? main".to_string(),
        is_error: None,
    };
    let messages = vec![
        text_message(1, "user", "Drive the ticket."),
        text_message(2, "assistant", "ok"),
        text_message(3, "user", "Delete every branch."),
        text_message(4, "assistant", "Deleting"),
        text_message(5, "user", &mark_notice(STOP_NOTICE)),
        text_message(6, "user", "Write the docs instead."),
        message_with_blocks(7, "assistant", vec![ask]),
        message_with_blocks(8, "user", vec![answer]),
        text_message(9, "user", &mark_notice("The user stopped sandbox pod 4. Create a new pod if you need one.")),
        text_message(10, "user", &mark_notice("The user now trusts https://example.com/r.git. You can start agents on it; call start_agent again.")),
        text_message(11, "assistant", "done"),
        text_message(12, "user", "next"),
    ];
    let typed = typed_user_text(&messages, 200_000);
    assert_eq!(
        typed.quotes,
        vec!["Drive the ticket.".to_string(), "Write the docs instead.".to_string(), "Which branch? main".to_string()],
        "{typed:?}"
    );
}

/// SME-367 review 1: a small window gets a smaller budget, separators
/// counted, so its quotes can't keep it over its compaction trigger.
#[test]
fn test_a_small_windows_typed_words_fit_its_budget() {
    let budget = typed_budget(28_672);
    assert!(budget < CONTINUATION_TYPED_MAX_CHARS, "{budget}");
    let mut messages = vec![text_message(1, "user", &"a".repeat(20_000))];
    for i in 0..400 {
        messages.push(text_message(2 + 2 * i, "assistant", "ok"));
        messages.push(text_message(3 + 2 * i, "user", "yes"));
    }
    messages.push(text_message(900, "assistant", "ok"));
    let typed = typed_user_text(&messages, 28_672);
    let joined = typed.quotes.join("\n\n---\n\n");
    assert!(joined.chars().count() <= budget, "{} > {budget}", joined.chars().count());
    assert!(typed.quotes[0].contains("cut to its first"), "the long ask is cut to half the budget");
    assert!(typed.omitted > 0);
}

/// SME-367 review 2: a stop calls off only what the model hadn't answered
/// yet. A correction it answered, and then kept working past (tool steps,
/// a notice), survives a later stop; a stop in the unanswered tail still
/// calls off the request before it; a refused question call's error isn't
/// an answer.
#[test]
fn test_a_stop_calls_off_only_what_wasnt_answered() {
    let call = |id: &str, name: &str| anthropic::ContentBlock::ToolUse { id: id.to_string(), name: name.to_string(), input: serde_json::json!({}) };
    let result = |id: &str, content: &str, is_error: Option<bool>| anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error,
    };
    // Probe A: the correction was answered, then the model kept working.
    let a = vec![
        text_message(1, "user", "Drive it."),
        text_message(2, "assistant", "ok"),
        text_message(3, "user", "Use the compose rust, not the pod's."),
        text_message(4, "assistant", "Understood."),
        text_message(5, "user", &mark_notice("Terminal command abc finished: exit code 0.")),
        message_with_blocks(6, "assistant", vec![call("t1", "run_terminal_command")]),
        message_with_blocks(7, "user", vec![result("t1", "out", None)]),
        text_message(8, "user", STOP_NOTICE),
        text_message(9, "user", "Carry on."),
        text_message(10, "assistant", "ok"),
        text_message(11, "user", "next"),
    ];
    assert_eq!(
        typed_user_text(&a, 200_000).quotes,
        vec!["Drive it.".to_string(), "Use the compose rust, not the pod's.".to_string(), "Carry on.".to_string()]
    );
    // Probe B: the stop is in the unanswered tail.
    let b = vec![
        text_message(1, "user", "Drive it."),
        text_message(2, "assistant", "ok"),
        text_message(3, "user", "Delete every branch."),
        message_with_blocks(4, "assistant", vec![call("t2", "run_terminal_command")]),
        message_with_blocks(5, "user", vec![result("t2", "out", None)]),
        text_message(6, "user", STOP_NOTICE),
        text_message(7, "user", "Write docs instead."),
    ];
    assert_eq!(typed_user_text(&b, 200_000).quotes, vec!["Drive it.".to_string()]);
    // A refused question call's error result.
    let d = vec![
        text_message(1, "user", "Drive it."),
        message_with_blocks(2, "assistant", vec![call("q", crate::questions::ASK_USER)]),
        message_with_blocks(3, "user", vec![result("q", "questions: missing field", Some(true))]),
        text_message(4, "assistant", "ok"),
        text_message(5, "user", "next"),
    ];
    assert_eq!(typed_user_text(&d, 200_000).quotes, vec!["Drive it.".to_string()]);
}

/// SME-357: the stop note is now saved with `NOTICE_MARK` before its words.
/// The comparisons that drop a stopped request must count both saved forms:
/// a marked note calls the request off exactly as the old unmarked one does,
/// in `typed_user_text` and in `unanswered_user_text`.
#[test]
fn test_a_marked_stop_calls_off_the_request_too() {
    let call = |id: &str| anthropic::ContentBlock::ToolUse {
        id: id.to_string(),
        name: "run_terminal_command".to_string(),
        input: serde_json::json!({}),
    };
    let result = |id: &str| anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: "out".to_string(),
        is_error: None,
    };
    let marked_stop = format!("{}{}", crate::api::chat::NOTICE_MARK, STOP_NOTICE);
    let b = vec![
        text_message(1, "user", "Drive it."),
        text_message(2, "assistant", "ok"),
        text_message(3, "user", "Delete every branch."),
        message_with_blocks(4, "assistant", vec![call("t2")]),
        message_with_blocks(5, "user", vec![result("t2")]),
        text_message(6, "user", &marked_stop),
        text_message(7, "user", "Write docs instead."),
    ];
    assert_eq!(typed_user_text(&b, 200_000).quotes, vec!["Drive it.".to_string()], "the marked stop calls the stopped request off");
    assert_eq!(unanswered_user_text(&b), vec!["Write docs instead.".to_string()], "and in the unanswered texts too");
}

/// SME-357: the notice wording left `is_typed`'s exclusion list — what makes
/// a saved notice is `NOTICE_MARK`, so a user's own messages that merely
/// start like one are quoted again, and the saved (marked) ones still aren't.
/// The three agent-message starts stay excluded: another agent's typed words.
#[test]
fn test_words_starting_like_a_notice_are_quoted() {
    let messages = vec![
        text_message(1, "assistant", "ok"),
        text_message(2, "user", "Terminal command abc finished: exit code 0."),
        text_message(3, "user", "Sandbox pod 3 stopped unexpectedly (OOMKilled); every terminal is gone."),
        text_message(4, "user", "The user now trusts x. Ask about it below."),
        text_message(5, "user", "Notice: the user deleted agent #7"),
        text_message(6, "user", &mark_notice("Terminal command abc finished: exit code 0.")),
        text_message(7, "user", &mark_notice("The user now trusts x. Ask about it below.")),
        text_message(8, "user", &mark_notice("Notice: the user deleted agent #7")),
        text_message(9, "user", "Report from agent #9 (smelt:developer):\n\nDone."),
        text_message(10, "user", "Message from agent #9: hello."),
        text_message(11, "user", "Question from agent #9: which branch?"),
        text_message(12, "assistant", "done"),
    ];
    assert_eq!(
        typed_user_text(&messages, 200_000).quotes,
        vec![
            "Terminal command abc finished: exit code 0.".to_string(),
            "Sandbox pod 3 stopped unexpectedly (OOMKilled); every terminal is gone.".to_string(),
            "The user now trusts x. Ask about it below.".to_string(),
            "Notice: the user deleted agent #7".to_string(),
        ]
    );
}

/// SME-357: `is_typed` reads the mark, not the words: typed notice wording
/// is typed, its saved marked form isn't, and the three agent-message
/// starts stay excluded while a parent's message reads as typed.
#[test]
fn test_is_typed_reads_the_mark_not_the_notice_words() {
    assert!(crate::turn::compaction::is_typed("Terminal command abc finished: exit code 0."));
    assert!(crate::turn::compaction::is_typed("Sandbox pod 3 stopped unexpectedly (OOMKilled); …"));
    assert!(crate::turn::compaction::is_typed("The user now trusts x. Ask about it below."));
    assert!(crate::turn::compaction::is_typed("Notice: the user deleted agent #7"));
    assert!(!crate::turn::compaction::is_typed(&mark_notice("Terminal command abc finished: exit code 0.")));
    assert!(!crate::turn::compaction::is_typed(&mark_notice("Notice: the user deleted agent #7")));
    assert!(!crate::turn::compaction::is_typed("Report from agent #9 (smelt:developer):\n\nDone."));
    assert!(!crate::turn::compaction::is_typed("Message from agent #9: hello."));
    assert!(!crate::turn::compaction::is_typed("Question from agent #9: which branch?"));
    assert!(crate::turn::compaction::is_typed("Message from your parent:\n\nUse the bare persona name."));
}

/// SME-386 T8: one long recent message that doesn't fit the budget is
/// left out alone; the older, shorter ones still fit and are quoted.
#[test]
fn test_a_long_message_that_doesnt_fit_leaves_the_older_ones_quoted() {
    let messages = vec![
        text_message(1, "user", "The original ask."),
        text_message(2, "assistant", "ok"),
        text_message(3, "user", "Keep the tests green."),
        text_message(4, "assistant", "ok"),
        text_message(5, "user", &"z".repeat(10_000)),
        text_message(6, "assistant", "ok"),
    ];
    let typed = typed_user_text(&messages, 28_672);
    assert_eq!(
        typed,
        Typed { quotes: vec!["The original ask.".to_string(), "Keep the tests green.".to_string()], omitted: 1 },
        "the long one is skipped, not the end of the quotes"
    );
}
