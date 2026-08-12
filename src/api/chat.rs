use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::models::{Conversation, Message};
use crate::{anthropic, events};

#[cfg(feature = "server")]
use crate::db;

#[cfg(feature = "server")]
use sqlx::PgPool;

#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{Arc, LazyLock, Mutex};

/// Events relayed to the browser over the `send_message` server function's
/// `ServerEvents` stream — distinct from Anthropic's own SSE event shapes,
/// which `anthropic::stream` already reduces down to plain text deltas.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    Delta {
        text: String,
    },
    Done {
        message_id: i64,
        role: String,
        content: String,
    },
    Error {
        message: String,
    },
}

#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/conversations")]
pub async fn create_conversation() -> ServerFnResult<Conversation> {
    db::create_conversation(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    db::list_messages(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

#[delete("/api/conversations/{id}")]
pub async fn delete_conversation(id: i64) -> ServerFnResult<()> {
    // Best-effort, unconditional (unlike terminate_pod, which the model
    // calls and which is guarded) — the conversation is going away
    // regardless, so nothing about the pod matters anymore either way.
    crate::sandbox::teardown_conversation(db::get(), id).await;
    db::delete_conversation(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

#[cfg(feature = "server")]
fn anthropic_model() -> String {
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claude-opus-4-8".to_string())
}

/// Bound on how many tool-use turns one `run_turn` call will chase before
/// giving up. A guessed default (see the plan's Open questions), not a
/// confirmed-settled value — clearly enough for `add`/`count`, clearly not
/// infinite. Exceeding it ends the turn with an error rather than looping
/// forever.
#[cfg(feature = "server")]
const MAX_TURNS: usize = 5;

/// A live `send_message` call and a background task's push-triggered
/// `run_turn` call (or two different tasks' pushes) can race for the same
/// conversation — Anthropic's strict user/assistant alternation breaks if
/// two writers persist a turn at once. Keyed by conversation id; which
/// caller acquires a given conversation's lock first when several are ready
/// is unspecified (see the plan's Open questions).
#[cfg(feature = "server")]
static CONVERSATION_LOCKS: LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "server")]
fn conversation_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = CONVERSATION_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(conversation_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Runs one full tool-use round trip for `conversation_id`: persists
/// `new_message`, then loops calling the real Anthropic API — executing any
/// tool the model asks for and persisting its result — until the model
/// produces a non-`tool_use` turn or `MAX_TURNS` is exceeded. Returns every
/// message persisted along the way, in order, starting with `new_message`
/// itself. `send_message` wires a live `on_delta` into the browser's SSE
/// stream for the token-by-token typing effect; a later stage's
/// background-task push notification calls this with `on_delta = None`.
/// Returns a boxed, type-erased future rather than using plain `async fn`
/// sugar: `run_turn` and `anthropic::tools::execute` call each other
/// (`execute`'s `run_async` branch spawns a task that can call back into
/// `run_turn` to push a notification, which calls `execute` again for the
/// *next* turn's tool calls) — that mutual recursion defeats rustc's
/// `Send`-auto-trait inference for plain `async fn`s ("cannot satisfy `impl
/// Future: Send`" with no useful location). Type-erasing one edge of the
/// cycle here breaks it.
#[cfg(feature = "server")]
pub(crate) fn run_turn<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: anthropic::AnthropicMessage,
    mut on_delta: Option<&'a mut (dyn FnMut(&str) + Send)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    Box::pin(async move {
        let lock = conversation_lock(conversation_id);
        let _guard = lock.lock().await;

        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ServerFnError::new("ANTHROPIC_API_KEY is not set on the server"))?;

        let mut persisted = Vec::new();
        let saved = db::create_message(
            pool,
            conversation_id,
            &new_message.role,
            &new_message.content,
        )
        .await
        .map_err(ServerFnError::new)?;
        persisted.push(saved);

        let mut history: Vec<anthropic::AnthropicMessage> =
            db::list_messages(pool, conversation_id)
                .await
                .map_err(ServerFnError::new)?
                .into_iter()
                .map(|m| {
                    let content = m.blocks().map_err(ServerFnError::new)?;
                    Ok(anthropic::AnthropicMessage {
                        role: m.role,
                        content,
                    })
                })
                .collect::<ServerFnResult<Vec<_>>>()?;

        for _ in 0..MAX_TURNS {
            // Checked at the top of every loop iteration, not just once per
            // `run_turn` call — this is what gives same-turn visibility: if
            // a command finishes partway through a turn's tool-calling
            // loop, the very next iteration already sees the notification,
            // without waiting for a fresh user message. See the plan's
            // "What" and "How" (the completion-notification design).
            for command in db::unnotified_finished_terminal_commands(pool, conversation_id)
                .await
                .map_err(ServerFnError::new)?
            {
                let text = if command.status == "finished" {
                    format!(
                        "Terminal command {} finished: exit code {}.",
                        command.command_id,
                        command
                            .exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    )
                } else {
                    format!(
                        "Terminal command {}'s outcome is unknown — the terminal became \
                         unreachable while it was running.",
                        command.command_id
                    )
                };
                let notification_content = vec![anthropic::ContentBlock::Text { text }];
                let saved = db::create_message(pool, conversation_id, "user", &notification_content)
                    .await
                    .map_err(ServerFnError::new)?;
                history.push(anthropic::AnthropicMessage {
                    role: "user".to_string(),
                    content: notification_content,
                });
                persisted.push(saved);
                db::mark_terminal_command_notified(pool, &command.command_id)
                    .await
                    .map_err(ServerFnError::new)?;
            }

            let request = anthropic::CreateMessageRequest {
                model: anthropic_model(),
                max_tokens: 4096,
                system: None,
                messages: history.clone(),
                stream: true,
                tools: anthropic::tools::tool_definitions(),
            };

            let turn = anthropic::stream::stream_anthropic_message(&api_key, &request, |delta| {
                if let Some(cb) = on_delta.as_deref_mut() {
                    cb(delta);
                }
            })
            .await
            .map_err(ServerFnError::new)?;

            let saved = db::create_message(pool, conversation_id, "assistant", &turn.content)
                .await
                .map_err(ServerFnError::new)?;
            history.push(anthropic::AnthropicMessage {
                role: "assistant".to_string(),
                content: turn.content.clone(),
            });
            persisted.push(saved);

            if turn.stop_reason != "tool_use" {
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::MessagesAppended(persisted.clone()),
                );
                return Ok(persisted);
            }

            let mut result_blocks = Vec::new();
            for block in &turn.content {
                if let anthropic::ContentBlock::ToolUse { id, name, input } = block {
                    let result =
                        anthropic::tools::execute(pool, conversation_id, id, name, input).await;
                    let (content, is_error) = match result {
                        Ok(output) => (output, None),
                        Err(message) => (message, Some(true)),
                    };
                    result_blocks.push(anthropic::ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                    });
                }
            }

            let saved = db::create_message(pool, conversation_id, "user", &result_blocks)
                .await
                .map_err(ServerFnError::new)?;
            history.push(anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: result_blocks,
            });
            persisted.push(saved);
        }

        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::MessagesAppended(persisted),
        );
        Err(ServerFnError::new(format!(
            "tool-use loop exceeded {MAX_TURNS} turns without reaching a final reply"
        )))
    })
}

#[post("/api/conversations/{id}/messages")]
pub async fn send_message(id: i64, content: String) -> ServerFnResult<ServerEvents<ChatEvent>> {
    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: content }],
    };

    Ok(ServerEvents::new(move |mut tx| async move {
        let mut on_delta = |delta: &str| {
            // `on_delta` is a plain sync `FnMut`, but `SseTx::send` is only
            // `async` for API symmetry — it wraps a synchronous
            // `unbounded_send` with no `.await` inside, so send through the
            // underlying channel directly here. This keeps delta ordering
            // exact (a spawned task per delta could interleave out of order).
            if let Ok(event) = axum::response::sse::Event::default().json_data(ChatEvent::Delta {
                text: delta.to_string(),
            }) {
                let _ = tx.unbounded_send(event);
            }
        };

        match run_turn(db::get(), id, new_message, Some(&mut on_delta)).await {
            Ok(messages) => {
                // messages[0] is the caller's own new user message — already
                // shown optimistically by the frontend the instant it was
                // sent, so only the turns `run_turn` produced afterward
                // (assistant replies, tool results) are new to relay.
                for message in messages.into_iter().skip(1) {
                    let _ = tx
                        .send(ChatEvent::Done {
                            message_id: message.id,
                            role: message.role,
                            content: message.content,
                        })
                        .await;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }))
}

/// Thin wrapper around `anthropic::tools::snapshot_tasks` for the browser —
/// a one-shot pull, not a subscription. Used both for the initial task-panel
/// load and for the reconciliation pull `subscribe_conversation_events`'s
/// caller does on connect/reconnect (a `broadcast` channel has no replay).
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>> {
    Ok(anthropic::tools::snapshot_tasks(id))
}

/// A dedicated, always-open per-conversation event stream — independent of
/// any particular `send_message` call, since task activity (a tick, a
/// finish) or another writer's pushed turn can happen with no request in
/// flight at all. The frontend opens this once per viewed conversation and
/// keeps it open for as long as that conversation is selected; see
/// `docs/architecture.md` for why this needs its own stream rather than
/// reusing `send_message`'s.
#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(
    id: i64,
) -> ServerFnResult<ServerEvents<events::ConversationEvent>> {
    Ok(ServerEvents::new(move |mut tx| async move {
        let mut rx = events::subscribe(id);
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = tx.send(event).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // A subscriber that fell behind just misses some
                    // ephemeral `TaskUpdate`s — the frontend's one-shot
                    // `get_messages`/`get_tasks` reconciliation pull on
                    // connect covers the durable state regardless.
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Spins up a mock Anthropic upstream that returns `bodies` in order (one
    /// per request, clamped to the last body once exhausted) and points
    /// `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY` (both process-global) at it.
    /// Callers must hold `anthropic::test_support::lock_anthropic_base_url`
    /// for the duration, same as `anthropic::stream`'s own mock-upstream
    /// tests.
    async fn start_mock_upstream(bodies: Vec<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let bodies = Arc::new(bodies);
        let counter = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let bodies = bodies.clone();
                let counter = counter.clone();
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

        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
    }

    fn sse_body(events: &[(&str, &str)]) -> String {
        events
            .iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect()
    }

    #[sqlx::test]
    async fn test_run_turn_persists_user_and_assistant_messages_for_text_only_reply(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Hi!".to_string()
            }]
        );
    }

    #[sqlx::test]
    async fn test_run_turn_executes_tool_and_persists_full_round_trip(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let tool_use_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":2,\"b\":3}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let final_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sum is 5"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![tool_use_body, final_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "please add 2 and 3".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        assert_eq!(
            messages.len(),
            4,
            "expected user, tool_use, tool_result, final assistant"
        );
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "add".to_string(),
                input: serde_json::json!({"a": 2, "b": 3}),
            }]
        );
        assert_eq!(messages[2].role, "user");
        assert_eq!(
            messages[2].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_01".to_string(),
                content: "5".to_string(),
                is_error: None,
            }]
        );
        assert_eq!(messages[3].role, "assistant");
        assert_eq!(
            messages[3].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Sum is 5".to_string()
            }]
        );
    }

    #[sqlx::test]
    async fn test_run_turn_errors_when_max_turns_exceeded(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Always responds with a tool_use turn calling `add` (a fast, valid
        // call), so the loop never reaches a final reply and must give up
        // after MAX_TURNS rather than looping forever.
        let tool_use_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1,\"b\":1}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![tool_use_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "loop forever".to_string(),
            }],
        };

        let result = run_turn(&pool, conversation.id, new_message, None).await;
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn test_conversation_lock_is_shared_per_conversation_id_only() {
        let a1 = conversation_lock(9001);
        let a2 = conversation_lock(9001);
        let b = conversation_lock(9002);
        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same conversation id should share one lock"
        );
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "different conversation ids should get different locks"
        );
    }

    #[sqlx::test]
    async fn test_run_turn_serializes_concurrent_calls_for_the_same_conversation(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let conversation_id = conversation.id;
        let pool_a = pool.clone();
        let pool_b = pool.clone();

        let task_a = tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "first".to_string(),
                }],
            };
            run_turn(&pool_a, conversation_id, message, None).await
        });
        let task_b = tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "second".to_string(),
                }],
            };
            run_turn(&pool_b, conversation_id, message, None).await
        });

        let (result_a, result_b) = tokio::join!(task_a, task_b);
        result_a
            .expect("task a should not panic")
            .expect("run_turn a should succeed");
        result_b
            .expect("task b should not panic")
            .expect("run_turn b should succeed");

        let all = db::list_messages(&pool, conversation_id)
            .await
            .expect("list messages");
        assert_eq!(all.len(), 4);
        let roles: Vec<&str> = all.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant"],
            "the per-conversation lock should serialize the two calls into two complete \
             (user, assistant) pairs, never interleaved"
        );
    }

    /// Regression test for a deadlock: `cancel_task` runs synchronously
    /// inside `run_turn`'s own tool-dispatch loop (same as `add`/`count`/
    /// any other tool), which already holds `conversation_id`'s lock for
    /// its entire duration. `cancel_task` also pushes a cancellation
    /// notification via `chat::run_turn` — if that push were awaited
    /// in-line rather than detached (`tokio::spawn`), it would try to
    /// re-acquire the same non-reentrant lock the outer call is still
    /// holding and hang forever. Wrapped in a timeout so a regression
    /// fails loudly instead of hanging the test suite.
    #[sqlx::test]
    async fn test_run_turn_does_not_deadlock_when_model_calls_cancel_task(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Seed a running task directly (bypassing the model) for cancel_task
        // to act on.
        let start_result = anthropic::tools::execute(
            &pool,
            conversation.id,
            "toolu_seed_task",
            "run_async",
            &serde_json::json!({"tool": "count", "input": {"target": 5, "interval_seconds": 5}}),
        )
        .await
        .expect("seeding the background task should succeed");
        assert!(start_result.contains("toolu_seed_task"));

        let cancel_turn_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_cancel","name":"cancel_task","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"task_id\":\"toolu_seed_task\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let final_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cancelled it"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![cancel_turn_body, final_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "cancel that task".to_string(),
            }],
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(&pool, conversation.id, new_message, None),
        )
        .await
        .expect("run_turn should complete well within the timeout, not deadlock")
        .expect("run_turn should succeed");

        assert_eq!(
            result.len(),
            4,
            "expected user, tool_use(cancel_task), tool_result, final assistant"
        );
        assert_eq!(
            result[2].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_cancel".to_string(),
                content: "task toolu_seed_task cancelled".to_string(),
                is_error: None,
            }]
        );
    }

    /// Regression test: `run_async` and the task-management suite were
    /// fully implemented and unit-tested in `anthropic::tools` before
    /// anyone noticed `tool_definitions()` never listed them — the model
    /// had no way to know they existed until a live test asked it to use
    /// `run_async` and it correctly said no such tool was available. This
    /// pins the two lists together so a newly dispatchable tool can't be
    /// implemented without also being offered to the model.
    #[test]
    fn test_tool_definitions_covers_every_dispatchable_tool_name() {
        let defined: std::collections::BTreeSet<String> = anthropic::tools::tool_definitions()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let dispatchable: std::collections::BTreeSet<&str> = [
            "add",
            "count",
            "echo",
            "run_async",
            "list_tasks",
            "task_status",
            "task_stdout",
            "task_stderr",
            "task_result",
            "wait_task",
            "cancel_task",
            "write_task_stdin",
            "create_pod",
            "terminate_pod",
            "list_pods",
            "create_terminal",
            "terminate_terminal",
            "list_terminals",
            "run_terminal_command",
            "send_signal",
            "terminal_command_status",
            "read_terminal_output",
            "list_commands",
        ]
        .into_iter()
        .collect();

        let missing: Vec<_> = dispatchable
            .iter()
            .filter(|name| !defined.contains(**name))
            .collect();
        assert!(
            missing.is_empty(),
            "tool_definitions() is missing: {missing:?}"
        );
    }
}
