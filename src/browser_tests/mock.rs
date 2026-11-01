//! The tier's mock Anthropic upstream: one server, one provider row
//! (`MOCK_PROVIDER`), and a lane per scenario lane (SME-341's speed pass).
//! Each lane answers its own model name, and keeps its own count of what it
//! has sent, its open replies, its reply length and its hurry, so scenarios
//! running side by side in different lanes never read each other's replies.
//! One provider row, not one per lane: the pages that list providers show
//! what they always did.

use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The mock's one reply streams this many words (`zebra0`..`zebra24`) 200ms
/// apart — slow enough to switch conversations mid-stream: about 5s. A
/// scenario that needs the turn to outlive a page reload, or two tabs,
/// raises it (see `MockUpstream::longer_reply`).
pub(super) const DEFAULT_REPLY_WORDS: usize = 25;

/// One lane's state, shared with the server that answers its model.
#[derive(Clone)]
struct Lane {
    chunks_sent: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
    reply_words: Arc<AtomicUsize>,
    hurried: Arc<AtomicBool>,
}

impl Lane {
    fn new() -> Self {
        Lane {
            chunks_sent: Arc::new(AtomicUsize::new(0)),
            open: Arc::new(AtomicUsize::new(0)),
            reply_words: Arc::new(AtomicUsize::new(DEFAULT_REPLY_WORDS)),
            hurried: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// One lane of the mock: the model its conversations run on, and what its
/// replies have done, so a scenario can wait for a reply to have finished
/// (or been cut off) upstream instead of sleeping for as long as a reply
/// takes.
pub(super) struct MockUpstream {
    pub(super) addr: std::net::SocketAddr,
    pub(super) model: String,
    lane: Lane,
}

/// Raises a lane's reply length for one scenario and puts the default back
/// when it drops, including when the scenario fails, since the lane's next
/// scenario shares it.
pub(super) struct LongerReply(Arc<AtomicUsize>);

impl Drop for LongerReply {
    fn drop(&mut self) {
        self.0.store(DEFAULT_REPLY_WORDS, Ordering::SeqCst);
    }
}

/// A lane sending without its 200ms pauses, until it drops (see
/// `MockUpstream::hurry`), including when the scenario fails.
pub(super) struct Hurried(Arc<AtomicBool>);

impl Drop for Hurried {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Counts one open reply for as long as its body stream lives: the body is
/// dropped when it ends or when the client hangs up (a stopped turn).
struct OpenReply(Arc<AtomicUsize>);

impl Drop for OpenReply {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn event(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// The reply's events: a start, `words` words, and the stop.
fn reply_chunks(words: usize) -> Vec<String> {
    let mut chunks = vec![event("message_start", r#"{"type":"message_start"}"#)];
    for i in 0..words {
        chunks.push(event(
            "content_block_delta",
            &format!(r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"zebra{i} "}}}}"#),
        ));
    }
    chunks.push(event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#));
    chunks.push(event("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#));
    chunks.push(event("message_stop", r#"{"type":"message_stop"}"#));
    chunks
}

/// Streams the lane's reply, a chunk every 200ms unless the lane is hurried.
fn reply(lane: Lane) -> axum::response::Response {
    let chunks = reply_chunks(lane.reply_words.load(Ordering::SeqCst));
    lane.open.fetch_add(1, Ordering::SeqCst);
    let open = OpenReply(lane.open.clone());
    let body = futures_util::StreamExt::then(futures_util::stream::iter(chunks), move |chunk| {
        let lane = lane.clone();
        async move {
            if !lane.hurried.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            lane.chunks_sent.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(chunk)
        }
    });
    let body = futures_util::StreamExt::map(body, move |chunk| {
        let _ = &open;
        chunk
    });
    axum::response::IntoResponse::into_response((
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        axum::body::Body::from_stream(body),
    ))
}

impl MockUpstream {
    /// Starts the server with a lane for each model in `models`. A request
    /// naming no model of theirs is answered by the first lane's.
    pub(super) async fn start(models: &[String]) -> Vec<Self> {
        let lanes: Vec<(String, Lane)> = models.iter().map(|m| (m.clone(), Lane::new())).collect();
        let by_model: Arc<HashMap<String, Lane>> = Arc::new(lanes.iter().cloned().collect());
        let first = lanes.first().map(|(_, lane)| lane.clone()).expect("at least one lane");
        let router = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |body: axum::body::Bytes| {
                let model = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string));
                let lane = model.and_then(|m| by_model.get(&m).cloned()).unwrap_or_else(|| first.clone());
                async move { reply(lane) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the mock upstream");
        let addr = listener.local_addr().expect("mock upstream address");
        tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        lanes.into_iter().map(|(model, lane)| MockUpstream { addr, model, lane }).collect()
    }

    pub(super) fn chunks_sent(&self) -> usize {
        self.lane.chunks_sent.load(Ordering::SeqCst)
    }

    /// Make the lane's reply `words` words long (200ms each) for as long as
    /// the returned guard lives: 5s by default, which is not long enough for
    /// a scenario that reloads a page mid-turn or opens a second tab into a
    /// running turn.
    pub(super) fn longer_reply(&self, words: usize) -> LongerReply {
        self.lane.reply_words.store(words, Ordering::SeqCst);
        LongerReply(self.lane.reply_words.clone())
    }

    /// Send the rest of every open reply at once, and any reply started
    /// while the returned guard lives: for a scenario whose long reply only
    /// had to outlast something (a reload, a second tab) and which then only
    /// waits for it to end (SME-341's speed pass). The reply still ends as it
    /// would, and the turn with it.
    pub(super) fn hurry(&self) -> Hurried {
        self.lane.hurried.store(true, Ordering::SeqCst);
        Hurried(self.lane.hurried.clone())
    }

    pub(super) fn open(&self) -> usize {
        self.lane.open.load(Ordering::SeqCst)
    }

    /// Waits until no reply is open upstream. False on timeout.
    pub(super) async fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.open() > 0 {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        true
    }
}
