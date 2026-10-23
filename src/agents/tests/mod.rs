//! Child agents' tests: each runs against a mock upstream and a database
//! of its own, with conversation ids moved clear of other tests' (turn
//! state is process-wide, keyed by id).

use super::*;
use crate::anthropic::ContentBlock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod start;
mod tools;

/// Ids clear of other tests', and a mock upstream answering every request
/// with `bodies` in order (the last repeated); returns the request log.
async fn setup(pool: &PgPool, bodies: Vec<String>) -> Arc<Mutex<Vec<serde_json::Value>>> {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    mock_upstream(pool, bodies).await
}

/// A mock Anthropic upstream that answers with `bodies` in order (the last
/// repeated) and keeps every request, saved as the default provider.
async fn mock_upstream(pool: &PgPool, bodies: Vec<String>) -> Arc<Mutex<Vec<serde_json::Value>>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |request: String| {
            let recorded = recorded.clone();
            let bodies = bodies.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&request).expect("a JSON request body");
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

fn sse_body(events: &[(&str, &str)]) -> String {
    events.iter().map(|(event, data)| format!("event: {event}\ndata: {data}\n\n")).collect()
}

/// A reply that's only `text`.
fn text_reply(text: &str) -> String {
    let delta = serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}});
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        ("content_block_delta", &delta.to_string()),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// Polls `check` until it holds, for up to 10 s.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(std::time::Instant::now() < deadline, "{what}: not within 10 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The text of every message in `id`, in order, as `(role, text)`.
async fn transcript(pool: &PgPool, id: i64) -> Vec<(String, String)> {
    db::list_messages(pool, id)
        .await
        .expect("list messages")
        .into_iter()
        .map(|m| {
            let text = m
                .blocks()
                .expect("valid blocks")
                .into_iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            (m.role, text)
        })
        .collect()
}

/// The agent's `state`.
async fn state(pool: &PgPool, id: i64) -> Option<String> {
    db::agent_state(pool, id).await.expect("read the agent's state")
}

/// Calls `start_agent` from `parent` with `brief` and returns the child's id.
async fn start_child(pool: &PgPool, parent: i64, brief: &str) -> i64 {
    let result = crate::anthropic::tools::execute(pool, parent, "toolu_s", "start_agent", &serde_json::json!({"brief": brief}))
        .await
        .expect("start_agent succeeds");
    child_of(pool, parent, &result).await
}

/// The newest child of `parent`, checked against `start_agent`'s result.
async fn child_of(pool: &PgPool, parent: i64, result: &str) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("SELECT id FROM conversations WHERE parent_id = $1 ORDER BY id DESC LIMIT 1")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("the child's row");
    assert!(result.contains(&format!("agent #{child}")), "{result}");
    child
}
