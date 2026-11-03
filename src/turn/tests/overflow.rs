//! SME-73: what a turn does with a refusal meaning "this didn't fit" —
//! learn the window, shrink, send once more. Each test is one row of the
//! plan's state model: budget-only, compact-then-retry, no window named,
//! can't-help, refused twice, and the two Stop races the pause hooks
//! exist for. The recovery's own wordings, what it saves, what it
//! publishes and how its spans read are all asserted here.

use super::*;
use std::sync::Arc;

/// How one arrival is answered: `Ok` streams that SSE reply, `Err` is a
/// flat HTTP refusal — the shape an overflow arrives in.
type Answer = Result<String, (axum::http::StatusCode, String)>;

/// A mock upstream answering each arrival in order from `answers`,
/// clamped to the last once they run out, and keeping every request
/// body parsed: `count()` says how many sends reached the server, and
/// `request(i)` what the (i+1)th carried. Same locking rule as
/// `start_mock_upstream`.
struct RefusalMock {
    answers: Vec<Answer>,
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl RefusalMock {
    fn count(&self) -> usize {
        self.requests.lock().expect("the request log").len()
    }

    fn request(&self, i: usize) -> serde_json::Value {
        self.requests.lock().expect("the request log")[i].clone()
    }
}

async fn start_refusal_mock(pool: &PgPool, answers: Vec<Answer>) -> Arc<RefusalMock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let mock = Arc::new(RefusalMock {
        answers,
        requests: Default::default(),
    });
    let state = mock.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            async move {
                use axum::response::IntoResponse;
                let parsed = serde_json::from_str(&body).expect("a JSON request body");
                let index = {
                    let mut log = state.requests.lock().expect("the request log");
                    log.push(parsed);
                    log.len() - 1
                };
                match &state.answers[index.min(state.answers.len() - 1)] {
                    Ok(body) => (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body.clone(),
                    )
                        .into_response(),
                    Err((status, body)) => (*status, body.clone()).into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    mock
}

/// Anthropic's refusal of every model, "prompt is too long: A tokens >
/// W maximum", as a flat 400.
fn too_long(input: u32, window: u32) -> Answer {
    Err((
        axum::http::StatusCode::BAD_REQUEST,
        format!(
            r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"prompt is too long: {input} tokens > {window} maximum"}},"request_id":"req_test"}}"#
        ),
    ))
}

/// llama.cpp's structured 400, naming its `n_ctx` beside the wording.
fn llama_refused(input: u32, window: u32) -> Answer {
    Err((
        axum::http::StatusCode::BAD_REQUEST,
        format!(
            r#"{{"error":{{"code":400,"type":"exceed_context_size_error","message":"request ({input} tokens) exceeds the available context size ({window} tokens), try increasing it","from_cache":false,"n_prompt_tokens":{input},"n_ctx":{window}}}}}"#
        ),
    ))
}

/// Ollama's refusal with context shift off — an overflow that names
/// neither window nor input.
fn ollama_refused() -> Answer {
    Err((
        axum::http::StatusCode::BAD_REQUEST,
        r#"{"error":{"type":"invalid_request_error","message":"the prompt is longer than the context length currently available to the model; shorten the prompt, or make num_ctx larger"}}"#
            .to_string(),
    ))
}

/// The Context window `mock-model` was saved with, if it was ever saved.
async fn saved_context_window(pool: &PgPool) -> Option<i32> {
    sqlx::query_scalar::<_, Option<i32>>(
        "SELECT context_window FROM provider_models WHERE provider_id = (SELECT id FROM inference_providers ORDER BY id) AND model = $1",
    )
    .bind(crate::providers::test_support::MOCK_MODEL)
    .fetch_optional(pool)
    .await
    .expect("the model row")
    .flatten()
}

/// The summaries of every compaction saved in conversation `id`.
async fn compaction_summaries(pool: &PgPool, id: i64) -> Vec<String> {
    let messages = db::list_messages(pool, id).await.expect("messages");
    messages
        .iter()
        .flat_map(|m| m.blocks().unwrap_or_default())
        .filter_map(|b| match b {
            anthropic::ContentBlock::CompactionSummary { summary, .. } => Some(summary),
            _ => None,
        })
        .collect()
}

/// The text of the last assistant message.
fn last_reply(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find(|m| m.role == "assistant").and_then(|m| {
        m.blocks()
            .ok()?
            .into_iter()
            .find_map(|b| match b {
                anthropic::ContentBlock::Text { text } => Some(text),
                _ => None,
            })
    })
}

/// The windows `ContextUsageUpdate`s published to watching tabs.
fn published_windows(events: Vec<events::ConversationEvent>) -> Vec<u32> {
    events
        .into_iter()
        .filter_map(|e| match e {
            events::ConversationEvent::ContextUsageUpdate { context_window, .. } => Some(context_window),
            _ => None,
        })
        .collect()
}

fn chat_spans(spans: &[opentelemetry_sdk::trace::SpanData]) -> Vec<&opentelemetry_sdk::trace::SpanData> {
    spans.iter().filter(|s| s.name.starts_with("chat ")).collect()
}

fn turn_span(spans: &[opentelemetry_sdk::trace::SpanData]) -> &opentelemetry_sdk::trace::SpanData {
    let turns: Vec<_> = spans.iter().filter(|s| s.name == "turn").collect();
    let [turn] = turns.as_slice() else {
        panic!("one turn span, got {:?}", spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>());
    };
    turn
}

/// R1 "Budget only, when the input fits": a refusal naming a window the
/// input still fits sends the same history again once the budget shrank —
/// no compaction — and the window is learned, published and spanned as
/// the plan says (recovered turn successful; refused call failed naming
/// the refusal; retry successful).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_budget_only_overflow_retries_the_same_history(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(12_000, 32_768), Ok(text_reply_body("Here you are."))]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(conversation.id);

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the refusal recovers and the retry answers");

    assert_eq!(upstream.count(), 2, "one refusal, one retry — at most one recovery");
    assert_eq!(
        upstream.request(0)["messages"],
        upstream.request(1)["messages"],
        "the input fit the window, so only the budget changed: the same history went again"
    );

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Here you are."), "the retry's reply is kept: {saved:?}");
    assert!(
        compaction_summaries(&pool, conversation.id).await.is_empty(),
        "budget-only recovers without compacting"
    );
    assert_eq!(saved_context_window(&pool).await, Some(32_768), "the window the refusal named is saved for the model");
    assert!(
        published_windows(drain_events(&mut rx).await).contains(&32_768),
        "the meter is told the new window with the retry's usage"
    );

    // § Spans: a recovered overflow leaves the turn successful; the
    // refused request really failed, its chat span says so, the retry's
    // doesn't.
    assert_eq!(turn_span(&spans).status, opentelemetry::trace::Status::Unset, "the recovered turn is successful");
    let chats = chat_spans(&spans);
    let [refused, retry] = chats.as_slice() else {
        panic!("the refused call and the retry, got {chats:?}");
    };
    let opentelemetry::trace::Status::Error { description } = &refused.status else {
        panic!("the refused call keeps being marked: {:?}", refused.status);
    };
    assert!(description.contains("prompt is too long"), "the chat span names the refusal: {description}");
    assert_eq!(retry.status, opentelemetry::trace::Status::Unset, "the retry's chat span is successful");
}

/// R2 "Compact, when it doesn't": a refusal naming an input the window
/// can't hold after its reply is allowed for summarizes first, and the
/// reply arrives after the divider.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_overflow_whose_input_doesnt_fit_compacts_then_retries(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(
        &pool,
        vec![
            too_long(120_000, 131_072),
            Ok(text_reply_body("Summary: it was going.")),
            Ok(text_reply_body("Done now.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    let mut rx = events::subscribe(conversation.id);

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the refusal recovers through a compaction");

    assert_eq!(upstream.count(), 3, "refusal, summary, retry");
    let summaries = compaction_summaries(&pool, conversation.id).await;
    assert_eq!(summaries, ["Summary: it was going."], "the divider's summary was saved");
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Done now."));
    assert_eq!(saved_context_window(&pool).await, Some(131_072));
    assert!(
        published_windows(drain_events(&mut rx).await).contains(&131_072),
        "the meter is told the window that the compaction was sized for"
    );
    assert_eq!(turn_span(&spans).status, opentelemetry::trace::Status::Unset, "the turn recovered, its span says so");
}

/// R2 with no window named: the refusal is an overflow all the same, and
/// the turn's assumed bound — three quarters of the refused request —
/// sizes this turn's compaction and retry but is saved for nobody.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_overflow_with_no_window_named_compacts_on_an_unsaved_guess(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(
        &pool,
        vec![
            ollama_refused(),
            Ok(text_reply_body("Summary: what came before.")),
            Ok(text_reply_body("Answered.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the refusal recovers");

    assert_eq!(upstream.count(), 3, "refusal, summary, retry — the guess still answers the refusal");
    assert_eq!(compaction_summaries(&pool, conversation.id).await, ["Summary: what came before."]);
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Answered."));
    assert_eq!(saved_context_window(&pool).await, None, "a guess is kept to the turn, never saved as the model's window");
}

/// R4 "Can't help": a window that can't hold even the system prompt and
/// tool definitions fails the turn at once — before any compaction or
/// retry — with the words that say what to change, and as a sent user
/// turn the reason is saved as the conversation's failure notice.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_window_too_small_for_the_system_prompt_fails_at_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(5_000, 1_024)]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("no compaction can fit this");
    let error = error.to_string();
    assert!(error.contains("(1,024 tokens) is too small for smelt's system prompt and tools"), "{error}");
    assert!(error.contains("Give the model a larger context on its server, or pick another model."), "{error}");
    assert_eq!(upstream.count(), 1, "it fails at once: no compaction, no retry");
    assert!(compaction_summaries(&pool, conversation.id).await.is_empty());
    assert_eq!(saved_context_window(&pool).await, Some(1_024), "even a hopeless window is learned");

    // A sent user turn keeps its reason the SME-123 DR3 way: a failure
    // notice in the conversation, `TURN_FAILED_NOTED` to reconnecting tabs.
    let sent = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(sent.id);
    start_turn(pool.clone(), sent.id, "hi".to_string()).await.expect("send");
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::TurnError { message }) => return message,
                Ok(_) => continue,
                Err(e) => panic!("the event stream ended: {e}"),
            }
        }
    })
    .await
    .expect("a TurnError within 10 s");
    assert_eq!(published, TURN_FAILED_NOTED);
    let reason = last_failure_reason(&pool, sent.id).await.expect("the conversation says why");
    assert!(reason.contains("is too small for smelt's system prompt and tools"), "{reason}");
}

/// R5 "Refused again": a retry refused as an overflow too ends the turn
/// with the worded reason — and a window that second refusal names is
/// still learned, so the next turn starts right.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_retry_refused_again_ends_the_turn_and_learns_whats_smaller(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(12_000, 32_768), llama_refused(24_000, 24_576)]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the retry is refused too");
    let error = error.to_string();
    assert!(error.contains("still didn't fit"), "{error}");
    assert!(error.contains("(24,576 tokens)"), "the failure names the window the second refusal gave: {error}");
    assert!(error.contains("exceeds the available context size"), "and what the server said: {error}");
    assert!(!error.contains("after compacting"), "no compaction happened, the failure doesn't claim one: {error}");
    assert_eq!(upstream.count(), 2, "one refusal, one refused retry — no third send");

    assert_eq!(saved_context_window(&pool).await, Some(24_576), "the smaller window is learned from the retry's refusal");
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("resolve");
    assert_eq!(model.context_window, 24_576, "so the next turn starts at 24,576");
}

/// Stop race one: a Stop that catches the recovery between the window
/// write and the summary call ends the turn stopped with only the
/// window saved — the summary is never asked for, nothing else lands.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_before_the_summary_saves_nothing_but_the_window(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(120_000, 131_072), Ok(text_reply_body("Summary: never sent."))]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let id = conversation.id;

    let (reached, resume) = test_hooks::pause_before_summary_call(id);
    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("the recovery reached the summary call")
        .expect("hook");

    stop_turn_now(id);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the stopped turn ends")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    drop(resume);

    assert_eq!(upstream.count(), 1, "the summary was never asked for");
    assert!(compaction_summaries(&pool, id).await.is_empty(), "nothing of a compaction landed");
    let saved = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&saved), None, "no reply");
    assert_eq!(saved_context_window(&pool).await, Some(131_072), "the window write happened before the pause and stays");
}

/// Stop race two: a Stop after the compaction, before the retry, keeps
/// both (the compacted conversation and the learned window) and sends
/// the retry never.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_before_the_retry_keeps_the_compaction_and_the_window(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(
        &pool,
        vec![
            too_long(120_000, 131_072),
            Ok(text_reply_body("Summary: it was going.")),
            Ok(text_reply_body("Never sent.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    let id = conversation.id;

    let (reached, resume) = test_hooks::pause_before_retry(id);
    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("the recovery reached the retry")
        .expect("hook");

    stop_turn_now(id);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the stopped turn ends")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    drop(resume);

    assert_eq!(upstream.count(), 2, "the refusal and the summary went, the retry never did");
    assert_eq!(compaction_summaries(&pool, id).await, ["Summary: it was going."], "a finished compaction stays");
    let saved = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&saved), None, "the retry's reply is never sent, so nothing to discard");
    assert_eq!(saved_context_window(&pool).await, Some(131_072));
}
