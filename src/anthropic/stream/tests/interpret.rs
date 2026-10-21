use super::*;

#[test]
fn test_interpret_text_delta_extracts_text() {
    let value = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": "Hello"}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::TextDelta("Hello".to_string())
    );
}

#[test]
fn test_interpret_unrecognized_delta_type_is_ignored() {
    let value = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "some_future_delta_type", "thinking": "..."}
    });
    assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
}

#[test]
fn test_interpret_thinking_delta_extracts_thinking_text() {
    let value = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "thinking_delta", "thinking": "Let me consider..."}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::ThinkingDelta("Let me consider...".to_string())
    );
}

#[test]
fn test_interpret_signature_delta_extracts_signature() {
    let value = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "signature_delta", "signature": "abc123"}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::ThinkingSignatureDelta("abc123".to_string())
    );
}

#[test]
fn test_interpret_message_stop_is_ignored() {
    let value = serde_json::json!({"type": "message_stop"});
    assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
}

#[test]
fn test_interpret_error_event_extracts_message() {
    let value = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::Error("Overloaded".to_string())
    );
}

#[test]
fn test_interpret_content_block_start_tool_use_captures_id_and_name() {
    let value = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "tool_use", "id": "toolu_01", "name": "add", "input": {}}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::ToolUseStart {
            id: "toolu_01".to_string(),
            name: "add".to_string()
        }
    );
}

#[test]
fn test_interpret_content_block_start_text_is_ignored() {
    let value = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "text", "text": ""}
    });
    assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
}

#[test]
fn test_interpret_input_json_delta_extracts_partial_json() {
    let value = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::ToolUseInputDelta("{\"a\":".to_string())
    );
}

#[test]
fn test_interpret_content_block_stop_returns_block_stop() {
    let value = serde_json::json!({"type": "content_block_stop", "index": 0});
    assert_eq!(interpret_stream_event(&value), StreamOutcome::BlockStop);
}

fn delta_usage(input: Option<i64>, output: Option<i64>, creation: Option<i64>, read: Option<i64>) -> DeltaUsage {
    DeltaUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: creation,
        cache_read_input_tokens: read,
    }
}

fn delta(stop_reason: Option<&str>, usage: DeltaUsage) -> StreamOutcome {
    StreamOutcome::Delta { stop_reason: stop_reason.map(str::to_string), usage }
}

/// Anthropic's plain reply: only `output_tokens` in the delta.
#[test]
fn test_interpret_message_delta_extracts_stop_reason_and_output_tokens() {
    let value = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "tool_use", "stop_sequence": null},
        "usage": {"output_tokens": 12}
    });
    assert_eq!(interpret_stream_event(&value), delta(Some("tool_use"), delta_usage(None, Some(12), None, None)));
}

#[test]
fn test_interpret_message_delta_without_usage_reports_no_output_tokens() {
    let value = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null}
    });
    assert_eq!(interpret_stream_event(&value), delta(Some("end_turn"), DeltaUsage::default()));
}

/// Ollama's `MessageDeltaEvent` (`anthropic/anthropic.go` at `2cd63f7`):
/// `DeltaUsage`'s `input_tokens`, `cache_read_input_tokens` (omitted when
/// Ollama has no cached count) and `output_tokens`.
#[test]
fn test_interpret_ollamas_message_delta_reads_its_input_and_cache_counts() {
    let value = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": {"input_tokens": 700, "cache_read_input_tokens": 2000, "output_tokens": 42}
    });
    assert_eq!(
        interpret_stream_event(&value),
        delta(Some("end_turn"), delta_usage(Some(700), Some(42), None, Some(2000)))
    );
    let uncached = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": {"input_tokens": 700, "output_tokens": 42}
    });
    assert_eq!(
        interpret_stream_event(&uncached),
        delta(Some("end_turn"), delta_usage(Some(700), Some(42), None, None)),
        "a missing cache count is None, not 0"
    );
}

/// LiteLLM's delta (`translate_openai_usage_to_anthropic_usage_delta`):
/// the real input, cache fields only when non-zero; and its placeholder.
#[test]
fn test_interpret_litellms_message_deltas() {
    let real = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {"input_tokens": 1500, "output_tokens": 30, "cache_creation_input_tokens": 12}
    });
    assert_eq!(
        interpret_stream_event(&real),
        delta(Some("end_turn"), delta_usage(Some(1500), Some(30), Some(12), None))
    );
    let placeholder = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {"input_tokens": 0, "output_tokens": 0}
    });
    assert_eq!(
        interpret_stream_event(&placeholder),
        delta(Some("end_turn"), delta_usage(Some(0), Some(0), None, None))
    );
}

/// Anthropic's server-tool delta: cumulative input and cache counts.
#[test]
fn test_interpret_anthropics_server_tool_message_delta() {
    let value = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {
            "input_tokens": 10682,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "output_tokens": 510,
            "server_tool_use": {"web_search_requests": 1}
        }
    });
    assert_eq!(
        interpret_stream_event(&value),
        delta(Some("end_turn"), delta_usage(Some(10682), Some(510), Some(0), Some(0)))
    );
}

/// A `message_delta` with usage and no `stop_reason` is read, not
/// ignored; one with neither is.
#[test]
fn test_interpret_message_delta_without_a_stop_reason() {
    let value = serde_json::json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 7}});
    assert_eq!(interpret_stream_event(&value), delta(None, delta_usage(None, Some(7), None, None)));
    let empty = serde_json::json!({"type": "message_delta", "delta": {}});
    assert_eq!(interpret_stream_event(&empty), StreamOutcome::Ignored);
}

/// The rule: non-zero input-side counts replace `message_start`'s, each
/// missing one kept; all zero or missing keeps them all.
#[test]
fn test_a_delta_replaces_the_input_side_only_when_it_has_a_non_zero_count() {
    let start = TokenUsage { input_tokens: 812, output_tokens: 0, cache_creation_input_tokens: 5, cache_read_input_tokens: 9 };
    let applied = |d: DeltaUsage| {
        let mut usage = start;
        d.apply_to(&mut usage);
        (usage.input_tokens, usage.output_tokens, usage.cache_creation_input_tokens, usage.cache_read_input_tokens)
    };
    assert_eq!(applied(delta_usage(Some(700), Some(42), None, Some(2000))), (700, 42, 5, 2000), "Ollama");
    assert_eq!(applied(delta_usage(Some(0), Some(42), None, Some(2700))), (0, 42, 5, 2700), "fully cached");
    assert_eq!(applied(delta_usage(Some(0), Some(0), None, None)), (812, 0, 5, 9), "placeholder zeros");
    assert_eq!(applied(delta_usage(None, Some(15), None, None)), (812, 15, 5, 9), "output only");
    assert_eq!(applied(delta_usage(None, None, Some(3), None)), (812, 0, 3, 9), "one cache count");
    assert_eq!(applied(DeltaUsage::default()), (812, 0, 5, 9), "nothing");
}

#[test]
fn test_interpret_message_start_extracts_initial_usage() {
    let value = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_01",
            "usage": {
                "input_tokens": 2095,
                "cache_creation_input_tokens": 10,
                "cache_read_input_tokens": 5,
                "output_tokens": 3
            }
        }
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::InitialUsage(TokenUsage {
            input_tokens: 2095,
            output_tokens: 3,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 5,
        })
    );
}

#[test]
fn test_interpret_message_start_without_usage_is_ignored() {
    let value = serde_json::json!({"type": "message_start", "message": {"id": "msg_01"}});
    assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
}
