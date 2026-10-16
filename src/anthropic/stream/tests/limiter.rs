//! SME-162: a provider's request slots. A model call holds one from
//! before it sends until it ends, so a call over the limit waits in
//! smelt, sending nothing, instead of queueing on the server.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A mock upstream that holds every request until the test releases it
/// (`release`), recording each one's model in arrival order and how many
/// it handled at once at most. It holds a request before answering, or,
/// with `mid_body`, after sending its headers and first event.
struct GatedUpstream {
    arrivals: std::sync::Mutex<Vec<String>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    release: tokio::sync::Semaphore,
    /// The model whose first request is refused with a 429.
    fail_first: Option<&'static str>,
    mid_body: bool,
}

/// `OK_BODY`'s first event, which a reply held mid-body sends at once.
const FIRST_EVENT: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";

impl GatedUpstream {
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

async fn start_gated_upstream(fail_first: Option<&'static str>, mid_body: bool) -> (std::net::SocketAddr, Arc<GatedUpstream>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let upstream = Arc::new(GatedUpstream {
        arrivals: Default::default(),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
        fail_first,
        mid_body,
    });
    let state = upstream.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            async move {
                use axum::response::IntoResponse;
                let request: Value = serde_json::from_str(&body).expect("a JSON request body");
                let model = request["model"].as_str().expect("a model").to_string();
                let first_of_its_model = {
                    let mut arrivals = state.arrivals.lock().unwrap_or_else(|e| e.into_inner());
                    let first = !arrivals.contains(&model);
                    arrivals.push(model.clone());
                    first
                };
                let now = state.active.fetch_add(1, Ordering::SeqCst) + 1;
                state.peak.fetch_max(now, Ordering::SeqCst);
                if state.mid_body {
                    let rest = OK_BODY.strip_prefix(FIRST_EVENT).expect("OK_BODY starts with message_start");
                    let held = futures_util::stream::once(async move {
                        state.release.acquire().await.expect("the gate stays open").forget();
                        state.active.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(rest.as_bytes()))
                    });
                    let body = futures_util::stream::once(async {
                        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(FIRST_EVENT.as_bytes()))
                    })
                    .chain(held);
                    return (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(body),
                    )
                        .into_response();
                }
                state.release.acquire().await.expect("the gate stays open").forget();
                state.active.fetch_sub(1, Ordering::SeqCst);
                if first_of_its_model && state.fail_first == Some(model.as_str()) {
                    return (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        PAUSED_PROVIDER_BODY,
                    )
                        .into_response();
                }
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], OK_BODY).into_response()
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, upstream)
}

/// An endpoint taking the slots of provider `id`, whose limit is set to
/// `slots`. The pools are process-wide by provider id: each test uses
/// an id of its own (9_162_000_1xx and 2xx).
fn limited(addr: std::net::SocketAddr, id: i64, slots: Option<u32>) -> Endpoint {
    crate::providers::limiter::set(id, chrono::NaiveDateTime::default(), slots);
    Endpoint { slots_of: Some(id), ..test_endpoint(addr) }
}

fn call(endpoint: &Endpoint, model: &str) -> impl std::future::Future<Output = Result<StreamedTurn, String>> + Send + 'static {
    let endpoint = endpoint.clone();
    let request = CreateMessageRequest {
        model: model.to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![],
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        chat_template_kwargs: None,
    };
    async move { stream_anthropic_message(&endpoint, &request, |_| {}).await }
}

/// Long enough for a request that wasn't held back to reach the mock.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

async fn finished(task: tokio::task::JoinHandle<Result<StreamedTurn, String>>) -> StreamedTurn {
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the call ends")
        .expect("the task ran")
        .expect("the call succeeds")
}

#[tokio::test]
async fn test_the_second_request_to_a_busy_provider_waits_for_a_slot() {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let endpoint = limited(addr, 9_162_000_111, Some(1));
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), ["a"], "b waits in smelt while a holds the only slot");

    upstream.release_one();
    finished(a).await;
    upstream.wait_for_arrivals(2).await;
    upstream.release_one();
    finished(b).await;
    assert_eq!(upstream.arrivals(), ["a", "b"]);
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 1, "never two at once");
}

/// A's reply has begun streaming (its headers and first event are in)
/// and is held there: its slot is held until the stream ends, not only
/// until the reply's headers.
#[tokio::test]
async fn test_the_slot_is_held_until_the_stream_ends() {
    let (addr, upstream) = start_gated_upstream(None, true).await;
    let endpoint = limited(addr, 9_162_000_112, Some(1));
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), ["a"], "b waits while a's reply streams");

    upstream.release_one();
    finished(a).await;
    upstream.wait_for_arrivals(2).await;
    upstream.release_one();
    finished(b).await;
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 1);
}

/// A's first request is refused with a 429 and retried: its slot is held
/// through the retry's wait, so b, waiting meanwhile, doesn't get in
/// between.
#[tokio::test]
async fn test_the_slot_is_held_across_transient_retries() {
    let (addr, upstream) = start_gated_upstream(Some("a"), false).await;
    let endpoint = limited(addr, 9_162_000_113, Some(1));
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;

    upstream.release_one(); // a's 429; a retries
    upstream.wait_for_arrivals(2).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), ["a", "a"], "a's retry, and b still waiting");

    upstream.release_one();
    finished(a).await;
    upstream.wait_for_arrivals(3).await;
    upstream.release_one();
    finished(b).await;
    assert_eq!(upstream.arrivals(), ["a", "a", "b"]);
}

/// A call stopped while it waits for a slot (a Stop drops the turn's
/// future) takes none: the next call gets the slot a's end frees.
#[tokio::test]
async fn test_a_cancelled_waiter_takes_no_slot() {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let endpoint = limited(addr, 9_162_000_114, Some(1));
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;
    b.abort();
    assert!(b.await.expect_err("aborted").is_cancelled());

    upstream.release_one();
    finished(a).await;
    let c = tokio::spawn(call(&endpoint, "c"));
    upstream.wait_for_arrivals(2).await;
    upstream.release_one();
    finished(c).await;
    assert_eq!(upstream.arrivals(), ["a", "c"], "b never sent");
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 1);
    let pool = crate::providers::limiter::current(endpoint.slots_of.expect("slots")).expect("a pool");
    assert_eq!(pool.available_permits(), 1, "every slot back");
}

/// With no limit set (every provider's default), calls go out at once,
/// as before SME-162.
#[tokio::test]
async fn test_a_provider_without_a_limit_sends_at_once() {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let endpoint = limited(addr, 9_162_000_115, None);
    let a = tokio::spawn(call(&endpoint, "a"));
    let b = tokio::spawn(call(&endpoint, "b"));
    upstream.wait_for_arrivals(2).await;
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 2);
    upstream.release_one();
    upstream.release_one();
    finished(a).await;
    finished(b).await;
}

/// SME-162 review 1: a limit set while a turn runs applies from its next
/// model call. The turn's endpoint was built with no limit; then the
/// user saves 1.
#[sqlx::test]
async fn test_a_limit_saved_mid_turn_applies_to_its_next_call(pool: sqlx::PgPool) {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let provider = provider_at(&pool, addr, 9_162_000_201, None).await;
    let endpoint = crate::providers::endpoint(&provider);
    save_limit(&pool, &provider, Some(1)).await;

    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), ["a"], "the running turn's endpoint takes the new limit");
    upstream.release_one();
    finished(a).await;
    upstream.wait_for_arrivals(2).await;
    upstream.release_one();
    finished(b).await;
}

/// SME-162 review 1: a call waiting for a slot when the limit is raised
/// goes at once, instead of waiting on the old pool.
#[sqlx::test]
async fn test_a_waiting_call_takes_a_raised_limit_at_once(pool: sqlx::PgPool) {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let provider = provider_at(&pool, addr, 9_162_000_202, Some(1)).await;
    let endpoint = crate::providers::endpoint(&provider);
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), ["a"]);

    save_limit(&pool, &provider, Some(2)).await;
    upstream.wait_for_arrivals(2).await;
    upstream.release_one();
    upstream.release_one();
    finished(a).await;
    finished(b).await;
}

/// A provider at `addr` with id `id` and the given limit.
async fn provider_at(pool: &sqlx::PgPool, addr: std::net::SocketAddr, id: i64, max: Option<i32>) -> crate::db::InferenceProvider {
    sqlx::query(&format!("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH {id}"))
        .execute(pool)
        .await
        .expect("renumber");
    let provider = crate::db::create_inference_provider(pool, "p", "other", &format!("http://{addr}"), "api_key", "test-key", false, None, true, max)
        .await
        .expect("create provider");
    assert_eq!(provider.id, id);
    provider
}

/// Saves `max` on the provider's form, as the user would.
async fn save_limit(pool: &sqlx::PgPool, provider: &crate::db::InferenceProvider, max: Option<u32>) {
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

/// SME-162 review 1: a call's wait for a slot is on its span, so time
/// queued in smelt isn't taken for the model's; a call with no limit
/// has none.
#[tokio::test]
async fn test_a_calls_wait_for_a_slot_is_on_its_span() {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    upstream.release.add_permits(10);
    let endpoint = limited(addr, 9_162_000_116, Some(1));
    let held = crate::providers::limiter::current(9_162_000_116)
        .expect("a pool")
        .try_acquire()
        .expect("the only slot");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        drop(held);
    });
    let (result, spans) = crate::telemetry::capture_spans(call(&endpoint, "a")).await;
    result.expect("the call succeeds");
    let chats = chat_spans(&spans);
    let [span] = chats.as_slice() else { panic!("one chat span: {spans:?}") };
    let waited = match span_attribute(span, "smelt.slot_wait_ms") {
        Some(opentelemetry::Value::I64(ms)) => *ms,
        other => panic!("smelt.slot_wait_ms: {other:?}"),
    };
    assert!((250..5_000).contains(&waited), "waited {waited} ms");

    let unlimited = limited(addr, 9_162_000_117, None);
    let (result, spans) = crate::telemetry::capture_spans(call(&unlimited, "b")).await;
    result.expect("the call succeeds");
    let chats = chat_spans(&spans);
    let [span] = chats.as_slice() else { panic!("one chat span: {spans:?}") };
    assert_eq!(span_attribute(span, "smelt.slot_wait_ms"), None, "no limit, no wait");
}

/// SME-162 review 2: deleting a provider (to add it again, say) doesn't
/// let the calls waiting on its slots all go at once: they still take
/// their turn on the server.
#[sqlx::test]
async fn test_deleting_a_provider_keeps_its_waiting_calls_waiting(pool: sqlx::PgPool) {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let provider = provider_at(&pool, addr, 9_162_000_203, Some(1)).await;
    let endpoint = crate::providers::endpoint(&provider);
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    tokio::time::sleep(SETTLE).await;

    crate::providers::delete_provider(&pool, provider.id).await.expect("delete");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), ["a"], "b still waits for a's slot");

    upstream.release_one();
    finished(a).await;
    upstream.wait_for_arrivals(2).await;
    upstream.release_one();
    finished(b).await;
}

/// SME-162 review 2: a limit lowered from 2 to 1 while two calls run lets
/// no new call in until they're under it: never more than either limit.
#[sqlx::test]
async fn test_a_lowered_limit_holds_new_calls_until_the_running_ones_are_under_it(pool: sqlx::PgPool) {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let provider = provider_at(&pool, addr, 9_162_000_204, Some(2)).await;
    let endpoint = crate::providers::endpoint(&provider);
    let a = tokio::spawn(call(&endpoint, "a"));
    let b = tokio::spawn(call(&endpoint, "b"));
    upstream.wait_for_arrivals(2).await;

    save_limit(&pool, &provider, Some(1)).await;
    let c = tokio::spawn(call(&endpoint, "c"));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals().len(), 2, "c waits: two running, limit 1");
    upstream.release_one();
    let first = upstream.arrivals();
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals(), first, "one still running: c still waits");

    upstream.release_one();
    upstream.wait_for_arrivals(3).await;
    upstream.release_one();
    for task in [a, b, c] {
        finished(task).await;
    }
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 2, "never more than the old limit");
}

/// SME-162 review 2: a limit raised from 1 to 2 while one call runs lets
/// one more in, not two: the running call counts.
#[sqlx::test]
async fn test_a_raised_limit_counts_the_calls_already_running(pool: sqlx::PgPool) {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let provider = provider_at(&pool, addr, 9_162_000_205, Some(1)).await;
    let endpoint = crate::providers::endpoint(&provider);
    let a = tokio::spawn(call(&endpoint, "a"));
    upstream.wait_for_arrivals(1).await;

    save_limit(&pool, &provider, Some(2)).await;
    let b = tokio::spawn(call(&endpoint, "b"));
    let c = tokio::spawn(call(&endpoint, "c"));
    upstream.wait_for_arrivals(2).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(upstream.arrivals().len(), 2, "a and one more: limit 2");

    for _ in 0..3 {
        upstream.release_one();
    }
    for task in [a, b, c] {
        finished(task).await;
    }
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 2);
}

/// SME-162 review 2: a call let go by its limit being cleared, after a
/// wait, still has the wait on its span.
#[tokio::test]
async fn test_a_wait_ended_by_clearing_the_limit_is_on_the_span() {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    upstream.release.add_permits(10);
    let id = 9_162_000_118;
    let endpoint = limited(addr, id, Some(1));
    let held = crate::providers::limiter::current(id).expect("a pool").try_acquire().expect("the only slot");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let later = chrono::DateTime::from_timestamp(60, 0).expect("a time").naive_utc();
        crate::providers::limiter::set(id, later, None);
    });
    let (result, spans) = crate::telemetry::capture_spans(call(&endpoint, "a")).await;
    result.expect("the call succeeds");
    drop(held);
    let chats = chat_spans(&spans);
    let [span] = chats.as_slice() else { panic!("one chat span: {spans:?}") };
    let waited = match span_attribute(span, "smelt.slot_wait_ms") {
        Some(opentelemetry::Value::I64(ms)) => *ms,
        other => panic!("smelt.slot_wait_ms: {other:?}"),
    };
    assert!((250..5_000).contains(&waited), "waited {waited} ms");
}
