//! SME-386 final review: what a turn does with a server that counts the
//! input and `max_tokens` against its window and says so when they don't
//! fit, as Anthropic does. Each test is a case the property test in
//! `escape.rs` found or the review named, against a server.

use super::ask_user::tool_calls_body;
use super::overflow::{max_context_only, too_long};
use super::window_stop::set_context_window;
use super::*;

/// A server with a window of `window` (`None`: it never refuses), answering
/// model steps in order from `plans`, each the input it counts and the
/// output it writes: only a step it takes moves to the next plan. The last
/// plan's step answers "Done."; the others call a tool, and one planned at
/// `FAILS` fails. A summary is refused once naming `refuse_summary`, if set.
/// With `retry_fresh_count` set, a refusal moves to the next plan too — a
/// retry of a request that shrunk (SME-421's scenario: compaction
/// summarized away the part the count named) is counted at its fresh size.
/// With `window_only` set, a refusal names only its window, never the input
/// it counted — vLLM's shape, and LiteLLM's when it relays it (SME-422).
/// With `cut_off_reply` set, the last plan's answer stops at `max_tokens`
/// rather than finishing — the shape that makes smelt save a cut-off notice
/// naming the budget it stopped at (SME-422 F1).
#[derive(Default)]
struct Counting {
    window: std::sync::Mutex<Option<u32>>,
    plans: std::sync::Mutex<Vec<(i64, i64)>>,
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
    taken: AtomicUsize,
    summaries: AtomicUsize,
    refuse_summary: std::sync::Mutex<Option<u32>>,
    retry_fresh_count: std::sync::atomic::AtomicBool,
    window_only: std::sync::atomic::AtomicBool,
    cut_off_reply: std::sync::atomic::AtomicBool,
}

impl Counting {
    /// The `max_tokens` of each step the server took.
    fn taken_budgets(&self) -> Vec<u64> {
        let log = self.requests.lock().expect("the request log");
        log.iter().filter(|r| r["taken"].as_bool() == Some(true)).filter_map(|r| r["max_tokens"].as_u64()).collect()
    }

    /// The `max_tokens` of every model send, taken or refused: two sends
    /// sharing one is the wedge's signature (SME-422).
    fn sent_budgets(&self) -> Vec<u64> {
        let log = self.requests.lock().expect("the request log");
        log.iter().filter_map(|r| r["max_tokens"].as_u64()).collect()
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
                if let Some(window) = refused {
                    if state.retry_fresh_count.load(Ordering::SeqCst) {
                        state.taken.fetch_add(1, Ordering::SeqCst);
                    }
                    let refusal = if state.window_only.load(Ordering::SeqCst) {
                        max_context_only(window)
                    } else {
                        too_long(u32::try_from(input).expect("a plan's input"), window)
                    };
                    if let Err((status, body)) = refusal {
                        return (status, body).into_response();
                    }
                }
                state.taken.fetch_add(1, Ordering::SeqCst);
                if (input, output) == FAILS {
                    return ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], OVERLOADED.to_string()).into_response();
                }
                let body = if index + 1 >= plans.len() {
                    let body = text_reply_body("Done.");
                    if state.cut_off_reply.load(Ordering::SeqCst) {
                        body.replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#)
                    } else {
                        body
                    }
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
    let (request, _) = build_request(pool, id, &model, None, &[], false, 0).await.expect("a request");
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

/// Found by the property test: a budget-only retry was built from the last
/// usage, which doesn't see the tools growing, so it had the refused
/// request's budget again. It's sized from what the server counted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_budget_only_retry_is_sized_from_the_servers_count(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // The first step measures 5,000; the second counts 60,000, with a budget
    // of half the window from the 5,000.
    let mock = counting_mock(&pool, Some(100_000), vec![(5_000, 10), (60_000, 10)]).await;
    // A cap above the half window, so the budget is that.
    set_context_window(&pool, 100_000, Some(60_000)).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the retry fits");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(mock.calls(), 3);
    assert_eq!(mock.summaries.load(Ordering::SeqCst), 0, "the budget was the whole problem");
}

/// Found by the property test: the server counted far more than the
/// estimate, and the compacted retry's budget, from the estimate alone, was
/// refused again on every send. The shortfall is added to the retry's.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compacted_retry_allows_for_what_the_estimate_missed(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // The estimate is about 10,900; the server counts 20,000.
    let mock = counting_mock(&pool, Some(32_768), vec![(20_000, 10)]).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the retry fits");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(mock.calls(), 2);
}

/// Found by the property test: inside the estimate's buffer a short reply
/// still fits, so a named window there isn't "too small for smelt's system
/// prompt and tools", which no send could get past. The floor says the
/// conversation doesn't fit, and the next send tries a short reply.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_window_inside_the_buffer_lets_the_next_send_try(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mock = counting_mock(&pool, None, vec![(0, 10)]).await;
    let window = window_leaving(&pool, conversation.id, 0).await - 1_000;
    *mock.window.lock().expect("the window") = Some(window);
    *mock.plans.lock().expect("the plans") = vec![(i64::from(window) - 2_000, 10)];
    set_context_window(&pool, window as i32, None).await;

    send(&pool, conversation.id, "hi").await;
    let reason = last_failure_reason(&pool, conversation.id).await.expect("the first send doesn't fit");
    send(&pool, conversation.id, "Again.").await;

    assert!(reason.contains("is too small for this conversation"), "{reason}");
    assert_eq!(mock.taken_budgets(), vec![u64::from(MIN_USEFUL_REPLY)]);
}

/// Found by the property test: a window too small for the system prompt
/// and tools was said on every wake, each after a refused call. It's said
/// without calling now, until the user writes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wakes_after_too_small_for_the_system_prompt_call_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = counting_mock(&pool, Some(2_000), vec![(5_000, 10)]).await;
    set_context_window(&pool, 2_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("too small");
    let calls = mock.calls();

    unnotified_finished_command(&pool, conversation.id, "sme-386-too-small").await;
    let error = wake_conversation(&pool, conversation.id).await.expect_err("still too small");

    assert!(error.to_string().contains("too small for smelt's system prompt and tools"), "{error}");
    assert_eq!(mock.calls(), calls, "the wake called nothing");
    send(&pool, conversation.id, "Again.").await;
    assert_eq!(mock.calls(), calls + 1, "the user's send tries");
}

/// L-4: the floor's own compaction learns a smaller window from its refused
/// summary. What was learned of the room was about the old window, so the
/// request after it is built for the new one, not refused by the floor.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_window_the_floors_compaction_learns_is_read_again(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // Learns it's tight, then a step compacts and its call fails, leaving no
    // usage row: the next request is under the floor, and compacts there.
    let mock = counting_mock(&pool, None, vec![(3_000, 10), (3_000, 10), (3_000, 10), (7_000, 2_000), FAILS, (3_000, 10)]).await;
    let window = window_leaving(&pool, conversation.id, 100).await;
    set_context_window(&pool, window as i32, None).await;
    run_turn(&pool, conversation.id, hello()).await.expect_err("the call fails");
    let smaller = window - 2_000;
    *mock.refuse_summary.lock().expect("the summary refusal") = Some(smaller);

    send(&pool, conversation.id, "Again.").await;

    assert_eq!(last_failure_reason(&pool, conversation.id).await, None);
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    let budget = *mock.taken_budgets().last().expect("a step taken");
    assert!(budget >= u64::from(smaller / 2), "built for the {smaller} window: {budget}");
}

/// SME-421, the ticket's scenario: a tool output the estimate saw at a
/// third of its server count; a refusal names the count; the compaction
/// that answers it summarizes that output away. The retry must be charged
/// only `alone`'s share of the shortfall — the whole shortfall belongs to
/// the history the summary replaced. Charged whole (the bug), the floor
/// believes a phantom size: the turn ends "too small for this conversation"
/// with a request of about `a` refused against a window of `a` plus tens of
/// thousands, `stuck` is set, and every later wake pays for a summary only
/// to re-refuse.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compacted_retry_isnt_refused_for_a_shortfall_the_summary_replaced(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // The mock first (its provider row is what a turn model resolves to),
    // with a placeholder window and plan; the real ones go in below.
    let mock = counting_mock(&pool, None, vec![(0, 10)]).await;
    // `a`: what no compaction shrinks, measured the way `window_leaving`
    // measures it — this binary's `build_request` estimate on an empty
    // conversation.
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("a model");
    let (empty, _) = build_request(&pool, conversation.id, &model, None, &[], false, 0).await.expect("a request");
    let a = estimate_request_tokens(&empty);
    // One user message of 120,000 characters: exactly `m` tokens to the
    // estimate, and 3× that to the server (a base64 blob's chars/4). An
    // assistant ack follows: a replied-to message, so `cap_unanswered`
    // (a summary-prompt-only thing, `continuation.rs`) is no factor.
    let m: u64 = 30_000;
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "a".repeat(120_000) }])
        .await
        .expect("seed the blob");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "Noted.".to_string() }])
        .await
        .expect("seed the ack");
    // The server's count of the refused request and the shortfall it names.
    // The one-token messages the turn adds move `e` by 2; the window lands
    // ~20k inside the interval below either way.
    let e = a + m;
    let r = a + 3 * m;
    let scaled = (r - e).saturating_mul(a) / e;
    // The floor fires once the projected size passes the line
    // `window − COMPACTION_SAFETY_BUFFER − MIN_USEFUL_REPLY`. Put the
    // window where today's projection (the whole shortfall on a request of
    // about `a`) is above it and the fixed one (`alone`'s share, the
    // compacted request's own estimate is `a` plus a handful) is below.
    let line = u64::from(COMPACTION_SAFETY_BUFFER) + u64::from(MIN_USEFUL_REPLY);
    let window = u32::try_from((a + 3 + scaled + line + a + 2 * m + line) / 2).expect("a window");
    *mock.window.lock().expect("the window") = Some(window);
    *mock.plans.lock().expect("the plans") = vec![(i64::try_from(r).expect("r"), 10), (i64::try_from(a + 3).expect("compacted"), 10)];
    mock.retry_fresh_count.store(true, Ordering::SeqCst);
    set_context_window(&pool, window as i32, None).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the compacted retry fits");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(mock.calls(), 2, "one refusal, one retry");
    assert_eq!(mock.summaries.load(Ordering::SeqCst), 1, "one summary answered the refusal");
    assert_eq!(last_failure_reason(&pool, conversation.id).await, None, "nothing was stuck");
}

/// SME-422, the ticket's server: it counts `max_tokens` against its window
/// and refuses when the two don't fit, but names only the window (vLLM, and
/// LiteLLM relaying it). The budget-only retry then repeated the same
/// `max_tokens` on every send — nothing about the request had changed, so
/// every send died and the conversation could never get a reply. Now: with
/// no count named, nothing vouches for the room an estimate-sized budget
/// takes, so the recovery compacts, and the retry answering the refusal
/// takes only a short reply. Sized so the red is the wedge — `W` is 512
/// past the budget-only gate on the estimate — and so the cap bites: the
/// retry's fresh count leaves room for `MIN_USEFUL_REPLY`, not for the
/// ~16,900-token budget the estimate sizes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refusal_that_names_only_its_window_gets_a_reply(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // The mock first (its provider row is what a turn model resolves to),
    // with a placeholder window and plan; the real ones go in below.
    let mock = counting_mock(&pool, None, vec![(0, 10)]).await;
    // `a`: what no compaction shrinks, measured the way `window_leaving`
    // measures it — this binary's `build_request` estimate on an empty
    // conversation.
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("a model");
    let (empty, _) = build_request(&pool, conversation.id, &model, None, &[], false, 0).await.expect("a request");
    let a = estimate_request_tokens(&empty);
    // One user message of 120,000 characters: `m` tokens to the estimate;
    // an assistant ack follows so `cap_unanswered` is no factor (SME-421).
    let m: u64 = 30_000;
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "a".repeat(120_000) }])
        .await
        .expect("seed the blob");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "Noted.".to_string() }])
        .await
        .expect("seed the ack");
    // W: 512 past today's budget-only gate on the estimate, so the red is
    // the wedge. Plan 2 counts the retry's request 11,000 tokens short of
    // the window: room for a 1,024-token reply, not for the estimate-sized
    // budget (~16,900) the wedge sent twice.
    let window = u32::try_from(a + m + 20_992).expect("a window");
    *mock.window.lock().expect("the window") = Some(window);
    *mock.plans.lock().expect("the plans") = vec![
        (i64::try_from(a + 3 * m).expect("first count"), 10),
        (i64::try_from(a + m + 9_992).expect("fresh count"), 10),
    ];
    mock.window_only.store(true, Ordering::SeqCst);
    mock.retry_fresh_count.store(true, Ordering::SeqCst);
    set_context_window(&pool, window as i32, None).await;

    let result = run_turn(&pool, conversation.id, hello()).await;
    // The wedge's count: two sends sharing one `max_tokens` means the retry
    // changed nothing the server could see. (Red today: they are identical,
    // and no summary was ever asked for.)
    let sent = mock.sent_budgets();
    let mut distinct = sent.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        sent.len(),
        "two sends shared a max_tokens — the wedge: {sent:?}, summaries: {}",
        mock.summaries.load(Ordering::SeqCst)
    );
    let saved = result.expect("with no count named the recovery compacts, and the short retry fits");
    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(mock.summaries.load(Ordering::SeqCst), 1, "one summary answered the refusal");
    assert_eq!(mock.taken_budgets().last(), Some(&u64::from(MIN_USEFUL_REPLY)), "the retry took a short reply: {:?}", mock.taken_budgets());
    assert_eq!(last_failure_reason(&pool, conversation.id).await, None, "nothing was stuck");
}

/// F1 (PR #327 round 1): with a model-own output cap below `MIN_USEFUL_REPLY`
/// the retry answering a no-count refusal is already clamped to the cap —
/// the short-reply cap must then touch neither its number nor its label: a
/// reply cut off at 768 tokens is "the most this model can write", not
/// "the room left in the conversation".
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_short_reply_at_a_model_own_cap_is_noticed_at_the_cap_not_the_room(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mock = counting_mock(&pool, None, vec![(0, 10)]).await;
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("a model");
    let (empty, _) = build_request(&pool, conversation.id, &model, None, &[], false, 0).await.expect("a request");
    let a = estimate_request_tokens(&empty);
    let m: u64 = 30_000;
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "a".repeat(120_000) }])
        .await
        .expect("seed the blob");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "Noted.".to_string() }])
        .await
        .expect("seed the ack");
    // Sized as the wedge test: the refusal is window-only, the retry's fresh
    // count leaves room for a short reply — here, for less than one: the
    // model's own maximum, as its listing reports it, is 768.
    let window = u32::try_from(a + m + 20_992).expect("a window");
    const CAP: u32 = 768;
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, context_window, max_output, reported_max_output) SELECT id, $1, $2, $3, $3 FROM inference_providers",
    )
    .bind(crate::providers::test_support::MOCK_MODEL)
    .bind(i32::try_from(window).expect("a window"))
    .bind(i32::try_from(CAP).expect("a cap"))
    .execute(&pool)
    .await
    .expect("seed the model's own cap");
    *mock.window.lock().expect("the window") = Some(window);
    *mock.plans.lock().expect("the plans") = vec![
        (i64::try_from(a + 3 * m).expect("first count"), 10),
        (i64::try_from(a + m + 9_992).expect("fresh count"), 10),
    ];
    mock.window_only.store(true, Ordering::SeqCst);
    mock.retry_fresh_count.store(true, Ordering::SeqCst);
    mock.cut_off_reply.store(true, Ordering::SeqCst);

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the capped retry fits");
    let taken = mock.taken_budgets();
    assert_eq!(taken.last(), Some(&u64::from(CAP)), "the retry took its own cap, not a short reply: {taken:?}");
    // The cut-off notice: what bound the reply was the model's own maximum,
    // not the conversation's room. (Red today: the short-reply cap relabels
    // the surviving budget RoomLeft, and the notice blames the room.)
    let (limit, notice) = saved
        .iter()
        .rev()
        .find_map(|m| {
            m.blocks().ok()?.iter().find_map(|b| match b {
                anthropic::ContentBlock::Text { text } => parse_cut_off_notice(text).map(|(_, limit, _)| (limit, text.clone())),
                _ => None,
            })
        })
        .expect("a cut-off notice was saved");
    assert_eq!(limit, ReplyLimit::ModelMaximum, "{notice}");
}
