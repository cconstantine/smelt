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

fn limited(addr: std::net::SocketAddr, slots: Option<usize>) -> Endpoint {
    Endpoint { limiter: slots.map(|n| Arc::new(tokio::sync::Semaphore::new(n))), ..test_endpoint(addr) }
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
    let endpoint = limited(addr, Some(1));
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
    let endpoint = limited(addr, Some(1));
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
    let endpoint = limited(addr, Some(1));
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
    let endpoint = limited(addr, Some(1));
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
    assert_eq!(endpoint.limiter.as_ref().expect("slots").available_permits(), 1, "every slot back");
}

/// With no limit set (every provider's default), calls go out at once,
/// as before SME-162.
#[tokio::test]
async fn test_a_provider_without_a_limit_sends_at_once() {
    let (addr, upstream) = start_gated_upstream(None, false).await;
    let endpoint = limited(addr, None);
    let a = tokio::spawn(call(&endpoint, "a"));
    let b = tokio::spawn(call(&endpoint, "b"));
    upstream.wait_for_arrivals(2).await;
    assert_eq!(upstream.peak.load(Ordering::SeqCst), 2);
    upstream.release_one();
    upstream.release_one();
    finished(a).await;
    finished(b).await;
}
