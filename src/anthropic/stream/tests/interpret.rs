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

#[test]
fn test_interpret_message_delta_extracts_stop_reason_and_output_tokens() {
    let value = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "tool_use", "stop_sequence": null},
        "usage": {"output_tokens": 12}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::StopReason {
            reason: "tool_use".to_string(),
            output_tokens: Some(12)
        }
    );
}

#[test]
fn test_interpret_message_delta_without_usage_reports_no_output_tokens() {
    let value = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null}
    });
    assert_eq!(
        interpret_stream_event(&value),
        StreamOutcome::StopReason {
            reason: "end_turn".to_string(),
            output_tokens: None
        }
    );
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
