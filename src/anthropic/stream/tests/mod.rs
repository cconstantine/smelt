use super::*;

fn test_endpoint(addr: std::net::SocketAddr) -> Endpoint {
    Endpoint {
        base_url: format!("http://{addr}"),
        auth: Auth::ApiKey("test-key".to_string()),
    }
}

/// Spins up a throwaway mock upstream returning `mock_body` verbatim
/// and runs `stream_anthropic_message` against it.
async fn run_against_mock_upstream(
    mock_body: &'static str,
    on_delta: impl FnMut(&str),
) -> Result<StreamedTurn, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || async move {
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                mock_body,
            )
        }),
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

    stream_anthropic_message(&test_endpoint(addr), &request, on_delta).await
}

/// The body a paused provider actually returned during the bug bash,
/// relayed through an Anthropic-compatible router: the provider's
/// message is JSON inside the Anthropic error's own `message`.
const PAUSED_PROVIDER_BODY: &str = r#"{"type":"error","error":{"type":"api_error","message":"{\"error\":\"Provider 'featherless-ai' is currently failing for model 'Qwen/Qwen3.8-27B' and has been paused by the circuit breaker. Retry later or use another provider.\"}"}}"#;

const OK_BODY: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\"}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Serves `responses` in order (the last one repeats) and returns the
/// outcome plus how many requests were made.
async fn run_against_responses(
    responses: Vec<(u16, &'static str)>,
) -> (Result<StreamedTurn, String>, usize) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let served = count.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let responses = responses.clone();
            let served = served.clone();
            async move {
                let i = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (status, body) = responses[i.min(responses.len() - 1)];
                let content_type = if status == 200 { "text/event-stream" } else { "application/json" };
                (
                    axum::http::StatusCode::from_u16(status).expect("valid status"),
                    [(axum::http::header::CONTENT_TYPE, content_type)],
                    body,
                )
            }
        }),
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
    let result = stream_anthropic_message(&test_endpoint(addr), &request, |_| {}).await;
    (result, count.load(std::sync::atomic::Ordering::SeqCst))
}

const USAGE_BODY: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":7,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"the secret reply\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn chat_spans(spans: &[opentelemetry_sdk::trace::SpanData]) -> Vec<&opentelemetry_sdk::trace::SpanData> {
    spans.iter().filter(|span| span.name.starts_with("chat ")).collect()
}

/// `key`'s value on `span`, which must have it at most once.
fn span_attribute<'a>(span: &'a opentelemetry_sdk::trace::SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    let values: Vec<_> = span.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| &kv.value).collect();
    assert!(values.len() <= 1, "{key} recorded more than once: {values:?}");
    values.first().copied()
}
const LLAMA_CPP_CUT_OFF_STREAM: &str = include_str!("../../fixtures/llama_cpp_messages_stream_cut_off.sse");

/// An SSE body of `data` payloads, each with its `type` as the event
/// name, kept for the test's whole run like a fixture.
fn sse(data: &[serde_json::Value]) -> &'static str {
    let body: String = data
        .iter()
        .map(|d| format!("event: {}\ndata: {d}\n\n", d["type"].as_str().expect("a type")))
        .collect();
    Box::leak(body.into_boxed_str())
}

fn text_at(index: usize, text: &str) -> serde_json::Value {
    serde_json::json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}})
}

fn tool_at(index: usize, id: &str, input: &str) -> [serde_json::Value; 2] {
    [
        serde_json::json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": "add"}}),
        serde_json::json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": input}}),
    ]
}

fn stop_at(index: usize) -> serde_json::Value {
    serde_json::json!({"type": "content_block_stop", "index": index})
}

fn tool_use_end() -> [serde_json::Value; 2] {
    [
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        serde_json::json!({"type": "message_stop"}),
    ]
}

fn add_call(id: &str, a: i64) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: "add".to_string(),
        input: serde_json::json!({"a": a}),
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.to_string() }
}

mod endpoint;
mod interpret;
mod retries;
mod spans;
mod binding;
mod assembly;
mod cut_off;
