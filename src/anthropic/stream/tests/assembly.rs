use super::*;

#[tokio::test]
async fn test_stream_anthropic_message_assembles_turns_from_mock_upstream() {
    let text_mock_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    let mut deltas = Vec::new();
    let turn =
        run_against_mock_upstream(text_mock_body, |delta| deltas.push(delta.to_string()))
            .await
            .expect("stream should succeed");

    assert_eq!(deltas, vec!["Hel".to_string(), "lo!".to_string()]);
    assert_eq!(
        turn.content,
        vec![ContentBlock::Text {
            text: "Hello!".to_string()
        }]
    );
    assert_eq!(turn.stop_reason, "end_turn");

    let tool_use_mock_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"add\",\"input\":{}}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"2,\\\"b\\\":3}\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    let turn = run_against_mock_upstream(tool_use_mock_body, |_| {
        panic!("no text deltas expected in a pure tool_use turn");
    })
    .await
    .expect("stream should succeed");

    assert_eq!(
        turn.content,
        vec![ContentBlock::ToolUse {
            id: "toolu_01".to_string(),
            name: "add".to_string(),
            input: serde_json::json!({"a": 2, "b": 3}),
        }]
    );
    assert_eq!(turn.stop_reason, "tool_use");

    // A thinking block, always first when present, followed by the
    // actual reply — `on_delta` should only ever see the text half.
    let thinking_mock_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"think.\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"42\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    let mut deltas = Vec::new();
    let turn =
        run_against_mock_upstream(thinking_mock_body, |delta| deltas.push(delta.to_string()))
            .await
            .expect("stream should succeed");

    assert_eq!(
        deltas,
        vec!["42".to_string()],
        "on_delta should only fire for the text block, never the thinking block"
    );
    assert_eq!(
        turn.content,
        vec![
            ContentBlock::Thinking {
                thinking: "Let me think.".to_string(),
                signature: "sig123".to_string(),
            },
            ContentBlock::Text {
                text: "42".to_string()
            },
        ]
    );
    assert_eq!(turn.stop_reason, "end_turn");
}

#[tokio::test]
async fn test_stream_anthropic_message_captures_real_usage_from_mock_upstream() {
    let mock_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"usage\":{\"input_tokens\":2095,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":5,\"output_tokens\":3}}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":45}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    let turn = run_against_mock_upstream(mock_body, |_| {})
        .await
        .expect("stream should succeed");

    // input_tokens/cache fields come from message_start; output_tokens
    // is message_delta's final value, not message_start's placeholder 3.
    assert_eq!(
        turn.usage,
        TokenUsage {
            input_tokens: 2095,
            output_tokens: 45,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 5,
        }
    );
}

/// llama.cpp's reply: thinking, commentary and a tool call, each block
/// opened without closing the one before, every stop at the end.
const LLAMA_CPP_STREAM: &str = include_str!("../../fixtures/llama_cpp_messages_stream.sse");

/// SME-112: commentary before a tool call was lost on llama.cpp, whose
/// blocks overlap. Every block is kept, in index order.
#[tokio::test]
async fn test_overlapping_blocks_from_llama_cpp_are_all_kept() {
    let mut deltas = Vec::new();
    let turn = run_against_mock_upstream(LLAMA_CPP_STREAM, |d| deltas.push(d.to_string()))
        .await
        .expect("stream should succeed");

    assert_eq!(deltas.concat(), "Let me check your todo list.");
    assert_eq!(
        turn.content,
        vec![
            ContentBlock::Thinking {
                thinking: "The user wants their todos.".to_string(),
                signature: String::new(),
            },
            ContentBlock::Text {
                text: "Let me check your todo list.".to_string()
            },
            ContentBlock::ToolUse {
                id: "k8ZpQ2wLm4RtY7vXc1NbJ6dHs9FgA3eU".to_string(),
                name: "todoread".to_string(),
                input: serde_json::json!({}),
            },
        ]
    );
    assert_eq!(turn.stop_reason, "tool_use");
    assert_eq!(turn.usage.input_tokens, 1834);
    assert_eq!(turn.usage.output_tokens, 27);
}

/// Two tool calls open at once, as llama.cpp sends them: each keeps
/// its own input.
#[tokio::test]
async fn test_two_overlapping_tool_calls_are_both_kept() {
    let [start_a, input_a] = tool_at(1, "call_a", r#"{"a":1}"#);
    let [start_b, input_b] = tool_at(2, "call_b", r#"{"a":2}"#);
    let [delta, stop] = tool_use_end();
    let body = sse(&[
        text_at(0, "Two sums."),
        start_a,
        input_a,
        start_b,
        input_b,
        stop_at(0),
        stop_at(1),
        stop_at(2),
        delta,
        stop,
    ]);
    let turn = run_against_mock_upstream(body, |_| {}).await.expect("stream should succeed");
    assert_eq!(turn.content, vec![text("Two sums."), add_call("call_a", 1), add_call("call_b", 2)]);
}

/// The reply's content is in index order, not the order its stops
/// came in.
#[tokio::test]
async fn test_blocks_stopped_out_of_order_are_kept_in_index_order() {
    let [start, input] = tool_at(1, "call_a", r#"{"a":1}"#);
    let [delta, stop] = tool_use_end();
    let body = sse(&[text_at(0, "Adding."), start, input, stop_at(1), stop_at(0), delta, stop]);
    let turn = run_against_mock_upstream(body, |_| {}).await.expect("stream should succeed");
    assert_eq!(turn.content, vec![text("Adding."), add_call("call_a", 1)]);
}

/// A stream that ends with no stops at all keeps every block.
#[tokio::test]
async fn test_blocks_never_stopped_are_kept_at_the_end_of_the_stream() {
    let [start, input] = tool_at(2, "call_a", r#"{"a":1}"#);
    let [delta, stop] = tool_use_end();
    let thinking = serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Hm."}});
    let body = sse(&[thinking, text_at(1, "Adding."), start, input, delta, stop]);
    let turn = run_against_mock_upstream(body, |_| {}).await.expect("stream should succeed");
    assert_eq!(
        turn.content,
        vec![
            ContentBlock::Thinking {
                thinking: "Hm.".to_string(),
                signature: String::new()
            },
            text("Adding."),
            add_call("call_a", 1),
        ]
    );
}

/// Ollama's order (`StreamConverter.Process`): each block started and
/// stopped before the next starts.
#[tokio::test]
async fn test_sequential_blocks_from_ollama_are_kept() {
    let [start, input] = tool_at(1, "call_a", r#"{"a":1}"#);
    let [delta, stop] = tool_use_end();
    let body = sse(&[
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "output_tokens": 0}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        text_at(0, "Add"),
        text_at(0, "ing."),
        stop_at(0),
        start,
        input,
        stop_at(1),
        delta,
        stop,
    ]);
    let turn = run_against_mock_upstream(body, |_| {}).await.expect("stream should succeed");
    assert_eq!(turn.content, vec![text("Adding."), add_call("call_a", 1)]);
}

/// A tool call whose input never parses still fails the call, whether
/// its stop came or the stream just ended.
#[tokio::test]
async fn test_an_unparseable_tool_input_still_fails_the_call() {
    for with_stop in [true, false] {
        let [start, input] = tool_at(0, "call_a", r#"{"a":"#);
        let [delta, stop] = tool_use_end();
        let mut events = vec![start, input];
        if with_stop {
            events.push(stop_at(0));
        }
        events.extend([delta, stop]);
        let err = run_against_mock_upstream(sse(&events), |_| {})
            .await
            .expect_err("a broken tool input fails");
        assert!(err.contains("failed to parse tool_use input JSON for add"), "{err}");
    }
}
