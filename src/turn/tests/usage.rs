//! SME-178 B25: every model call that got as far as `message_start`, or
//! streamed anything, leaves one `model_call_usage` row, however it ends.

use super::*;

/// A row of `model_call_usage`: conversation, kind, input, cache read,
/// output.
type Row = (Option<i64>, String, i64, i64, i64);

async fn rows(pool: &PgPool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT conversation_id, kind, input_tokens, cache_read_input_tokens, output_tokens
         FROM model_call_usage ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the recorded calls")
}

/// The rows once there are `n` of them, waiting up to 5 s: a stopped
/// call's row is written by a spawned task.
async fn wait_for_rows(pool: &PgPool, n: usize) -> Vec<Row> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let found = rows(pool).await;
        if found.len() >= n || std::time::Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn message_start(input: i64, cache_read: i64) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{input},\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":{cache_read},\"output_tokens\":1}}}}}}\n\n"
    )
}

fn text_delta(text: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
    )
}

/// A mock upstream that sends `prefix` and then never finishes; counts
/// the requests it gets. Same locking rule as `start_mock_upstream`.
async fn start_prefix_then_hanging_mock_upstream(pool: &PgPool, prefix: String) -> Arc<AtomicUsize> {
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let prefix = prefix.clone();
            counted.fetch_add(1, Ordering::SeqCst);
            async move {
                let body = futures_util::StreamExt::chain(
                    futures_util::stream::once(async move { Ok::<_, std::io::Error>(prefix) }),
                    futures_util::stream::pending(),
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(body),
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    requests
}

/// Waits (up to 5 s) until `conversation_id`'s reply so far is `len`
/// bytes: the stream has reached the point the test stops it at.
async fn wait_for_reply_of(conversation_id: i64, len: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while reply_in_progress(conversation_id).map(|r| r.len()) != Some(len) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reply streams");
}

/// Stops `turn` and waits (up to 2 s) for it to end stopped.
async fn stop_and_wait(conversation_id: i64, turn: tokio::task::JoinHandle<TurnResult>) {
    stop_turn_now(conversation_id);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
        .await
        .expect("the turn ends after the stop")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
}

fn spawn_turn(pool: &PgPool, conversation_id: i64) -> tokio::task::JoinHandle<TurnResult> {
    let pool = pool.clone();
    tokio::spawn(async move { run_turn(&pool, conversation_id, hello()).await })
}

/// A Stop mid-stream records what the call used: `message_start`'s input
/// and cache counts, and its output estimated from what streamed (400
/// bytes, 100 tokens). It isn't the conversation's last-known usage.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_stopped_mid_stream_is_recorded_with_what_it_used(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000001).await.expect("conversation");
    let text = "a".repeat(400);
    start_prefix_then_hanging_mock_upstream(&pool, message_start(1200, 300) + &text_delta(&text)).await;

    let turn = spawn_turn(&pool, conversation.id);
    wait_for_reply_of(conversation.id, 400).await;
    stop_and_wait(conversation.id, turn).await;

    assert_eq!(
        wait_for_rows(&pool, 1).await,
        vec![(Some(conversation.id), "turn".to_string(), 1200, 300, 100)]
    );
    assert_eq!(db::get_conversation_usage(&pool, conversation.id).await.expect("usage"), None);
    resume_turns(conversation.id);
}

/// A Stop after the reply is saved and before its usage is recorded
/// records the call's full usage, but not as the last-known usage.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_stopped_after_its_reply_is_saved_is_recorded(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000002).await.expect("conversation");
    start_mock_upstream(&pool, vec![with_usage(&text_reply_body("Hi!"), 1000, 2000, 40)]).await;
    let (reached, _release) = test_hooks::pause_after_reply_saved(conversation.id);

    let turn = spawn_turn(&pool, conversation.id);
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn saves its reply")
        .expect("hook");
    stop_and_wait(conversation.id, turn).await;

    assert_eq!(
        wait_for_rows(&pool, 1).await,
        vec![(Some(conversation.id), "turn".to_string(), 1000, 2000, 40)]
    );
    assert_eq!(db::get_conversation_usage(&pool, conversation.id).await.expect("usage"), None);
    resume_turns(conversation.id);
}

/// A Stop while a finished call is being recorded doesn't record it
/// twice: the record goes on. Its turn gone, it isn't the last-known
/// usage (code review 1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_while_a_call_is_recorded_records_it_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000003).await.expect("conversation");
    start_mock_upstream(&pool, vec![with_usage(&text_reply_body("Hi!"), 1000, 2000, 40)]).await;
    let (reached, release) = test_hooks::pause_during_record(conversation.id);

    let turn = spawn_turn(&pool, conversation.id);
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn records its call")
        .expect("hook");
    stop_and_wait(conversation.id, turn).await;
    let _ = release.send(());

    let expected = vec![(Some(conversation.id), "turn".to_string(), 1000, 2000, 40)];
    assert_eq!(wait_for_rows(&pool, 1).await, expected);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(rows(&pool).await, expected, "recorded once");
    assert_eq!(db::get_conversation_usage(&pool, conversation.id).await.expect("usage"), None);
    resume_turns(conversation.id);
}

/// A stream that fails after `message_start` (an `error` event) records
/// what the call used before it failed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_failing_mid_stream_is_recorded(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000004).await.expect("conversation");
    let body = message_start(500, 0)
        + &text_delta("partial!")
        + "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    start_mock_upstream(&pool, vec![body]).await;

    let result = run_turn(&pool, conversation.id, hello()).await;
    assert!(result.is_err(), "{result:?}");

    assert_eq!(wait_for_rows(&pool, 1).await, vec![(Some(conversation.id), "turn".to_string(), 500, 0, 2)]);
}

/// Ollama's flat 500 before anything streams leaves no row of its own:
/// nothing is known of it. The retry that succeeds is the one row.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_retried_call_that_streamed_nothing_leaves_no_row(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000005).await.expect("conversation");
    start_mock_upstream_failing_n_times(&pool, 1, Some(with_usage(&text_reply_body("Hi!"), 10, 0, 3))).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the retry succeeds");

    assert_eq!(wait_for_rows(&pool, 1).await, vec![(Some(conversation.id), "turn".to_string(), 10, 0, 3)]);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(rows(&pool).await.len(), 1);
}

/// A compaction's call stopped mid-stream is recorded as one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_stopped_mid_stream_is_recorded(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000006).await.expect("conversation");
    db::create_message(&pool, conversation.id, "user", &[text_block("earlier message")])
        .await
        .expect("seed earlier message");
    // Near the window, so the turn compacts before its call.
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage { input_tokens: 190_000, output_tokens: 0, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
    )
    .await
    .expect("seed usage");
    let requests =
        start_prefix_then_hanging_mock_upstream(&pool, message_start(5000, 0) + &text_delta("Summary")).await;

    let turn = spawn_turn(&pool, conversation.id);
    // The compaction's stream relays no reply: wait for its call, then
    // a moment for the prefix sent with it to be read.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while requests.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the compaction calls the model");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop_and_wait(conversation.id, turn).await;

    assert_eq!(
        wait_for_rows(&pool, 1).await,
        vec![(Some(conversation.id), "compaction".to_string(), 5000, 0, 1)]
    );
    resume_turns(conversation.id);
}

/// A conversation deleted mid-call: the call stopped after the delete is
/// still recorded, with no conversation.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_on_a_conversation_deleted_mid_turn_is_kept(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000007).await.expect("conversation");
    start_prefix_then_hanging_mock_upstream(&pool, message_start(1200, 0) + &text_delta("abcd")).await;

    let turn = spawn_turn(&pool, conversation.id);
    wait_for_reply_of(conversation.id, 4).await;
    db::delete_conversation(&pool, conversation.id).await.expect("delete");
    stop_turn_now(conversation.id);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
        .await
        .expect("the turn ends after the stop");

    assert_eq!(wait_for_rows(&pool, 1).await, vec![(None, "turn".to_string(), 1200, 0, 1)]);
    resume_turns(conversation.id);
}

/// A Stop while a finished call is recorded cuts the record loose: it
/// can commit after the next turn's. Being no longer the turn's, it
/// doesn't overwrite the next turn's last-known usage (code review 1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_turns_late_record_doesnt_overwrite_the_next_turns_usage(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000008).await.expect("conversation");
    start_mock_upstream(
        &pool,
        vec![with_usage(&text_reply_body("Hi!"), 1000, 0, 40), with_usage(&text_reply_body("Again!"), 5000, 0, 50)],
    )
    .await;
    let (reached, release) = test_hooks::pause_during_record(conversation.id);

    let turn = spawn_turn(&pool, conversation.id);
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn records its call")
        .expect("hook");
    stop_and_wait(conversation.id, turn).await;
    resume_turns(conversation.id);
    run_turn(&pool, conversation.id, hello()).await.expect("the next turn");
    let input = |u: Option<anthropic::TokenUsage>| u.map(|u| u.input_tokens);
    assert_eq!(input(db::get_conversation_usage(&pool, conversation.id).await.expect("usage")), Some(5000));

    let _ = release.send(());
    assert_eq!(wait_for_rows(&pool, 2).await.len(), 2, "the stopped turn's call is still counted");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        input(db::get_conversation_usage(&pool, conversation.id).await.expect("usage")),
        Some(5000),
        "the next turn's usage stands"
    );
    resume_turns(conversation.id);
}

/// A stopped or failed call's input falls back to the request's estimate
/// only when `message_start` reported no input at all, as LiteLLM's does,
/// whose real counts come only at the end (code review 1).
#[test]
fn test_an_unfinished_calls_input_is_estimated_only_when_none_was_reported() {
    let usage = |input, write, read| anthropic::TokenUsage {
        input_tokens: input,
        output_tokens: 7,
        cache_creation_input_tokens: write,
        cache_read_input_tokens: read,
    };
    assert_eq!(with_estimated_input(usage(0, 0, 0), 3000), usage(3000, 0, 0));
    assert_eq!(with_estimated_input(usage(12, 0, 0), 3000), usage(12, 0, 0));
    assert_eq!(with_estimated_input(usage(0, 0, 900), 3000), usage(0, 0, 900), "fully cached");
    assert_eq!(with_estimated_input(usage(0, 40, 0), 3000), usage(0, 40, 0), "all written to the cache");
}

/// A LiteLLM call stopped mid-stream: its `message_start` says 0 input,
/// so its row has the request's estimate instead.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_stopped_after_a_zero_message_start_records_the_requests_estimate(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9178000009).await.expect("conversation");
    let text = "a".repeat(400);
    start_prefix_then_hanging_mock_upstream(&pool, message_start(0, 0) + &text_delta(&text)).await;

    let turn = spawn_turn(&pool, conversation.id);
    wait_for_reply_of(conversation.id, 400).await;
    stop_and_wait(conversation.id, turn).await;

    let found = wait_for_rows(&pool, 1).await;
    assert_eq!(found.len(), 1, "{found:?}");
    let (_, _, input, cache_read, output) = found[0].clone();
    // The system prompt and tool definitions alone are thousands of tokens.
    assert!(input > 1000, "input {input} isn't the request's estimate");
    assert_eq!((cache_read, output), (0, 100));
    resume_turns(conversation.id);
}
