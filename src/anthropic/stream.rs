//! Streaming client for the real Anthropic Messages API (`stream: true`).
//! Distinct from and unrelated to Dioxus's own `ServerEvents` transport
//! between the browser and this server — this module only talks to Anthropic.

use futures_util::StreamExt;
use serde_json::Value;

use super::types::CreateMessageRequest;

/// Upstream base URL, overridable via `ANTHROPIC_BASE_URL` (used by tests to
/// point at a mock; also handy for routing through an API-compatible gateway).
fn anthropic_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string())
}

/// A single interpreted Anthropic SSE payload, reduced to what
/// `stream_anthropic_message` needs to act on. Anthropic's stream carries
/// several event types (message_start, content_block_start,
/// content_block_delta, content_block_stop, message_delta, message_stop) —
/// only text deltas and errors matter for v1, everything else is ignored.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutcome {
    TextDelta(String),
    Error(String),
    Ignored,
}

/// Parse one decoded JSON payload from an Anthropic SSE `data:` line.
/// Pure and synchronous — unit-testable without any network access.
fn interpret_stream_event(value: &Value) -> StreamOutcome {
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            let is_text_delta = value
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
                == Some("text_delta");
            if is_text_delta {
                let text = value
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                StreamOutcome::TextDelta(text.to_string())
            } else {
                StreamOutcome::Ignored
            }
        }
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

/// Bound on how long we'll wait for the *next* chunk from Anthropic before
/// giving up — a dropped upstream connection must not hang the caller's
/// background task forever. Applied per-chunk, not to the whole stream, since
/// a long response is expected to take a while overall.
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Stream a message from the real Anthropic API, calling `on_delta` for each
/// text chunk as it arrives and returning the fully assembled text once the
/// stream ends. `request.stream` should be `true`.
pub async fn stream_anthropic_message(
    api_key: &str,
    request: &CreateMessageRequest,
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", anthropic_base_url()))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(request)
        .send()
        .await
        .map_err(|e| format!("Claude API request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Anthropic API error {status}: {body}"));
    }

    let mut byte_stream = response.bytes_stream();
    let mut line_buffer = String::new();
    let mut assembled = String::new();

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
                    assembled.push_str(&text);
                }
                StreamOutcome::Error(message) => return Err(message),
                StreamOutcome::Ignored => {}
            }
        }
    }

    Ok(assembled)
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
    fn test_interpret_non_text_delta_is_ignored() {
        let value = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{}"}
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

    #[tokio::test]
    async fn test_stream_anthropic_message_assembles_deltas_from_mock_upstream() {
        let mock_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\"}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

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

        // ANTHROPIC_BASE_URL is process-global; this is the only test that
        // sets it, so parallel test threads can't observe a torn value.
        unsafe { std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}")) };

        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
        };

        let mut deltas = Vec::new();
        let assembled = stream_anthropic_message("test-key", &request, |delta| {
            deltas.push(delta.to_string());
        })
        .await
        .expect("stream should succeed");

        assert_eq!(deltas, vec!["Hel".to_string(), "lo!".to_string()]);
        assert_eq!(assembled, "Hello!");
    }
}
