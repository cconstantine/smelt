//! SME-386 final review: what a turn does with a server that counts the
//! input and `max_tokens` against its window and says so when they don't
//! fit, as Anthropic does.

use super::ask_user::tool_calls_body;
use super::overflow::too_long;
use super::window_stop::set_context_window;
use super::*;

/// A server with a window of `window` (`None`: it never refuses), answering
/// model steps in order from `plans`, each the input it counts and the
/// output it writes: only a step it takes moves to the next plan. The last
/// plan's step answers "Done."; the others call a tool, and one planned at
/// `FAILS` fails. A summary is refused once naming `refuse_summary`, if set.
#[derive(Default)]
struct Counting {
    window: std::sync::Mutex<Option<u32>>,
    plans: std::sync::Mutex<Vec<(i64, i64)>>,
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
    taken: AtomicUsize,
    summaries: AtomicUsize,
    refuse_summary: std::sync::Mutex<Option<u32>>,
}

impl Counting {
    /// The `max_tokens` of each step the server took.
    fn taken_budgets(&self) -> Vec<u64> {
        let log = self.requests.lock().expect("the request log");
        log.iter().filter(|r| r["taken"].as_bool() == Some(true)).filter_map(|r| r["max_tokens"].as_u64()).collect()
    }

    fn calls(&self) -> usize {
        self.requests.lock().expect("the request log").len()
    }
}

/// A step whose stream fails.
const FAILS: (i64, i64) = (-1, 0);
const OVERLOADED: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";

async fn counting_mock(pool: &PgPool, window: Option<u32>, plans: Vec<(i64, i64)>) -> Arc<Counting> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let mock = Arc::new(Counting { window: std::sync::Mutex::new(window), plans: std::sync::Mutex::new(plans), ..Counting::default() });
    let state = mock.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            async move {
                let mut parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
                if parsed["system"].to_string().contains("You are compacting") {
                    state.summaries.fetch_add(1, Ordering::SeqCst);
                    if let Some(window) = state.refuse_summary.lock().expect("the summary refusal").take()
                        && let Err((status, body)) = too_long(window + 1_000, window)
                    {
                        return (status, body).into_response();
                    }
                    return ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], text_reply_body("The summary.")).into_response();
                }
                let max_tokens = parsed["max_tokens"].as_i64().expect("max_tokens");
                let index = state.taken.load(Ordering::SeqCst);
                let plans = state.plans.lock().expect("the plans").clone();
                let (input, output) = plans[index.min(plans.len() - 1)];
                let window = *state.window.lock().expect("the window");
                let refused = window.filter(|window| input + max_tokens > i64::from(*window));
                parsed["taken"] = serde_json::json!(refused.is_none());
                state.requests.lock().expect("the request log").push(parsed);
                if let Some(window) = refused
                    && let Err((status, body)) = too_long(u32::try_from(input).expect("a plan's input"), window)
                {
                    return (status, body).into_response();
                }
                state.taken.fetch_add(1, Ordering::SeqCst);
                if (input, output) == FAILS {
                    return ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], OVERLOADED.to_string()).into_response();
                }
                let body = if index + 1 >= plans.len() {
                    text_reply_body("Done.")
                } else {
                    tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))])
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], with_usage(&body, input, 0, output)).into_response()
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    mock
}

/// The window at which, with this conversation's fixed content, a compacted
/// request leaves `room` tokens for a reply.
async fn window_leaving(pool: &PgPool, id: i64, room: u64) -> u32 {
    let model = crate::providers::resolve_turn_model(pool, id).await.expect("a model");
    let (request, _) = build_request(pool, id, &model, None, &[], false).await.expect("a request");
    u32::try_from(estimate_request_tokens(&request) + u64::from(COMPACTION_SAFETY_BUFFER) + room).expect("a window")
}

async fn send(pool: &PgPool, id: i64, text: &str) {
    let mut rx = events::subscribe(id);
    start_turn(pool.clone(), id, text.to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;
}

/// M-1: a size measured after a compaction, then the system prompt and
/// tools grew. The floor believed the old size, the server refused the
/// budget it gave, and the retry believed it again, on every send. Now a
/// refusal of that budget stops it being believed, and the next send tries
/// with a short reply, which fits.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_size_measured_before_the_tools_grew_isnt_believed_after_a_refusal(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // Leaves the estimate just under the floor, with room for the system
    // prompt and tools and the estimate's buffer. Replies measure 3,000 after
    // two compactions, which learns it's tight; the 10,000 has the next step
    // compact; and by then the tools have grown, and the server counts 10,900.
    let mock = counting_mock(&pool, None, vec![(3_000, 10), (3_000, 10), (3_000, 10), (7_000, 3_000), (10_900, 10)]).await;
    let window = window_leaving(&pool, conversation.id, 1_030).await;
    *mock.window.lock().expect("the window") = Some(window);
    set_context_window(&pool, window as i32, None).await;

    send(&pool, conversation.id, "hi").await;
    let reason = last_failure_reason(&pool, conversation.id).await.expect("the first send doesn't fit");
    send(&pool, conversation.id, "Again.").await;

    assert!(reason.contains("is too small for this conversation"), "{reason}");
    assert_eq!(mock.taken_budgets().last(), Some(&u64::from(MIN_USEFUL_REPLY)), "the short reply went: {:?}", mock.taken_budgets());
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
}

/// L-1: a refusal naming a smaller window, before anything's learned of it:
/// the retry keeps to the room it leaves, rather than the usual reply,
/// which is half this window and would be refused again on every send.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_retry_after_a_window_is_named_keeps_to_its_room(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = counting_mock(&pool, Some(20_000), vec![(10_900, 10)]).await;
    set_context_window(&pool, 100_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the retry fits");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(mock.calls(), 2, "one refusal, one retry");
}
