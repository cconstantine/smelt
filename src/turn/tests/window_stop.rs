//! SME-73, the replies that run into the window rather than being
//! refused before they start: a stop that wrote far less than its budget
//! read the window's edge, and an Ollama reply measured far below the
//! request that went was quietly truncated to it. Each test pins both
//! halves: what the notice says, and what the turn learned as the window.

use super::*;

/// Saves `mock-model`'s Context window (and Max reply tokens, if given),
/// as the provider page's field would.
pub(super) async fn set_context_window(pool: &PgPool, window: i32, max_output: Option<i32>) {
    sqlx::query("INSERT INTO provider_models (provider_id, model, context_window, max_output) SELECT id, $1, $2, $3 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .bind(window)
        .bind(max_output)
        .execute(pool)
        .await
        .expect("seed the window");
}

/// W1: a llama.cpp slot that fills stops at `max_tokens` written far short
/// of the budget — the notice reads the room left with what it wrote, the
/// call's whole measured size (below the believed window by more than the
/// safety buffer) is learned for the next request to compact on, and
/// nothing is retried. A llama.cpp kind provider: that reading is its
/// stop's tell, W5 pins the Anthropic-kind same shape staying a budget stop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_max_tokens_stop_short_of_its_budget_reads_as_the_window(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // 12,000 written against a 16,384 budget; 42,000 measured against the
    // 64,000 the turn believed.
    let body = with_usage(&text_reply_body("Right up to the room."), 30_000, 0, 12_000)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    let requests = start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_context_window(&pool, 64_000, None).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("a window-stopped reply still succeeds");

    assert_eq!(requests.lock().expect("the request log").len(), 1, "a window stop retries nothing");
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Right up to the room."), "the cut-off reply is kept");
    let notice = match saved.last().expect("a notice after the reply").blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(
        parse_cut_off_notice(&notice),
        Some((12_000, ReplyLimit::RoomLeft, None)),
        "the room and what it wrote, not the budget it never reached: {notice}"
    );
    assert_eq!(saved_context_window(&pool).await, Some(42_000), "the call's whole measured size is the window");
    assert!(published_windows(drain_events(&mut rx).await).contains(&42_000), "the meter reads the new window too");
}

/// W5: a stop short of its budget reads as the room only where a stop
/// word can't be trusted to mean the budget — llama.cpp maps anything
/// but one to `max_tokens`. An Anthropic-kind server writes that reason
/// for its own budget and nothing else, so the same shape stays a
/// budget stop there: the notice names the budget, and nothing is
/// learned (the CI failure of `cut_off_in_a_call`, whose captured
/// llama.cpp stream arrives through an Anthropic-kind shim).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_short_stop_from_an_anthropic_kind_server_stays_a_budget_stop(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("Right up to the room."), 30_000, 0, 12_000)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream(&pool, vec![body]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_context_window(&pool, 64_000, None).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let notice = match saved.last().expect("a notice").blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(parse_cut_off_notice(&notice), Some((16_384, ReplyLimit::OutputCap, None)), "{notice}");
    assert_eq!(saved_context_window(&pool).await, Some(64_000), "an Anthropic stop measures nothing");
}

/// W2: on llama.cpp, a stop that wrote the whole budget was bound by the
/// budget: the notice says so and nothing is learned, however far the
/// call measured under the believed window.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_budget_stop_still_names_its_budget_and_learns_nothing(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("Every token I had."), 100, 0, 16_384)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    let requests = start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_context_window(&pool, 64_000, Some(16_384)).await;

    run_turn(&pool, conversation.id, hello()).await.expect("a budget-stopped reply still succeeds");

    assert_eq!(requests.lock().expect("the request log").len(), 1);
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let notice = match saved.last().expect("a notice").blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(parse_cut_off_notice(&notice), Some((16_384, ReplyLimit::OutputCap, None)), "{notice}");
    assert_eq!(saved_context_window(&pool).await, Some(64_000), "a budget stop measures nothing of the window");
}

/// W3: on llama.cpp, 32 tokens short is within the slack — usage counts
/// round, and a budget stop stays a budget stop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_within_its_slack_of_the_budget_keeps_the_budget_wording(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("Almost every token."), 100, 0, 16_352)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_context_window(&pool, 64_000, Some(16_384)).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let notice = match saved.last().expect("a notice").blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(parse_cut_off_notice(&notice), Some((16_384, ReplyLimit::OutputCap, None)), "{notice}");
    assert_eq!(saved_context_window(&pool).await, Some(64_000));
}

/// W4: Claude 4.5+'s own stop reason measures the room too — its notice
/// wording SME-111 already pinned, and now the size behind it is learned
/// when the call ended far under the window smelt believed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_stopped_by_the_window_learns_its_measured_size(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("Half a thought."), 30_000, 0, 5_000)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"model_context_window_exceeded""#);
    start_recording_mock_upstream(&pool, vec![body]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_context_window(&pool, 64_000, None).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let notice = match saved.last().expect("a notice").blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(parse_cut_off_notice(&notice), Some((5_000, ReplyLimit::RoomLeft, None)), "{notice}");
    assert_eq!(saved_context_window(&pool).await, Some(35_000), "measured, not believed");
}

/// An Ollama-kind provider: `/v1/messages` answers `reply`, `/api/ps`
/// answers `ps` and counts the ask — the turn should knock there only
/// when a reply measured like a quietly truncated request (SME-73).
struct OllamaMock {
    ps_hits: std::sync::atomic::AtomicUsize,
}

async fn start_ollama_mock(pool: &PgPool, reply: String, ps: String) -> Arc<OllamaMock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let mock = Arc::new(OllamaMock { ps_hits: Default::default() });
    let state = mock.clone();
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(move || {
                let reply = reply.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], reply) }
            }),
        )
        .route(
            "/api/ps",
            axum::routing::get(move || {
                let ps = ps.clone();
                let state = state.clone();
                async move {
                    state.ps_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ([(axum::http::header::CONTENT_TYPE, "application/json")], ps)
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let provider = db::create_inference_provider(
        pool,
        &format!("mock ollama {addr}"),
        "ollama",
        &format!("http://{addr}"),
        "api_key",
        "test-key",
        false,
        None,
        true,
        None,
    )
    .await
    .expect("create the ollama mock provider");
    db::set_default_model(pool, provider.id, crate::providers::test_support::MOCK_MODEL)
        .await
        .expect("make the ollama mock the default");
    mock
}

/// A user message of `chars` characters: enough for
/// `estimate_request_tokens` to clear the 8,192-token floor the
/// quiet-truncation tell needs.
async fn seed_long_message(pool: &PgPool, id: i64, chars: usize) {
    db::create_message(pool, id, "user", &[anthropic::ContentBlock::Text { text: "x".repeat(chars) }])
        .await
        .expect("seed");
}

/// O1: an Ollama reply that measured below half of the request smelt sent
/// was quietly truncated: the turn asks `/api/ps` once, learns the loaded
/// window, keeps the written reply, and the meter reads the real measured
/// input (the `message_delta` shape, SME-178) beside the new window.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_quietly_truncated_ollama_reply_learns_the_loaded_window(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let reply = with_usage(&text_reply_body("Answered from less than I sent."), 400, 0, 60);
    let mock = start_ollama_mock(&pool, reply, r#"{"models":[{"name":"mock-model","context_length":32768}]}"#.to_string()).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    seed_long_message(&pool, conversation.id, 40_000).await;
    set_context_window(&pool, 64_000, None).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("the quietly truncated reply still succeeds");

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(
        last_reply(&saved).as_deref(),
        Some("Answered from less than I sent."),
        "the written reply is kept: {saved:?}"
    );
    assert_eq!(mock.ps_hits.load(std::sync::atomic::Ordering::SeqCst), 1, "asked once what's loaded");
    assert_eq!(saved_context_window(&pool).await, Some(32_768), "the loaded window, not the believed one");

    let updates: Vec<(i64, u32)> = drain_events(&mut rx)
        .await
        .into_iter()
        .filter_map(|e| match e {
            events::ConversationEvent::ContextUsageUpdate { usage, context_window } => Some((usage.input_tokens, context_window)),
            _ => None,
        })
        .collect();
    assert_eq!(updates.last(), Some(&(400, 32_768)), "measured input with the learned window: {updates:?}");
}

/// O2: a reply that measured as much as smelt estimated went whole:
/// `/api/ps` is never asked and nothing is learned.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_whole_ollama_reply_never_asks_the_server(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let reply = with_usage(&text_reply_body("All of it, no shifting."), 20_000, 0, 60);
    let mock = start_ollama_mock(&pool, reply, r#"{"models":[{"name":"mock-model","context_length":32768}]}"#.to_string()).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    seed_long_message(&pool, conversation.id, 40_000).await;
    set_context_window(&pool, 64_000, None).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    assert_eq!(mock.ps_hits.load(std::sync::atomic::Ordering::SeqCst), 0, "a whole request never knocks");
    assert_eq!(saved_context_window(&pool).await, Some(64_000), "the field keeps the user's value");
}

/// O3: a loaded window that isn't smaller than the believed one teaches
/// nothing: the learners only lower.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_ollama_window_that_isnt_smaller_teaches_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let reply = with_usage(&text_reply_body("Answered from less than I sent."), 400, 0, 60);
    let mock = start_ollama_mock(&pool, reply, r#"{"models":[{"name":"mock-model","context_length":131072}]}"#.to_string()).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    seed_long_message(&pool, conversation.id, 40_000).await;
    set_context_window(&pool, 64_000, None).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    assert_eq!(mock.ps_hits.load(std::sync::atomic::Ordering::SeqCst), 1, "asked, and answered");
    assert_eq!(saved_context_window(&pool).await, Some(64_000), "learners only ever lower");
}

/// The tell reads only requests big enough for its gap to mean anything,
/// and reads "below half", never "half".
#[test]
fn test_the_ollama_tell_reads_only_requests_worth_reading() {
    let says = crate::turn::overflow::ollama_tell_says_truncated;
    assert!(!says(8_191, 0), "below the floor a gap means nothing");
    assert!(!says(8_192, 4_096), "half the estimate is still whole");
    assert!(says(8_192, 4_095), "below half of a request at the floor reads truncated");
    assert!(!says(20_000, 10_000));
    assert!(says(20_000, 400));
}
