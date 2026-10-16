use super::*;
use crate::api::chat::*;
use crate::events;
use dioxus::fullstack::ServerEvents;
use axum::response::IntoResponse;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::providers::test_support::lock_turn_tests;

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

fn text_block(text: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::Text {
        text: text.to_string(),
    }
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

async fn next_turn_state(
    rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
) -> Option<bool> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::TurnState { running, .. }) => return Some(running),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

fn hello() -> anthropic::AnthropicMessage {
    anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "hello".to_string(),
        }],
    }
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

mod history;
mod budget;
mod streams;
mod providers;
mod wake;
mod prompt;
mod model_calls;
mod cut_off;
mod stop;
mod run;
mod live;
mod tools;
mod ask_user;
mod spans;
mod lifecycle;
