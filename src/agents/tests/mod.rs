//! Child agents' tests: each runs against a mock upstream and a database
//! of its own, with conversation ids moved clear of other tests' (turn
//! state is process-wide, keyed by id).

use super::*;
use crate::anthropic::ContentBlock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod delete;
mod ended;
mod list;
mod pause;
mod persona;
mod reports;
pub(super) mod repo;
mod start;
mod stop;
mod sweep;
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
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    mock_with(pool, move |_| {
        let i = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let body = bodies[i.min(bodies.len() - 1)].clone();
        Box::pin(async move { body })
    })
    .await
}

type Reply = std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>;

/// A mock Anthropic upstream whose reply to each request is `answer`'s,
/// which may wait; keeps every request, saved as the default provider.
async fn mock_with(
    pool: &PgPool,
    answer: impl Fn(&serde_json::Value) -> Reply + Send + Sync + 'static,
) -> Arc<Mutex<Vec<serde_json::Value>>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let answer = Arc::new(answer);
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |request: String| {
            let recorded = recorded.clone();
            let answer = answer.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&request).expect("a JSON request body");
                let reply = answer(&parsed);
                recorded.lock().expect("the request log").push(parsed);
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], reply.await)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    requests
}

/// The agent a request is for, read from its system prompt; `None` for a
/// conversation without a parent.
fn agent_of(request: &serde_json::Value) -> Option<i64> {
    let system = request["system"].to_string();
    let rest = &system[system.find("You are agent #")? + "You are agent #".len()..];
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// The requests made for `agent` (`None`: for conversations without a parent).
fn requests_for(requests: &Mutex<Vec<serde_json::Value>>, agent: Option<i64>) -> Vec<serde_json::Value> {
    requests.lock().expect("the request log").iter().filter(|r| agent_of(r) == agent).cloned().collect()
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

/// A reply that calls `name` with `input` (id `toolu_<name>`).
fn tool_call_reply(name: &str, input: serde_json::Value) -> String {
    let start = serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "tool_use", "id": format!("toolu_{name}"), "name": name, "input": {}}
    });
    let delta = serde_json::json!({
        "type": "content_block_delta", "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": input.to_string()}
    });
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        ("content_block_start", &start.to_string()),
        ("content_block_delta", &delta.to_string()),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
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

/// What a restart does to `ids`: their turn state is forgotten, then the
/// start restores the stored pauses and runs the startup sweep, whose
/// wakes this awaits.
async fn restart(pool: &PgPool, ids: &[i64]) {
    for id in ids {
        crate::turn::forget_conversation_lock(*id);
    }
    crate::turn::restore_pauses(pool).await;
    for wake in startup_sweep(pool).await {
        tokio::time::timeout(Duration::from_secs(30), wake).await.expect("the wake ends").expect("the wake");
    }
}

/// The conversation's stored pause.
async fn paused_at(pool: &PgPool, id: i64) -> Option<chrono::NaiveDateTime> {
    sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>("SELECT paused_at FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the conversation's row")
}

/// A child of `parent` made directly, `active` and started: no start and
/// no turn has run in it.
async fn active_child(pool: &PgPool, parent: i64) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at) VALUES ($1, $2, 1, 'b', 'active', now())")
        .bind(child)
        .bind(parent)
        .execute(pool)
        .await
        .expect("its agent row");
    child
}
