//! SME-178: the usage meter a model call's stream keeps.

use super::*;

/// Streams `body` from a mock upstream into a meter, and returns both.
async fn run_metered(body: &'static str) -> (Result<StreamedTurn, String>, UsageMeter) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || async move { ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body) }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let request = CreateMessageRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![],
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        chat_template_kwargs: None,
    };
    let meter = UsageMeter::default();
    let result = stream_anthropic_message_metered(&test_endpoint(addr), &request, |_| {}, &meter).await;
    (result, meter)
}

fn start(input: i64, cache_read: i64) -> serde_json::Value {
    serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": input, "cache_read_input_tokens": cache_read, "output_tokens": 1}}})
}

fn failed() -> serde_json::Value {
    serde_json::json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
}

/// A call failing mid-stream: its input from `message_start`, its output
/// estimated from the text, thinking and tool input that streamed (4
/// bytes a token); a thinking signature isn't output, and isn't counted.
#[tokio::test]
async fn test_a_failed_calls_output_is_estimated_from_what_streamed() {
    let [tool_start, tool_input] = tool_at(2, "call_a", r#"{"a":12345}"#);
    let body = sse(&[
        start(300, 50),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Hmm, twelve."}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "x".repeat(400)}}),
        text_at(1, "Adding it up."),
        tool_start,
        tool_input,
        failed(),
    ]);
    let (result, meter) = run_metered(body).await;
    assert_eq!(result.expect_err("the stream failed"), "Overloaded");
    // 12 + 13 + 11 bytes.
    let known = meter.known().expect("something is known");
    assert_eq!(
        (known.input_tokens, known.cache_read_input_tokens, known.output_tokens),
        (300, 50, 9)
    );
}

/// Output a `message_delta` reported is used as is, not the estimate.
#[tokio::test]
async fn test_a_reported_output_count_beats_the_estimate() {
    let body = sse(&[
        start(300, 0),
        text_at(0, &"a".repeat(400)),
        serde_json::json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 7}}),
        failed(),
    ]);
    let (_, meter) = run_metered(body).await;
    assert_eq!(meter.known().map(|u| u.output_tokens), Some(7));
}

/// A call that ends before anything is known leaves nothing to record;
/// one with only text streamed (no `message_start` usage) has its
/// output estimated.
#[tokio::test]
async fn test_nothing_known_is_none() {
    let (_, meter) = run_metered(sse(&[serde_json::json!({"type": "message_start"}), failed()])).await;
    assert_eq!(meter.known(), None);
    let (_, meter) = run_metered(sse(&[serde_json::json!({"type": "message_start"}), text_at(0, "abcdefgh"), failed()])).await;
    assert_eq!(meter.known(), Some(TokenUsage { output_tokens: 2, ..TokenUsage::default() }));
}

/// A finished call's usage is the meter's, as the stream reported it.
#[tokio::test]
async fn test_a_finished_calls_usage_is_the_meters() {
    let body = sse(&[
        start(300, 0),
        text_at(0, "Hi."),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
    ]);
    let (result, meter) = run_metered(body).await;
    let turn = result.expect("the stream finished");
    assert_eq!(turn.usage, TokenUsage { input_tokens: 300, output_tokens: 2, ..TokenUsage::default() });
    assert_eq!(meter.known(), Some(turn.usage));
}
