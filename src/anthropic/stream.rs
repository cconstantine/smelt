//! Streaming client for the real Anthropic Messages API (`stream: true`).
//! Distinct from and unrelated to Dioxus's own `ServerEvents` transport
//! between the browser and this server — this module only talks to Anthropic.

use futures_util::StreamExt;
use serde_json::Value;

use super::types::{ContentBlock, CreateMessageRequest};

/// The fully assembled result of one streamed Anthropic turn: every content
/// block (text and/or tool_use) in order, plus the `stop_reason` that says
/// whether this turn is final or wants a tool run (`"tool_use"`).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamedTurn {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
}

/// Upstream base URL, overridable via `ANTHROPIC_BASE_URL` (used by tests to
/// point at a mock; also handy for routing through an API-compatible gateway).
fn anthropic_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".to_string())
}

/// A single interpreted Anthropic SSE payload, reduced to what
/// `stream_anthropic_message` needs to act on. Anthropic's stream carries
/// several event types (message_start, content_block_start,
/// content_block_delta, content_block_stop, message_delta, message_stop) —
/// only text deltas and errors matter for v1, everything else is ignored.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutcome {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta(String),
    BlockStop,
    StopReason(String),
    Error(String),
    Ignored,
}

/// Parse one decoded JSON payload from an Anthropic SSE `data:` line.
/// Pure and synchronous — unit-testable without any network access.
fn interpret_stream_event(value: &Value) -> StreamOutcome {
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_start") => {
            let block = value.get("content_block");
            let is_tool_use =
                block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use");
            if is_tool_use {
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                StreamOutcome::ToolUseStart {
                    id: id.to_string(),
                    name: name.to_string(),
                }
            } else {
                StreamOutcome::Ignored
            }
        }
        Some("content_block_delta") => {
            match value
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
            {
                Some("text_delta") => {
                    let text = value
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::TextDelta(text.to_string())
                }
                Some("input_json_delta") => {
                    let partial_json = value
                        .get("delta")
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ToolUseInputDelta(partial_json.to_string())
                }
                _ => StreamOutcome::Ignored,
            }
        }
        Some("content_block_stop") => StreamOutcome::BlockStop,
        Some("message_delta") => match value
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(Value::as_str)
        {
            Some(reason) => StreamOutcome::StopReason(reason.to_string()),
            None => StreamOutcome::Ignored,
        },
        Some("error") => {
            let message = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error from Anthropic");
            StreamOutcome::Error(message.to_string())
        }
        _ => StreamOutcome::Ignored,
    }
}

/// Accumulates one in-progress content block across its
/// `content_block_start` → `content_block_delta`* → `content_block_stop`
/// events. Anthropic emits blocks sequentially, never interleaved, so
/// tracking a single "current" accumulator (rather than a map keyed by
/// block index) is sufficient.
enum PartialBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
}

impl PartialBlock {
    fn finalize(self) -> Result<ContentBlock, String> {
        match self {
            PartialBlock::Text(text) => Ok(ContentBlock::Text { text }),
            PartialBlock::ToolUse {
                id,
                name,
                partial_json,
            } => {
                let input = if partial_json.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&partial_json).map_err(|e| {
                        format!("failed to parse tool_use input JSON for {name}: {e}")
                    })?
                };
                Ok(ContentBlock::ToolUse { id, name, input })
            }
        }
    }
}

/// Bound on how long we'll wait for the *next* chunk from Anthropic before
/// giving up — a dropped upstream connection must not hang the caller's
/// background task forever. Applied per-chunk, not to the whole stream, since
/// a long response is expected to take a while overall.
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Bound on how long we'll wait for Anthropic to even start responding —
/// TCP connect through receiving the response headers — before giving up.
/// Distinct from `CHUNK_TIMEOUT`, which only bounds the gap between chunks
/// once a response is already streaming; this call had no bound at all
/// before (see the tool-use-round-trip retrospective: a stalled connect
/// here is indistinguishable from the caller just hanging forever — and
/// since a `run_async` task with `stream_output: true` awaits exactly this
/// call once per line before it can continue, that manifested as the
/// wrapped tool looking permanently "stuck" rather than as a visible
/// error).
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Sends the request and waits for Anthropic's response headers, bounded by
/// `response_timeout` — factored out from `stream_anthropic_message` so a
/// test can exercise the timeout with a short duration instead of the real
/// `RESPONSE_TIMEOUT`.
async fn send_and_await_response(
    api_key: &str,
    request: &CreateMessageRequest,
    base_url: &str,
    response_timeout: std::time::Duration,
) -> Result<reqwest::Response, String> {
    tokio::time::timeout(
        response_timeout,
        reqwest::Client::new()
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(request)
            .send(),
    )
    .await
    .map_err(|_| "timed out waiting for Anthropic to respond".to_string())?
    .map_err(|e| format!("Claude API request failed: {e}"))
}

/// Stream a message from the real Anthropic API, calling `on_delta` for each
/// text chunk as it arrives (live typing effect) and returning every content
/// block — text and/or tool_use — plus the turn's `stop_reason` once the
/// stream ends. Tool-use blocks accumulate silently; `on_delta` only ever
/// fires for text. `request.stream` should be `true`.
pub async fn stream_anthropic_message(
    api_key: &str,
    request: &CreateMessageRequest,
    mut on_delta: impl FnMut(&str),
) -> Result<StreamedTurn, String> {
    let response =
        send_and_await_response(api_key, request, &anthropic_base_url(), RESPONSE_TIMEOUT).await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Anthropic API error {status}: {body}"));
    }

    let mut byte_stream = response.bytes_stream();
    let mut line_buffer = String::new();
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut current: Option<PartialBlock> = None;
    let mut stop_reason = String::new();

    loop {
        let next = tokio::time::timeout(CHUNK_TIMEOUT, byte_stream.next())
            .await
            .map_err(|_| "timed out waiting for the next chunk from Anthropic".to_string())?;

        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| format!("error reading Anthropic response stream: {e}"))?;
        line_buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = line_buffer.find('\n') {
            let line: String = line_buffer.drain(..=newline_pos).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match interpret_stream_event(&value) {
                StreamOutcome::TextDelta(text) => {
                    on_delta(&text);
                    match &mut current {
                        Some(PartialBlock::Text(existing)) => existing.push_str(&text),
                        _ => current = Some(PartialBlock::Text(text)),
                    }
                }
                StreamOutcome::ToolUseStart { id, name } => {
                    current = Some(PartialBlock::ToolUse {
                        id,
                        name,
                        partial_json: String::new(),
                    });
                }
                StreamOutcome::ToolUseInputDelta(partial_json) => {
                    if let Some(PartialBlock::ToolUse {
                        partial_json: existing,
                        ..
                    }) = &mut current
                    {
                        existing.push_str(&partial_json);
                    }
                }
                StreamOutcome::BlockStop => {
                    if let Some(block) = current.take() {
                        content.push(block.finalize()?);
                    }
                }
                StreamOutcome::StopReason(reason) => stop_reason = reason,
                StreamOutcome::Error(message) => return Err(message),
                StreamOutcome::Ignored => {}
            }
        }
    }

    if let Some(block) = current.take() {
        content.push(block.finalize()?);
    }

    Ok(StreamedTurn {
        content,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
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
            "delta": {"type": "thinking_delta", "thinking": "..."}
        });
        assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
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
    fn test_interpret_message_delta_extracts_stop_reason() {
        let value = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use", "stop_sequence": null},
            "usage": {"output_tokens": 12}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::StopReason("tool_use".to_string())
        );
    }

    /// Spins up a throwaway mock upstream returning `mock_body` verbatim and
    /// points `ANTHROPIC_BASE_URL` (process-global) at it, then runs
    /// `stream_anthropic_message` against it. Holds the shared
    /// `test_support` lock for the duration, since `ANTHROPIC_BASE_URL` is
    /// process-global and other test files (`api::chat`) mutate it too.
    async fn run_against_mock_upstream(
        mock_body: &'static str,
        on_delta: impl FnMut(&str),
    ) -> Result<StreamedTurn, String> {
        let _guard = crate::anthropic::test_support::lock_anthropic_base_url();
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

        unsafe { std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}")) };

        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
        };

        stream_anthropic_message("test-key", &request, on_delta).await
    }

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
    }

    /// Regression test for the tool-use-round-trip retrospective's hung
    /// live model call: before `RESPONSE_TIMEOUT` existed, a connection
    /// that never got a response at all (accepted, then silence) hung
    /// `stream_anthropic_message` forever. Accepts the connection but never
    /// writes a response, so `send_and_await_response` has nothing to read
    /// — a short `response_timeout` (not the real `RESPONSE_TIMEOUT`, which
    /// would make this test take 90 real seconds) must still make it
    /// return promptly rather than hang.
    #[tokio::test]
    async fn test_send_and_await_response_times_out_when_upstream_never_responds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Hold the connection open without ever writing a response,
                // for comfortably longer than the test's own timeout below.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                drop(stream);
            }
        });

        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send_and_await_response(
                "test-key",
                &request,
                &format!("http://{addr}"),
                std::time::Duration::from_millis(50),
            ),
        )
        .await
        .expect("send_and_await_response itself should not hang past its own timeout");

        match result {
            Err(e) => assert_eq!(e, "timed out waiting for Anthropic to respond"),
            Ok(_) => panic!("expected a timeout error, got a response"),
        }
    }
}
