use super::*;

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
fn test_is_safe_compaction_boundary_false_when_tool_use_present() {
    let blocks = vec![anthropic::ContentBlock::ToolUse {
        id: "t1".to_string(),
        name: "add".to_string(),
        input: serde_json::json!({}),
    }];
    assert!(!is_safe_compaction_boundary(&blocks));
}
