//! SME-343: a running turn holds its provider's request slot from its first
//! model call until it ends, so a one-slot server isn't switched between
//! conversations between one turn's calls (the ticket's Evidence: every
//! switch re-prefills the prompt).
//!
//! The turn-level fixtures: a gated upstream classifies each request by the
//! marker text in its conversation's message (`a`, `b`) or as compaction's
//! own (`summary`), answers them in arrival order from a list of bodies, and
//! holds every request until the test releases it — the same shape as
//! `anthropic::stream::tests::limiter`'s, and the reason each test names its
//! provider ids uniquely (9162000311–317: the pools are process-wide by
//! provider id, and 9162000301/302 are `providers/tests/server_tests/llama`'s).

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the gate answers one request with.
enum GateResponse {
    Sse(String),
    /// A flat HTTP error — deliberately not Ollama's tool-call-corruption
    /// shape, so the turn fails on it instead of retrying.
    Error(axum::http::StatusCode, String),
}

/// A mock upstream for the turn-slot tests: classifies each request, records
/// the kinds in arrival order, answers from `responses` by arrival index
/// (clamped to the last), and holds every request until `release_one`.
/// Tracks how many it served at once — with one request slot there is never
/// a reason to see two, and `peak == 1` says the slot was never double-handed.
struct SlotGate {
    arrivals: std::sync::Mutex<Vec<String>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    release: tokio::sync::Semaphore,
    responses: Vec<GateResponse>,
}

/// Decrements `active` however the handler ends — including a Stop aborting
/// the request mid-stream.
struct ActiveGuard(Arc<SlotGate>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The compaction summary T2's gate answers with. Compaction replaces
/// conversation A's raw messages, so afterwards its summary text is the
/// only thing in A's requests that still identifies it — `classify`
/// matches on it.
const T2_SUMMARY_BODY: &str = "Summary: it was going.";

/// Which conversation (or compaction) a request belongs to. Compaction is
/// checked first: its transcript quotes the turn's own messages. A's
/// post-compaction requests no longer carry `call-a` — it lives inside the
/// summary — so A is also recognised by the summary text it carries.
fn classify(request: &serde_json::Value) -> String {
    let text = request.to_string();
    if text.contains("Conversation so far:") {
        "summary".to_string()
    } else if text.contains("call-a") || text.contains(T2_SUMMARY_BODY) {
        "a".to_string()
    } else if text.contains("call-b") {
        "b".to_string()
    } else {
        "unknown".to_string()
    }
}

async fn start_slot_gate(responses: Vec<GateResponse>) -> (std::net::SocketAddr, Arc<SlotGate>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let gate = Arc::new(SlotGate {
        arrivals: Default::default(),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
        responses,
    });
    let state = gate.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            async move {
                use axum::response::IntoResponse;
                let request: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
                let index = {
                    let mut arrivals = state.arrivals.lock().unwrap_or_else(|e| e.into_inner());
                    arrivals.push(classify(&request));
                    arrivals.len() - 1
                };
                let peak = state.active.fetch_add(1, Ordering::SeqCst) + 1;
                state.peak.fetch_max(peak, Ordering::SeqCst);
                let _active = ActiveGuard(state.clone());
                state.release.acquire().await.expect("the gate stays open").forget();
                let response = &state.responses[index.min(state.responses.len() - 1)];
                match response {
                    GateResponse::Sse(body) => {
                        ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body.clone()).into_response()
                    }
                    GateResponse::Error(status, body) => (*status, body.clone()).into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, gate)
}

impl SlotGate {
    fn arrivals(&self) -> Vec<String> {
        self.arrivals.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Waits, at most 5 s, until `n` requests have arrived.
    async fn wait_for_arrivals(&self, n: usize) {
        let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while self.arrivals().len() < n {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "expected {n} requests, saw {:?}", self.arrivals());
    }

    /// Lets one held request answer.
    fn release_one(&self) {
        self.release.add_permits(1);
    }
}

/// Long enough for a request that wasn't held back to reach the gate.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// A conversation's user message, naming it for [`classify`].
fn call_message(marker: &'static str) -> anthropic::AnthropicMessage {
    anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: format!("call-{marker}") }],
    }
}

/// A reply with a text line and a call to a tool that doesn't exist: running
/// it answers with an error result and the loop carries on, so a turn makes
/// two model calls without a sandbox pod anywhere.
fn unknown_tool_call_body(text: &str, name: &str) -> String {
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
            "content_block_start",
            &format!(
                r#"{{"type":"content_block_start","index":1,"content_block":{{"type":"tool_use","id":"toolu_01","name":"{name}","input":{{}}}}}}"#
            ),
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// A provider (id `id`, `max` concurrent requests) at the gate's address,
/// and conversations on it for `a` and `b` — the provider ids unique per
/// test, like `anthropic::stream::tests::limiter`'s, because the limiter's
/// pools are process-wide by provider id. Every turn test holds
/// `lock_turn_tests`, so conversation ids don't collide while one runs.
async fn slot_provider_and_conversations(
    pool: &PgPool,
    addr: std::net::SocketAddr,
    id: i64,
    max: Option<i32>,
) -> (db::InferenceProvider, i64, i64) {
    sqlx::query(&format!("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH {id}"))
        .execute(pool)
        .await
        .expect("renumber");
    let provider = db::create_inference_provider(
        pool, "slot p", "other", &format!("http://{addr}"), "api_key", "k", false, None, true, max,
    )
    .await
    .expect("create provider");
    assert_eq!(provider.id, id);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let conversation = db::create_conversation(pool).await.expect("create conversation");
        db::set_conversation_model(pool, conversation.id, provider.id, "m")
            .await
            .expect("pick the model");
        ids.push(conversation.id);
    }
    (provider, ids[0], ids[1])
}

/// Saves `max` on the provider's form, as the user would.
async fn save_limit(pool: &PgPool, provider: &db::InferenceProvider, max: Option<u32>) {
    let input = crate::providers::ProviderInput {
        name: provider.name.clone(),
        kind: crate::providers::ProviderKind::Other,
        base_url: provider.base_url.clone(),
        auth_kind: crate::providers::AuthKind::ApiKey,
        secret: String::new(),
        prompt_caching: false,
        price_catalog_provider: None,
        keep_reasoning: true,
        max_concurrent_requests: max,
    };
    crate::providers::update_provider(pool, provider.id, input).await.expect("save the limit");
}

async fn finished(turn: tokio::task::JoinHandle<TurnResult>) -> Vec<Message> {
    tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the turn ends")
        .expect("the task ran")
        .expect("the turn succeeds")
}

/// T1: on a one-request provider, a turn keeps its slot between its own
/// calls: B's turn, started in A's tool gap, reaches the server only after
/// A's turn ends — never between A's two calls. Red before the hold exists:
/// A's per-call slot frees when its first call ends, and B takes it there.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_holds_one_slot_across_its_calls(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (addr, gate) = start_slot_gate(vec![
        GateResponse::Sse(unknown_tool_call_body("Thinking.", "no_such_tool_343")),
        GateResponse::Sse(text_reply_body("Done.")),
    ])
    .await;
    let (_provider, a, b) = slot_provider_and_conversations(&pool, addr, 9_162_000_311, Some(1)).await;

    // A pauses between its first reply being saved and its tool call, with
    // its first call's response already released: the gap this ticket is about.
    let (reached, resume) = test_hooks::pause_after_reply_saved(a);
    let turn_a = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a, call_message("a")).await }
    });
    gate.wait_for_arrivals(1).await;
    gate.release_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("A's first reply is saved")
        .expect("hook");

    let turn_b = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, b, call_message("b")).await }
    });
    tokio::time::sleep(SETTLE).await;
    assert_eq!(gate.arrivals(), ["a"], "B waits for the slot A's turn holds; A's second call takes no new one");

    drop(resume); // A's tool runs (an error result), its second call goes on
    gate.wait_for_arrivals(2).await;
    assert_eq!(gate.arrivals(), ["a", "a"], "A's second call went ahead of B");
    gate.release_one(); // A's final reply
    finished(turn_a).await;

    gate.wait_for_arrivals(3).await;
    assert_eq!(gate.arrivals(), ["a", "a", "b"], "B runs only once A's turn ended");
    gate.release_one();
    finished(turn_b).await;
    assert_eq!(gate.peak.load(Ordering::SeqCst), 1, "the slot never double-handed");
}

/// T2: compaction runs inside the turn, so it must reuse the held slot: on a
/// one-slot pool a compaction that acquires its own slot waits forever on
/// the slot the turn itself holds — a self-deadlock. Green before the hold
/// (per-call slots), and the test the stubbed-reuse mutant goes red on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compaction_inside_a_turn_reuses_its_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    // Usage close to the 200,000 assumed window: the second iteration's
    // `should_compact` fires (the ceiling is 200,000 − 16,384).
    let near_limit_tool = with_usage(&unknown_tool_call_body("Thinking.", "no_such_tool_343"), 190_000, 0, 10);
    let (addr, gate) = start_slot_gate(vec![
        GateResponse::Sse(near_limit_tool),
        GateResponse::Sse(text_reply_body(T2_SUMMARY_BODY)),
        GateResponse::Sse(text_reply_body("Done.")),
    ])
    .await;
    let (_provider, a, _b) = slot_provider_and_conversations(&pool, addr, 9_162_000_312, Some(1)).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a, call_message("a")).await }
    });
    // Deadlocked, the turn never ends and the summary never arrives.
    gate.wait_for_arrivals(1).await;
    gate.release_one();
    gate.wait_for_arrivals(2).await;
    gate.release_one();
    gate.wait_for_arrivals(3).await;
    gate.release_one();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the turn ends — compaction reused the held slot, it didn't queue behind it");
    assert_eq!(gate.arrivals(), ["a", "summary", "a"], "compaction's own request reached the server inside the turn");
    assert_eq!(gate.peak.load(Ordering::SeqCst), 1, "the slot never double-handed");
}

/// T3: a Stop drops the turn wherever it waits, and its held slot with it:
/// B, waiting on the slot, goes at once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_stop_frees_the_turns_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (addr, gate) = start_slot_gate(vec![GateResponse::Sse(text_reply_body("held"))]).await;
    let (provider, a, b) = slot_provider_and_conversations(&pool, addr, 9_162_000_313, Some(1)).await;

    let turn_a = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a, call_message("a")).await }
    });
    gate.wait_for_arrivals(1).await;
    let pool_now = crate::providers::limiter::current(provider.id).expect("a pool");
    assert_eq!(pool_now.available_permits(), 0, "A's turn holds the slot while its reply streams");

    let turn_b = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, b, call_message("b")).await }
    });
    tokio::time::sleep(SETTLE).await;
    assert_eq!(gate.arrivals(), ["a"], "B waits for the slot");

    stop_turn_now(a);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn_a)
        .await
        .expect("the stopped turn ends")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");

    gate.wait_for_arrivals(2).await;
    gate.release_one();
    finished(turn_b).await;
    assert_eq!(pool_now.available_permits(), 1, "the stop freed A's slot, and B's came back with its turn");
}

/// T4: a turn that fails frees the slot: B, waiting on it, runs after A's
/// 500. (A held it mid-call, so this is a leak guard for the body-drop path.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_turn_frees_its_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (addr, gate) = start_slot_gate(vec![
        GateResponse::Error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "upstream boom".to_string()),
        GateResponse::Sse(text_reply_body("B answers.")),
    ])
    .await;
    let (provider, a, b) = slot_provider_and_conversations(&pool, addr, 9_162_000_314, Some(1)).await;

    let turn_a = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a, call_message("a")).await }
    });
    gate.wait_for_arrivals(1).await;
    let turn_b = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, b, call_message("b")).await }
    });
    tokio::time::sleep(SETTLE).await;
    assert_eq!(gate.arrivals(), ["a"], "B waits for the slot A's failing call holds");

    gate.release_one(); // A's 500
    let error = tokio::time::timeout(std::time::Duration::from_secs(30), turn_a)
        .await
        .expect("the failed turn ends")
        .expect("join")
        .expect_err("the turn fails on the 500");
    assert!(error.message().contains("upstream boom"), "{error:?}");

    gate.wait_for_arrivals(2).await;
    gate.release_one();
    finished(turn_b).await;
    let pool_now = crate::providers::limiter::current(provider.id).expect("a pool");
    assert_eq!(pool_now.available_permits(), 1, "every slot back");
}

/// T5: a turn that `ask_user`s ends (that is what ends the turn), and frees
/// the slot with it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ask_user_frees_the_turns_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (addr, gate) = start_slot_gate(vec![
        GateResponse::Sse(super::ask_user::ask_body()),
        GateResponse::Sse(text_reply_body("B answers.")),
    ])
    .await;
    let (provider, a, b) = slot_provider_and_conversations(&pool, addr, 9_162_000_315, Some(1)).await;

    let turn_a = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a, call_message("a")).await }
    });
    gate.wait_for_arrivals(1).await;
    gate.release_one();
    finished(turn_a).await;
    assert!(
        db::get_pending_question(&pool, a).await.expect("question").is_some(),
        "A asked and waits on the answer"
    );
    let pool_now = crate::providers::limiter::current(provider.id).expect("a pool");
    assert_eq!(pool_now.available_permits(), 1, "the ending turn's slot came back");

    let turn_b = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, b, call_message("b")).await }
    });
    gate.wait_for_arrivals(2).await;
    gate.release_one();
    finished(turn_b).await;
    assert_eq!(gate.arrivals(), ["a", "b"]);
}

/// T6: a limit saved mid-turn applies at the running turn's next call (the
/// turn-level companion of the stream layer's test of the same name). Guards
/// a design that looked the pool up once: on an unlimited provider the turn
/// holds nothing, and must find the limit that appeared.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_limit_saved_mid_turn_applies_at_the_turns_next_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (addr, gate) = start_slot_gate(vec![
        GateResponse::Sse(unknown_tool_call_body("Thinking.", "no_such_tool_343")),
        GateResponse::Sse(text_reply_body("Done.")),
    ])
    .await;
    let (provider, a, _b) = slot_provider_and_conversations(&pool, addr, 9_162_000_316, None).await;

    let (reached, resume) = test_hooks::pause_after_reply_saved(a);
    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a, call_message("a")).await }
    });
    gate.wait_for_arrivals(1).await;
    gate.release_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("A's first reply is saved")
        .expect("hook");

    // The user sets the limit now, and something else holds the slot: A's
    // next call is the one that has to wait for it.
    save_limit(&pool, &provider, Some(1)).await;
    let held = crate::providers::limiter::current(provider.id)
        .expect("the new pool")
        .try_acquire()
        .expect("the only slot");
    drop(resume);
    tokio::time::sleep(SETTLE).await;
    assert_eq!(gate.arrivals(), ["a"], "A's next call waits on the limit saved mid-turn");

    drop(held);
    gate.wait_for_arrivals(2).await;
    gate.release_one();
    finished(turn).await;
}

/// T7: clearing a limit mid-turn lets the running turn go: its held slot is
/// given up at the turn's next refresh (the closed pool's permit comes back
/// while the turn still runs), and its next call carries no new wait — the
/// turn's own acquire already put its one wait on the turn span, and its
/// calls record none. Red when the refresh's release is stubbed: the turn
/// keeps the closed pool's permit until it ends.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_clearing_a_limit_mid_turn_releases_the_held_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (addr, gate) = start_slot_gate(vec![
        GateResponse::Sse(unknown_tool_call_body("Thinking.", "no_such_tool_343")),
        GateResponse::Sse(text_reply_body("Done.")),
    ])
    .await;
    let (provider, a, _b) = slot_provider_and_conversations(&pool, addr, 9_162_000_317, Some(1)).await;

    let (result, spans) = crate::telemetry::capture_spans(async move {
        let (reached, resume) = test_hooks::pause_after_reply_saved(a);
        let turn = tokio::spawn({
            let pool = pool.clone();
            async move { run_turn(&pool, a, call_message("a")).await }
        });
        gate.wait_for_arrivals(1).await;
        gate.release_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), reached)
            .await
            .expect("A's first reply is saved")
            .expect("hook");

        let old_pool = crate::providers::limiter::current(provider.id).expect("the pool it holds a slot from");
        save_limit(&pool, &provider, None).await;
        drop(resume); // the refresh runs: pool gone → give the slot up

        gate.wait_for_arrivals(2).await; // A's second call — no limit now, sends at once
        assert_eq!(old_pool.available_permits(), 1, "the turn gave the closed pool's slot back while it runs");
        gate.release_one(); // A's final reply
        tokio::time::timeout(std::time::Duration::from_secs(30), turn)
            .await
            .expect("the turn ends")
            .expect("join")
    })
    .await;
    result.expect("the turn succeeds");

    let turns: Vec<_> = spans.iter().filter(|span| span.name == "turn").collect();
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    let records = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
        span.attributes.iter().filter(|kv| kv.key.as_str() == key).count()
    };
    assert_eq!(records(turn, "smelt.slot_wait_ms"), 1, "the turn's own acquire is on its span, once");
    let chats: Vec<_> = spans.iter().filter(|span| span.name.starts_with("chat ")).collect();
    assert_eq!(chats.len(), 2, "both of the turn's calls: {spans:?}");
    assert!(chats.iter().all(|chat| records(chat, "smelt.slot_wait_ms") == 0), "the calls record no wait of their own");
}
