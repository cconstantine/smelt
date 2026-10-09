use super::*;
use crate::api::chat::*;
use crate::events;
use dioxus::fullstack::ServerEvents;
use axum::response::IntoResponse;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::providers::test_support::lock_turn_tests;

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

/// `history_for_request` with nothing stripped.
fn history_for_request_all(
    messages: Vec<Message>,
) -> Result<Vec<anthropic::AnthropicMessage>, serde_json::Error> {
    history_for_request(messages, None)
}

fn message_with_blocks(id: i64, role: &str, blocks: Vec<anthropic::ContentBlock>) -> Message {
    Message {
        id,
        conversation_id: 1,
        role: role.to_string(),
        content: serde_json::to_string(&blocks).expect("ContentBlock always serializes"),
        created_at: chrono::Utc::now().naive_utc(),
    }
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

fn text_block(text: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::Text {
        text: text.to_string(),
    }
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

fn usage(
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
) -> anthropic::TokenUsage {
    anthropic::TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
    }
}

#[test]
fn test_estimate_tokens_uses_chars_over_4_heuristic() {
    let blocks = vec![anthropic::ContentBlock::Text {
        text: "a".repeat(400),
    }];
    assert_eq!(estimate_tokens(&blocks), 100);
}

#[test]
fn test_estimate_tokens_sums_every_block_and_every_category() {
    let blocks = vec![
        anthropic::ContentBlock::Text {
            text: "a".repeat(40),
        },
        anthropic::ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "b".repeat(40),
            is_error: None,
        },
    ];
    assert_eq!(estimate_tokens(&blocks), 20);
}

#[test]
fn test_should_compact_false_when_nothing_sent_yet() {
    assert!(!should_compact(None, 1000, 200_000));
}

#[test]
fn test_should_compact_false_comfortably_under_ceiling() {
    let last = usage(10_000, 2_000, 0, 0);
    assert!(!should_compact(Some(&last), 500, 200_000));
}

#[test]
fn test_should_compact_true_when_projected_crosses_reserved_ceiling() {
    // ceiling = 200_000 - max(MIN_REPLY_TOKENS, COMPACTION_SAFETY_BUFFER) = 183_616
    let last = usage(183_000, 0, 0, 0);
    assert!(should_compact(Some(&last), 1_000, 200_000));
}

/// SME-111: the reply budget is the smallest of the model's cap, the room
/// left and half the window, never below what every turn asked for.
#[test]
fn test_the_reply_budget_grows_with_the_room_left() {
    // The user's llama.cpp model at the start of a conversation.
    assert_eq!(reply_budget(262_144, 35_000, None).tokens, 131_072, "half the window");
    // A model whose output cap is the smallest.
    assert_eq!(reply_budget(1_000_000, 35_000, Some(128_000)).tokens, 128_000);
    assert_eq!(reply_budget(200_000, 35_000, Some(16_384)).tokens, 16_384, "an unknown Claude cap");
    // Room left is the smallest: the request must fit.
    assert_eq!(reply_budget(262_144, 220_000, None).tokens, 262_144 - 220_000 - COMPACTION_SAFETY_BUFFER);
    // Less room than the floor: what every turn asked for before.
    assert_eq!(reply_budget(262_144, 250_000, None).tokens, MIN_REPLY_TOKENS);
    assert_eq!(reply_budget(262_144, 400_000, None).tokens, MIN_REPLY_TOKENS, "past the window");
    // A small window: at most half of it.
    assert_eq!(reply_budget(4_096, 1_000, None).tokens, 2_048);
    assert_eq!(reply_budget(4_096, 1_000, Some(1_024)).tokens, 1_024, "the cap wins over the floor");
    assert_eq!(reply_budget(0, 0, None).tokens, 1, "never zero, which the API refuses");
}

/// SME-111: a budget says what bound it, for a cut-off notice.
#[test]
fn test_the_reply_budget_says_what_bound_it() {
    use crate::api::chat::ReplyLimit;
    assert_eq!(reply_budget(262_144, 35_000, None).limit, ReplyLimit::HalfWindow);
    assert_eq!(reply_budget(1_000_000, 35_000, Some(128_000)).limit, ReplyLimit::OutputCap);
    assert_eq!(reply_budget(262_144, 220_000, None).limit, ReplyLimit::RoomLeft);
    assert_eq!(reply_budget(262_144, 250_000, None).limit, ReplyLimit::RoomLeft, "the floor: nearly full");
    assert_eq!(reply_budget(262_144, 250_000, Some(8_192)).limit, ReplyLimit::OutputCap);
    assert_eq!(reply_budget(4_096, 1_000, None).limit, ReplyLimit::HalfWindow);
}

#[test]
fn test_a_requests_estimate_counts_its_messages_system_and_tools() {
    let request = anthropic::CreateMessageRequest {
        model: "m".to_string(),
        max_tokens: 1,
        system: Some("s".repeat(400)),
        messages: vec![anthropic::AnthropicMessage { role: "user".to_string(), content: vec![text_block(&"a".repeat(800))] }],
        stream: true,
        tools: vec![anthropic::ToolDefinition {
            name: "t".repeat(20),
            description: "d".repeat(380),
            input_schema: serde_json::json!({}),
        }],
        thinking: None,
        output_config: None,
        chat_template_kwargs: None,
        prompt_caching: false,
    };
    let tools = serde_json::to_string(&request.tools).expect("tools encode").len() as u64 / 4;
    assert_eq!(estimate_request_tokens(&request), 100 + 200 + tools);
    assert!(tools >= 100);
}

#[test]
fn test_projected_input_needs_a_measured_usage() {
    assert_eq!(projected_input(None, 500), None);
    assert_eq!(projected_input(Some(&usage(1_000, 200, 30, 4_000)), 500), Some(5_730));
}

#[test]
fn test_should_compact_counts_cache_tokens_in_already_used() {
    let last = usage(0, 0, 90_000, 90_000);
    assert!(should_compact(Some(&last), 10_000, 200_000));
}

#[test]
fn test_is_safe_compaction_boundary_true_for_plain_text() {
    let blocks = vec![anthropic::ContentBlock::Text {
        text: "hi".to_string(),
    }];
    assert!(is_safe_compaction_boundary(&blocks));
}

#[test]
fn test_is_safe_compaction_boundary_true_for_tool_result_only() {
    let blocks = vec![anthropic::ContentBlock::ToolResult {
        tool_use_id: "t1".to_string(),
        content: "3".to_string(),
        is_error: None,
    }];
    assert!(is_safe_compaction_boundary(&blocks));
}

#[test]
fn test_forget_conversation_lock_drops_it() {
    let conversation_id = 987_654_011;
    let first = conversation_lock(conversation_id);
    let _ = stop_receiver(conversation_id);
    forget_conversation_lock(conversation_id);
    assert!(
        !holds_lock_and_stops(conversation_id).1,
        "the deleted conversation's stop signal is still registered"
    );
    let second = conversation_lock(conversation_id);
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the deleted conversation's lock is still registered"
    );
    forget_conversation_lock(conversation_id);
}

/// A tab watching a conversation also hears app-wide pod changes, on
/// the same stream.
#[tokio::test]
async fn test_a_conversation_stream_relays_pods_changed() {
    let stream = conversation_event_stream(9_000_000_011);
    futures_util::pin_mut!(stream);
    // Another test's app event first, as one running alongside can
    // publish.
    events::publish_app(events::AppEvent::TurnsChanged);
    events::publish_app(events::AppEvent::PodsChanged);
    let event = next_skipping_other_app_relays(&mut stream, |e| {
        matches!(e, events::ConversationEvent::PodsChanged {})
    })
    .await;
    assert!(
        matches!(event, Some(Ok(events::ConversationEvent::PodsChanged {}))),
        "got {event:?}"
    );
}

/// Whether `event` is one a conversation's stream relays from the
/// app-wide channel, which every test running alongside can publish to.
fn is_app_relay(event: &events::ConversationEvent) -> bool {
    matches!(
        event,
        events::ConversationEvent::PodsChanged {}
            | events::ConversationEvent::TurnsChanged {}
            | events::ConversationEvent::QuestionsChanged {}
            | events::ConversationEvent::ProvidersChanged {}
    )
}

/// The stream's next item that is `wanted`, or isn't a relayed app
/// event at all: another test's app events aren't this test's to judge
/// (SME-135). Gives up after a second.
async fn next_skipping_other_app_relays(
    stream: &mut (impl futures_util::Stream<Item = Result<events::ConversationEvent, axum::BoxError>> + Unpin),
    wanted: impl Fn(&events::ConversationEvent) -> bool,
) -> Option<Result<events::ConversationEvent, axum::BoxError>> {
    use futures_util::StreamExt;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match stream.next().await {
                Some(Ok(event)) if is_app_relay(&event) && !wanted(&event) => continue,
                other => return other,
            }
        }
    })
    .await
    .expect("the wanted event should arrive on the conversation stream")
}

/// SME-51 B3: each delta says where it starts, measured against the
/// same text `get_reply_in_progress` returns.
#[tokio::test]
async fn test_reply_deltas_carry_their_offset_in_the_reply_so_far() {
    let conversation_id = 9_000_000_052;
    let mut rx = events::subscribe(conversation_id);
    relay_reply_delta(conversation_id, "héllo ");
    relay_reply_delta(conversation_id, "world");
    let mut offsets = Vec::new();
    while let Ok(events::ConversationEvent::ReplyDelta { offset, .. }) = rx.try_recv() {
        offsets.push(offset);
    }
    assert_eq!(offsets, vec![0, "héllo ".len()]);
    assert_eq!(reply_in_progress(conversation_id).as_deref(), Some("héllo world"));
    clear_reply_in_progress(conversation_id);
    events::forget(conversation_id);
}

/// SME-51 B3: a tab that falls behind loses events it can't get back
/// (a saved message, the turn ending). Its stream ends instead, so the
/// tab reconnects and pulls the current state again.
#[tokio::test]
async fn test_a_stream_that_falls_behind_ends_instead_of_skipping() {
    use futures_util::StreamExt;
    let conversation_id = 9_000_000_051;
    let stream = conversation_event_stream(conversation_id);
    futures_util::pin_mut!(stream);
    for i in 0..2_000 {
        events::publish(
            conversation_id,
            events::ConversationEvent::ReplyDelta { text: format!("{i} "), offset: 0 },
        );
    }
    // An app event another test can publish meanwhile (the providers
    // tests do), which the stream relays.
    events::publish_app(events::AppEvent::ProvidersChanged);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match stream.next().await {
                None => return true,
                // App-wide events, this test's own and those of tests
                // running alongside: all four kinds the stream relays.
                Some(Ok(event)) if is_app_relay(&event) => continue,
                Some(_) => return false,
            }
        }
    })
    .await
    .expect("the stream should answer");
    assert!(ended, "a lagging stream kept going with events missing");
    events::forget(conversation_id);
}

/// SME-41 D9: a turn starting or ending is announced app-wide, and
/// the conversation is listed as busy while it runs, so the sidebar
/// can mark it in every tab.
#[tokio::test]
async fn test_a_running_turn_marks_its_conversation_busy() {
    let conversation_id = 9_000_000_041;
    let mut app = events::subscribe_app();
    let next_turns_changed = |app: &mut tokio::sync::broadcast::Receiver<events::AppEvent>| {
        let mut app = app.resubscribe();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if let Ok(events::AppEvent::TurnsChanged) = app.recv().await {
                        return;
                    }
                }
            })
            .await
            .is_ok()
        }
    };
    let started = next_turns_changed(&mut app);
    let turn = TurnInFlight::start(conversation_id);
    assert!(started.await, "starting a turn should announce it");
    assert!(busy_conversations().contains(&conversation_id));

    let ended = next_turns_changed(&mut app);
    drop(turn);
    assert!(ended.await, "ending a turn should announce it");
    assert!(!busy_conversations().contains(&conversation_id));
}

#[tokio::test]
async fn test_a_conversation_stream_relays_turns_changed() {
    let stream = conversation_event_stream(9_000_000_042);
    futures_util::pin_mut!(stream);
    // Another test's app event first, as one running alongside can
    // publish (a sandbox test's pod going away, say).
    events::publish_app(events::AppEvent::PodsChanged);
    events::publish_app(events::AppEvent::TurnsChanged);
    let event = next_skipping_other_app_relays(&mut stream, |e| {
        matches!(e, events::ConversationEvent::TurnsChanged {})
    })
    .await;
    assert!(
        matches!(event, Some(Ok(events::ConversationEvent::TurnsChanged {}))),
        "got {event:?}"
    );
}

/// A subscription belongs to its connection: once the response is
/// dropped (the tab closed or reloaded), nothing should still be
/// listening on the conversation's channel.
#[sqlx::test]
async fn test_a_dropped_event_subscription_stops_listening(pool: PgPool) {
    // An id no other test's conversation has: subscriptions are counted
    // per id across the process, and every test database counts from 1
    // (it failed twice in SME-94's gates, with turn tests running
    // alongside once nothing held their lock for minutes).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    // What `subscribe_conversation_events` answers, on this test's pool.
    let subscription = ServerEvents::from_stream(
        open_conversation_events(&pool, conversation_id).await.expect("subscribe"),
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(events::subscriber_count(conversation_id), 1);
    drop(subscription);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        events::subscriber_count(conversation_id),
        0,
        "a dropped subscription is still listening"
    );
}

#[test]
fn test_is_safe_compaction_boundary_false_when_tool_use_present() {
    let blocks = vec![anthropic::ContentBlock::ToolUse {
        id: "t1".to_string(),
        name: "add".to_string(),
        input: serde_json::json!({}),
    }];
    assert!(!is_safe_compaction_boundary(&blocks));
}

/// Spins up a mock Anthropic upstream that returns `bodies` in order (one
/// per request, clamped to the last body once exhausted) and saves it
/// as `pool`'s default provider. Callers hold `lock_turn_tests`.
async fn start_mock_upstream(
    pool: &PgPool,
    bodies: Vec<String>) -> Arc<AtomicUsize> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let bodies = Arc::new(bodies);
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_route = counter.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let bodies = bodies.clone();
            let counter = counter_for_route.clone();
            async move {
                let i = counter.fetch_add(1, Ordering::SeqCst);
                let body = bodies[i.min(bodies.len() - 1)].clone();
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    crate::providers::test_support::add_mock_provider(pool, addr).await;
    counter
}

fn sse_body(events: &[(&str, &str)]) -> String {
    events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

/// Like `start_mock_upstream`, but the first `fail_count` requests get
/// back a flat HTTP 500 with Ollama's real "error parsing tool call"
/// body — the exact shape `is_ollama_thinking_tool_call_corruption` is
/// meant to recognize — and every request after that gets
/// `success_body` (a normal 200 SSE stream), if any. `success_body:
/// None` means every request fails, for testing the give-up path once
/// `TOOL_CALL_PARSE_RETRIES` is exhausted.
async fn start_mock_upstream_failing_n_times(
    pool: &PgPool,
    fail_count: usize, success_body: Option<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let counter = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let counter = counter.clone();
            let success_body = success_body.clone();
            async move {
                let i = counter.fetch_add(1, Ordering::SeqCst);
                if i < fail_count || success_body.is_none() {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        format!(
                            r#"{{"type":"error","error":{{"type":"api_error","message":"error parsing tool call: raw='attempt {i}' err=invalid character '?' after object key:value pair"}},"request_id":"req_test"}}"#
                        ),
                    )
                        .into_response()
                } else {
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        success_body.expect("checked above"),
                    )
                        .into_response()
                }
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    crate::providers::test_support::add_mock_provider(pool, addr).await;
}

#[test]
fn test_is_ollama_thinking_tool_call_corruption_matches_the_known_error_shape() {
    let real_error = r#"Anthropic API error 500 Internal Server Error: {"type":"error","error":{"type":"api_error","message":"error parsing tool call: raw='...' err=invalid character 'T' looking for beginning of value"},"request_id":"req_123"}"#;
    assert!(is_ollama_thinking_tool_call_corruption(real_error));
}

#[test]
fn test_is_ollama_thinking_tool_call_corruption_does_not_match_unrelated_errors() {
    assert!(!is_ollama_thinking_tool_call_corruption(
        "Anthropic API error 529 Overloaded: the server is overloaded"
    ));
    assert!(!is_ollama_thinking_tool_call_corruption(
        "timed out waiting for Anthropic to respond"
    ));
}

/// A bearer provider's turn authenticates with its token as a bearer
/// header, and no `x-api-key` (an Anthropic-compatible gateway such as
/// Hugging Face's wants exactly that).
#[sqlx::test]
async fn test_a_bearer_providers_turn_sends_its_token_as_a_bearer_header(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let body = text_reply_body("Hi!");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let recorded = recorded.clone();
            let body = body.clone();
            async move {
                recorded.lock().unwrap_or_else(|e| e.into_inner()).push(headers);
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let provider = db::create_inference_provider(
        &pool,
        "gateway",
        "other",
        &format!("http://{addr}"),
        "bearer",
        "hf-token",
        false,
        None,
        true,
    )
    .await
    .expect("create provider");
    db::set_default_model(&pool, provider.id, "some-model")
        .await
        .expect("set default");

    let messages = run_turn(&pool, conversation.id, hello())
        .await
        .expect("the turn should run on the bearer provider");

    assert_eq!(messages.len(), 2);
    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let headers = seen.first().expect("the mock should have been called");
    assert_eq!(headers.get("authorization").expect("Authorization header"), "Bearer hf-token");
    assert!(headers.get("x-api-key").is_none(), "no x-api-key for a bearer provider");
}

/// A finished, not-yet-notified terminal command — the state
/// `wake_conversation` is meant to react to. Mirrors `db.rs`'s own
/// `test_terminal` helper shape.
async fn unnotified_finished_command(pool: &PgPool, conversation_id: i64, command_id: &str) {
    let pod = db::create_sandbox_pod(pool, conversation_id)
        .await
        .expect("create sandbox pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id)
        .await
        .expect("create sandbox terminal");
    db::create_terminal_command(pool, conversation_id, terminal.id, command_id, "echo hi")
        .await
        .expect("create terminal command");
    db::mark_terminal_command_finished(pool, command_id, 0)
        .await
        .expect("mark terminal command finished");
}

#[sqlx::test]
async fn test_wake_conversation_is_a_noop_when_nothing_is_pending(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let counter = start_mock_upstream(&pool, vec!["unused".to_string()]).await;

    let result = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed even with nothing pending");
    assert!(
        result.is_empty(),
        "expected no persisted messages, got {result:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "nothing pending should mean no API call at all"
    );

    let messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert!(
        messages.is_empty(),
        "no message should be persisted when nothing is pending"
    );
}

#[sqlx::test]
async fn test_wake_conversation_drains_a_pending_command_and_completes_a_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;

    let messages = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed");

    assert_eq!(
        messages.len(),
        2,
        "expected the notification plus the assistant's reply, got {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
    assert_eq!(
        messages[0].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Terminal command cmd-1 finished: exit code 0.".to_string()
        }]
    );
    assert_eq!(messages[1].role, "assistant");

    let remaining = db::unnotified_finished_terminal_commands(&pool, conversation.id)
        .await
        .expect("query unnotified commands");
    assert!(
        remaining.is_empty(),
        "the command should now be marked notified"
    );
}

#[sqlx::test]
async fn test_wake_conversation_second_call_is_a_noop_once_the_first_drained_everything(
    pool: PgPool,
) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let counter = start_mock_upstream(&pool, vec![body]).await;

    let first = wake_conversation(&pool, conversation.id)
        .await
        .expect("first wake should succeed");
    assert_eq!(first.len(), 2);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "first wake should make exactly one API call"
    );

    // Simulates a second, near-simultaneous exit event's own detached
    // wake_conversation call — nothing should be left to drain, so this
    // must not persist another message or make another API call.
    let second = wake_conversation(&pool, conversation.id)
        .await
        .expect("second wake should succeed");
    assert!(
        second.is_empty(),
        "second wake should find nothing left to drain, got {second:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "second wake should not make another API call"
    );
}

/// SME-72: with no model to run on, a wake-up still saves the finished
/// command's notice (the model sees it once there is one) and says why
/// it couldn't deliver it; a wake with nothing pending doesn't take the
/// default or say anything.
#[sqlx::test]
async fn test_a_wake_with_no_model_still_saves_the_notice(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    let mut rx = events::subscribe(conversation.id);

    let error = wake_conversation(&pool, conversation.id)
        .await
        .expect_err("no model to deliver to");

    assert_eq!(error.message(), crate::providers::NO_MODEL_CONFIGURED);
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert_eq!(saved.len(), 1, "the notice is saved: {saved:?}");
    let failure = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let events::ConversationEvent::NotificationDeliveryFailed { detail } =
                rx.recv().await.expect("event channel should not close")
            {
                return detail;
            }
        }
    })
    .await
    .expect("the failure is published");
    assert_eq!(failure, crate::providers::NO_MODEL_CONFIGURED);
}

#[sqlx::test]
async fn test_a_wake_with_nothing_pending_leaves_the_model_alone(pool: PgPool) {
    let _guard = lock_turn_tests();
    // The mock first: it moves conversations already there onto it.
    start_mock_upstream(&pool, vec!["unused".to_string()]).await;
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    wake_conversation(&pool, conversation.id).await.expect("nothing to do");

    let model = db::get_conversation_model(&pool, conversation.id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(model.provider_id, None, "a wake with nothing to say doesn't take the default");
}

#[sqlx::test]
async fn test_wake_conversation_publishes_notification_delivery_failed_on_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    // `fail_count` is irrelevant when `success_body` is `None` — every
    // request fails regardless (see the helper's doc comment).
    start_mock_upstream_failing_n_times(&pool, 0, None).await;

    let mut rx = events::subscribe(conversation.id);

    let result = wake_conversation(&pool, conversation.id).await;
    assert!(
        result.is_err(),
        "expected wake_conversation to surface the underlying failure, got {result:?}"
    );

    // The turn publishes other events too (its state, the drained
    // notice, reply resets); wait for this one.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("event channel should not close") {
                event @ events::ConversationEvent::NotificationDeliveryFailed { .. } => return event,
                _ => continue,
            }
        }
    })
    .await
    .expect("should not time out waiting for the event");
    assert!(
        matches!(
            event,
            events::ConversationEvent::NotificationDeliveryFailed { .. }
        ),
        "expected NotificationDeliveryFailed, got {event:?}"
    );

    // The notification text itself is durably persisted regardless —
    // the drain step commits before the API call that failed.
    let messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert_eq!(
        messages.len(),
        1,
        "the notification message should still be persisted, got {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
}

#[test]
fn test_chat_error_text_drops_the_server_function_wrapper() {
    let error = ServerFnError::new("model provider error 503 Service Unavailable: paused");
    assert_eq!(
        chat_error_text(&error),
        "model provider error 503 Service Unavailable: paused"
    );
}

fn prompt_env() -> PromptEnvironment {
    PromptEnvironment {
        date: chrono::NaiveDate::from_ymd_opt(2026, 9, 25).expect("a valid date"),
        model: "claude-test-model".to_string(),
        volumes: vec![("cargo-cache".to_string(), "/home/sandbox/.cargo".to_string())],
        mcp_servers: vec!["exa".to_string(), "github".to_string()],
        repos: Vec::new(),
        instructions: Vec::new(),
    }
}

#[test]
fn test_system_prompt_lists_repos_and_ends_with_their_instructions() {
    let mut env = prompt_env();
    env.repos = vec![crate::git::RepoSummary {
        id: 1,
        url: "git@github.com:o/smelt.git".to_string(),
        path: "/workspace/smelt".to_string(),
        requested_branch: None,
        branch: Some("main".to_string()),
        commit: Some("43835b44f939".to_string()),
        status: crate::git::RepoStatus::Ready,
        error: None,
        agents_files: vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()],
        loaded_instructions: vec!["AGENTS.md".to_string()],
        trust_requests: vec![],
    }];
    env.instructions = vec![crate::git::ProjectInstructions {
        repo_url: "git@github.com:o/smelt.git".to_string(),
        path: "/workspace/smelt/AGENTS.md".to_string(),
        commit: Some("43835b44f939".to_string()),
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        truncated: false,
    }];
    let prompt = system_prompt(&env);
    assert!(
        prompt.contains("- Repositories:\n  - /workspace/smelt: git@github.com:o/smelt.git (main); AGENTS.md files: AGENTS.md (loaded), web/AGENTS.md\n"),
        "{prompt}"
    );
    assert!(
        prompt.ends_with(&crate::git::render_project_instructions(&env.instructions)),
        "{prompt}"
    );
}

/// SME-30: replies render as markdown now, so the prompt stops telling
/// the model to avoid it.
#[test]
fn test_system_prompt_says_markdown_renders() {
    assert!(!BASE_SYSTEM_PROMPT.contains("Markdown is not rendered"), "the prompt still says markdown doesn't render");
    assert!(!BASE_SYSTEM_PROMPT.contains("Don't use tables, headings"), "the prompt still rules out tables and headings");
    assert!(BASE_SYSTEM_PROMPT.contains("rendered as Markdown"), "the prompt should say replies are rendered as Markdown");
}

#[test]
fn test_system_prompt_starts_with_the_base_prompt() {
    let prompt = system_prompt(&prompt_env());
    assert!(prompt.starts_with(BASE_SYSTEM_PROMPT), "got: {prompt}");
}

#[test]
fn test_system_prompt_describes_the_environment() {
    let prompt = system_prompt(&prompt_env());
    let environment = prompt
        .strip_prefix(BASE_SYSTEM_PROMPT)
        .expect("the environment follows the base prompt");
    for expected in [
        "Today's date: 2026-09-25",
        "Model: claude-test-model",
        "cargo-cache mounted at /home/sandbox/.cargo",
        "MCP servers: exa, github",
    ] {
        assert!(environment.contains(expected), "missing {expected:?} in: {environment}");
    }
}

#[test]
fn test_system_prompt_leaves_out_empty_volume_and_mcp_lines() {
    let mut env = prompt_env();
    env.volumes.clear();
    env.mcp_servers.clear();
    let prompt = system_prompt(&env);
    let environment = prompt
        .strip_prefix(BASE_SYSTEM_PROMPT)
        .expect("the environment follows the base prompt");
    assert!(environment.contains("Today's date: 2026-09-25"), "got: {environment}");
    assert!(!environment.contains("mounted at"), "got: {environment}");
    assert!(!environment.contains("MCP servers"), "got: {environment}");
}

/// Like `start_mock_upstream` (replies in order, the last one repeated),
/// but also keeps every request body it receives, parsed as JSON, so a
/// test can check what was actually sent. Same locking rule as
/// `start_mock_upstream`.
async fn start_recording_mock_upstream(
    pool: &PgPool,
    
    bodies: Vec<String>,
) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |request: String| {
            let recorded = recorded.clone();
            let bodies = bodies.clone();
            async move {
                let parsed = serde_json::from_str(&request).expect("a JSON request body");
                let mut log = recorded.lock().expect("the request log");
                log.push(parsed);
                let body = bodies[(log.len() - 1).min(bodies.len() - 1)].clone();
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    requests
}

fn text_reply_body(text: &str) -> String {
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
            ),
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

#[sqlx::test]
async fn test_prompt_environment_reads_volumes_and_mcp_servers(pool: PgPool) {
    db::create_sandbox_volume(&pool, "cargo-cache", "/home/sandbox/.cargo")
        .await
        .expect("create volume");
    db::ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
        .await
        .expect("add mcp server");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "git@github.com:o/r.git", "github.com/o/r", None, "r")
        .await
        .expect("repo");
    db::set_repo_cloned(&pool, repo.id, "main", Some("abc123")).await.expect("cloned");
    db::set_repo_agents_files(&pool, repo.id, &["AGENTS.md".to_string(), "web/AGENTS.md".to_string()])
        .await
        .expect("agents files");
    let loaded = db::InstructionsFile {
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        hash: "h".to_string(),
        commit: Some("abc123".to_string()),
    };
    db::load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &loaded).await.expect("load");
    let env = prompt_environment(&pool, conversation.id, "some-model").await;
    assert_eq!(env.repos[0].loaded_instructions, vec!["AGENTS.md".to_string()]);
    assert_eq!(env.repos.len(), 1);
    assert_eq!(env.repos[0].path, "/workspace/r");
    assert_eq!(
        env.instructions,
        vec![crate::git::ProjectInstructions {
            repo_url: "git@github.com:o/r.git".to_string(),
            path: "/workspace/r/AGENTS.md".to_string(),
            commit: Some("abc123".to_string()),
            content: "Run make test.\n".to_string(),
            file_bytes: 15,
            truncated: false,
        }]
    );
    assert_eq!(env.date, chrono::Utc::now().date_naive());
    assert_eq!(env.model, "some-model");
    assert_eq!(
        env.volumes,
        vec![("cargo-cache".to_string(), "/home/sandbox/.cargo".to_string())]
    );
    assert_eq!(env.mcp_servers, vec!["exa".to_string()]);
}

/// Every turn's request carries the system prompt built from that
/// turn's environment.
#[sqlx::test]
async fn test_run_turn_sends_the_system_prompt(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_sandbox_volume(&pool, "cargo-cache", "/home/sandbox/.cargo")
        .await
        .expect("create volume");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 1);
    let expected = system_prompt(
        &prompt_environment(&pool, conversation.id, crate::providers::test_support::MOCK_MODEL).await,
    );
    assert_eq!(requests[0]["system"].as_str(), Some(expected.as_str()));
    assert!(expected.contains("cargo-cache mounted at /home/sandbox/.cargo"));
}

/// Compaction's summarization call keeps its own prompt; only the real
/// turn after it gets the agent's system prompt.
#[sqlx::test]
async fn test_compaction_keeps_its_own_system_prompt(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    // Usage past the ceiling, so the next turn compacts first (see
    // `test_run_turn_compacts_before_sending_when_usage_is_near_the_ceiling`).
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi again"),
    ])
    .await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert_eq!(requests[0]["system"].as_str(), Some(COMPACTION_SYSTEM_PROMPT));
    let expected = system_prompt(
        &prompt_environment(&pool, conversation.id, crate::providers::test_support::MOCK_MODEL).await,
    );
    assert_eq!(requests[1]["system"].as_str(), Some(expected.as_str()));
}

/// On a provider with prompt caching on, the turn's request carries both
/// cache markers; compaction's one-off summarization call carries neither.
#[sqlx::test]
async fn test_a_caching_provider_marks_turn_requests_but_not_compaction(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi again"),
    ])
    .await;
    sqlx::query("UPDATE inference_providers SET prompt_caching = true")
        .execute(&pool)
        .await
        .expect("turn caching on");

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert!(requests[0].get("cache_control").is_none(), "compaction isn't marked: {}", requests[0]);
    assert_eq!(requests[0]["system"].as_str(), Some(COMPACTION_SYSTEM_PROMPT));
    let ephemeral = serde_json::json!({"type": "ephemeral"});
    assert_eq!(requests[1]["cache_control"], ephemeral);
    let expected = system_prompt(
        &prompt_environment(&pool, conversation.id, crate::providers::test_support::MOCK_MODEL).await,
    );
    assert_eq!(
        requests[1]["system"],
        serde_json::json!([{"type": "text", "text": expected, "cache_control": ephemeral}])
    );
}

/// `body` reporting `input`/`cache_read` input tokens and `output` output
/// tokens, as a real reply's `message_start` and `message_delta` do.
fn with_usage(body: &str, input: i64, cache_read: i64, output: i64) -> String {
    body.replacen(
        r#"{"type":"message_start"}"#,
        &format!(
            r#"{{"type":"message_start","message":{{"usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{cache_read},"output_tokens":1}}}}}}"#
        ),
        1,
    )
    .replacen(r#""delta":{"stop_reason""#, &format!(r#""usage":{{"output_tokens":{output}}},"delta":{{"stop_reason""#), 1)
}

/// Every completed model call is recorded with its own usage: the
/// compaction's summary call, then each call of the turn's tool loop.
#[sqlx::test]
async fn test_each_model_call_of_a_turn_is_recorded_with_its_usage(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let tool_use_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_recording_mock_upstream(&pool, vec![
        with_usage(&text_reply_body("Summary: nothing live."), 5_000, 0, 300),
        with_usage(&tool_use_body, 1_000, 2_000, 40),
        with_usage(&text_reply_body("Done"), 100, 3_000, 7),
    ])
    .await;
    let provider_id: i64 = sqlx::query_scalar("SELECT default_provider_id FROM inference_settings")
        .fetch_one(&pool)
        .await
        .expect("the mock provider");

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let rows: Vec<(Option<i64>, String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT provider_id, model, kind, input_tokens, cache_read_input_tokens, output_tokens
         FROM model_call_usage WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conversation.id)
    .fetch_all(&pool)
    .await
    .expect("the recorded calls");
    let model = crate::providers::test_support::MOCK_MODEL.to_string();
    assert_eq!(
        rows,
        vec![
            (Some(provider_id), model.clone(), "compaction".to_string(), 5_000, 0, 300),
            (Some(provider_id), model.clone(), "turn".to_string(), 1_000, 2_000, 40),
            (Some(provider_id), model.clone(), "turn".to_string(), 100, 3_000, 7),
        ]
    );
    assert_eq!(
        db::get_conversation_usage(&pool, conversation.id).await.expect("usage").map(|u| u.input_tokens),
        Some(100),
        "the last turn call is the conversation's last known usage"
    );
    let spend = context_detail(&pool, conversation.id).await.expect("context detail").spend;
    assert_eq!(
        (spend.calls, spend.input_tokens, spend.cache_read_input_tokens, spend.output_tokens),
        (3, 6_100, 5_000, 347),
        "the detail view sums every call, compaction included"
    );
}

/// Each call is priced from the catalog when it's recorded, as the
/// provider's catalog entry; a provider without one records no cost.
#[sqlx::test]
async fn test_each_recorded_call_is_priced_from_the_catalog(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    start_recording_mock_upstream(&pool, vec![with_usage(&text_reply_body("Hi!"), 10_000, 100_000, 1_000)]).await;
    let mut providers = crate::pricing::Providers::new();
    providers.insert(
        "priced".to_string(),
        crate::pricing::CatalogProvider {
            name: "Priced".to_string(),
            api: None,
            models: [(
                crate::providers::test_support::MOCK_MODEL.to_string(),
                crate::pricing::ModelPrices {
                    base: crate::pricing::Prices { input: 4.0, output: 20.0, cache_read: Some(0.2), cache_write: Some(5.0) },
                    tiers: vec![],
                },
            )]
            .into(),
        },
    );
    crate::pricing::CATALOG.set_for_test(providers);

    run_turn(&pool, conversation.id, hello()).await.expect("unpriced turn");
    sqlx::query("UPDATE inference_providers SET price_catalog_provider = 'priced'")
        .execute(&pool)
        .await
        .expect("price the mock provider");
    run_turn(&pool, conversation.id, hello()).await.expect("priced turn");

    let costs: Vec<Option<f64>> =
        sqlx::query_scalar("SELECT cost_usd FROM model_call_usage WHERE conversation_id = $1 ORDER BY id")
            .bind(conversation.id)
            .fetch_all(&pool)
            .await
            .expect("costs");
    // 10k uncached at $4, 100k read at $0.20, 1k out at $20 per million.
    assert_eq!(costs.len(), 2);
    assert_eq!(costs[0], None, "no catalog entry, no cost");
    assert!((costs[1].expect("priced") - 0.08).abs() < 1e-9, "{costs:?}");
}

/// A model's effort goes out as `output_config.effort` on its turns, and
/// nothing goes out while it's unset.
#[sqlx::test]
async fn test_a_models_effort_is_sent_on_its_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn without effort");
    sqlx::query("INSERT INTO provider_models (provider_id, model, effort) SELECT id, $1, 'low' FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("set the mock model's effort");
    run_turn(&pool, conversation.id, hello()).await.expect("turn with effort");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("output_config").is_none(), "unset sends none: {}", requests[0]);
    assert_eq!(requests[1]["output_config"], serde_json::json!({"effort": "low"}));
}

/// SME-111: a llama.cpp provider's turn tells the chat template its
/// settings in `chat_template_kwargs`, never as `output_config`, and its
/// compaction turns the template's reasoning off.
#[sqlx::test]
async fn test_a_llama_cpp_turn_sends_template_settings_and_compaction_turns_thinking_off(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier message".to_string() }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage { input_tokens: 190_000, ..Default::default() },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi"),
    ])
    .await;
    sqlx::query(
        "UPDATE inference_providers SET kind = 'llama_cpp', keep_reasoning = false,
         server_caps = '{\"template_caps\": {\"supports_reasoning_effort\": true, \"supports_preserve_reasoning\": true}}'",
    )
    .execute(&pool)
    .await
    .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, effort) SELECT id, $1, 'high' FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("set the mock model's effort");

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert_eq!(
        requests[0]["chat_template_kwargs"],
        serde_json::json!({"enable_thinking": false, "preserve_thinking": false}),
        "the summary isn't spent on reasoning: {}",
        requests[0]
    );
    assert_eq!(
        requests[1]["chat_template_kwargs"],
        serde_json::json!({"reasoning_effort": "high", "preserve_thinking": false})
    );
    assert!(requests[1].get("output_config").is_none(), "{}", requests[1]);
}

/// SME-111: a turn asks for a reply as long as the conversation has room
/// for: half a llama.cpp model's window at the start, and after a
/// compaction the new, smaller history's room, not the old one's.
#[sqlx::test]
async fn test_a_turns_reply_budget_grows_with_the_room_left(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier message".to_string() }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage { input_tokens: 250_000, ..Default::default() },
    )
    .await
    .expect("seed usage near the window");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        with_usage(&text_reply_body("Hi"), 40_000, 0, 10),
        text_reply_body("Again"),
    ])
    .await;
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window) SELECT id, $1, 262144 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("size the mock model");

    run_turn(&pool, conversation.id, hello()).await.expect("first turn");
    run_turn(&pool, conversation.id, hello()).await.expect("second turn");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 3, "a compaction, then two turns");
    assert_eq!(requests[0]["max_tokens"], 2048, "the summary's own");
    assert_eq!(requests[1]["max_tokens"], 131_072, "half the window, after compacting: {}", requests[1]["max_tokens"]);
    // 262,144 - (40,010 measured + "hello") - 4,096 of headroom.
    let second = requests[2]["max_tokens"].as_u64().expect("a number");
    assert!((131_072 - 1..=131_072).contains(&second), "still half the window: {second}");
}

/// SME-111: the user's "Max reply tokens" caps a turn's reply budget.
#[sqlx::test]
async fn test_a_models_max_reply_tokens_caps_its_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window, max_output) SELECT id, $1, 262144, 8192 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("cap the mock model");
    run_turn(&pool, conversation.id, hello()).await.expect("turn");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 8192);
}

/// SME-111: a llama.cpp model's turn caps its thinking at three quarters
/// of the reply budget, or the model's reasoning budget; an Anthropic
/// one's thinking stays adaptive.
#[sqlx::test]
async fn test_a_llama_cpp_turn_sends_a_thinking_budget(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("on Anthropic");
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window) SELECT id, $1, 262144 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("size the mock model");
    run_turn(&pool, conversation.id, hello()).await.expect("automatic budget");
    sqlx::query("UPDATE provider_models SET reasoning_budget = 20000")
        .execute(&pool)
        .await
        .expect("cap its reasoning");
    run_turn(&pool, conversation.id, hello()).await.expect("capped budget");
    sqlx::query("UPDATE provider_models SET thinking = false")
        .execute(&pool)
        .await
        .expect("thinking off");
    run_turn(&pool, conversation.id, hello()).await.expect("thinking off");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["thinking"], serde_json::json!({"type": "adaptive"}));
    let budget = requests[1]["max_tokens"].as_u64().expect("a number");
    assert_eq!(
        requests[1]["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": budget * 3 / 4}),
        "three quarters of {budget}"
    );
    assert_eq!(requests[2]["thinking"], serde_json::json!({"type": "enabled", "budget_tokens": 20000}));
    assert!(requests[3].get("thinking").is_none(), "{}", requests[3]);
    assert_eq!(requests[3]["chat_template_kwargs"], serde_json::json!({"enable_thinking": false}));
}

fn cut_off_reply_body(text: &str) -> String {
    text_reply_body(text).replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#)
}

/// SME-111: a reply that hits its `max_tokens` is kept, followed by a
/// notice every tab gets live, naming the limit and what bound it; a
/// reply that finishes gets none.
#[sqlx::test]
async fn test_a_cut_off_reply_is_followed_by_a_notice(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![cut_off_reply_body("Half a tho"), text_reply_body("Done.")]).await;
    let mut events = crate::events::subscribe(conversation.id);

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a cut-off turn still succeeds");

    let notice = cut_off_notice(16_384, ReplyLimit::OutputCap, None);
    let texts: Vec<(String, String)> = saved
        .iter()
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect();
    assert_eq!(texts.len(), 3, "the message, the reply and the notice: {texts:?}");
    assert!(texts[1].1.contains("Half a tho"), "the reply is kept: {texts:?}");
    assert_eq!(texts[2].0, "user");
    let notice_text = match saved[2].blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(notice_text, notice);
    assert_eq!(
        crate::api::chat::parse_cut_off_notice(&notice_text),
        Some((16_384, ReplyLimit::OutputCap, None)),
        "an unsized Anthropic model's cap bound it"
    );
    let mut published = false;
    while let Ok(event) = events.try_recv() {
        if let crate::events::ConversationEvent::MessagesAppended { messages } = event {
            published |= messages.iter().any(|m| m.id == saved[2].id);
        }
    }
    assert!(published, "every tab is told");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a finished turn");
    assert_eq!(saved.len(), 2, "no notice after a finished reply");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 16_384);
}

/// A reply cut off at `max_tokens` (60 tokens written): `blocks` in
/// Anthropic's order, each a content block's SSE events.
fn cut_off_in_a_call_body(blocks: &[[String; 3]]) -> String {
    let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}"#;
    let end = r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":60}}"#;
    std::iter::once(start)
        .chain(blocks.iter().flatten().map(String::as_str))
        .chain([end, r#"{"type":"message_stop"}"#])
        .map(|data| format!("data: {data}\n\n"))
        .collect()
}

/// A text block at `index`, started, written and stopped.
fn text_events(index: usize, text: &str) -> [String; 3] {
    [
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"text","text":""}}}}"#),
        format!(r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"text_delta","text":"{text}"}}}}"#),
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    ]
}

/// A `todowrite` call at `index` with `input` (escaped for a JSON string)
/// as its input so far, started, written and stopped.
fn todowrite_events(index: usize, id: &str, input: &str) -> [String; 3] {
    [
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"todowrite","input":{{}}}}}}"#),
        format!(r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":"{input}"}}}}"#),
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    ]
}

const WHOLE_TODOS: &str = r#"{\"todos\":[{\"content\":\"a\",\"status\":\"pending\"}]}"#;
const CUT_TODOS: &str = r#"{\"todos\":[{\"content\":\"a long"#;

fn only_text(message: &crate::models::Message) -> String {
    match message.blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    }
}

/// SME-126: a reply cut off at its limit in the middle of a tool call
/// keeps its text and gets the notice naming the call, which never runs;
/// its usage is recorded, and the next request sends the reply, the
/// notice and the next message, and is accepted.
#[sqlx::test]
async fn test_a_reply_cut_off_in_a_call_keeps_its_text_and_names_the_call(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[text_events(0, "Writing the list."), todowrite_events(1, "toolu_cut", CUT_TODOS)]);
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a call cut off doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{saved:?}");
    assert_eq!(only_text(&saved[1]), "Writing the list.", "the reply's text, without the call");
    assert_eq!(only_text(&saved[2]), cut_off_notice(16_384, ReplyLimit::OutputCap, Some("todowrite")));
    assert!(db::get_conversation_todos(&pool, conversation.id).await.expect("todos").is_empty(), "the cut call never ran");
    let output: Vec<i64> = sqlx::query_scalar("SELECT output_tokens FROM model_call_usage WHERE conversation_id = $1")
        .bind(conversation.id)
        .fetch_all(&pool)
        .await
        .expect("the recorded calls");
    assert_eq!(output, [60], "the cut-off call's usage is recorded");

    run_turn(&pool, conversation.id, hello()).await.expect("the next turn is accepted");
    let requests = requests.lock().expect("the request log");
    let sent: Vec<(String, serde_json::Value)> = requests[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| (m["role"].as_str().expect("a role").to_string(), m["content"].clone()))
        .collect();
    let roles: Vec<&str> = sent.iter().map(|(role, _)| role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "user"], "{sent:?}");
    assert_eq!(sent[1].1, serde_json::json!([{"type": "text", "text": "Writing the list."}]));
    assert!(sent[2].1.to_string().contains("in the middle of a call to `todowrite`"), "{sent:?}");
    assert!(!requests[1].to_string().contains("toolu_cut"), "the dropped call appears nowhere");
}

/// SME-126: complete calls before the cut one are kept, not run, and
/// each gets a saved "not run" result, sent right after the call.
#[sqlx::test]
async fn test_whole_calls_before_a_cut_one_are_kept_with_a_not_run_result(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[todowrite_events(0, "toolu_whole", WHOLE_TODOS), todowrite_events(1, "toolu_cut", CUT_TODOS)]);
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a call cut off doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "user"], "the message, the reply, its results, the notice: {saved:?}");
    assert!(
        matches!(saved[1].blocks().expect("blocks").as_slice(), [anthropic::ContentBlock::ToolUse { id, .. }] if id == "toolu_whole"),
        "{saved:?}"
    );
    assert_eq!(
        saved[2].blocks().expect("blocks"),
        vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "toolu_whole".to_string(),
            content: CUT_OFF_TOOL_CALL.to_string(),
            is_error: Some(true),
        }]
    );
    assert!(crate::api::chat::parse_cut_off_notice(&only_text(&saved[3])).is_some(), "{saved:?}");
    assert!(db::get_conversation_todos(&pool, conversation.id).await.expect("todos").is_empty(), "neither call ran");

    run_turn(&pool, conversation.id, hello()).await.expect("the next turn is accepted");
    let requests = requests.lock().expect("the request log");
    let sent = requests[1]["messages"].as_array().expect("messages");
    assert_eq!(sent[2]["content"][0]["tool_use_id"], "toolu_whole", "the result follows the call: {sent:?}");
    assert_eq!(sent[2]["content"][0]["content"], CUT_OFF_TOOL_CALL);
}

/// SME-126 review 1: a cut-off reply's whole calls get their "not run"
/// results right after the reply is saved, so a failure after that (here
/// recording the call's usage) doesn't leave them to be answered as
/// stopped mid-run, which they weren't.
#[sqlx::test]
async fn test_not_run_results_are_saved_before_anything_that_can_fail(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[todowrite_events(0, "toolu_whole", WHOLE_TODOS), todowrite_events(1, "toolu_cut", CUT_TODOS)]);
    start_recording_mock_upstream(&pool, vec![body]).await;
    sqlx::query("ALTER TABLE model_call_usage RENAME TO model_call_usage_gone")
        .execute(&pool)
        .await
        .expect("make recording the call fail");

    run_turn(&pool, conversation.id, hello()).await.expect_err("recording the call fails the turn");

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "the message, the reply and its results: {saved:?}");
    assert_eq!(
        saved[2].blocks().expect("blocks"),
        vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "toolu_whole".to_string(),
            content: CUT_OFF_TOOL_CALL.to_string(),
            is_error: Some(true),
        }]
    );
}

/// SME-126: a reply that is nothing but the cut call saves no assistant
/// message (the API refuses an empty one); the notice follows the
/// user's message alone.
#[sqlx::test]
async fn test_a_reply_that_is_only_the_cut_call_saves_just_the_notice(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[todowrite_events(0, "toolu_cut", CUT_TODOS)]);
    start_recording_mock_upstream(&pool, vec![body]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a call cut off doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "user"], "{saved:?}");
    assert_eq!(only_text(&saved[1]), cut_off_notice(16_384, ReplyLimit::OutputCap, Some("todowrite")));
}

/// SME-111 review 2: a reply bound by an Anthropic model's own reported
/// cap, which "Max reply tokens" can't raise, says it's the model's own
/// maximum rather than telling the user to raise the setting.
#[sqlx::test]
async fn test_a_reply_at_the_models_own_cap_doesnt_say_raise_it(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    start_recording_mock_upstream(&pool, vec![cut_off_reply_body("Half a tho")]).await;
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_max_output, max_output) SELECT id, $1, 8192, 64000 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("a reported cap below the user's");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let notice = match saved[2].blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(parse_cut_off_notice(&notice), Some((8_192, ReplyLimit::ModelMaximum, None)));
}

/// SME-111 review 1: a reply the context window stopped
/// (`model_context_window_exceeded`, Claude 4.5+) is cut off too, and the
/// notice says the conversation ran out of room, whatever bound the budget.
#[sqlx::test]
async fn test_a_reply_the_window_stopped_gets_a_room_left_notice(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    // Stopped by the window after 9,000 tokens, well short of its budget.
    let body = with_usage(&text_reply_body("Half a tho"), 100, 0, 9_000)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"model_context_window_exceeded""#);
    start_recording_mock_upstream(&pool, vec![body]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn still succeeds");

    assert_eq!(saved.len(), 3, "the message, the reply and a notice: {saved:?}");
    let notice = match saved[2].blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(
        parse_cut_off_notice(&notice),
        Some((9_000, ReplyLimit::RoomLeft, None)),
        "what it wrote, not the budget it didn't reach (review 2)"
    );
}

/// SME-111 review 1: a llama.cpp reply budget too small to think in
/// (4,096 or less) turns thinking off, said plainly, instead of sending a
/// reasoning budget of 0 while thinking is on.
#[sqlx::test]
async fn test_a_reply_budget_too_small_to_think_in_turns_thinking_off(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window, max_output) SELECT id, $1, 262144, 4096 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("a small reply cap");
    run_turn(&pool, conversation.id, hello()).await.expect("turn");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 4096);
    assert!(requests[0].get("thinking").is_none(), "{}", requests[0]);
    assert_eq!(requests[0]["chat_template_kwargs"], serde_json::json!({"enable_thinking": false}));
}

/// A model on an Anthropic provider that nothing sized keeps the reply
/// budget every turn had: a Claude model refuses one above its own cap.
#[sqlx::test]
async fn test_an_unsized_anthropic_models_reply_budget_stays_as_it_was(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 16_384);
}

/// The detail view shows exactly the system prompt a turn sends.
#[sqlx::test]
async fn test_context_detail_shows_the_system_prompt_a_turn_sends(pool: PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let detail = context_detail(&pool, conversation.id)
        .await
        .expect("context detail");
    // No provider, so no model to name.
    let expected = system_prompt(&prompt_environment(&pool, conversation.id, "").await);
    assert_eq!(detail.system.as_deref(), Some(expected.as_str()));
}

/// The detail view lists the loaded AGENTS.md files on their own.
#[sqlx::test]
async fn test_context_detail_lists_loaded_instructions(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "git@github.com:o/r.git", "github.com/o/r", None, "r")
        .await
        .expect("repo");
    let loaded = db::InstructionsFile {
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        hash: "h".to_string(),
        commit: Some("abc123".to_string()),
    };
    db::load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &loaded).await.expect("load");
    let detail = context_detail(&pool, conversation.id).await.expect("context detail");
    assert_eq!(detail.instructions.len(), 1);
    assert_eq!(detail.instructions[0].path, "/workspace/r/AGENTS.md");
    assert_eq!(detail.instructions[0].content, "Run make test.\n");
}

/// Every tool the base prompt names in backticks must exist, so
/// renaming or removing a tool fails here until the prompt is updated.
/// A backticked word counts as a tool name when it's lowercase letters
/// and underscores; the few that aren't tools are listed.
#[test]
fn test_system_prompt_only_names_real_tools() {
    // Backticked words that aren't tools: a user, commands, tool
    // parameters and an example container name.
    const NOT_TOOLS: &[&str] = &[
        "sandbox",
        "sudo",
        "web_search",
        "localhost",
        "docker_memory_limit",
        "host",
        "web",
    ];
    let tools: Vec<String> = anthropic::tools::native_tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let named: Vec<&str> = BASE_SYSTEM_PROMPT
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .filter(|word| !NOT_TOOLS.contains(word))
        .collect();
    assert!(named.len() > 10, "expected the prompt to name its tools, found {named:?}");
    let unknown: Vec<&&str> = named.iter().filter(|word| !tools.iter().any(|t| t == **word)).collect();
    assert!(unknown.is_empty(), "the system prompt names tools that don't exist: {unknown:?}");
}

/// A mock Anthropic upstream that accepts requests and never answers,
/// for a turn that stays in flight until stopped. Same locking rule as
/// `start_mock_upstream`.
async fn start_hanging_mock_upstream(
    pool: &PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            std::future::pending::<()>().await;
            ""
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
}

/// Stopping ends an in-flight turn at once, frees the turn lock, and
/// keeps the user's message.
#[sqlx::test]
async fn test_stop_turn_ends_a_turn_in_flight(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000001)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop_turn_now(conversation.id);

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), turn)
        .await
        .expect("the turn should end within a second of stopping")
        .expect("join");
    let error = result.expect_err("a stopped turn ends with an error").to_string();
    assert!(error.contains(TURN_STOPPED), "got: {error}");
    assert!(
        conversation_lock(conversation.id).try_lock().is_ok(),
        "a stopped turn should release the turn lock"
    );
    let saved = db::list_messages(&pool, conversation.id).await.expect("list");
    assert_eq!(saved.len(), 2, "the user's message is kept, then the stop is noted");
    // SME-51 B10: the stop is in the conversation, for the model (and
    // a compaction) to see that request was called off.
    assert!(
        saved[1].content.contains(STOP_NOTICE),
        "the stop isn't recorded: {}",
        saved[1].content
    );

    // A turn started after the stop isn't affected by it.
    let later = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    run_turn(&pool, conversation.id, hello())
        .await
        .expect("a later turn runs normally");
    assert_eq!(later.lock().expect("log").len(), 1);
}

/// SME-91: a Stop landing just after the turn saved its own message
/// (the INSERT committed, the turn not yet told) doesn't save the
/// message a second time.
#[sqlx::test]
async fn test_a_stop_just_after_the_message_is_saved_keeps_it_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000091)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let (reached, release) = test_hooks::pause_after_message_saved(conversation.id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn saves its message")
        .expect("hook");
    stop_turn_now(conversation.id);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = release.send(());

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
        .await
        .expect("the turn ends after the stop")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    let saved = db::list_messages(&pool, conversation.id).await.expect("list");
    let copies = saved.iter().filter(|m| m.content.contains("\"hello\"")).count();
    assert_eq!(copies, 1, "the user's message is saved once: {saved:?}");
    resume_turns(conversation.id);
}

/// SME-91: a stopped reply doesn't stay on screen while another turn
/// is still in flight: the stop clears it and tells every tab.
#[sqlx::test]
async fn test_a_stop_clears_the_reply_even_with_another_turn_in_flight(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000092)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    // The turn is waiting on the model by now; its reply has begun.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    relay_reply_delta(conversation.id, "Once upon a ");
    let _another = TurnInFlight::start(conversation.id);
    let mut rx = events::subscribe(conversation.id);

    stop_turn_now(conversation.id);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
        .await
        .expect("the turn ends after the stop");

    assert_eq!(reply_in_progress(conversation.id), None, "the stopped reply is cleared");
    let reset = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::ReplyReset {}) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(reset, "tabs are told to drop the stopped reply");
    resume_turns(conversation.id);
}

/// SME-91: a tab still reconnecting to a deleted conversation (or any
/// that doesn't exist) is refused, and makes no channel for it.
#[sqlx::test]
async fn test_subscribing_to_a_missing_conversation_is_refused(pool: PgPool) {
    let missing = 9_100_000_099;
    assert!(open_conversation_events(&pool, missing).await.is_err());
    assert!(!events::has_channel(missing), "a refused subscription leaves no channel");

    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let stream = open_conversation_events(&pool, conversation.id).await;
    assert!(stream.is_ok(), "an existing conversation's events open");
}

/// SME-91 review: an idle release landing between a turn starting
/// (its generation recorded) and failing mustn't lose that failure.
#[test]
fn test_an_idle_release_keeps_a_starting_turns_generation() {
    let conversation_id = 9_100_000_192;
    let generation = new_turn_generation(conversation_id);
    release_idle_turn_state(conversation_id);
    keep_turn_error(conversation_id, generation, "it failed".to_string());
    assert_eq!(last_turn_error(conversation_id).as_deref(), Some("it failed"));
}

/// SME-91: a conversation's turn lock and stop counter go once its
/// turns are done, but not while someone still holds the lock.
#[sqlx::test]
async fn test_a_conversations_turn_state_is_freed_after_its_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000191)
        .await
        .expect("create conversation");
    start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    let turn_state_kept = holds_lock_and_stops;

    run_turn(&pool, conversation.id, hello()).await.expect("a turn");
    assert_eq!(turn_state_kept(conversation.id), (false, false), "freed after the turn");

    let held = conversation_lock(conversation.id);
    run_turn(&pool, conversation.id, hello()).await.expect("a turn");
    assert!(turn_state_kept(conversation.id).0, "a lock someone holds isn't replaced");
    drop(held);
}

/// Every tab can tell a turn is running, including one a background
/// notice started, and when it ends.
#[sqlx::test]
async fn test_turn_state_is_published_while_a_turn_runs(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000002)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation.id);
    assert!(!turn_running(conversation.id));

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    let started = next_turn_state(&mut rx).await;
    assert_eq!(started, Some(true), "a turn starting should say so");
    assert!(turn_running(conversation.id));

    stop_turn_now(conversation.id);
    let _ = turn.await;
    let ended = next_turn_state(&mut rx).await;
    assert_eq!(ended, Some(false), "a turn ending (here, stopped) should say so");
    assert!(!turn_running(conversation.id));
}

async fn next_turn_state(
    rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
) -> Option<bool> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::TurnState { running }) => return Some(running),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[test]
fn test_stop_turn_with_nothing_running_does_nothing() {
    stop_turn_now(9_000_000_012);
    let receiver = stop_receiver(9_000_000_012);
    assert!(!receiver.has_changed().expect("open"), "an earlier stop must not affect a new turn");
}

/// After a stop, a finished command doesn't wake the model; the user's
/// next message does, and its turn includes the command's notice.
#[sqlx::test]
async fn test_a_stopped_conversation_waits_for_the_user_before_waking(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000003)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    stop_turn_now(conversation.id);
    assert!(is_paused(conversation.id), "a stop pauses the conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-after-stop").await;
    wake_conversation(&pool, conversation.id)
        .await
        .expect("a paused wake does nothing, successfully");
    assert!(
        requests.lock().expect("log").is_empty(),
        "a paused conversation shouldn't call the model"
    );

    resume_turns(conversation.id);
    assert!(!is_paused(conversation.id));
    run_turn(&pool, conversation.id, hello())
        .await
        .expect("the user's next turn runs");
    let requests = requests.lock().expect("log");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["messages"].to_string().contains("cmd-after-stop"),
        "the next turn should include the command's notice"
    );
}

/// A notice delivered while nothing runs gets an answer from the model;
/// after the user stopped the conversation, it's only saved (SME-32
/// code review 6: the old save-then-wake never ran a turn).
#[sqlx::test]
async fn test_a_delivered_notice_is_answered_unless_stopped(pool: PgPool) {
    let _guard = lock_turn_tests();
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    deliver_notice(&pool, conversation.id, "Docker in your sandbox was restarted.".to_string()).await;
    {
        let requests = requests.lock().expect("the request log");
        assert_eq!(requests.len(), 1, "the model is asked about the notice");
        let last = requests[0]["messages"].as_array().expect("messages").last().expect("a message").to_string();
        assert!(last.contains("Docker in your sandbox was restarted."), "{last}");
    }

    let stopped = db::create_conversation(&pool).await.expect("create conversation");
    stop_turn_now(stopped.id);
    deliver_notice(&pool, stopped.id, "The user trusts it.".to_string()).await;
    assert_eq!(requests.lock().expect("the request log").len(), 1, "no model call after a stop");
    let saved = db::list_messages(&pool, stopped.id).await.expect("messages");
    assert!(saved.iter().any(|m| m.content.contains("The user trusts it.")), "saved for later");
}

#[sqlx::test]
async fn test_a_notice_queued_behind_a_stopped_turn_is_still_saved(pool: PgPool) {
    // SME-40 F4: a notice arriving during a turn (a command finishing,
    // say) queues as a turn of its own, behind the running one. Stop
    // ended both before the queued one had saved anything, so the notice
    // was lost and the model never learned the command finished.
    let conversation = db::create_conversation_with_id(&pool, 9100000040)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let running = lock.lock().await;

    let notice = "Command `make` in terminal 1 finished with exit code 0.";
    let queued = tokio::spawn({
        let pool = pool.clone();
        async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text { text: notice.to_string() }],
            };
            run_turn(&pool, conversation.id, message).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_turn_now(conversation.id);
    // The running turn ends on the stop too, releasing the lock.
    drop(running);
    let result = queued.await.expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");

    let saved = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let messages = db::list_messages(&pool, conversation.id).await.expect("list");
            if messages.iter().any(|m| m.content.contains("finished with exit code 0")) {
                return messages;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(saved.is_ok(), "the queued notice was lost when the turn was stopped");
    resume_turns(conversation.id);
}

/// SME-91: one Stop is noted once, however many turns it ended, even
/// when the queued turns' own messages land after the first note.
#[sqlx::test]
async fn test_one_stop_is_noted_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000093)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let running = lock.lock().await;

    let queued = |text: &'static str| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text { text: text.to_string() }],
            };
            run_turn(&pool, conversation.id, message).await
        })
    };
    let first = queued("first notice");
    let second = queued("second notice");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_turn_now(conversation.id);
    drop(running);
    for turn in [first, second] {
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("a stopped turn ends")
            .expect("join");
        assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    }

    let saved = db::list_messages(&pool, conversation.id).await.expect("list");
    let notes = saved.iter().filter(|m| m.content.contains(STOP_NOTICE)).count();
    assert_eq!(notes, 1, "one stop, one note: {saved:?}");
    assert!(saved.iter().any(|m| m.content.contains("first notice")));
    assert!(saved.iter().any(|m| m.content.contains("second notice")));
    resume_turns(conversation.id);
}

/// SME-91 (SME-51 code review 2): a turn that fails after the user
/// already sent the next message doesn't leave its error behind once
/// that next turn succeeds.
#[sqlx::test]
async fn test_an_older_turns_failure_isnt_kept_after_a_newer_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000094)
        .await
        .expect("create conversation");
    // The first request waits for `release`, then fails; later ones
    // answer.
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let gate = Arc::new(tokio::sync::Mutex::new(Some((reached_tx, release_rx))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let gate = gate.clone();
            async move {
                if let Some((reached, release)) = gate.lock().await.take() {
                    let _ = reached.send(());
                    let _ = release.await;
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"type":"error","error":{"type":"invalid_request_error","message":"turn A failed"}}"#.to_string(),
                    )
                        .into_response();
                }
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    text_reply_body("B answered"),
                )
                    .into_response()
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(&pool, addr).await;

    start_turn(pool.clone(), conversation.id, "A".to_string()).await.expect("send A");
    tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx)
        .await
        .expect("A reaches the model")
        .expect("gate");
    start_turn(pool.clone(), conversation.id, "B".to_string()).await.expect("send B");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = release_tx.send(());

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let messages = db::list_messages(&pool, conversation.id).await.expect("list");
            if messages.iter().any(|m| m.content.contains("B answered")) && !turn_running(conversation.id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("B finishes");
    assert_eq!(last_turn_error(conversation.id), None, "A's error outlived B's success");
}

/// A notice waits for a running turn to end before it's saved, and
/// tells a watching tab once it is.
/// `notify` returns at once, even with a turn holding the lock (a caller
/// inside a turn can't wait for it), and its notices land in order once
/// the turn ends (SME-52).
#[sqlx::test]
async fn test_notify_returns_at_once_and_saves_in_order(pool: PgPool) {
    let conversation = db::create_conversation_with_id(&pool, 9100000052)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let turn = lock.lock().await;
    let notifying = notify(
        &pool,
        conversation.id,
        vec![Notice::Save("first".to_string()), Notice::Save("second".to_string())],
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        db::list_messages(&pool, conversation.id).await.expect("list").is_empty(),
        "a notice was saved while a turn held the lock"
    );
    drop(turn);
    tokio::time::timeout(std::time::Duration::from_secs(5), notifying)
        .await
        .expect("the notices are saved once the turn ends")
        .expect("join");
    let texts: Vec<String> = db::list_messages(&pool, conversation.id)
        .await
        .expect("list")
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .filter_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
}

#[sqlx::test]
async fn test_save_notice_between_turns_waits_for_the_turn_lock(pool: PgPool) {
    let conversation = db::create_conversation_with_id(&pool, 9100000004)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let turn = lock.lock().await;
    let mut events = events::subscribe(conversation.id);

    let saving = tokio::spawn({
        let pool = pool.clone();
        async move {
            save_notice_between_turns(&pool, conversation.id, "pod stopped".to_string()).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        db::list_messages(&pool, conversation.id).await.expect("list").is_empty(),
        "the notice was saved while a turn held the lock"
    );

    drop(turn);
    let saved = saving.await.expect("join").expect("save the notice");
    assert_eq!(saved.role, "user");
    assert_eq!(
        saved.blocks().expect("blocks"),
        vec![anthropic::ContentBlock::Text { text: "pod stopped".to_string() }]
    );
    let appended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(events::ConversationEvent::MessagesAppended { messages }) => return messages,
                Ok(_) => continue,
                Err(e) => panic!("event channel: {e}"),
            }
        }
    })
    .await
    .expect("a MessagesAppended event");
    assert_eq!(appended.len(), 1);
}

fn hello() -> anthropic::AnthropicMessage {
    anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "hello".to_string(),
        }],
    }
}

#[sqlx::test]
async fn test_run_turn_for_a_missing_conversation_says_so(pool: PgPool) {
    let _guard = lock_turn_tests();
    start_mock_upstream(&pool, vec![String::new()]).await;
    let error = run_turn(&pool, 987_654_321, hello())
        .await
        .expect_err("a turn for a conversation that doesn't exist should fail")
        .to_string();
    assert!(error.contains("conversation not found"), "got: {error}");
    assert!(!error.contains("foreign key"), "a raw database error leaked: {error}");
}

/// With no model to run on the turn can't run, but what the user
/// typed is still theirs: it shouldn't vanish on the next reload.
#[sqlx::test]
async fn test_run_turn_without_a_model_still_saves_the_message(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let error = run_turn(&pool, conversation.id, hello())
        .await
        .expect_err("no provider should fail the turn");
    assert_eq!(error.message(), crate::providers::NO_MODEL_CONFIGURED);
    let saved = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert_eq!(saved.len(), 1, "the user's message should have been saved");
    assert_eq!(saved[0].role, "user");
}

#[sqlx::test]
async fn test_run_turn_persists_user_and_assistant_messages_for_text_only_reply(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "hello".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should succeed");

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(
        messages[0].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "hello".to_string()
        }]
    );
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Hi!".to_string()
        }]
    );
}

/// Regression test for conversation 43's real "500 error parsing tool
/// call" incident: with thinking on (the default) and pointed at a
/// mock upstream that fails the *first* request with Ollama's exact
/// error shape, `run_turn` should retry without thinking and still
/// complete — not surface the 500 to the caller.
#[sqlx::test]
async fn test_run_turn_retries_without_thinking_after_ollama_tool_call_corruption(
    pool: PgPool,
) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let success_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream_failing_n_times(&pool, 1, Some(success_body)).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "ping".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should recover from the failed first attempt and succeed");

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "pong".to_string()
        }],
        "the retried (thinking-free) attempt's reply should be what actually got persisted"
    );
}

/// Dropping `thinking` doesn't help every case (a local model can flub
/// a tool call's JSON on its own — see `is_ollama_thinking_tool_call_corruption`'s
/// doc comment) — this fails *twice*, past the thinking-drop, and
/// relies on `TOOL_CALL_PARSE_RETRIES` allowing one further plain
/// regeneration to still recover.
#[sqlx::test]
async fn test_run_turn_recovers_after_two_ollama_tool_call_failures(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let success_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"third time's the charm"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream_failing_n_times(&pool, 2, Some(success_body)).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "ping".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect(
            "run_turn should recover after exhausting the thinking-drop and one plain retry",
        );

    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "third time's the charm".to_string()
        }]
    );
}

/// Once `TOOL_CALL_PARSE_RETRIES` is exhausted, `run_turn` gives up and
/// surfaces the error rather than retrying forever against a call the
/// model is reliably bad at.
#[sqlx::test]
async fn test_run_turn_gives_up_after_exhausting_tool_call_parse_retries(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // `fail_count` is irrelevant when `success_body` is `None` — every
    // request fails regardless (see the helper's doc comment).
    start_mock_upstream_failing_n_times(&pool, 0, None).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "ping".to_string(),
        }],
    };

    let err = run_turn(&pool, conversation.id, new_message)
        .await
        .expect_err("should give up and surface the error once retries are exhausted");
    assert!(
        err.to_string().contains("error parsing tool call"),
        "got {err}"
    );
}

/// A two-step reply: "Adding." and a call to `todoread` (a cheap tool
/// that needs only the database), then (after the tool result) "Sum is
/// 5". For the mock upstream, in order.
fn text_tool_then_text_bodies() -> Vec<String> {
    let first = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Adding."}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    vec![first, text_reply_body("Sum is 5")]
}

/// Collects `rx`'s events until nothing arrives for half a second.
async fn drain_events(
    rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
) -> Vec<events::ConversationEvent> {
    let mut seen = Vec::new();
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
    {
        seen.push(event);
    }
    seen
}

/// Every tab can show a reply as it streams, one bubble per model
/// call: a reset when each call starts, then its text.
#[sqlx::test]
async fn test_a_turn_streams_its_reply_to_every_tab(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_007)
        .await
        .expect("create conversation");
    start_mock_upstream(&pool, text_tool_then_text_bodies()).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let streamed: Vec<String> = drain_events(&mut rx)
        .await
        .into_iter()
        .filter_map(|event| match event {
            events::ConversationEvent::ReplyReset {} => Some("<reset>".to_string()),
            events::ConversationEvent::ReplyDelta { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, vec!["<reset>", "Adding.", "<reset>", "Sum is 5"]);
    assert_eq!(reply_in_progress(conversation.id), None, "nothing in progress once the turn ends");
}

/// A mock upstream that streams `text` and then never finishes. Same
/// locking rule as `start_mock_upstream`.
async fn start_partial_then_hanging_mock_upstream(
    pool: &PgPool,
    text: &'static str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || async move {
            let start = format!(
                "event: message_start\ndata: {{\"type\":\"message_start\"}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
            );
            let body = futures_util::StreamExt::chain(
                futures_util::stream::once(async move { Ok::<_, std::io::Error>(start) }),
                futures_util::stream::pending(),
            );
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
}

/// A tab that connects mid-reply can show the text so far.
#[sqlx::test]
async fn test_the_reply_so_far_is_available_mid_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_008)
        .await
        .expect("create conversation");
    start_partial_then_hanging_mock_upstream(&pool, "Partial answer").await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    let seen = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(text) = reply_in_progress(conversation.id) {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the partial reply should be available mid-turn");
    assert_eq!(seen, "Partial answer");

    stop_turn_now(conversation.id);
    let _ = turn.await;
    assert_eq!(reply_in_progress(conversation.id), None, "a stopped turn leaves nothing in progress");
}

/// Sending returns at once; the turn carries on and every tab hears it.
#[sqlx::test]
async fn test_start_turn_returns_before_the_turn_finishes(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_010)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation.id);

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        start_turn(pool.clone(), conversation.id, "hello there".to_string()),
    )
    .await
    .expect("sending shouldn't wait for the model")
    .expect("sending should succeed");

    let user_message = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(events::ConversationEvent::MessagesAppended { messages }) = rx.recv().await {
                return messages;
            }
        }
    })
    .await
    .expect("the user's message should be published");
    assert_eq!(user_message.len(), 1);
    assert!(user_message[0].content.contains("hello there"));
    assert!(turn_running(conversation.id), "the turn should still be running");
    stop_turn_now(conversation.id);
}

#[sqlx::test]
async fn test_start_turn_refuses_a_missing_conversation(pool: PgPool) {
    let error = start_turn(pool, 9_100_000_011, "hi".to_string())
        .await
        .expect_err("a conversation that doesn't exist")
        .to_string();
    assert!(error.contains("conversation not found"), "got: {error}");
}

/// A sent turn that fails, or is stopped, tells every tab why.
#[sqlx::test]
async fn test_a_sent_turn_publishes_its_failure(pool: PgPool) {
    let _guard = lock_turn_tests();
    let failing = db::create_conversation_with_id(&pool, 9_100_000_012)
        .await
        .expect("create conversation");
    start_mock_upstream_failing_n_times(&pool, 0, None).await;
    let mut rx = events::subscribe(failing.id);
    start_turn(pool.clone(), failing.id, "hi".to_string()).await.expect("send");
    let error = next_turn_error(&mut rx).await.expect("a TurnError");
    assert!(error.contains("error parsing tool call"), "got: {error}");

    let stopped = db::create_conversation_with_id(&pool, 9_100_000_013)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(stopped.id);
    start_turn(pool.clone(), stopped.id, "hi".to_string()).await.expect("send");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop_turn_now(stopped.id);
    assert_eq!(next_turn_error(&mut rx).await.as_deref(), Some(TURN_STOPPED));
}

/// SME-51 B11: a failed turn's error was only an event, so a tab that
/// reloaded (or connected) afterwards showed a message with no reply
/// and no reason. The last error is kept for the reconnect pull, and
/// the user's next message clears it.
#[sqlx::test]
async fn test_a_turn_error_is_still_there_after_a_reload(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_014)
        .await
        .expect("create conversation");
    start_mock_upstream_failing_n_times(&pool, 0, None).await;
    let mut rx = events::subscribe(conversation.id);
    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    next_turn_error(&mut rx).await.expect("a TurnError");
    let kept = last_turn_error(conversation.id).expect("the error is kept");
    assert!(kept.contains("error parsing tool call"), "got: {kept}");

    start_hanging_mock_upstream(&pool).await;
    start_turn(pool.clone(), conversation.id, "again".to_string()).await.expect("send");
    assert_eq!(last_turn_error(conversation.id), None, "the next message clears it");
    stop_turn_now(conversation.id);
    // The stop pauses the conversation for the rest of the process; a
    // later test on this id would find its wakes starting no turn.
    resume_turns(conversation.id);
}

/// SME-51 code review 1: a turn the user didn't send (a finished
/// task's, say) that goes fine replaces an earlier failure; a reload
/// mustn't show that error under the newer reply.
#[sqlx::test]
async fn test_any_new_turn_clears_the_kept_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_015)
        .await
        .expect("create conversation");
    remember_turn_error(conversation.id, Some("an earlier failure".to_string()));
    start_recording_mock_upstream(&pool, vec![text_reply_body("Done.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("a background turn");
    assert_eq!(last_turn_error(conversation.id), None);
}

async fn next_turn_error(
    rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
) -> Option<String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::TurnError { message }) => return Some(message),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Stopping a turn a finished command started isn't a failed
/// notification.
#[sqlx::test]
async fn test_stopping_a_woken_turn_is_not_reported_as_a_failure(pool: PgPool) {
    let _guard = lock_turn_tests();
    // Not `test_a_turn_error_is_still_there_after_a_reload`'s id: that
    // test's stop leaves its conversation paused for the rest of the
    // process, and a wake there starts no turn (SME-135).
    let conversation = db::create_conversation_with_id(&pool, 9_135_000_014)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-woken").await;
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation.id);

    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation.id).await }
    });
    // Stop once the turn has started, not after a fixed wait: a stop
    // landing before the wake checks for a pause leaves nothing to stop,
    // and a wake that never started a turn (its conversation left paused
    // by another test's stop, say) would pass the check below untested.
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let events::ConversationEvent::TurnState { running: true } =
                rx.recv().await.expect("event channel should not close")
            {
                return;
            }
        }
    })
    .await;
    assert!(started.is_ok(), "the wake should have started a turn");
    stop_turn_now(conversation.id);
    let _ = wake.await;

    let reported = drain_events(&mut rx)
        .await
        .into_iter()
        .any(|e| matches!(e, events::ConversationEvent::NotificationDeliveryFailed { .. }));
    assert!(!reported, "a stop isn't a failed notification");
    // As above: don't leave the conversation paused for later tests.
    resume_turns(conversation.id);
}

/// Every tab sees each message as it's saved: the user's own first,
/// then each step of a multi-step reply, not one batch at the end.
#[sqlx::test]
async fn test_a_turn_publishes_each_message_as_it_is_saved(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_006)
        .await
        .expect("create conversation");
    start_mock_upstream(&pool, text_tool_then_text_bodies()).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let batches: Vec<Vec<String>> = drain_events(&mut rx)
        .await
        .into_iter()
        .filter_map(|event| match event {
            events::ConversationEvent::MessagesAppended { messages } => {
                Some(messages.into_iter().map(|m| m.role).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        batches,
        vec![
            vec!["user".to_string()],
            vec!["assistant".to_string()],
            vec!["user".to_string()],
            vec!["assistant".to_string()],
        ],
        "one event per saved message, in order"
    );
}

/// SME-112: on llama.cpp, whose blocks overlap, the commentary before a
/// tool call is saved with it (it used to show while streaming and then
/// vanish), every tab gets it, and the next request replays it.
#[sqlx::test]
async fn test_commentary_before_a_tool_call_on_llama_cpp_is_saved(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_309)
        .await
        .expect("create conversation");
    let llama_cpp = include_str!("../anthropic/fixtures/llama_cpp_messages_stream.sse");
    let requests =
        start_recording_mock_upstream(&pool, vec![llama_cpp.to_string(), text_reply_body("Nothing to do.")]).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let commentary = anthropic::ContentBlock::Text {
        text: "Let me check your todo list.".to_string(),
    };
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    let first_reply = &saved[1];
    assert_eq!(first_reply.role, "assistant");
    let blocks = first_reply.blocks().expect("blocks");
    assert!(
        matches!(&blocks[..], [anthropic::ContentBlock::Thinking { .. }, text, anthropic::ContentBlock::ToolUse { .. }] if *text == commentary),
        "{blocks:?}"
    );
    let published = drain_events(&mut rx).await.into_iter().any(|event| match event {
        events::ConversationEvent::MessagesAppended { messages } => {
            messages.iter().any(|m| m.role == "assistant" && m.blocks().expect("blocks").contains(&commentary))
        }
        _ => false,
    });
    assert!(published, "every tab gets the commentary");
    let second_request = requests.lock().expect("the request log")[1].clone();
    let replayed = &second_request["messages"][1];
    assert_eq!(replayed["role"], "assistant");
    assert_eq!(replayed["content"][1], serde_json::json!({"type": "text", "text": "Let me check your todo list."}));
}

#[sqlx::test]
async fn test_run_turn_executes_tool_and_persists_full_round_trip(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let tool_use_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let final_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sum is 5"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![tool_use_body, final_body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "what's on the todo list?".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should succeed");

    assert_eq!(
        messages.len(),
        4,
        "expected user, tool_use, tool_result, final assistant"
    );
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::ToolUse {
            id: "toolu_01".to_string(),
            name: "todoread".to_string(),
            input: serde_json::json!({}),
        }]
    );
    assert_eq!(messages[2].role, "user");
    assert_eq!(
        messages[2].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "toolu_01".to_string(),
            content: "[]".to_string(),
            is_error: None,
        }]
    );
    assert_eq!(messages[3].role, "assistant");
    assert_eq!(
        messages[3].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Sum is 5".to_string()
        }]
    );
}

#[sqlx::test]
async fn test_describe_live_state_lists_real_pods_and_terminals(pool: PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // Deliberately real rows, not hand-built
    // strings — this is the exact data `compact_conversation`'s
    // summarization prompt hands the model, so the format it actually
    // produces from real fixtures is what matters, not an assumption
    // about it.
    let pod = db::create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id)
        .await
        .expect("create terminal");
    let description = describe_live_state(&pool, conversation.id).await;

    assert!(
        description.contains(&format!("pod_id {}", pod.id)),
        "expected the real pod id in: {description}"
    );
    assert!(
        description.contains(&format!("terminal_id {} in pod_id {}", terminal.id, pod.id)),
        "expected the real terminal id (and its pod) in: {description}"
    );
}

#[sqlx::test]
async fn test_describe_live_state_lists_the_current_todo_list(pool: PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::set_conversation_todos(
        &pool,
        conversation.id,
        &[
            anthropic::tools::TodoItem {
                content: "write the plan".to_string(),
                status: anthropic::tools::TodoStatus::Completed,
            },
            anthropic::tools::TodoItem {
                content: "implement".to_string(),
                status: anthropic::tools::TodoStatus::InProgress,
            },
        ],
    )
    .await
    .expect("seed todos");

    let description = describe_live_state(&pool, conversation.id).await;

    assert!(
        description.contains("write the plan") && description.contains("completed"),
        "expected the first todo and its status in: {description}"
    );
    assert!(
        description.contains("implement") && description.contains("in_progress"),
        "expected the second todo and its status in: {description}"
    );
}

/// SME-51 B10: the usage that triggered a compaction describes the old,
/// long history. Kept after it, a failed or stopped next call left the
/// following turn compacting again at once.
#[sqlx::test]
async fn test_a_compaction_forgets_the_usage_that_triggered_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    let near_limit = anthropic::TokenUsage {
        input_tokens: 190_000,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };
    db::upsert_conversation_usage(&pool, conversation.id, &near_limit).await.expect("seed usage");
    start_mock_upstream(&pool, vec![text_reply_body("Summary: earlier.")]).await;
    let turn_model = crate::providers::resolve_turn_model(&pool, conversation.id)
        .await
        .expect("the mock is the default");
    compact_conversation(&pool, conversation.id, &turn_model)
        .await
        .expect("compaction");
    assert_eq!(db::get_conversation_usage(&pool, conversation.id).await.expect("usage"), None);
}

#[sqlx::test]
async fn test_run_turn_compacts_before_sending_when_usage_is_near_the_ceiling(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // A prior turn's persisted message plus usage close enough to the
    // reserved ceiling (200_000 - 16_384 = 183_616 for the default
    // "claude-opus-4-8" model — see `context_window_for`/`MIN_REPLY_TOKENS`)
    // that the very next turn must compact before sending, regardless
    // of how small the new message's own estimate is.
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");

    // First request the mock upstream sees is the compaction's own
    // summarization call; the second is the real turn, sent afterward
    // using the now-compacted history.
    let summary_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summary: discussed earlier topics. Nothing currently live."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let real_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi again"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![summary_body, real_body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "new question".to_string(),
        }],
    };

    let mut events = events::subscribe(conversation.id);
    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should succeed");

    // SME-40 F5: the compaction's messages reach watching tabs live,
    // like every other saved message, not only after a reload.
    let published_summary = drain_events(&mut events).await.into_iter().any(|event| match event {
        events::ConversationEvent::MessagesAppended { messages } => messages.iter().any(|m| {
            m.blocks().unwrap_or_default().iter().any(|b| {
                matches!(b, anthropic::ContentBlock::CompactionSummary { .. })
            })
        }),
        _ => false,
    });
    assert!(published_summary, "the compaction summary wasn't published to watching tabs");

    let all_messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    let compaction_summary = all_messages.iter().find_map(|m| {
        m.blocks().ok()?.into_iter().find_map(|b| match b {
            anthropic::ContentBlock::CompactionSummary {
                summary,
                covers_through_message_id,
            } => Some((summary, covers_through_message_id)),
            _ => None,
        })
    });
    assert!(
        compaction_summary.is_some(),
        "expected a CompactionSummary message to have been persisted, got: {all_messages:?}"
    );
    assert!(
        compaction_summary
            .unwrap()
            .0
            .contains("discussed earlier topics"),
        "expected the real summarization response's text to be what got persisted"
    );

    // Nothing already stored was rewritten or deleted — the original
    // seeded message and the new question are both still there in
    // full, untouched.
    assert!(
        all_messages
            .iter()
            .any(|m| m
                .blocks()
                .unwrap_or_default()
                .contains(&anthropic::ContentBlock::Text {
                    text: "earlier message".to_string()
                }))
    );
    assert!(
        all_messages
            .iter()
            .any(|m| m
                .blocks()
                .unwrap_or_default()
                .contains(&anthropic::ContentBlock::Text {
                    text: "new question".to_string()
                }))
    );

    // The real turn still completed successfully, replying to the
    // post-compaction continuation prompt.
    let last = messages.last().expect("at least one message");
    assert_eq!(last.role, "assistant");
    assert_eq!(
        last.blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Hi again".to_string()
        }]
    );
}

#[sqlx::test]
async fn test_run_turn_errors_when_max_turns_exceeded(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // Always responds with a tool_use turn calling `todoread` (a fast,
    // valid call), so the loop never reaches a final reply and must give up
    // after MAX_TURNS rather than looping forever.
    let tool_use_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![tool_use_body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "loop forever".to_string(),
        }],
    };

    // Goes through run_turn_bounded directly with a small bound rather
    // than run_turn (which would replay the mock upstream the real
    // MAX_TURNS — 10,000 — times just to prove the same "give up and
    // error" behavior).
    let result = run_turn_bounded(&pool, conversation.id, Some(new_message), 3, false, false).await;
    assert!(result.is_err(), "expected an error, got {result:?}");
}

#[test]
fn test_conversation_lock_is_shared_per_conversation_id_only() {
    let a1 = conversation_lock(9001);
    let a2 = conversation_lock(9001);
    let b = conversation_lock(9002);
    assert!(
        Arc::ptr_eq(&a1, &a2),
        "same conversation id should share one lock"
    );
    assert!(
        !Arc::ptr_eq(&a1, &b),
        "different conversation ids should get different locks"
    );
}

#[sqlx::test]
async fn test_run_turn_serializes_concurrent_calls_for_the_same_conversation(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;

    let conversation_id = conversation.id;
    let pool_a = pool.clone();
    let pool_b = pool.clone();

    let task_a = tokio::spawn(async move {
        let message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "first".to_string(),
            }],
        };
        run_turn(&pool_a, conversation_id, message).await
    });
    let task_b = tokio::spawn(async move {
        let message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "second".to_string(),
            }],
        };
        run_turn(&pool_b, conversation_id, message).await
    });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    result_a
        .expect("task a should not panic")
        .expect("run_turn a should succeed");
    result_b
        .expect("task b should not panic")
        .expect("run_turn b should succeed");

    let all = db::list_messages(&pool, conversation_id)
        .await
        .expect("list messages");
    assert_eq!(all.len(), 4);
    let roles: Vec<&str> = all.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user", "assistant"],
        "the per-conversation lock should serialize the two calls into two complete \
         (user, assistant) pairs, never interleaved"
    );
}

// --- ask_user (SME-34) ---

/// A reply that calls each of `calls` (id, tool name, input) and stops for
/// their results.
fn tool_calls_body(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut events: Vec<(String, String)> = vec![("message_start".into(), r#"{"type":"message_start"}"#.into())];
    for (index, (id, name, input)) in calls.iter().enumerate() {
        events.push((
            "content_block_start".into(),
            serde_json::json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}})
            .to_string(),
        ));
        events.push((
            "content_block_delta".into(),
            serde_json::json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": input.to_string()}})
            .to_string(),
        ));
        events.push((
            "content_block_stop".into(),
            serde_json::json!({"type": "content_block_stop", "index": index}).to_string(),
        ));
    }
    events.push(("message_delta".into(), r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#.into()));
    events.push(("message_stop".into(), r#"{"type":"message_stop"}"#.into()));
    let borrowed: Vec<(&str, &str)> = events.iter().map(|(e, d)| (e.as_str(), d.as_str())).collect();
    sse_body(&borrowed)
}

fn ask_input() -> serde_json::Value {
    serde_json::json!({"questions": [
        {"question": "Delete the build directory?", "header": "Delete",
         "options": [{"label": "Yes (Recommended)"}, {"label": "No"}]}
    ]})
}

fn ask_body() -> String {
    tool_calls_body(&[("toolu_ask", crate::questions::ASK_USER, ask_input())])
}

fn notice(text: &str) -> anthropic::AnthropicMessage {
    anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: text.to_string() }],
    }
}

/// Each saved message's blocks, in order.
async fn saved_blocks(pool: &PgPool, conversation_id: i64) -> Vec<(String, Vec<anthropic::ContentBlock>)> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("messages")
        .into_iter()
        .map(|m| {
            let blocks = m.blocks().expect("blocks");
            (m.role, blocks)
        })
        .collect()
}

/// Waits (up to 10 s) until the recording upstream has had `count`
/// requests.
async fn wait_for_requests(requests: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>, count: usize) {
    for _ in 0..200 {
        if requests.lock().expect("requests").len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("expected {count} requests, got {}", requests.lock().expect("requests").len());
}

/// The user message right after the assistant message that called
/// `tool_use_id`, in a request body.
fn message_after_call(request: &serde_json::Value, tool_use_id: &str) -> serde_json::Value {
    let messages = request["messages"].as_array().expect("messages");
    let call = messages
        .iter()
        .position(|m| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["id"] == tool_use_id)))
        .expect("the call is in the request");
    messages[call + 1].clone()
}

#[sqlx::test]
async fn test_ask_user_ends_the_turn_and_waits(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_300).await.expect("conversation");
    let calls = start_mock_upstream(&pool, vec![ask_body(), text_reply_body("unreached")]).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    assert_eq!(calls.load(Ordering::SeqCst), 1, "no model call after the question");
    let saved = saved_blocks(&pool, conversation.id).await;
    assert_eq!(saved.len(), 2, "the user's message and the call, no result: {saved:?}");
    let pending = pending_question(&pool, conversation.id).await.expect("read").expect("a question waits");
    assert_eq!(pending.tool_use_id, "toolu_ask");
    assert_eq!(pending.questions[0].header, "Delete");
    let seen = drain_events(&mut rx).await;
    assert!(
        seen.iter().any(|e| matches!(e, events::ConversationEvent::QuestionUpdate { question: Some(q) } if q.tool_use_id == "toolu_ask")),
        "every tab hears of the question: {seen:?}"
    );
}

#[sqlx::test]
async fn test_ask_user_with_another_tool_saves_its_result_and_waits(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_301).await.expect("conversation");
    let body = tool_calls_body(&[
        ("toolu_read", "todoread", serde_json::json!({})),
        ("toolu_ask", crate::questions::ASK_USER, ask_input()),
    ]);
    let calls = start_mock_upstream(&pool, vec![body, text_reply_body("unreached")]).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let saved = saved_blocks(&pool, conversation.id).await;
    assert_eq!(saved.len(), 3, "{saved:?}");
    let results: Vec<&String> = saved[2]
        .1
        .iter()
        .filter_map(|b| match b {
            anthropic::ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["toolu_read"], "only the other tool's result is saved");
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some());
}

#[sqlx::test]
async fn test_a_bad_or_second_ask_user_is_an_error_result(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_302).await.expect("conversation");
    let body = tool_calls_body(&[
        ("toolu_ask", crate::questions::ASK_USER, ask_input()),
        ("toolu_ask2", crate::questions::ASK_USER, ask_input()),
        ("toolu_bad", crate::questions::ASK_USER, serde_json::json!({"questions": []})),
    ]);
    start_mock_upstream(&pool, vec![body, text_reply_body("unreached")]).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    let saved = saved_blocks(&pool, conversation.id).await;
    let errors: Vec<(String, String)> = saved
        .last()
        .expect("a results message")
        .1
        .iter()
        .filter_map(|b| match b {
            anthropic::ContentBlock::ToolResult { tool_use_id, content, is_error: Some(true) } => {
                Some((tool_use_id.clone(), content.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2, "{saved:?}");
    assert_eq!(errors[0].0, "toolu_ask2");
    assert!(errors[0].1.contains("one"), "{errors:?}");
    assert_eq!(errors[1].0, "toolu_bad");
    assert_eq!(
        pending_question(&pool, conversation.id).await.expect("read").expect("waits").tool_use_id,
        "toolu_ask"
    );
}

#[sqlx::test]
async fn test_a_notice_while_a_question_waits_is_saved_without_calling_the_model(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_303).await.expect("conversation");
    let calls = start_mock_upstream(&pool, vec![ask_body(), text_reply_body("unreached")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    run_turn(&pool, conversation.id, notice("Terminal command c1 finished: exit code 0."))
        .await
        .expect("the notice's turn ends cleanly");

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the notice doesn't reach the model yet");
    let saved = saved_blocks(&pool, conversation.id).await;
    assert_eq!(saved.last().expect("saved").1, notice("Terminal command c1 finished: exit code 0.").content);
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some(), "still waiting");
}

#[sqlx::test]
async fn test_answering_sends_the_result_right_after_the_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_304).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("Deleting.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    run_turn(&pool, conversation.id, notice("a notice")).await.expect("notice saved");

    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes (Recommended)".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers)
        .await
        .expect("the answer is taken");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["role"], "user");
    assert_eq!(after["content"][0]["type"], "tool_result", "{after}");
    assert_eq!(after["content"][0]["tool_use_id"], "toolu_ask");
    assert_eq!(after["content"][0]["content"], "The user answered:\n1. Delete: Yes (Recommended)");
    assert_eq!(after["content"][1]["text"], "a notice", "the notice follows the result: {after}");
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
}

#[sqlx::test]
async fn test_a_message_instead_of_an_answer_dismisses_the_question(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_305).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("OK.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    start_turn(pool.clone(), conversation.id, "keep it, and do X instead".to_string())
        .await
        .expect("the message is sent");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["content"][0]["tool_use_id"], "toolu_ask", "{after}");
    assert_eq!(after["content"][0]["content"], crate::questions::NOT_ANSWERED);
    assert_eq!(after["content"][1]["text"], "keep it, and do X instead");
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
}

#[sqlx::test]
async fn test_a_question_takes_one_valid_answer(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_306).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("OK.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    let answer = |label: &str| vec![crate::questions::QuestionAnswer { selected: vec![label.to_string()], other: None }];

    let err = answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answer("Maybe"))
        .await
        .expect_err("a choice that wasn't offered");
    assert!(err.to_string().contains("Maybe"), "{err}");
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some(), "still waiting");

    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answer("No"))
        .await
        .expect("the first answer");
    let err = answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answer("Yes (Recommended)"))
        .await
        .expect_err("a second answer");
    assert!(err.to_string().contains(ALREADY_ANSWERED), "{err}");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    assert_eq!(message_after_call(&request, "toolu_ask")["content"][0]["content"], "The user answered:\n1. Delete: No");
}

/// Code review 1 (high): an answer recorded while a turn that already
/// saved its own message is running (a notice's turn, paused here right
/// after its save) is taken by that turn before it calls the model, not
/// left behind with the call answered "unfinished".
#[sqlx::test]
async fn test_an_answer_landing_mid_turn_is_taken_before_the_model_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_307).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("Deleting.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    let (reached, release) = test_hooks::pause_after_message_saved(conversation.id);
    let notice_turn = {
        let pool = pool.clone();
        tokio::spawn(async move { run_turn(&pool, conversation.id, notice("a notice")).await })
    };
    reached.await.expect("the notice's turn saved its message");
    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["No".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers)
        .await
        .expect("the answer is recorded");
    release.send(()).expect("let the notice's turn go on");
    notice_turn.await.expect("join").expect("the notice's turn");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["content"][0]["content"], "The user answered:\n1. Delete: No", "{after}");
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
}

/// Code review 2: the question is recorded before the reply's other
/// tools run, so a Stop (or restart) while they run leaves it on its card
/// rather than losing it.
#[sqlx::test]
async fn test_the_question_is_recorded_before_the_replys_other_tools_run(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_308).await.expect("conversation");
    let body = tool_calls_body(&[
        ("toolu_read", "todoread", serde_json::json!({})),
        ("toolu_ask", crate::questions::ASK_USER, ask_input()),
    ]);
    start_mock_upstream(&pool, vec![body, text_reply_body("unreached")]).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    let order: Vec<&str> = drain_events(&mut rx)
        .await
        .iter()
        .filter_map(|e| match e {
            events::ConversationEvent::QuestionUpdate { question: Some(_) } => Some("question"),
            events::ConversationEvent::MessagesAppended { messages }
                if messages.iter().any(|m| m.content.contains("toolu_read") && m.role == "user") =>
            {
                Some("results")
            }
            _ => None,
        })
        .collect();
    assert_eq!(order, vec!["question", "results"]);
}

// SME-137: a turn's spans.

fn spans_named<'a>(
    spans: &'a [opentelemetry_sdk::trace::SpanData],
    name: &str,
) -> Vec<&'a opentelemetry_sdk::trace::SpanData> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn span_attribute<'a>(span: &'a opentelemetry_sdk::trace::SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    let values: Vec<_> = span.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| &kv.value).collect();
    assert!(values.len() <= 1, "{key} recorded more than once: {values:?}");
    values.first().copied()
}

fn todoread_call_body() -> String {
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// A turn is one `turn` span, with its model calls and tool calls under it,
/// all in one trace.
#[sqlx::test]
async fn test_a_turns_model_and_tool_calls_are_children_of_its_span(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_mock_upstream(&pool, vec![todoread_call_body(), text_reply_body("done")]).await;
    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn runs");

    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    assert_eq!(span_attribute(turn, "conversation_id"), Some(&conversation.id.into()));
    assert!(span_attribute(turn, "model").is_some(), "{turn:?}");
    assert!(span_attribute(turn, "provider_id").is_some(), "{turn:?}");
    assert_eq!(span_attribute(turn, "turn.steps"), Some(&2_i64.into()));
    assert_eq!(turn.status, opentelemetry::trace::Status::Unset);

    let children: Vec<&str> = spans
        .iter()
        .filter(|span| span.parent_span_id == turn.span_context.span_id())
        .map(|span| span.name.as_ref())
        .collect();
    let calls: Vec<&str> = children.iter().copied().filter(|name| *name != "tool todoread").collect();
    assert_eq!(calls.len(), 2, "two model calls under the turn: {children:?}");
    assert!(calls.iter().all(|name| name.starts_with("chat ")), "{children:?}");
    assert!(children.contains(&"tool todoread"), "{children:?}");
    let trace = turn.span_context.trace_id();
    assert!(spans.iter().all(|span| span.span_context.trace_id() == trace), "one trace");
    assert!(!format!("{spans:?}").contains("hello"), "no message text in a span");
}

#[sqlx::test]
async fn test_a_turn_whose_model_call_fails_is_a_failed_span(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_mock_upstream_failing_n_times(&pool, 0, None).await;
    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect_err("the model call fails");
    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    let opentelemetry::trace::Status::Error { description } = &turn.status else { panic!("{:?}", turn.status) };
    assert!(description.contains("error parsing tool call"), "{description}");
}

#[sqlx::test]
async fn test_a_stopped_turn_says_so_without_failing_its_span(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let id = conversation.id;
    let ((), spans) = crate::telemetry::capture_spans(async {
        let turn = tokio::spawn({
            let pool = pool.clone();
            async move { run_turn(&pool, id, hello()).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop_turn_now(id);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("the turn ends")
            .expect("join");
        assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    })
    .await;
    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    assert_eq!(span_attribute(turn, "turn.stopped"), Some(&true.into()));
    assert_eq!(turn.status, opentelemetry::trace::Status::Unset);
}

/// The user's turn runs in the background, as a child of the request that
/// sent it.
#[sqlx::test]
async fn test_a_sent_turn_is_a_child_of_the_request_that_sent_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_mock_upstream(&pool, vec![text_reply_body("hi")]).await;
    let mut rx = events::subscribe(conversation.id);
    let ((), spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(
            tracing::info_span!("request"),
            start_turn(pool.clone(), conversation.id, "hi".to_string()),
        )
        .await
        .expect("send");
        loop {
            if next_turn_state(&mut rx).await.expect("a turn state") == false {
                break;
            }
        }
        // The turn's span closes as its task ends, just after it says so.
        tokio::task::yield_now().await;
    })
    .await;
    let requests = spans_named(&spans, "request");
    let [request] = requests.as_slice() else { panic!("one request span: {spans:?}") };
    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    assert_eq!(turn.parent_span_id, request.span_context.span_id());
}

#[sqlx::test]
async fn test_a_compaction_is_a_span_with_its_model_call_under_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    start_mock_upstream(&pool, vec![text_reply_body("Summary: earlier.")]).await;
    let turn_model = crate::providers::resolve_turn_model(&pool, conversation.id)
        .await
        .expect("the mock is the default");
    let (result, spans) =
        crate::telemetry::capture_spans(compact_conversation(&pool, conversation.id, &turn_model)).await;
    result.expect("compaction");
    let compactions = spans_named(&spans, "compaction");
    let [compaction] = compactions.as_slice() else { panic!("one compaction span: {spans:?}") };
    assert_eq!(span_attribute(compaction, "conversation_id"), Some(&conversation.id.into()));
    let chat = spans.iter().find(|span| span.name.starts_with("chat ")).expect("its model call");
    assert_eq!(chat.parent_span_id, compaction.span_context.span_id());
}
