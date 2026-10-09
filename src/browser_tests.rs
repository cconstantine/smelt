//! The one comprehensive browser test tier — started for `sandbox-visibility`
//! (see SME-10), extended for
//! `auto-compaction`'s context-usage indicator/detail view and compaction
//! divider (see SME-18) — per
//! `docs/testing.md`'s own note that this tier was "worth extending once
//! another feature has a similar need for real-DOM verification." Runs the
//! real app in-process (no `lib.rs` exists, so an external `tests/`
//! integration test couldn't reach `db`/`sandbox`/`anthropic::tools` at all)
//! against a real headless `chrome-headless-shell`, driven over CDP via
//! `chromiumoxide` (no `chromedriver` to download/manage). `#[ignore]`d by
//! default: needs `scripts/browser-check/setup.sh` run first, and a real
//! Postgres + k3s cluster reachable the same way every other real-cluster
//! test in this codebase already assumes.
//!
//! One `#[tokio::test]`, sharing one browser/server/`MANAGER` instance for
//! its whole duration — more than one `#[tokio::test]` here touching
//! `sandbox::init()`/`db::init()` would risk the same
//! `OnceLock`-across-separate-runtimes hazard `docs/testing.md` documents
//! for `PgPool`, the same reasoning `sandbox-terminal`'s own real-cluster
//! test already applied. Inside it, each scenario is its own `scenario_*`
//! function with its own conversations and tabs, run by `run_scenario`
//! under its own time limit: a failing or hung scenario ends alone, the
//! rest still run, and the test fails at the end listing every failure
//! (SME-59). `SMELT_BROWSER_SCENARIOS=name,name` runs just those.
//!
//! Scenarios seed state directly via `db`/`anthropic::tools`; where one
//! needs the model, the model is a slow local mock upstream — this tier
//! verifies the browser/live-event pipeline and DOM rendering, not
//! tool-selection or compaction-trigger *logic* (already covered by
//! `turn`'s own mock-upstream tests), and this test environment (like
//! CI) has no real Anthropic credentials to make a live call with anyway.

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::Browser;

use crate::{anthropic, db, sandbox};

struct BrowserTestHarness {
    browser: Browser,
    server_task: tokio::task::JoinHandle<()>,
    preview_task: tokio::task::JoinHandle<()>,
    base_url: String,
}

impl BrowserTestHarness {
    async fn start() -> Self {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        // dioxus-server's `serve_dioxus_application` needs a pre-bundled
        // WASM/assets directory — the CLI (`dx build`/`dx serve`) normally
        // produces this next to the built executable, which plain `cargo
        // test` never runs. `DIOXUS_PUBLIC_PATH` is dioxus-server's own
        // escape hatch for pointing at one built out-of-band — discovered
        // while first running this test, not anticipated in SME-10's plan.
        let public_path = repo_root.join("target/dx/smelt/debug/web/public");
        if !public_path.is_dir() {
            panic!(
                "no built frontend bundle at {} — run `dx build --platform web` first",
                public_path.display()
            );
        }
        // SAFETY: this test binary is single-threaded at this point (no
        // other threads have been spawned yet that could race a concurrent
        // std::env read) — see the harness's own doc comment on why this is
        // the one test in this module.
        unsafe {
            std::env::set_var("DIOXUS_PUBLIC_PATH", &public_path);
        }

        // Sandbox previews (SME-42) on a port of this test's own, with
        // `SMELT_PREVIEW_URL` naming it, so a link the model shares opens
        // in this browser just as it would for a user. `*.localhost`
        // resolves to this machine inside Chrome with no DNS set up.
        let preview_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the preview listener");
        let preview_url = format!(
            "http://{{port}}-{{conversation}}.preview.localhost:{}",
            preview_listener.local_addr().expect("preview address").port()
        );
        // SAFETY: as above — still single-threaded.
        unsafe {
            std::env::set_var("SMELT_PREVIEW_URL", &preview_url);
        }
        let preview_template = crate::preview::configured_template().expect("the test's preview template");
        let preview_task = tokio::spawn(crate::preview::serve(
            preview_listener,
            preview_template,
            std::sync::Arc::new(|conversation| crate::egress_proxy::sandbox_dial(db::get().clone(), conversation)),
            None,
        ));

        // A plain `cargo test` build doesn't bundle assets: `asset!()`
        // resolves to the source file's own path, which nothing serves, so
        // every page would run unstyled (SME-40 F17). Serve the stylesheet
        // at exactly the URL the page asks for.
        // The same for the highlighting stylesheet (SME-30).
        let stylesheets = {
            use dioxus::prelude::*;
            [
                (asset!("/assets/chat.css").to_string(), "assets/chat.css"),
                (asset!("/assets/highlight.css").to_string(), "assets/highlight.css"),
            ]
        };
        let mut router = crate::build_router();
        for (url, path) in stylesheets {
            let stylesheet = std::fs::read_to_string(repo_root.join(path)).expect("read a stylesheet");
            router = router.route(
                &url,
                axum::routing::get(move || {
                    let stylesheet = stylesheet.clone();
                    async move { ([(axum::http::header::CONTENT_TYPE, "text/css")], stylesheet) }
                }),
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind a test-local port");
        let port = listener
            .local_addr()
            .expect("listener should have a local address")
            .port();
        let server_task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test server error");
        });

        // Same launcher as the app's shared browser, so this Chrome can't
        // outlive the test process either.
        let browser = crate::headless_chrome::launch(&[])
            .await
            .expect("chrome-headless-shell should launch");

        Self {
            browser,
            server_task,
            preview_task,
            base_url: format!("http://127.0.0.1:{port}/"),
        }
    }

    /// Called explicitly at the end of the test rather than via `Drop`
    /// (which can't `.await`); the test runs its scenarios under
    /// `catch_unwind`, so this still runs when one fails.
    async fn shutdown(mut self) {
        let _ = self.browser.close().await;

        self.server_task.abort();
        self.preview_task.abort();
    }
}

/// Polls `document.body.innerText` for `needle` up to `timeout` — the same
/// bounded-retry shape `poll_until_finished` already uses in
/// `sandbox.rs`'s own integration test, applied to DOM content instead of a
/// DB row.
async fn wait_for_text(page: &chromiumoxide::Page, needle: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let js = format!("document.body.innerText.includes({needle:?})");
        if let Ok(result) = page.evaluate(js).await {
            if let Ok(true) = result.into_value::<bool>() {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Not a real UUID — just enough entropy to avoid `command_id` colliding
/// with a leftover row from a previous (especially a panicked, so
/// never-cleaned-up) run against this same real, persistent dev database —
/// same reasoning `sandbox.rs`'s own tests already apply to pod naming.
fn unique_id(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("browser-test-{label}-{nanos}")
}

/// Clicks `selector` once it exists and has stopped moving, and makes sure
/// the click landed on it, retrying until `timeout` if it didn't.
///
/// A chromiumoxide click scrolls the element into view, reads its position,
/// then presses and releases the mouse there, in separate round trips. In
/// the first moments after a load the chat page keeps re-scrolling its
/// transcript to the bottom as data arrives, so a click aimed at an element
/// that moves in between lands on something else. On SME-68 that was 8
/// clicks in 40 on the compaction divider, throttled: the press hit
/// `.messages` and nothing opened. So this waits until the element is still
/// (`wait_for_stable`), clicks, then checks the resulting `click` event's
/// target was that very element (held by reference, so a list re-sorting
/// meanwhile doesn't confuse it): a miss is tried again. A click that
/// loaded another page takes the check with it, and counts as landed. A
/// miss that hit something with its own action does that action too; in
/// practice it has been the transcript's background.
async fn click_when_present(page: &chromiumoxide::Page, selector: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    let watch = format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return false; \
         window.__smeltClickTarget = el; window.__smeltClick = 'no click seen'; \
         if (!window.__smeltClickWatcher) {{ window.__smeltClickWatcher = true; \
         document.addEventListener('click', e => {{ const t = window.__smeltClickTarget; \
         window.__smeltClick = t && (t === e.target || t.contains(e.target)) ? 'hit' : 'clicked ' + (e.target.className || e.target.tagName); }}, true); }} \
         return true; }})()"
    );
    let outcome = "typeof window.__smeltClick === 'string' ? window.__smeltClick : 'navigated'";
    let mut last = String::new();
    loop {
        wait_for_stable(page, selector, deadline).await;
        let watching = match page.evaluate(watch.as_str()).await {
            Ok(value) => value.into_value::<bool>().unwrap_or(false),
            Err(e) => {
                last = format!("couldn't watch the click: {e}");
                false
            }
        };
        if watching {
            match page.find_element(selector).await {
                Err(e) => last = format!("gone before the click: {e}"),
                Ok(element) => match element.click().await {
                    Err(e) => last = format!("the click failed: {e}"),
                    Ok(_) => match page.evaluate(outcome).await {
                        // The page went away mid-read: the click did something.
                        Err(_) => return,
                        Ok(value) => {
                            let result: String = value.into_value().unwrap_or_default();
                            if result == "hit" || result == "navigated" {
                                return;
                            }
                            last = result;
                        }
                    },
                },
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "clicking {selector} didn't land on it: {last}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Waits until `selector`'s element exists and has the same box on two
/// reads 150 ms apart, without scrolling anything itself (that would both
/// hide movement and change the page: scrolling the transcript turns off
/// its stick-to-bottom). When its centre is on screen, it must also be the
/// element there (or contain it), so nothing covers it. Returns its box;
/// panics at `deadline` naming what kept it from settling.
async fn wait_for_stable(page: &chromiumoxide::Page, selector: &str, deadline: tokio::time::Instant) -> String {
    let probe = format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return 'missing'; \
         const r = el.getBoundingClientRect(); if (r.width === 0 || r.height === 0) return 'hidden'; \
         const x = r.x + r.width / 2, y = r.y + r.height / 2; \
         if (x >= 0 && y >= 0 && x < innerWidth && y < innerHeight) {{ const hit = document.elementFromPoint(x, y); \
         if (!hit || !(hit === el || el.contains(hit))) return 'covered by ' + (hit ? (hit.className || hit.tagName) : 'nothing'); }} \
         return [r.x, r.y, r.width, r.height].map(Math.round).join(','); }})()"
    );
    let mut last = String::new();
    loop {
        let state: String = match page.evaluate(probe.as_str()).await {
            Ok(value) => value.into_value().unwrap_or_default(),
            Err(e) => format!("couldn't read it: {e}"),
        };
        if state.contains(',') && !state.starts_with("couldn't") && state == last {
            return state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{selector} never settled to be clicked: {state}"
        );
        last = state;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

const CHAT_INPUT: &str = "input[placeholder=\"Type a message...\"]";

/// What the chat view shows, for scenario 8.
#[derive(Debug, serde::Deserialize)]
struct ViewState {
    input_enabled: bool,
    streaming_bubble: bool,
    shows_reply: bool,
}

async fn view_state(page: &chromiumoxide::Page) -> ViewState {
    page.evaluate(format!(
        "({{
            input_enabled: !document.querySelector({CHAT_INPUT:?}).disabled,
            streaming_bubble: !!document.querySelector('.message-streaming'),
            shows_reply: document.querySelector('.messages').innerText.includes('zebra'),
        }})"
    ))
    .await
    .expect("read the view")
    .into_value()
    .expect("view state")
}

async fn wait_for_element(
    page: &chromiumoxide::Page,
    selector: &str,
    timeout: Duration,
) -> chromiumoxide::Element {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(element) = page.find_element(selector).await {
            return element;
        }
        assert!(tokio::time::Instant::now() < deadline, "{selector} never appeared");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `selector`'s position and size on screen, rounded to whole pixels:
/// `(x, y, width, height)`.
async fn element_box(page: &chromiumoxide::Page, selector: &str) -> (i64, i64, i64, i64) {
    let rect: Vec<f64> = page
        .evaluate(format!(
            "(() => {{ const r = document.querySelector({selector:?}).getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; }})()"
        ))
        .await
        .expect("measure an element")
        .into_value()
        .expect("four numbers");
    (rect[0].round() as i64, rect[1].round() as i64, rect[2].round() as i64, rect[3].round() as i64)
}

/// Waits until exactly `count` elements match `selector`. False on timeout.
async fn wait_for_count(page: &chromiumoxide::Page, selector: &str, count: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let found: usize = page
            .evaluate(format!("document.querySelectorAll({selector:?}).length"))
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or(usize::MAX);
        if found == count {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Waits until the page's client has requested a URL ending in `suffix`,
/// the sign it's hydrated on pages without a conversation (the
/// server-rendered page's own fetches never show up in the browser's
/// resource timings).
async fn wait_for_resource(page: &chromiumoxide::Page, suffix: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let seen: bool = page
            .evaluate(format!(
                "performance.getEntriesByType('resource').some(e => e.name.endsWith({suffix:?}))"
            ))
            .await
            .expect("read resource timings")
            .into_value()
            .expect("a bool");
        if seen {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "the page never requested {suffix}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether the page is asking for a reload (SME-43).
async fn banner_shown(page: &chromiumoxide::Page) -> bool {
    page.evaluate("!!document.querySelector('.stale-bundle-banner')")
        .await
        .expect("look for the reload banner")
        .into_value()
        .expect("a bool")
}

/// Waits until the page's WASM client has hydrated and is live on
/// `conversation_id`: subscribed to its live events, with the one-shot
/// snapshot of each panel pulled. The page says so itself, with
/// `data-live` on the chat panel. Until then the server-rendered page
/// accepts typing with no handlers attached, so input is silently lost.
async fn wait_for_live_client(page: &chromiumoxide::Page, conversation_id: i64) {
    let marker = format!("[data-live=\"{conversation_id}\"]");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let live: bool = page
            .evaluate(format!("document.querySelector({marker:?}) !== null"))
            .await
            .expect("look for the live marker")
            .into_value()
            .expect("a bool");
        if live {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page's client never went live on conversation {conversation_id} (no {marker})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How many times the page has connected to `conversation_id`'s live
/// events and pulled its snapshot (`data-live-pulls`); `None` while it
/// isn't connected. More than one means its stream was reconnected.
async fn live_pulls(page: &chromiumoxide::Page, conversation_id: i64) -> Option<u32> {
    let pulls: Option<String> = page
        .evaluate(format!(
            "document.querySelector('[data-live=\"{conversation_id}\"]')?.dataset.livePulls ?? null"
        ))
        .await
        .expect("read the live marker")
        .into_value()
        .expect("a string or null");
    pulls.and_then(|n| n.parse().ok())
}

/// Clicks conversation `id`'s sidebar entry — an in-app navigation, like a
/// user's click, not a page load (which would end any reply in flight and
/// hide the bug being tested) — and waits until the app is showing it. By
/// id, not title: titles repeat across runs against the same database.
async fn click_conversation(page: &chromiumoxide::Page, id: i64) {
    let clicked: bool = page
        .evaluate(format!(
            "(() => {{
                const item = document.querySelector('.conversation-item[data-conversation-id=\"{id}\"]');
                if (item) item.click();
                return !!item;
            }})()"
        ))
        .await
        .expect("click a conversation")
        .into_value()
        .expect("a bool");
    assert!(clicked, "no sidebar entry for conversation {id}");
    let path = format!("/conversation/{id}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let current: String = page
            .evaluate("location.pathname")
            .await
            .expect("read the location")
            .into_value()
            .expect("a string");
        if current == path {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "clicking conversation {id} left the app at {current}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A mock Anthropic upstream whose one reply streams 25 words
/// (`zebra0`..`zebra24`) 200ms apart — slow enough to switch conversations
/// mid-stream. The test saves it as a provider every conversation it
/// creates uses (`MOCK_PROVIDER`). It counts what it has sent and which
/// replies are still open, so a scenario can wait for a reply to have
/// finished (or been cut off) upstream instead of sleeping for as long as a
/// reply takes.
struct MockUpstream {
    addr: std::net::SocketAddr,
    chunks_sent: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    open: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Counts one open reply for as long as its body stream lives: the body is
/// dropped when it ends or when the client hangs up (a stopped turn).
struct OpenReply(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for OpenReply {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl MockUpstream {
    async fn start() -> Self {
        fn event(name: &str, data: &str) -> String {
            format!("event: {name}\ndata: {data}\n\n")
        }
        let chunks_sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let open = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (sent, opened) = (chunks_sent.clone(), open.clone());
        let router = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let (sent, opened) = (sent.clone(), opened.clone());
                async move {
                    let mut chunks = vec![event("message_start", r#"{"type":"message_start"}"#)];
                    for i in 0..25 {
                        chunks.push(event(
                            "content_block_delta",
                            &format!(
                                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"zebra{i} "}}}}"#
                            ),
                        ));
                    }
                    chunks.push(event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#));
                    chunks.push(event(
                        "message_delta",
                        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                    ));
                    chunks.push(event("message_stop", r#"{"type":"message_stop"}"#));
                    opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let reply = OpenReply(opened);
                    let body = futures_util::StreamExt::then(futures_util::stream::iter(chunks), move |chunk| {
                        let sent = sent.clone();
                        async move {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok::<_, std::io::Error>(chunk)
                        }
                    });
                    let body = futures_util::StreamExt::map(body, move |chunk| {
                        let _ = &reply;
                        chunk
                    });
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(body),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock upstream");
        let addr = listener.local_addr().expect("mock upstream address");
        tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        Self { addr, chunks_sent, open }
    }

    fn chunks_sent(&self) -> usize {
        self.chunks_sent.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn open(&self) -> usize {
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Waits until no reply is open upstream. False on timeout.
    async fn wait_until_idle(&self, timeout: Duration) -> bool {
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

async fn wait_for_text_gone(page: &chromiumoxide::Page, needle: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let js = format!("!document.body.innerText.includes({needle:?})");
        if let Ok(result) = page.evaluate(js).await {
            if let Ok(true) = result.into_value::<bool>() {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What one scenario works with: the shared browser, server and database,
/// plus the conversations and tabs it makes. Every tab a scenario opens
/// through `tab` is closed after it, however it ended: every open smelt
/// tab holds one always-open event stream, and over plain HTTP/1.1 the
/// browser allows only 6 connections per host across all tabs, so tabs a
/// failed scenario left open would starve the ones after it.
struct Scenario<'a> {
    pool: &'a sqlx::PgPool,
    harness: &'a BrowserTestHarness,
    mock: &'a MockUpstream,
    created: &'a std::sync::Mutex<Vec<i64>>,
    tabs: std::sync::Mutex<Vec<chromiumoxide::Page>>,
}

impl Scenario<'_> {
    /// `path` under the app's address.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.harness.base_url)
    }

    /// Opens `url` in a new tab, closed after the scenario.
    async fn tab(&self, url: impl Into<String>) -> chromiumoxide::Page {
        let url: String = url.into();
        let page = self
            .harness
            .browser
            .new_page(url.as_str())
            .await
            .unwrap_or_else(|e| panic!("open {url}: {e}"));
        self.tabs.lock().expect("the tab list lock").push(page.clone());
        page
    }

    /// A new conversation on the mock model, removed at the end of the run.
    async fn conversation(&self) -> crate::models::Conversation {
        new_conversation(self.pool, self.created).await
    }

    /// Closes every tab the scenario opened. A tab it already closed itself
    /// just fails to close again.
    async fn close_tabs(&self) {
        let tabs = std::mem::take(&mut *self.tabs.lock().expect("the tab list lock"));
        for tab in tabs {
            let _ = tokio::time::timeout(Duration::from_secs(5), tab.close()).await;
        }
    }
}

/// One scenario's outcome: how long it took, or why it failed.
type ScenarioResult = (&'static str, Result<Duration, String>);

/// Runs one scenario, unless `only` names others: under its own time limit
/// and `catch_unwind`, so a failure or a hang ends that scenario alone, and
/// its tabs are closed whatever happened.
async fn run_scenario(
    t: &Scenario<'_>,
    only: Option<&[String]>,
    results: &mut Vec<ScenarioResult>,
    known: &mut Vec<&'static str>,
    name: &'static str,
    limit_secs: u64,
    // Boxed by the caller: the biggest scenarios' futures don't fit the
    // test thread's stack inline (SME-90 hit it when every scenario was one
    // future).
    scenario: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>,
) {
    known.push(name);
    if only.is_some_and(|only| !only.iter().any(|n| n == name)) {
        return;
    }
    eprintln!("browser tier: {name} ...");
    let started = tokio::time::Instant::now();
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout(
        Duration::from_secs(limit_secs),
        scenario,
    )))
    .await;
    t.close_tabs().await;
    let result = match outcome {
        Ok(Ok(())) => Ok(started.elapsed()),
        Ok(Err(_)) => Err(format!("didn't finish within {limit_secs} s")),
        Err(panic) => Err(panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panicked".to_string())),
    };
    match &result {
        Ok(took) => eprintln!("browser tier: {name} ok ({:.1} s)", took.as_secs_f64()),
        Err(why) => eprintln!("browser tier: {name} FAILED: {why}"),
    }
    results.push((name, result));
}

/// `SMELT_BROWSER_SCENARIOS`: a comma-separated list of scenario names to
/// run instead of all of them. Set-but-empty is unset, as everywhere in
/// smelt.
fn selected_scenarios() -> Option<Vec<String>> {
    let names = std::env::var("SMELT_BROWSER_SCENARIOS").ok()?;
    let names: Vec<String> = names
        .split(',')
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    (!names.is_empty()).then_some(names)
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_browser_scenarios() {
    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("migrations should apply");
    // Pods and claims are named after conversation, pod and volume ids,
    // and the scratch database would count them from 1 like every
    // real-cluster unit test's, in the same namespace: a pod another run
    // was still stopping held up this run's first sandbox (SME-99).
    db::test_support::start_ids_clear_of_other_runs(pool)
        .await
        .expect("start the ids clear of other runs");
    sandbox::init().await.expect("sandbox manager");

    let harness = BrowserTestHarness::start().await;
    // The model every conversation the test creates runs on: a provider of
    // the test's own, so the dev database's providers and default are left
    // alone (SME-72). Removed with the conversations at the end.
    let mock = MockUpstream::start().await;
    let mock_provider = db::create_inference_provider(
        pool,
        &format!("browser tier mock {}", std::process::id()),
        "anthropic",
        &format!("http://{}", mock.addr),
        "api_key",
        "test-key",
    )
    .await
    .expect("save the mock provider");
    MOCK_PROVIDER.set(mock_provider.id).expect("the mock provider is set once");
    // Every conversation a scenario creates, so they (and their sandbox
    // pods) can be removed afterwards — this runs against a real database
    // and cluster, so leftovers show up in the app's own sidebar and pile
    // up pods until new ones stop starting.
    let created = std::sync::Mutex::new(Vec::new());
    let t = Scenario { pool, harness: &harness, mock: &mock, created: &created, tabs: std::sync::Mutex::new(Vec::new()) };

    let only = selected_scenarios();
    let only = only.as_deref();
    let mut results = Vec::new();
    let mut known = Vec::new();
    let r = &mut results;
    let k = &mut known;
    run_scenario(&t, only, r, k, "terminals", 120, Box::pin(scenario_terminals(&t))).await;
    run_scenario(&t, only, r, k, "context_usage", 60, Box::pin(scenario_context_usage(&t))).await;
    run_scenario(&t, only, r, k, "compaction_divider", 60, Box::pin(scenario_compaction_divider(&t))).await;
    run_scenario(&t, only, r, k, "todo_panel", 60, Box::pin(scenario_todo_panel(&t))).await;
    run_scenario(&t, only, r, k, "reply_stays_in_its_conversation", 60, Box::pin(scenario_reply_stays_in_its_conversation(&t))).await;
    run_scenario(&t, only, r, k, "unrequested_reply", 60, Box::pin(scenario_unrequested_reply(&t))).await;
    run_scenario(&t, only, r, k, "missing_conversation", 30, Box::pin(scenario_missing_conversation(&t))).await;
    run_scenario(&t, only, r, k, "notification_failure_stays", 60, Box::pin(scenario_notification_failure_stays(&t))).await;
    run_scenario(&t, only, r, k, "pods", 120, Box::pin(scenario_pods(&t))).await;
    run_scenario(&t, only, r, k, "stop_a_turn", 60, Box::pin(scenario_stop_a_turn(&t))).await;
    run_scenario(&t, only, r, k, "reply_on_conversation_stream", 60, Box::pin(scenario_reply_on_conversation_stream(&t))).await;
    run_scenario(&t, only, r, k, "busy_session_layout", 120, Box::pin(scenario_busy_session_layout(&t))).await;
    run_scenario(&t, only, r, k, "sidebar", 30, Box::pin(scenario_sidebar(&t))).await;
    run_scenario(&t, only, r, k, "phone_width", 30, Box::pin(scenario_phone_width(&t))).await;
    run_scenario(&t, only, r, k, "not_found", 30, Box::pin(scenario_not_found(&t))).await;
    run_scenario(&t, only, r, k, "tool_rows", 30, Box::pin(scenario_tool_rows(&t))).await;
    run_scenario(&t, only, r, k, "dark_mode", 30, Box::pin(scenario_dark_mode(&t))).await;
    run_scenario(&t, only, r, k, "forms", 30, Box::pin(scenario_forms(&t))).await;
    run_scenario(&t, only, r, k, "empty_conversation", 30, Box::pin(scenario_empty_conversation(&t))).await;
    run_scenario(&t, only, r, k, "sandbox_dev_server", 300, Box::pin(scenario_sandbox_dev_server(&t))).await;
    run_scenario(&t, only, r, k, "agents_md_trust", 60, Box::pin(scenario_agents_md_trust(&t))).await;
    run_scenario(&t, only, r, k, "switch_resets_context_detail", 30, Box::pin(scenario_switch_resets_context_detail(&t))).await;
    run_scenario(&t, only, r, k, "model_picker", 60, Box::pin(scenario_model_picker(&t))).await;
    run_scenario(&t, only, r, k, "oauth_headers", 30, Box::pin(scenario_oauth_headers(&t))).await;
    run_scenario(&t, only, r, k, "stale_bundle", 60, Box::pin(scenario_stale_bundle(&t))).await;
    run_scenario(&t, only, r, k, "transcript_scroll", 60, Box::pin(scenario_transcript_scroll(&t))).await;
    run_scenario(&t, only, r, k, "terminal_follow_after_switch", 180, Box::pin(scenario_terminal_follow_after_switch(&t))).await;
    run_scenario(&t, only, r, k, "context_from_the_keyboard", 60, Box::pin(scenario_context_from_the_keyboard(&t))).await;
    run_scenario(&t, only, r, k, "address_bar_after_session_closes", 60, Box::pin(scenario_address_bar_after_session_closes(&t))).await;
    run_scenario(&t, only, r, k, "error_text", 60, Box::pin(scenario_error_text(&t))).await;
    run_scenario(&t, only, r, k, "settings_two_step", 60, Box::pin(scenario_settings_two_step(&t))).await;
    run_scenario(&t, only, r, k, "pointer_keeps_text_still", 120, Box::pin(scenario_pointer_keeps_text_still(&t))).await;
    run_scenario(&t, only, r, k, "ask_user_card", 60, Box::pin(scenario_ask_user_card(&t))).await;
    run_scenario(&t, only, r, k, "markdown_reply", 60, Box::pin(scenario_markdown_reply(&t))).await;
    run_scenario(&t, only, r, k, "markdown_streaming", 60, Box::pin(scenario_markdown_streaming(&t))).await;
    run_scenario(&t, only, r, k, "markdown_late_image", 90, Box::pin(scenario_markdown_late_image(&t))).await;
    run_scenario(&t, only, r, k, "markdown_long_reply", 90, Box::pin(scenario_markdown_long_reply(&t))).await;
    run_scenario(&t, only, r, k, "streaming_into_a_long_transcript", 120, Box::pin(scenario_streaming_into_a_long_transcript(&t))).await;
    run_scenario(&t, only, r, k, "unreadable_message", 30, Box::pin(scenario_unreadable_message(&t))).await;
    run_scenario(&t, only, r, k, "chat_errors_are_alerts", 30, Box::pin(scenario_chat_errors_are_alerts(&t))).await;
    run_scenario(&t, only, r, k, "message_box_label", 30, Box::pin(scenario_message_box_label(&t))).await;
    run_scenario(&t, only, r, k, "conversation_rows_by_keyboard", 60, Box::pin(scenario_conversation_rows_by_keyboard(&t))).await;

    let mut failures: Vec<String> = results
        .iter()
        .filter_map(|(name, result)| result.as_ref().err().map(|why| format!("{name}: {why}")))
        .collect();
    for name in only.unwrap_or_default() {
        if !known.iter().any(|k| *k == name.as_str()) {
            failures.push(format!("{name}: no such scenario (SMELT_BROWSER_SCENARIOS)"));
        }
    }

    // Cleanup and the leftover check both run before `harness.shutdown()`.
    // The server's handlers run on dioxus's own worker runtimes, and
    // database and cluster connections they opened stay tied to those
    // runtimes. Once the harness shuts down, using one either fails ("A
    // Tokio 1.x context was found, but it is being shutdown", "runtime
    // dropped the dispatch task") or hangs forever (SME-39).
    drop(t);
    let created = created.into_inner().expect("the conversation list lock");
    let pod_ids = sandbox_pod_ids(pool, &created).await;
    remove_conversations(pool, &created).await;
    if let Err(e) = db::delete_inference_provider(pool, mock_provider.id).await {
        eprintln!("failed to delete the mock provider: {e}");
    }
    let mut leftovers = find_leftovers(pool, &created, &pod_ids).await;
    if let Ok(Some(_)) = db::get_inference_provider(pool, mock_provider.id).await {
        leftovers.push(format!("model provider {} in the database", mock_provider.id));
    }
    harness.shutdown().await;
    assert!(
        failures.is_empty(),
        "{} of {} browser scenarios failed:\n  {}",
        failures.len(),
        results.len(),
        failures.join("\n  ")
    );
    assert!(leftovers.is_empty(), "the test left things behind: {leftovers:?}");
}

/// Scenarios 1–4: the sandbox panel's terminals, one flow on one pod. A
/// conversation has at most one live pod now (see SME-11's "One pod per
/// conversation"), so there's no tab bar to click through — both
/// terminals render straight through as soon as the panel loads.
async fn scenario_terminals(t: &Scenario<'_>) {
    let pool = t.pool;
    let conversation = t.conversation().await;

    // --- Scenario 1: cold-load panel population, one pod, two terminals
    // in it. ---
    sandbox::create_pod(pool, conversation.id, Default::default()).await.expect("create_pod");
    let terminal_a1 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a1)");
    let terminal_a2 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a2)");

    let page = t.tab(t.url("")).await;
    // Every scenario assumes the page is styled; a layout check on an
    // unstyled page proves nothing (SME-40 F17: the stylesheet was a 404
    // in this tier, so every page ran unstyled).
    let stylesheet_status: u16 = page
        .evaluate("fetch(document.querySelector('link[rel=stylesheet]').href).then(r => r.status)")
        .await
        .expect("fetch the stylesheet")
        .into_value()
        .expect("a status code");
    assert_eq!(stylesheet_status, 200, "the page's stylesheet doesn't load");
    // Clicking its sidebar entry, same as a real user, though a direct
    // `/conversation/{id}` URL would work too now that routing exists. By
    // id: another scenario's conversation can sort first.
    click_when_present(
        &page,
        &format!(".conversation-item[data-conversation-id=\"{}\"]", conversation.id),
        Duration::from_secs(10),
    )
    .await;

    for text in [format!("terminal {terminal_a1}"), format!("terminal {terminal_a2}")] {
        assert!(
            wait_for_text(&page, &text, Duration::from_secs(10)).await,
            "cold snapshot should render {text}"
        );
    }

    // --- Scenario 2: live streaming output, no reload ---
    // `anthropic::tools::execute` (unlike sandbox.rs's own lower-level
    // integration test, which calls db::create_terminal_command +
    // sandbox::send_command directly) already creates the
    // terminal_commands row itself — this is the real tool-dispatch
    // entry point, one level up.
    let command_id = unique_id("cmd-1");
    anthropic::tools::execute(
        pool,
        conversation.id,
        &command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal_a1, "command": "echo hello_from_browser_test"}),
    )
    .await
    .expect("run_terminal_command");
    assert!(
        wait_for_text(&page, "hello_from_browser_test", Duration::from_secs(15)).await,
        "live output should stream into the DOM with no page reload"
    );

    // --- Scenario 3: terminate_terminal removes exactly the right card ---
    sandbox::terminate_terminal(pool, terminal_a2).await.expect("terminate_terminal (a2)");
    assert!(
        wait_for_text_gone(&page, &format!("terminal {terminal_a2}"), Duration::from_secs(10)).await,
        "a terminated terminal's card should disappear"
    );
    assert!(
        page.evaluate("document.body.innerText").await.expect("read body text").into_value::<String>().expect("string")
            .contains(&format!("terminal {terminal_a1}")),
        "a sibling terminal in the same pod should be completely unaffected"
    );
    // A terminal that goes away while the tab is reconnecting: its
    // live event reaches nobody, and the reconnect's snapshot is what
    // must take the card away (SME-43; it used to stay for good).
    // `forget` ends the tab's stream, which reconnects 1.5 s later.
    let terminal_a3 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a3)");
    assert!(
        wait_for_text(&page, &format!("terminal {terminal_a3}"), Duration::from_secs(10)).await,
        "a new terminal's card should appear live"
    );
    crate::events::forget(conversation.id);
    sandbox::terminate_terminal(pool, terminal_a3).await.expect("terminate_terminal (a3)");
    assert!(
        wait_for_text_gone(&page, &format!("terminal {terminal_a3}"), Duration::from_secs(10)).await,
        "a terminal terminated while the tab was reconnecting should be gone once it has"
    );

    // --- Scenario 4: reload mid-command reconstructs state, live updates resume ---
    let long_command_id = unique_id("cmd-2");
    anthropic::tools::execute(
        pool,
        conversation.id,
        &long_command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal_a1, "command": "sleep 3 && echo done_after_reload"}),
    )
    .await
    .expect("run_terminal_command");

    page.goto(t.url("")).await.expect("reload the app");
    click_when_present(
        &page,
        &format!(".conversation-item[data-conversation-id=\"{}\"]", conversation.id),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal_a1}"), Duration::from_secs(10)).await,
        "the fresh page load's snapshot pull should reconstruct the terminal"
    );
    assert!(
        wait_for_text(&page, "done_after_reload", Duration::from_secs(15)).await,
        "live updates should resume after the reload, not just the pre-reload snapshot"
    );

    // Best-effort teardown of what this scenario created.
    let _ = sandbox::terminate_terminal(pool, terminal_a1).await;
    let _ = sandbox::terminate_pod(pool, conversation.id).await;
}

/// Seeds `conversation` with one user message and `input`/`output` tokens of
/// usage, for the context meter.
async fn seed_usage(pool: &sqlx::PgPool, conversation: i64, input_tokens: i64, output_tokens: i64) {
    db::create_message(
        pool,
        conversation,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "hello".to_string(),
        }],
    )
    .await
    .expect("seed a message");
    db::upsert_conversation_usage(
        pool,
        conversation,
        &anthropic::TokenUsage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
}

/// Scenario 5: the always-visible context-usage indicator and its
/// click-through detail view — see SME-18. Seeded directly
/// (db::create_message/upsert_conversation_usage) rather than sent through
/// the model: this tier verifies the DOM, and a real Anthropic call needs
/// credentials this test environment (and CI) doesn't have.
async fn scenario_context_usage(t: &Scenario<'_>) {
    let context_conversation = t.conversation().await;
    seed_usage(t.pool, context_conversation.id, 40_000, 10_000).await;

    let context_page = t.tab(t.url(&format!("conversation/{}", context_conversation.id))).await;
    assert!(
        wait_for_text(&context_page, "25% of context", Duration::from_secs(10)).await,
        "the always-visible indicator should reflect the seeded usage \
         (40_000 + 10_000 of a 200_000 default window = 25%)"
    );

    click_when_present(&context_page, ".context-usage-bar", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&context_page, "Tools (", Duration::from_secs(10)).await,
        "clicking the indicator should open the detail view, listing every available tool"
    );
    assert!(
        wait_for_text(
            &context_page,
            "Tokens — input: 40000, output: 10000",
            Duration::from_secs(5)
        )
        .await,
        "the detail view should show the same real usage numbers the indicator did"
    );
    // The system prompt keeps its line breaks, and the dialog doesn't
    // scroll sideways: it was one run-together paragraph in a dialog
    // that scrolled both ways (SME-41 D7).
    let prompt_view: Vec<String> = context_page
        .evaluate(
            "(() => { const p = document.querySelector('.context-detail-prompt'); const panel = document.querySelector('.context-detail-panel'); \
             return [p ? getComputedStyle(p).whiteSpace : 'missing', String(panel.scrollWidth - panel.clientWidth)]; })()",
        )
        .await
        .expect("inspect the detail view")
        .into_value()
        .expect("strings");
    assert_eq!(prompt_view[0], "pre-wrap", "the system prompt should keep its line breaks: {prompt_view:?}");
    assert!(prompt_view[1].parse::<f64>().unwrap_or(1.0) <= 0.0, "the detail view scrolls sideways: {prompt_view:?}");
}

/// Scenario 6: a compaction event renders as a distinct, collapsed-by-default
/// divider — not an ordinary chat bubble — and expands to reveal the real
/// summary text on click. Seeded directly (a real trigger/summarization
/// round trip is already covered by turn's own mock-upstream
/// integration test; this tier's job is the DOM, not the backend logic).
async fn scenario_compaction_divider(t: &Scenario<'_>) {
    let pool = t.pool;
    let conversation = t.conversation().await;
    seed_usage(pool, conversation.id, 40_000, 10_000).await;
    db::create_message(
        pool,
        conversation.id,
        "assistant",
        &[anthropic::ContentBlock::CompactionSummary {
            summary: "the user said hello; nothing else happened".to_string(),
            covers_through_message_id: 1,
        }],
    )
    .await
    .expect("seed a compaction summary message");

    let context_page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&context_page, conversation.id).await;
    assert!(
        wait_for_text(&context_page, "Conversation compacted", Duration::from_secs(10)).await,
        "a CompactionSummary block should render as its own distinct divider"
    );
    assert!(
        wait_for_text_gone(
            &context_page,
            "the user said hello; nothing else happened",
            Duration::from_secs(2)
        )
        .await,
        "collapsed by default — the summary text itself shouldn't be visible yet"
    );
    click_when_present(
        &context_page,
        ".compaction-summary-header",
        Duration::from_secs(5),
    )
    .await;
    // The open state first: if it's closed, the click missed or
    // something closed it (SME-68), which the text alone can't tell.
    assert!(
        wait_for_count(&context_page, ".compaction-summary-block[open]", 1, Duration::from_secs(5)).await,
        "the divider should be open after clicking its header"
    );
    assert!(
        wait_for_text(
            &context_page,
            "the user said hello; nothing else happened",
            Duration::from_secs(5)
        )
        .await,
        "expanding the divider should reveal the real summary text"
    );
}

/// Scenario 7: the todo panel — cold-load population from a seeded list,
/// then a live, full-replace update with no reload, via the real todowrite
/// tool (not a hand-built event) — see SME-20.
async fn scenario_todo_panel(t: &Scenario<'_>) {
    let pool = t.pool;
    let todo_conversation = t.conversation().await;
    db::set_conversation_todos(
        pool,
        todo_conversation.id,
        &[
            anthropic::tools::TodoItem {
                content: "write the plan".to_string(),
                status: anthropic::tools::TodoStatus::Completed,
            },
            anthropic::tools::TodoItem {
                content: "implement".to_string(),
                status: anthropic::tools::TodoStatus::InProgress,
            },
        ],
    )
    .await
    .expect("seed initial todos");

    let todo_page = t.tab(t.url(&format!("conversation/{}", todo_conversation.id))).await;
    assert!(
        wait_for_text(&todo_page, "write the plan", Duration::from_secs(10)).await,
        "cold load should show the seeded todo list"
    );
    assert!(
        wait_for_text(&todo_page, "implement", Duration::from_secs(5)).await,
        "cold load should show every seeded item, not just the first"
    );

    anthropic::tools::execute(
        pool,
        todo_conversation.id,
        "toolu_todo_test",
        "todowrite",
        &serde_json::json!({"todos": [{"content": "ship it", "status": "pending"}]}),
    )
    .await
    .expect("todowrite should succeed");

    assert!(
        wait_for_text(&todo_page, "ship it", Duration::from_secs(10)).await,
        "a live todowrite call should update the panel with no reload"
    );
    assert!(
        wait_for_text_gone(&todo_page, "write the plan", Duration::from_secs(5)).await,
        "todowrite is a full replace — the old list shouldn't still be showing"
    );
}

/// Seeds `conversation` with one user message saying `text`, which also
/// titles it.
async fn seed_user_message(pool: &sqlx::PgPool, conversation: i64, text: &str) {
    db::create_message(
        pool,
        conversation,
        "user",
        &[anthropic::ContentBlock::Text { text: text.to_string() }],
    )
    .await
    .expect("seed a user message");
}

/// Types `text` into the page's message box and sends it.
async fn send_from(page: &chromiumoxide::Page, text: &str) {
    let input = wait_for_element(page, CHAT_INPUT, Duration::from_secs(10)).await;
    input.focus().await.expect("focus the message box");
    input.type_str(text).await.expect("type a message");
    input.press_key("Enter").await.expect("send it");
}

/// Scenario 8: a reply streaming into one conversation stays in that
/// conversation. Switching to another mid-stream must leave the other one
/// usable (not disabled while the first one's reply is in flight) and must
/// never show the first one's reply there; going back shows the finished
/// reply, once. The model is a slow mock upstream, so the switch lands
/// mid-stream.
async fn scenario_reply_stays_in_its_conversation(t: &Scenario<'_>) {
    let pool = t.pool;
    let streaming = t.conversation().await;
    let other = t.conversation().await;
    seed_user_message(pool, other.id, "seeded message in B").await;

    let chat = t.tab(t.url(&format!("conversation/{}", streaming.id))).await;
    wait_for_live_client(&chat, streaming.id).await;
    send_from(&chat, "hello from A").await;
    assert!(
        wait_for_text(&chat, "zebra0", Duration::from_secs(10)).await,
        "A's reply should start streaming"
    );

    click_conversation(&chat, other.id).await;
    assert!(
        wait_for_text(&chat, "seeded message in B", Duration::from_secs(5)).await,
        "should now be showing B"
    );
    // A's sidebar row says it's working. Its going away below is how this
    // tab shows it has had the events after A's turn ended.
    let a_busy = format!(".conversation-item[data-conversation-id='{}'] .conversation-busy", streaming.id);
    assert!(
        wait_for_count(&chat, &a_busy, 1, Duration::from_secs(5)).await,
        "the sidebar should mark A as working while its reply streams"
    );
    for moment in ["right after switching", "a second later"] {
        let b = view_state(&chat).await;
        assert!(b.input_enabled, "B's message box is disabled {moment}: {b:?}");
        assert!(!b.streaming_bubble, "B shows a streaming bubble {moment}: {b:?}");
        assert!(!b.shows_reply, "B shows A's reply {moment}: {b:?}");
        // Five more of A's words (a second's worth) go out meanwhile.
        let sent = t.mock.chunks_sent();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while t.mock.chunks_sent() < sent + 5 && t.mock.open() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // Let A's reply finish, and be saved, while B is still on screen.
    assert!(t.mock.wait_until_idle(Duration::from_secs(15)).await, "A's reply never finished upstream");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let saved = db::list_messages(pool, streaming.id).await.expect("read A's messages");
        if saved.iter().any(|m| m.role == "assistant" && m.content.contains("zebra24")) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "A's finished reply was never saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Saved isn't shown: the turn's events still have to reach this tab. A
    // turn ends after its reply is saved, and this tab hears of it on B's
    // own stream (the sidebar's busy mark), so once the mark is gone,
    // anything of A's that was going to land in B has had its chance.
    assert!(
        wait_for_count(&chat, &a_busy, 0, Duration::from_secs(10)).await,
        "A's busy mark never went away after its turn ended"
    );
    let b = view_state(&chat).await;
    assert!(!b.shows_reply, "A's finished reply landed in B: {b:?}");
    assert!(b.input_enabled, "B's message box is disabled after A finished: {b:?}");

    click_conversation(&chat, streaming.id).await;
    assert!(
        wait_for_text(&chat, "zebra24", Duration::from_secs(10)).await,
        "A should show its finished reply after switching back"
    );
    let copies: usize = chat
        .evaluate("document.querySelector('.messages').innerText.split('zebra24').length - 1")
        .await
        .expect("count the reply")
        .into_value()
        .expect("a number");
    assert_eq!(copies, 1, "A's reply should appear exactly once");
    // A's first message titled it; the sidebar should show that
    // without a reload.
    let title_selector = format!(
        ".conversation-item[data-conversation-id=\"{}\"] .conversation-title",
        streaming.id
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let title: String = chat
            .evaluate(format!("document.querySelector({title_selector:?})?.innerText ?? ''"))
            .await
            .expect("read A's sidebar title")
            .into_value()
            .expect("a string");
        if title.contains("hello from A") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "A's sidebar title is still {title:?} after its first message"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Scenario 9: a reply the watching tab didn't ask for — a finished
/// command's notice, another tab's send — still reaches it live, with no
/// reload.
async fn scenario_unrequested_reply(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let watcher = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&watcher, conversation.id).await;
    crate::turn::run_turn(
        t.pool,
        conversation.id,
        anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "sent from somewhere else".to_string(),
            }],
        },
    )
    .await
    .expect("a turn run outside the watching tab should succeed");
    assert!(
        wait_for_text(&watcher, "zebra24", Duration::from_secs(10)).await,
        "the watching tab never showed a reply it didn't send itself"
    );
}

/// Scenario 10: a conversation that doesn't exist says so, rather than
/// offering a chat box whose send fails with a raw database error.
async fn scenario_missing_conversation(t: &Scenario<'_>) {
    let missing = t.tab(t.url("conversation/987654321")).await;
    assert!(
        wait_for_text(&missing, "doesn't exist", Duration::from_secs(10)).await,
        "a missing conversation should say it doesn't exist"
    );
    assert!(
        missing.find_element(CHAT_INPUT).await.is_err(),
        "a missing conversation shouldn't offer a message box"
    );
}

/// Scenario 11: a failed background notification belongs to its
/// conversation — switching away clears it.
async fn scenario_notification_failure_stays(t: &Scenario<'_>) {
    let failing = t.conversation().await;
    let elsewhere = t.conversation().await;
    seed_user_message(t.pool, elsewhere.id, "hello from A").await;
    let watcher = t.tab(t.url(&format!("conversation/{}", failing.id))).await;
    wait_for_live_client(&watcher, failing.id).await;
    crate::events::publish(
        failing.id,
        crate::events::ConversationEvent::NotificationDeliveryFailed {
            detail: "scenario 11 failure".to_string(),
        },
    );
    assert!(
        wait_for_text(&watcher, "scenario 11 failure", Duration::from_secs(10)).await,
        "the watching tab should show B's notification failure"
    );
    click_conversation(&watcher, elsewhere.id).await;
    assert!(
        wait_for_text(&watcher, "hello from A", Duration::from_secs(10)).await,
        "switching to A should show A's messages"
    );
    assert!(
        wait_for_text_gone(&watcher, "scenario 11 failure", Duration::from_secs(2)).await,
        "B's notification failure followed the tab to A"
    );
}

/// Scenario 12: pods. A pod created in one conversation shows up as a dot
/// in another tab's sidebar, live; the pods page lists it; Stop there
/// removes the row, and the dot goes away, live.
async fn scenario_pods(t: &Scenario<'_>) {
    let pool = t.pool;
    let with_pod = t.conversation().await;
    seed_user_message(pool, with_pod.id, "scenario 12").await;
    let watching = t.conversation().await;
    let sidebar_tab = t.tab(t.url(&format!("conversation/{}", watching.id))).await;
    wait_for_live_client(&sidebar_tab, watching.id).await;
    let dot = format!(".conversation-item[data-conversation-id=\"{}\"] .live-pod-dot", with_pod.id);
    assert!(
        wait_for_count(&sidebar_tab, &dot, 0, Duration::from_secs(5)).await,
        "no dot before the conversation has a pod"
    );
    let pod_id = sandbox::create_pod(pool, with_pod.id, Default::default()).await.expect("create_pod");
    assert!(
        wait_for_count(&sidebar_tab, &dot, 1, Duration::from_secs(10)).await,
        "the sidebar should mark a conversation whose pod started, without a reload"
    );

    let pods_page = t.tab(t.url("pods")).await;
    wait_for_resource(&pods_page, "/api/pods").await;
    let row = format!("tr[data-pod-id=\"{pod_id}\"]");
    assert!(
        wait_for_count(&pods_page, &row, 1, Duration::from_secs(10)).await,
        "the pods page should list the pod"
    );
    assert!(
        wait_for_text(&pods_page, "scenario 12", Duration::from_secs(5)).await,
        "the row should name its conversation"
    );
    let stop = format!("{row} .pod-stop");
    let neighbour = format!("{row} td:nth-last-child(2)");
    // In view and still first, so the click's own scrolling can't move
    // it between the two measurements.
    wait_for_stable(&pods_page, &stop, tokio::time::Instant::now() + Duration::from_secs(5)).await;
    let before = (element_box(&pods_page, &stop).await, element_box(&pods_page, &neighbour).await);
    click_when_present(&pods_page, &stop, Duration::from_secs(5)).await;
    let confirm = format!("{row} .pod-stop.confirm");
    wait_for_element(&pods_page, &confirm, Duration::from_secs(5)).await;
    let after = (element_box(&pods_page, &confirm).await, element_box(&pods_page, &neighbour).await);
    assert_eq!(
        before, after,
        "arming Stop must not move or resize the button or its neighbours (button, cell to its left)"
    );
    click_when_present(&pods_page, &confirm, Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&pods_page, &row, 0, Duration::from_secs(20)).await,
        "a stopped pod's row should go away"
    );
    assert!(
        wait_for_count(&sidebar_tab, &dot, 0, Duration::from_secs(10)).await,
        "the dot should go away when the pod stops, without a reload"
    );
}

/// Scenario 13: stopping a turn. The model (the slow mock) is mid-reply;
/// Stop shows in the sending tab and in a second tab watching the same
/// conversation; stopping ends the reply for good, says "Stopped.", and
/// leaves the chat usable.
async fn scenario_stop_a_turn(t: &Scenario<'_>) {
    let to_stop = t.conversation().await;
    let url = t.url(&format!("conversation/{}", to_stop.id));
    let sender = t.tab(url.clone()).await;
    let observer = t.tab(url).await;
    wait_for_live_client(&sender, to_stop.id).await;
    wait_for_live_client(&observer, to_stop.id).await;
    send_from(&sender, "please stop me").await;
    assert!(
        wait_for_text(&sender, "zebra0", Duration::from_secs(10)).await,
        "the reply should start streaming"
    );
    assert!(
        wait_for_count(&observer, ".stop-turn", 1, Duration::from_secs(5)).await,
        "a tab that didn't send should also offer Stop while the turn runs"
    );
    // And both say the model is working, with how long it's been at it:
    // before, a slow model's turn looked like nothing at all was
    // happening (SME-41 D1).
    for tab in [&sender, &observer] {
        assert!(
            wait_for_count(tab, ".turn-working", 1, Duration::from_secs(5)).await,
            "a running turn should show that the model is working"
        );
    }
    // And the sidebar marks the conversation as busy, so it's visible
    // which conversations are working (SME-41 D9).
    let busy_mark = format!(".conversation-item[data-conversation-id='{}'] .conversation-busy", to_stop.id);
    assert!(
        wait_for_count(&observer, &busy_mark, 1, Duration::from_secs(5)).await,
        "the sidebar should mark a conversation whose turn is running"
    );
    click_when_present(&sender, ".stop-turn", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&sender, "Stopped.", Duration::from_secs(5)).await,
        "stopping should say so"
    );
    assert!(
        wait_for_count(&sender, ".stop-turn", 0, Duration::from_secs(5)).await
            && wait_for_count(&observer, ".stop-turn", 0, Duration::from_secs(5)).await,
        "Stop should go away in every tab once the turn has ended"
    );
    assert!(
        wait_for_count(&sender, ".turn-working", 0, Duration::from_secs(5)).await
            && wait_for_count(&observer, ".turn-working", 0, Duration::from_secs(5)).await,
        "the working line should go away once the turn has ended"
    );
    assert!(
        wait_for_count(&observer, &busy_mark, 0, Duration::from_secs(5)).await,
        "the sidebar's busy mark should go away once the turn has ended"
    );
    // Once the mock's reply is no longer open upstream (cut off, or, if the
    // stop didn't take, finished), nothing more of it can arrive.
    assert!(
        t.mock.wait_until_idle(Duration::from_secs(10)).await,
        "the stopped reply's response was never closed"
    );
    assert!(
        !wait_for_text(&sender, "zebra24", Duration::from_millis(500)).await,
        "a stopped reply must not keep arriving"
    );
    let input = wait_for_element(&sender, CHAT_INPUT, Duration::from_secs(5)).await;
    let disabled: bool = input
        .call_js_fn("function() { return this.disabled; }", false)
        .await
        .expect("read disabled")
        .result
        .value
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    assert!(!disabled, "the message box should be usable after a stop");
}

/// Scenario 14: replies travel on the conversation stream, so every tab
/// sees one live, a reload mid-reply keeps the text so far, and the sender
/// sees its own message once. And five tabs, a streaming reply and a Stop
/// from another tab fit under the browser's 6-connections-per-host limit:
/// a tab holds one connection even while a reply streams, which leaves the
/// sixth for ordinary requests (a tab's own loading needs one too). When
/// the reply had its own stream, it took the sixth, and Stop queued.
async fn scenario_reply_on_conversation_stream(t: &Scenario<'_>) {
    let shared = t.conversation().await;
    let url = t.url(&format!("conversation/{}", shared.id));
    let mut tabs = Vec::new();
    for _ in 0..5 {
        let tab = t.tab(url.clone()).await;
        wait_for_live_client(&tab, shared.id).await;
        tabs.push(tab);
    }
    send_from(&tabs[0], "stream to everyone").await;
    assert!(
        wait_for_text(&tabs[4], "zebra2", Duration::from_secs(10)).await,
        "a tab that didn't send should see the reply stream in"
    );
    assert!(
        !wait_for_text(&tabs[4], "zebra24", Duration::from_millis(100)).await,
        "...while it's still streaming"
    );
    tabs[3].reload().await.expect("reload a tab mid-reply");
    wait_for_live_client(&tabs[3], shared.id).await;
    assert!(
        wait_for_text(&tabs[3], "zebra", Duration::from_secs(3)).await
            && !wait_for_text(&tabs[3], "zebra24", Duration::from_millis(100)).await,
        "a tab reloaded mid-reply should show the reply so far"
    );
    let sent_count: usize = tabs[0]
        .evaluate("document.querySelector('.messages').innerText.split('stream to everyone').length - 1")
        .await
        .expect("count the sent message")
        .into_value()
        .expect("a number");
    assert_eq!(sent_count, 1, "the sender should see its own message once in the conversation");
    click_when_present(&tabs[4], ".stop-turn", Duration::from_secs(5)).await;
    for (i, tab) in tabs.iter().enumerate() {
        assert!(
            wait_for_count(tab, ".stop-turn", 0, Duration::from_secs(5)).await,
            "tab {i}: Stop should go away once the turn has stopped"
        );
    }
}

/// Scenario 15: with a browsing session open, the chat stays usable at a
/// laptop width (this browser is 1400x900). The live frame used to be a
/// fixed 1280px that couldn't shrink, which squeezed the messages and
/// input to 48px and made the page scroll sideways (SME-40 F2).
async fn scenario_busy_session_layout(t: &Scenario<'_>) {
    let pool = t.pool;
    let browsing = t.conversation().await;
    crate::browsing::open_session(browsing.id, crate::egress_proxy::sandbox_dial(pool.clone(), browsing.id))
        .await
        .expect("open a browsing session");
    // With a todo list and a sandbox terminal open too, as in a real
    // session: side by side, the three panels shared ~450px, so the
    // browser was tiny and the terminal pushed out of sight (SME-41 D16).
    db::set_conversation_todos(pool, browsing.id, &[anthropic::tools::TodoItem {
        content: "check the page".to_string(),
        status: anthropic::tools::TodoStatus::InProgress,
    }])
    .await
    .expect("seed a todo list");
    sandbox::create_pod(pool, browsing.id, Default::default()).await.expect("create_pod");
    sandbox::create_terminal(pool, browsing.id).await.expect("create_terminal");
    let page = t.tab(t.url(&format!("conversation/{}", browsing.id))).await;
    wait_for_live_client(&page, browsing.id).await;
    wait_for_element(&page, ".browsing-panel-frame-wrap", Duration::from_secs(10)).await;
    let layout: Vec<f64> = page
        .evaluate(
            "(() => { const w = s => document.querySelector(s).getBoundingClientRect().width; \
             return [w('.messages'), w('.composer input'), document.documentElement.scrollWidth - document.documentElement.clientWidth, \
             w('.browsing-panel-frame-wrap'), w('.browsing-panel'), w('.browsing-address-bar')]; })()",
        )
        .await
        .expect("measure the layout")
        .into_value()
        .expect("numbers");
    wait_for_element(&page, ".sandbox-panel .task-terminal", Duration::from_secs(10)).await;
    let panels: Vec<f64> = page
        .evaluate(
            "(() => { const b = s => document.querySelector(s).getBoundingClientRect(); \
             const frame = b('.browsing-panel-frame-wrap'), sandbox = b('.sandbox-panel'), todo = b('.todo-panel'); \
             return [frame.width, sandbox.width, Math.min(sandbox.bottom, innerHeight) - Math.max(sandbox.top, 0), todo.width]; })()",
        )
        .await
        .expect("measure the side panels")
        .into_value()
        .expect("numbers");
    // Closed before the checks, so a failing one doesn't leave the session open.
    crate::browsing::close_session(browsing.id).await.expect("close the browsing session");
    assert!(layout[0] >= 300.0, "the messages are too narrow to use: {layout:?}");
    assert!(layout[1] >= 150.0, "the message box is too narrow to use: {layout:?}");
    assert!(layout[2] <= 0.0, "the page scrolls sideways: {layout:?}");
    assert!(layout[3] <= layout[4], "the frame overflows its panel: {layout:?}");
    assert!(layout[5] <= layout[4], "the address bar overflows its panel: {layout:?}");
    assert!(panels[0] >= 400.0, "the live browser is too small to use: {panels:?}");
    assert!(panels[1] >= 400.0, "the sandbox panel is too narrow to read: {panels:?}");
    assert!(panels[2] >= 240.0, "the sandbox panel should be visible without scrolling: {panels:?}");
}

/// Scenario 16: every settings page is reachable from the sidebar. Sandbox
/// volumes had no link anywhere; the only way in was typing the URL
/// (SME-40 F7).
async fn scenario_sidebar(t: &Scenario<'_>) {
    // A row to measure, even when this scenario runs alone.
    let conversation = t.conversation().await;
    seed_user_message(t.pool, conversation.id, "a sidebar row").await;
    let page = t.tab(t.url("")).await;
    // And the sidebar's two-step Delete keeps its size when armed: the
    // armed label was bold, so it came out wider than the width the
    // button had reserved for it (SME-40 F9). Arming is client-side
    // only; closing this tab disarms it.
    let delete = ".conversation-item .delete-conversation";
    wait_for_stable(&page, delete, tokio::time::Instant::now() + Duration::from_secs(10)).await;
    let before = element_box(&page, delete).await;
    click_when_present(&page, delete, Duration::from_secs(5)).await;
    wait_for_element(&page, ".conversation-item .delete-conversation.confirm", Duration::from_secs(5)).await;
    let after = element_box(&page, ".conversation-item .delete-conversation.confirm").await;
    assert_eq!(before.2, after.2, "arming the sidebar's Delete changed its width");
    // A title uses its row, and each row says how long ago the
    // conversation was active. Titles were cut at about 15 characters
    // with most of the row empty (SME-41 D8).
    let row: Vec<String> = page
        .evaluate(
            "(() => { const item = document.querySelector('.conversation-item:not(:hover)') || document.querySelector('.conversation-item'); \
             const w = s => item.querySelector(s).getBoundingClientRect().width; \
             const age = item.querySelector('.conversation-age'); \
             return [String(w('.conversation-title') / item.getBoundingClientRect().width), age ? age.innerText : '']; })()",
        )
        .await
        .expect("measure a sidebar row")
        .into_value()
        .expect("strings");
    assert!(row[0].parse::<f64>().unwrap_or(0.0) > 0.6, "the title should use most of its row: {row:?}");
    assert!(
        row[1] == "now" || row[1].trim_end_matches(['m', 'h', 'd', 'w']).parse::<u32>().is_ok(),
        "each row should say how long ago it was active: {row:?}"
    );
    page.close().await.expect("close the tab");
    let page = t.tab(t.url("")).await;
    // Named the way a user thinks of them: "Sandboxes", not "Pods"
    // (SME-41 D11).
    let links: String = page
        .evaluate("Array.from(document.querySelectorAll('.sidebar-body a')).map(a => a.innerText).join('|')")
        .await
        .expect("read the sidebar links")
        .into_value()
        .expect("text");
    assert!(links.contains("Sandboxes") && !links.contains("Pods"), "sidebar links: {links}");
    click_when_present(&page, ".sidebar a[href='/sandbox-volumes']", Duration::from_secs(10)).await;
    assert!(
        wait_for_text(&page, "Sandbox volumes", Duration::from_secs(10)).await,
        "the sidebar's volumes link should open the volumes page"
    );
}

/// Scenario 17: a phone-width window. The sidebar kept its 272px, the chat
/// got about 100px and the page scrolled sideways (SME-40 F8). With
/// `mobile` on, a page without a viewport meta tag lays out at 980px, so
/// this also checks the tag is there.
async fn scenario_phone_width(t: &Scenario<'_>) {
    let phone_conversation = t.conversation().await;
    let page = t.tab("about:blank").await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone");
    page.goto(t.url(&format!("conversation/{}", phone_conversation.id)))
        .await
        .expect("open the conversation");
    wait_for_live_client(&page, phone_conversation.id).await;
    let layout: Vec<f64> = page
        .evaluate(
            "(() => { const w = s => document.querySelector(s).getBoundingClientRect().width; \
             return [innerWidth, w('.messages'), w('.composer input'), \
             document.documentElement.scrollWidth - document.documentElement.clientWidth]; })()",
        )
        .await
        .expect("measure the layout")
        .into_value()
        .expect("numbers");
    assert_eq!(layout[0], 390.0, "the page isn't laid out at the phone's width: {layout:?}");
    assert!(layout[1] >= 300.0, "the messages are too narrow on a phone: {layout:?}");
    assert!(layout[2] >= 200.0, "the message box is too narrow on a phone: {layout:?}");
    assert!(layout[3] <= 0.0, "the page scrolls sideways on a phone: {layout:?}");
    // The conversation comes first: the list is folded behind a
    // button, and the chat sits above the side panels. The list and
    // the sandbox panel used to push the transcript to the bottom of
    // the screen (SME-41 D4).
    let order: Vec<f64> = page
        .evaluate(
            "(() => { const top = s => { const e = document.querySelector(s); return e ? e.getBoundingClientRect().top : -1; }; \
             const list = document.querySelector('.conversation-list'); \
             return [list && list.offsetParent !== null ? 1 : 0, top('.chat-main'), top('.side-panels-row')]; })()",
        )
        .await
        .expect("measure the order")
        .into_value()
        .expect("numbers");
    assert_eq!(order[0], 0.0, "the conversation list should be folded away on a phone: {order:?}");
    assert!(order[2] < 0.0 || order[1] < order[2], "the chat should come before the side panels: {order:?}");
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    let mut list_shown = false;
    for _ in 0..25 {
        list_shown = page
            .evaluate("document.querySelector('.conversation-list').offsetParent !== null")
            .await
            .expect("check the list")
            .into_value()
            .expect("a bool");
        if list_shown {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(list_shown, "the button should open the conversation list");
}

/// Scenario 18: a URL that isn't a page says so, with a way back. It used
/// to show the router's raw "Failed to parse route" dump (SME-40 F10).
async fn scenario_not_found(t: &Scenario<'_>) {
    for path in ["nope", "conversation/abc"] {
        let page = t.tab(t.url(path)).await;
        assert!(
            wait_for_text(&page, "Page not found", Duration::from_secs(10)).await,
            "/{path} should say the page doesn't exist"
        );
        let body: String = page.evaluate("document.body.innerText").await.expect("read").into_value().expect("text");
        assert!(!body.contains("Failed to parse route"), "/{path} shows the router's debug output: {body}");
        click_when_present(&page, "a[href='/']", Duration::from_secs(5)).await;
        assert!(
            wait_for_count(&page, ".conversation-list", 1, Duration::from_secs(10)).await,
            "the link back should reach the conversations"
        );
        page.close().await.expect("close the tab");
    }
}

/// A conversation holding one successful and one failed tool call, for the
/// tool rows.
async fn conversation_with_tool_calls(t: &Scenario<'_>) -> crate::models::Conversation {
    let tools = t.conversation().await;
    let call = |id: &str, name: &str, input: serde_json::Value| anthropic::ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    };
    let result = |id: &str, content: &str, is_error: bool| anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error: is_error.then_some(true),
    };
    db::create_message(t.pool, tools.id, "assistant", &[
        call("toolu_ok", "run_terminal_command", serde_json::json!({"command": "ls /tmp", "terminal_id": 1})),
        call("toolu_bad", "read_file", serde_json::json!({"path": "/nope.txt"})),
    ]).await.expect("seed the calls");
    db::create_message(t.pool, tools.id, "user", &[
        result("toolu_ok", "command sent (id: abc)", false),
        result("toolu_bad", "No such file or directory", true),
    ]).await.expect("seed the results");
    tools
}

/// Scenario 19: a tool call reads as one compact line that says what it
/// did, with its result folded in; a failed one is open. Each call and each
/// result used to be its own card of raw JSON, 24 of them for a three-line
/// answer (SME-41 D2).
async fn scenario_tool_rows(t: &Scenario<'_>) {
    let tools = conversation_with_tool_calls(t).await;
    let page = t.tab(t.url(&format!("conversation/{}", tools.id))).await;
    wait_for_live_client(&page, tools.id).await;
    assert!(wait_for_count(&page, ".tool-row", 2, Duration::from_secs(10)).await, "one row per call");
    // With no usage yet, the meter says so in words (SME-41 D11).
    assert!(
        wait_for_text(&page, "No usage yet", Duration::from_secs(5)).await,
        "the context meter should say there's no usage yet"
    );
    let rows: Vec<(String, bool)> = page
        .evaluate("Array.from(document.querySelectorAll('.tool-row')).map(r => [r.querySelector('summary').innerText, r.open])")
        .await
        .expect("read the rows")
        .into_value()
        .expect("rows");
    assert!(rows[0].0.contains("Ran `ls /tmp`") && !rows[0].1, "a successful call is one closed line: {rows:?}");
    assert!(rows[1].0.contains("Read /nope.txt") && rows[1].1, "a failed call is open: {rows:?}");
    assert_eq!(
        page.evaluate("document.querySelectorAll('.tool-result, .tool-call').length").await.expect("count").into_value::<i64>().expect("n"),
        0,
        "results shouldn't be separate cards any more"
    );
    let body: String = page.evaluate("document.querySelector('.tool-row').textContent").await.expect("read").into_value().expect("text");
    assert!(body.contains("command sent"), "the raw result is still there on expand: {body}");
}

/// Scenario 20: dark mode follows the system setting. There was none: a
/// dark-mode system got a bright white page (SME-41 D5).
async fn scenario_dark_mode(t: &Scenario<'_>) {
    let tools = conversation_with_tool_calls(t).await;
    let page = t.tab("about:blank").await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetEmulatedMediaParams::builder()
            .feature(chromiumoxide::cdp::browser_protocol::emulation::MediaFeature::new(
                "prefers-color-scheme",
                "dark",
            ))
            .build(),
    )
    .await
    .expect("ask for dark mode");
    page.goto(t.url(&format!("conversation/{}", tools.id))).await.expect("open the conversation");
    wait_for_live_client(&page, tools.id).await;
    wait_for_element(&page, ".tool-row-text", Duration::from_secs(10)).await;
    let luminance: Vec<f64> = page
        .evaluate(
            "(() => { const lum = c => { const [r, g, b] = c.match(/\\d+/g).map(Number); return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255; }; \
             const bg = el => getComputedStyle(el).backgroundColor; \
             return [lum(bg(document.body)), lum(bg(document.querySelector('.sidebar'))), lum(getComputedStyle(document.querySelector('.tool-row-text')).color)]; })()",
        )
        .await
        .expect("measure the colours")
        .into_value()
        .expect("numbers");
    assert!(luminance[0] < 0.2 && luminance[1] < 0.25, "the page should be dark in dark mode: {luminance:?}");
    assert!(luminance[2] > 0.55, "text should be light in dark mode: {luminance:?}");
}

/// Scenario 21: one primary action per form, and intro text lines up with
/// its heading. Every button in the MCP form was solid black, "Remove"
/// included, and intro text sat 24px in from the heading (SME-41 D6).
async fn scenario_forms(t: &Scenario<'_>) {
    let page = t.tab(t.url("mcp-servers")).await;
    wait_for_element(&page, "h1", Duration::from_secs(10)).await;
    let edges: Vec<f64> = page
        .evaluate(
            "(() => { const p = document.querySelector('.mcp-servers-page p.muted'); \
             return [document.querySelector('h1').getBoundingClientRect().left, \
             p.getBoundingClientRect().left + parseFloat(getComputedStyle(p).paddingLeft)]; })()",
        )
        .await
        .expect("measure")
        .into_value()
        .expect("numbers");
    assert!((edges[0] - edges[1]).abs() < 1.0, "intro text should line up with the heading: {edges:?}");
    page.goto(t.url("mcp-servers/new")).await.expect("open the new-server form");
    wait_for_element(&page, ".mcp-remove-header", Duration::from_secs(10)).await;
    let fills: Vec<String> = page
        .evaluate(
            "['button[type=submit]', '.mcp-add-header', '.mcp-remove-header'].map(s => getComputedStyle(document.querySelector(s)).backgroundColor)",
        )
        .await
        .expect("read the buttons")
        .into_value()
        .expect("colours");
    assert_ne!(fills[0], fills[1], "the main action should stand out from the secondary ones: {fills:?}");
    assert_eq!(fills[1], fills[2], "secondary actions should share one style: {fills:?}");
    // Each auth choice's radio button sits beside its label, not above
    // it (SME-41 D13).
    let radios: Vec<f64> = page
        .evaluate(
            "(() => { const r = document.querySelector('input[name=mcp-new-auth-mode]').getBoundingClientRect(); \
             const l = document.querySelector('input[name=mcp-new-auth-mode]').parentElement.getBoundingClientRect(); \
             return [r.top + r.height / 2, l.top + l.height / 2, l.height]; })()",
        )
        .await
        .expect("measure the radio")
        .into_value()
        .expect("numbers");
    assert!((radios[0] - radios[1]).abs() < 4.0 && radios[2] < 30.0, "a radio should sit beside its label: {radios:?}");
}

/// Scenario 22: a new conversation says what smelt can do and offers
/// example asks; one fills the message box without sending. It was a blank
/// screen (SME-41 D12).
async fn scenario_empty_conversation(t: &Scenario<'_>) {
    let empty = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", empty.id))).await;
    wait_for_live_client(&page, empty.id).await;
    assert!(
        wait_for_count(&page, ".conversation-empty .example-ask", 3, Duration::from_secs(10)).await,
        "an empty conversation should offer example asks"
    );
    let example: String = page.evaluate("document.querySelector('.example-ask').innerText").await.expect("read").into_value().expect("text");
    click_when_present(&page, ".example-ask", Duration::from_secs(5)).await;
    let mut filled = String::new();
    for _ in 0..25 {
        filled = page.evaluate(format!("document.querySelector({CHAT_INPUT:?}).value")).await.expect("read").into_value().expect("text");
        if !filled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(filled, example, "picking an example should fill the message box");
    assert_eq!(
        page.evaluate("document.querySelectorAll('.message-user').length").await.expect("count").into_value::<i64>().expect("n"),
        0,
        "picking an example shouldn't send it"
    );
}

/// Scenario 23 (SME-42): a dev server in the sandbox, end to end. It's
/// bound to 127.0.0.1 inside the pod, as dev servers are by default. The
/// model's own browser loads it at localhost; the model shares a preview;
/// the link shows up in the sandbox panel live, opens the same server in a
/// tab of its own, and survives a reload. Then (SME-33) the same for a
/// Docker container in the pod.
async fn scenario_sandbox_dev_server(t: &Scenario<'_>) {
    let pool = t.pool;
    let serving = t.conversation().await;
    sandbox::create_pod(pool, serving.id, Default::default()).await.expect("create_pod");
    let terminal = sandbox::create_terminal(pool, serving.id).await.expect("create_terminal");
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("write"),
        "write_file",
        &serde_json::json!({
            "path": "/home/sandbox/site/index.html",
            "content": "<html><body><h1>Hello from the sandbox dev server</h1></body></html>",
        }),
    )
    .await
    .expect("write the page");
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("serve"),
        "run_terminal_command",
        &serde_json::json!({
            "terminal_id": terminal,
            "command": "cd /home/sandbox/site && python3 -m http.server 8000 --bind 127.0.0.1",
        }),
    )
    .await
    .expect("start the server");
    let mut listening = false;
    for _ in 0..50 {
        if sandbox::pod_port_is_listening(pool, serving.id, sandbox::PodHost::Localhost, 8000)
            .await
            .expect("probe port 8000")
        {
            listening = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(listening, "the dev server never started listening");

    // The model's side: localhost in its browsing session is the sandbox.
    crate::browsing::open_session(serving.id, crate::egress_proxy::sandbox_dial(pool.clone(), serving.id))
        .await
        .expect("open a browsing session");
    let seen = crate::browsing::navigate(serving.id, "http://localhost:8000/").await;
    crate::browsing::close_session(serving.id).await.expect("close the browsing session");
    let seen = seen.expect("the model's browser should load the sandbox's dev server");
    assert!(seen.text.contains("Hello from the sandbox dev server"), "got {:?}", seen.text);

    // The user's side.
    let page = t.tab(t.url(&format!("conversation/{}", serving.id))).await;
    wait_for_live_client(&page, serving.id).await;
    let shared = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("preview"),
        "sandbox_preview_url",
        &serde_json::json!({"port": 8000}),
    )
    .await
    .expect("share a preview");
    let shared: serde_json::Value = serde_json::from_str(&shared).expect("the tool's JSON");
    assert_eq!(shared["listening"], true, "got {shared}");
    let link = shared["url"].as_str().expect("a url").to_string();
    assert_eq!(link, crate::preview::configured_template().expect("the template").url_for(serving.id, crate::sandbox::PodHost::Localhost, 8000));
    // Without a reload: it arrives on the conversation's live stream.
    wait_for_element(&page, ".sandbox-preview", Duration::from_secs(10)).await;
    let href: String = page
        .evaluate("document.querySelector('.sandbox-preview').href")
        .await
        .expect("read the link")
        .into_value()
        .expect("an href");
    assert_eq!(href.trim_end_matches('/'), link, "the panel links to the shared preview");
    let preview_tab = t.tab(link.clone()).await;
    assert!(
        wait_for_text(&preview_tab, "Hello from the sandbox dev server", Duration::from_secs(10)).await,
        "the preview link should show the sandbox's dev server"
    );
    preview_tab.close().await.expect("close the preview tab");
    page.reload().await.expect("reload the conversation");
    wait_for_live_client(&page, serving.id).await;
    // Still there after a reload: `get_sandbox_state` brings it back.
    wait_for_element(&page, ".sandbox-preview", Duration::from_secs(10)).await;

    // SME-33: a Docker container in the pod, its port not published, as
    // both browsers reach it on a Linux host: at its own address. A
    // network with a fixed address saves reading it from the terminal;
    // the base image is the sandbox's own files, so no Docker Hub.
    let container = std::net::Ipv4Addr::new(172, 21, 9, 9);
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("write"),
        "write_file",
        &serde_json::json!({
            "path": "/workspace/site/index.html",
            "content": "<html><body><h1>Hello from a Docker container</h1></body></html>",
        }),
    )
    .await
    .expect("write the container's page");
    // SME-90: Chrome sends a container's plain-http address no
    // Sec-Fetch-*, so the egress proxy goes by Referer. The page's own
    // script carries one and loads; one fetched with no referrer arrives
    // with nothing to tell it from another site's, and is refused.
    for (path, content) in [
        (
            "/workspace/site/scripts.html",
            "<html><body><p id=out>scripts:</p>\
             <script src=\"own.js\" onload=\"out.append(' own-loaded')\" onerror=\"out.append(' own-refused')\"></script>\
             <script src=\"anon.js\" referrerpolicy=\"no-referrer\" onload=\"out.append(' anon-loaded')\" \
             onerror=\"out.append(' anon-refused')\"></script></body></html>",
        ),
        ("/workspace/site/own.js", "1;"),
        ("/workspace/site/anon.js", "1;"),
    ] {
        anthropic::tools::execute(
            pool,
            serving.id,
            &unique_id("write"),
            "write_file",
            &serde_json::json!({"path": path, "content": content}),
        )
        .await
        .expect("write the container's script page");
    }
    let docker_terminal = sandbox::create_terminal(pool, serving.id).await.expect("create_terminal");
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("container"),
        "run_terminal_command",
        &serde_json::json!({
            "terminal_id": docker_terminal,
            "command": format!(
                "sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/base \
                 && docker network create --subnet 172.21.9.0/24 site \
                 && docker run -d --network site --ip {container} -v /workspace/site:/w -w /w \
                    local/base python3 -m http.server 8001"
            ),
        }),
    )
    .await
    .expect("start the container");
    let host = sandbox::PodHost::Container(container);
    let mut listening = false;
    for _ in 0..300 {
        if sandbox::pod_port_is_listening(pool, serving.id, host, 8001).await.expect("probe the container") {
            listening = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(listening, "the container's server never started listening");

    // The model's side: webfetch and a browsing session.
    let fetched = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("fetch"),
        "webfetch",
        &serde_json::json!({"url": format!("http://{container}:8001/")}),
    )
    .await
    .expect("webfetch the container");
    assert!(fetched.contains("Hello from a Docker container"), "got {fetched}");
    let refused = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("fetch"),
        "webfetch",
        &serde_json::json!({"url": "http://172.24.0.1:8001/"}),
    )
    .await;
    assert!(refused.is_err(), "a private address outside the pod's Docker range must stay refused: {refused:?}");
    crate::browsing::open_session(serving.id, crate::egress_proxy::sandbox_dial(pool.clone(), serving.id))
        .await
        .expect("open a browsing session");
    let seen = crate::browsing::navigate(serving.id, &format!("http://{container}:8001/")).await;
    let scripts = crate::browsing::navigate(serving.id, &format!("http://{container}:8001/scripts.html")).await;
    crate::browsing::close_session(serving.id).await.expect("close the browsing session");
    let seen = seen.expect("the model's browser should load the container's server");
    assert!(seen.text.contains("Hello from a Docker container"), "got {:?}", seen.text);
    let scripts = scripts.expect("the model's browser should load the container's script page");
    assert!(
        scripts.text.contains("own-loaded") && scripts.text.contains("anon-refused"),
        "the page's own script should load and an unidentified one be refused: {:?}",
        scripts.text
    );

    // The user's side: a preview of the container, in the panel and in a tab.
    let shared = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("preview"),
        "sandbox_preview_url",
        &serde_json::json!({"port": 8001, "host": container.to_string()}),
    )
    .await
    .expect("share the container's preview");
    let shared: serde_json::Value = serde_json::from_str(&shared).expect("the tool's JSON");
    assert_eq!(shared["listening"], true, "got {shared}");
    let link = shared["url"].as_str().expect("a url").to_string();
    assert!(wait_for_text(&page, "172.21.9.9:8001", Duration::from_secs(10)).await, "the panel should name the container");
    let preview_tab = t.tab(link).await;
    assert!(
        wait_for_text(&preview_tab, "Hello from a Docker container", Duration::from_secs(10)).await,
        "the container's preview link should show its server"
    );
}

/// A repo's AGENTS.md waits for the user's trust (SME-32): when the model
/// asks to load one from an unknown remote, the chat shows the file with
/// Trust / Don't trust, and trusting loads exactly that file. The model the
/// decision wakes is the mock upstream.
async fn scenario_agents_md_trust(t: &Scenario<'_>) {
    let pool = t.pool;
    let trusting = t.conversation().await;
    let remote = format!("example.com/browser-tier/{}", unique_id("trust"));
    let repo = db::create_conversation_repo(pool, trusting.id, &format!("https://{remote}.git"), &remote, None, "trust-me")
        .await
        .expect("seed a repo");
    db::set_repo_cloned(pool, repo.id, "main", Some("abc1234def")).await.expect("seed the clone");
    // The model asked to load its AGENTS.md (load_instructions).
    db::request_instruction(
        pool,
        trusting.id,
        repo.id,
        "AGENTS.md",
        &db::InstructionsFile {
            content: "Browser tier rule: always run the linter.\n".to_string(),
            file_bytes: 42,
            hash: "browser-tier-hash".to_string(),
            commit: Some("abc1234def".to_string()),
        },
    )
    .await
    .expect("seed the request");
    let trust_page = t.tab(t.url(&format!("conversation/{}", trusting.id))).await;
    assert!(
        wait_for_text(&trust_page, "Browser tier rule: always run the linter.", Duration::from_secs(10)).await,
        "the trust card should show the AGENTS.md it asks about"
    );
    // The repo shows in the panel with no pod running (SME-32 code
    // review 10, finding 1).
    // (By the panel's own element: the card's file path has the same text.)
    assert!(
        wait_for_count(&trust_page, ".sandbox-repo", 1, Duration::from_secs(10)).await,
        "the panel should list the conversation's repos even without a pod"
    );
    click_when_present(&trust_page, ".trust-card-trust", Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&trust_page, ".trust-card", 0, Duration::from_secs(10)).await,
        "trusting should take the card away, live"
    );
    let trusted = db::get_repo_trust(pool, &remote).await.expect("read trust");
    db::delete_repo_trust(pool, &remote).await.expect("clean up the trust decision");
    assert_eq!(trusted, Some(true), "the decision is remembered for the remote");
    assert_eq!(
        crate::git::project_instructions(pool, trusting.id).await.expect("loaded")[0].content,
        "Browser tier rule: always run the linter.\n"
    );
}

/// SME-34: the model's question waits on a card. It shows in every tab and
/// survives a reload, the sidebar marks the conversation, and choosing an
/// option answers it: the card goes from every tab, the mark clears, and
/// the call becomes a row with the answer the model got.
async fn scenario_ask_user_card(t: &Scenario<'_>) {
    let pool = t.pool;
    let asking = t.conversation().await;
    seed_user_message(pool, asking.id, "clean up the build").await;
    let input = serde_json::json!({"questions": [
        {"question": "Delete the build directory?", "header": "Delete",
         "options": [{"label": "Yes (Recommended)"}, {"label": "No", "description": "keep it"}]}
    ]});
    let tool_use_id = unique_id("toolu_ask");
    db::create_message(
        pool,
        asking.id,
        "assistant",
        &[anthropic::ContentBlock::ToolUse { id: tool_use_id.clone(), name: crate::questions::ASK_USER.to_string(), input: input.clone() }],
    )
    .await
    .expect("seed the call");
    let questions = crate::questions::parse_questions(&input).expect("valid questions");
    db::create_pending_question(pool, asking.id, &tool_use_id, &questions).await.expect("seed the question");

    let url = t.url(&format!("conversation/{}", asking.id));
    let first = t.tab(url.clone()).await;
    let second = t.tab(url.clone()).await;
    for page in [&first, &second] {
        assert!(
            wait_for_text(page, "Delete the build directory?", Duration::from_secs(10)).await,
            "every tab shows the question"
        );
        assert!(wait_for_count(page, ".question-option", 2, Duration::from_secs(5)).await, "with its two options");
    }
    let mark = format!("[data-conversation-id='{}'] .conversation-waiting", asking.id);
    assert!(wait_for_count(&first, &mark, 1, Duration::from_secs(10)).await, "the sidebar marks it waiting");

    first.goto(url.as_str()).await.expect("reload");
    assert!(
        wait_for_count(&first, ".question-card", 1, Duration::from_secs(10)).await,
        "the card survives a reload"
    );

    click_when_present(&first, ".question-option", Duration::from_secs(5)).await;
    for page in [&first, &second] {
        assert!(
            wait_for_count(page, ".question-card", 0, Duration::from_secs(10)).await,
            "answering takes the card away from every tab"
        );
        assert!(
            wait_for_text(page, "1. Delete: Yes (Recommended)", Duration::from_secs(10)).await,
            "the call shows the answer the model got"
        );
    }
    assert!(wait_for_count(&first, &mark, 0, Duration::from_secs(10)).await, "the mark clears");
}

/// SME-51 B11: switching conversations resets what belonged to the one
/// left: its context detail view doesn't stay open over the next
/// conversation.
async fn scenario_switch_resets_context_detail(t: &Scenario<'_>) {
    let leaving = t.conversation().await;
    let arriving = t.conversation().await;
    seed_usage(t.pool, leaving.id, 1_000, 100).await;
    let switching = t.tab(t.url(&format!("conversation/{}", leaving.id))).await;
    click_when_present(&switching, ".context-usage-bar", Duration::from_secs(10)).await;
    assert!(
        wait_for_count(&switching, ".context-detail-overlay", 1, Duration::from_secs(5)).await,
        "the context detail should open"
    );
    click_conversation(&switching, arriving.id).await;
    assert!(
        wait_for_count(&switching, ".context-detail-overlay", 0, Duration::from_secs(5)).await,
        "the first conversation's context detail stayed open after switching"
    );
}

/// Scenario 24 (SME-72): the model picker above the message box says which
/// model the conversation runs on, and choosing another in one tab shows in
/// every tab on it, with no reload.
async fn scenario_model_picker(t: &Scenario<'_>) {
    let pool = t.pool;
    let picking = t.conversation().await;
    let provider_name = db::get_inference_provider(pool, *MOCK_PROVIDER.get().expect("the mock provider"))
        .await
        .expect("read the mock provider")
        .expect("the mock provider exists")
        .name;
    let first_tab = t.tab(t.url(&format!("conversation/{}", picking.id))).await;
    let second_tab = t.tab(t.url(&format!("conversation/{}", picking.id))).await;
    wait_for_live_client(&first_tab, picking.id).await;
    wait_for_live_client(&second_tab, picking.id).await;
    let current = format!("{provider_name} \u{b7} {MOCK_MODEL}");
    for tab in [&first_tab, &second_tab] {
        assert!(
            wait_for_text(tab, &current, Duration::from_secs(10)).await,
            "the picker should show the conversation's model, {current:?}"
        );
    }
    click_when_present(&first_tab, ".model-picker-button", Duration::from_secs(5)).await;
    // Clear the field and type, until it holds exactly the new id: a
    // re-render between selecting the old text and typing would
    // otherwise leave the two run together.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let model_field = wait_for_element(&first_tab, ".model-chooser-model", Duration::from_secs(5)).await;
        model_field.focus().await.expect("focus the model field");
        first_tab
            .evaluate("document.querySelector('.model-chooser-model').select()")
            .await
            .expect("select the current model id");
        model_field.type_str("other-model").await.expect("type a model id");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let typed: String = first_tab
            .evaluate("document.querySelector('.model-chooser-model')?.value ?? ''")
            .await
            .expect("read the model field")
            .into_value()
            .expect("a string");
        if typed == "other-model" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the model field never held just the new id: {typed:?}");
    }
    click_when_present(&first_tab, ".model-chooser button[type=\"submit\"]", Duration::from_secs(5)).await;
    let chosen = format!("{provider_name} \u{b7} other-model");
    if !wait_for_text(&first_tab, &chosen, Duration::from_secs(10)).await {
        let shown: String = first_tab
            .evaluate("document.querySelector('.model-picker')?.innerText ?? ''")
            .await
            .expect("read the picker")
            .into_value()
            .expect("a string");
        panic!("the choosing tab should show the new model; the picker says {shown:?}");
    }
    assert!(
        wait_for_text(&second_tab, &chosen, Duration::from_secs(10)).await,
        "the other tab should show the new model live"
    );
    let stored = db::get_conversation_model(pool, picking.id)
        .await
        .expect("read the conversation's model")
        .expect("the conversation exists");
    assert_eq!(stored.model.as_deref(), Some("other-model"));
}

/// Scenario 25 (SME-76): an OAuth server takes extra headers too, such as
/// GitHub's `X-MCP-Toolsets` (which turns on the tools that read CI logs).
/// The edit page only showed its header editor to static-header servers.
async fn scenario_oauth_headers(t: &Scenario<'_>) {
    let pool = t.pool;
    let oauth_server = db::create_mcp_server_config(
        pool,
        &unique_id("oauth-headers"),
        "http://127.0.0.1:9/mcp",
        &std::collections::HashMap::new(),
        "oauth",
        Some("client-id"),
        None,
    )
    .await
    .expect("create an OAuth server");
    let edit = t.tab(t.url(&format!("mcp-servers/{}", oauth_server.id))).await;
    wait_for_element(&edit, ".mcp-oauth-panel", Duration::from_secs(10)).await;
    click_when_present(&edit, ".mcp-add-header", Duration::from_secs(5)).await;
    let name_field = wait_for_element(&edit, ".mcp-header-name", Duration::from_secs(5)).await;
    name_field.focus().await.expect("focus the header name");
    name_field.type_str("X-MCP-Toolsets").await.expect("type the header name");
    let value_field = wait_for_element(&edit, ".mcp-header-row .mcp-header-value", Duration::from_secs(5)).await;
    value_field.focus().await.expect("focus the header value");
    value_field.type_str("repos,actions").await.expect("type the header value");
    click_when_present(&edit, ".mcp-save-edit", Duration::from_secs(5)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let saved = loop {
        let config = db::get_mcp_server_config(pool, oauth_server.id)
            .await
            .expect("read the server")
            .expect("the server exists");
        if !config.extra_headers.0.is_empty() || tokio::time::Instant::now() >= deadline {
            break config;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // Deleted before the checks, so a failing one doesn't leave it behind.
    db::delete_mcp_server_config(pool, oauth_server.id).await.expect("delete the OAuth server");
    assert_eq!(
        saved.extra_headers.0.get("X-MCP-Toolsets").map(String::as_str),
        Some("repos,actions"),
        "the header should be saved: {:?}",
        saved.extra_headers.0.keys().collect::<Vec<_>>()
    );
    assert_eq!(saved.auth_mode, "oauth", "saving headers keeps the server on OAuth");
}

/// Puts the server's build id back to its own when dropped, so a failing
/// stale-bundle scenario doesn't leave every later page asking for a
/// reload.
struct ResetBuildId;

impl Drop for ResetBuildId {
    fn drop(&mut self) {
        crate::api::version::test_override::set(None);
    }
}

/// Scenario 26 (SME-43): a tab running an older bundle than the server.
async fn scenario_stale_bundle(t: &Scenario<'_>) {
    let _reset = ResetBuildId;
    // (a) An event type the bundle doesn't know is skipped: the stream stays
    // up (one connection's pull, not a second after a reconnect), and the
    // next event still arrives live. It also proves the server is newer,
    // so the tab asks for a reload.
    let stale = t.conversation().await;
    let stale_url = t.url(&format!("conversation/{}", stale.id));
    let stale_tab = t.tab(stale_url.as_str()).await;
    wait_for_live_client(&stale_tab, stale.id).await;
    assert!(
        !banner_shown(&stale_tab).await,
        "a page built from the server's own tree shouldn't ask for a reload"
    );
    crate::events::publish(stale.id, crate::events::ConversationEvent::BrowserTestAddedLater {});
    crate::events::publish(
        stale.id,
        crate::events::ConversationEvent::TodoListUpdate {
            items: vec![anthropic::tools::TodoItem {
                content: "scenario 26 todo".to_string(),
                status: anthropic::tools::TodoStatus::Pending,
            }],
        },
    );
    assert!(
        wait_for_text(&stale_tab, "scenario 26 todo", Duration::from_secs(5)).await,
        "an event after one of an unknown type should still arrive live"
    );
    assert_eq!(
        live_pulls(&stale_tab, stale.id).await,
        Some(1),
        "an unknown event type shouldn't end the stream"
    );
    assert!(
        wait_for_text(&stale_tab, "smelt was updated", Duration::from_secs(5)).await,
        "an unknown event type should ask for a reload"
    );
    stale_tab.close().await.expect("close the stale tab");

    // (b) The server is redeployed: the tab's stream drops, and the
    // reconnect finds another build. The Sandboxes page, whose stream
    // is the app-wide one, notices too. Reloading clears it.
    let tab = t.tab(stale_url.as_str()).await;
    wait_for_live_client(&tab, stale.id).await;
    assert!(!banner_shown(&tab).await, "no reload banner before the server changes");
    crate::api::version::test_override::set(Some("a-newer-build"));
    crate::events::forget(stale.id);
    assert!(
        wait_for_text(&tab, "smelt was updated", Duration::from_secs(10)).await,
        "a reconnect to a newer server should ask for a reload"
    );
    let pods_tab = t.tab(t.url("pods")).await;
    assert!(
        wait_for_text(&pods_tab, "smelt was updated", Duration::from_secs(10)).await,
        "the Sandboxes page should notice a newer server too"
    );
    pods_tab.close().await.expect("close the Sandboxes page");
    crate::api::version::test_override::set(None);
    click_when_present(&tab, ".stale-bundle-banner button", Duration::from_secs(5)).await;
    assert!(
        wait_for_text_gone(&tab, "smelt was updated", Duration::from_secs(10)).await,
        "Reload should load the page again"
    );
    wait_for_live_client(&tab, stale.id).await;
    assert!(!banner_shown(&tab).await, "the reloaded page is current");
}

/// Scenario 27 (SME-83): scrolling the transcript up a little leaves it
/// there. Every scroll event set the stuck-to-bottom flag, which reran the
/// auto-scroll effect, so a scroll that stayed within the 32px slack (a
/// trackpad's first small steps) snapped back.
async fn scenario_transcript_scroll(t: &Scenario<'_>) {
    let long_conversation = t.conversation().await;
    for i in 0..40 {
        db::create_message(
            t.pool,
            long_conversation.id,
            if i % 2 == 0 { "user" } else { "assistant" },
            &[anthropic::ContentBlock::Text { text: format!("scroll filler message {i}") }],
        )
        .await
        .expect("seed a message");
    }
    let long_page = t.tab(t.url(&format!("conversation/{}", long_conversation.id))).await;
    wait_for_live_client(&long_page, long_conversation.id).await;
    // Settled at the bottom: the same distance on two reads apart.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = f64::NAN;
    loop {
        let distance = transcript_distance_from_bottom(&long_page).await;
        if distance <= 1.0 && distance == last {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the transcript never settled at its bottom: {distance}px away"
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    long_page
        .evaluate("(() => { const m = document.querySelector('.messages'); m.scrollTop = m.scrollTop - 10; })()")
        .await
        .expect("scroll the transcript up");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let distance = transcript_distance_from_bottom(&long_page).await;
    assert!(
        (8.0..=12.0).contains(&distance),
        "a 10px scroll up should stay where it was put, but the transcript is {distance}px from its bottom"
    );
}

/// Scenario 28 (SME-108): switching away from a conversation and back leaves
/// its sandbox terminal following its last line, not scrolled to the top. The
/// terminal body is destroyed on the switch and rebuilt on the way back,
/// filled in the same write that creates it, so the follow-bottom must land on
/// that first mount — before any frame could show the body at its top. The
/// ticket's capture came from a busy session mid-turn, so the switch is tried
/// at every timing it can land in: once the output had finished, the back-click
/// racing the rebuild, mid-burst, and rapid repeated switches. A frame-rate
/// recorder across each switch says whether a rebuilt body was ever *painted*
/// at its top, which a settled read can miss even though the ticket's own
/// capture would have caught it.
async fn scenario_terminal_follow_after_switch(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let other = t.conversation().await;
    // A transcript with real height, as a busy session's has: the side-panel
    // row's bounded height then resolves through the flex chain late in the
    // same batch that creates the terminal body.
    for i in 0..40 {
        db::create_message(
            t.pool,
            conversation.id,
            if i % 2 == 0 { "user" } else { "assistant" },
            &[anthropic::ContentBlock::Text { text: format!("switch-filler message {i}") }],
        )
        .await
        .expect("seed a message");
    }
    sandbox::create_pod(t.pool, conversation.id, Default::default())
        .await
        .expect("create_pod");
    let terminal =
        sandbox::create_terminal(t.pool, conversation.id).await.expect("create_terminal");

    let page = t.tab(t.url("")).await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("set the window size");
    let row = |id: i64| format!(".conversation-item[data-conversation-id=\"{id}\"]");
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    wait_for_live_client(&page, conversation.id).await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal}"), Duration::from_secs(15)).await,
        "the terminal card should render"
    );

    // Phase A — the ticket's sequence, unhurried: a burst well past the body's
    // height, followed live, finished before the switch.
    let command_id = unique_id("cmd-follow");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 90); do echo \"follow-seed-$i\"; done"}),
    )
    .await
    .expect("run_terminal_command");
    assert!(
        wait_for_text(&page, "follow-seed-90", Duration::from_secs(30)).await,
        "the command's last line should stream in"
    );
    wait_command_finished(t.pool, terminal).await;
    // Following live through the burst, and settled before the switch: the
    // starting state the ticket's capture had ("shows 31–40 at the bottom").
    settle_terminal_bottom(&page, 8).await;
    let scrollable: bool = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             return b && b.scrollHeight > b.clientHeight + 40; })()",
        )
        .await
        .expect("measure the terminal body")
        .into_value()
        .expect("a bool");
    assert!(
        scrollable,
        "the seeded output doesn't overflow the terminal body, so the follow checks measure nothing"
    );

    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    wait_for_live_client(&page, other.id).await;
    assert!(
        wait_for_count(&page, ".task-terminal-body", 0, Duration::from_secs(10)).await,
        "switching away should take the terminal body with it"
    );
    set_scroll_mark(&page, "sme108A").await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    wait_for_live_client(&page, conversation.id).await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal}"), Duration::from_secs(15)).await,
        "the terminal should be back with the conversation"
    );
    settle_terminal_bottom(&page, 8).await;
    assert_never_painted_at_top(&page, "sme108A").await;

    // Phase B — the switch-back racing the rebuild itself: away and back with
    // nothing waited between the clicks, so the back-click lands while the
    // away-switch is still resolving.
    set_scroll_mark(&page, "sme108B").await;
    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    settle_terminal_bottom(&page, 8).await;
    assert_never_painted_at_top(&page, "sme108B").await;

    // Phase C — the ticket's actual conditions: the switch lands while a
    // second command's output is still streaming. Lines that arrive while the
    // page is on the other conversation never reach the old body; the rebuilt
    // body comes back holding the snapshot's share and must keep following the
    // lines that arrive after.
    let busy_id = unique_id("cmd-busy");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &busy_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 120); do echo \"busy-$i\"; sleep 0.05; done"}),
    )
    .await
    .expect("run_terminal_command for the mid-stream switch");
    assert!(
        wait_for_text(&page, "busy-010", Duration::from_secs(20)).await,
        "the second command's output should be streaming"
    );
    // A streaming reply alongside the terminal's output, as the busy turn had:
    // transcript churn racing the rebuild too. The mock replies slowly (a word
    // every 200 ms), so it's still streaming through the switch below.
    send_from(&page, "churn the transcript while the terminal fills").await;
    assert!(
        wait_for_text(&page, "zebra0", Duration::from_secs(10)).await,
        "the reply should start streaming"
    );
    set_scroll_mark(&page, "sme108C").await;
    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    // Through 3 s of streaming the follow keeps it within a few lines of the
    // bottom; a body that stopped following drifts a whole body-height away as
    // lines pile up below the fold.
    let worst = terminal_max_distance(&page, 3).await;
    assert!(
        worst <= 60.0,
        "streaming after the switch-back left the terminal {worst}px from its bottom — it stopped \
         following its last line (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    assert_never_painted_at_top(&page, "sme108C").await;
    assert!(
        t.mock.wait_until_idle(Duration::from_secs(30)).await,
        "the streaming reply never finished upstream"
    );
    assert!(
        wait_for_text(&page, "busy-120", Duration::from_secs(30)).await,
        "the streaming command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // Phase E — rapid repeated switches mid-stream: four clicks, away and back
    // twice, nothing waited for.
    let rapid_id = unique_id("cmd-rapid");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &rapid_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 400); do echo \"rapid-$i\"; sleep 0.05; done"}),
    )
    .await
    .expect("run_terminal_command for the rapid switches");
    assert!(
        wait_for_text(&page, "rapid-005", Duration::from_secs(20)).await,
        "the rapid command's output should be streaming"
    );
    set_scroll_mark(&page, "sme108E").await;
    for _ in 0..2 {
        click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
        click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    }
    let worst = terminal_max_distance(&page, 3).await;
    assert!(
        worst <= 120.0,
        "rapid switching mid-stream left the terminal {worst}px from its bottom \
         (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    assert_never_painted_at_top(&page, "sme108E").await;
    assert!(
        wait_for_text(&page, "rapid-400", Duration::from_secs(40)).await,
        "the rapid command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // Phase F — a phone-width window, the layout the ticket's capture most
    // resembles: the side panels stack in a column that itself scrolls, the
    // terminal body is short, and the conversation list is a drawer, so the
    // switch-back is a toggle + row click rather than a row click alone.
    //
    // F1, recorded not asserted: shrink the window while the body holds the
    // (finished) rapid output and was following it, then send a short burst.
    // A resize is not a user scrolling up, so the body should keep following;
    // the sticky logic reads the resize's own clamp as a scroll, though,
    // which is exactly what would stop a follow — so the follow is asserted
    // here once the burst finishes and the body has had lines to catch up on.
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resize_id = unique_id("cmd-resize");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &resize_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 20); do echo \\\"resize-$i\\\"; sleep 0.05; done"}),
    )
    .await
    .expect("run_terminal_command after the resize");
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // F1b: the resize landing mid-burst, so the clamp arrives while the
    // follow is running — the race the sticky logic can't see its way out of.
    let burst_id = unique_id("cmd-resize-mid");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &burst_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 300); do echo \\\"mid-$i\\\"; sleep 0.02; done"}),
    )
    .await
    .expect("start the mid-burst command");
    assert!(
        wait_for_text(&page, "mid-010", Duration::from_secs(20)).await,
        "the mid-burst command's output should be streaming"
    );
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("widen the window mid-burst");
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // F2: a fresh mount from a reload — the ticket's open question: open a
    // conversation whose terminal has more output than fits, and the body
    // must show its last line, not its first.
    page.reload().await.expect("reload the page");
    wait_for_live_client(&page, conversation.id).await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal}"), Duration::from_secs(15)).await,
        "the terminal card should render after the reload"
    );
    settle_terminal_bottom(&page, 8).await;

    // F3: streaming at phone width, then the drawer switch-back. (F1b widened
    // the window again; narrow it here so the drawer — and only the drawer —
    // carries the switch.)
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone again");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let phone_id = unique_id("cmd-phone");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &phone_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 200); do echo \\\"phone-$i\\\"; sleep 0.04; done"}),
    )
    .await
    .expect("run_terminal_command for the phone-width switch");
    assert!(
        wait_for_text(&page, "phone-005", Duration::from_secs(20)).await,
        "the phone-width command's output should be streaming"
    );
    settle_terminal_bottom(&page, 8).await;
    set_scroll_mark(&page, "sme108F").await;
    // The drawer holds the list: open it, pick the other conversation (which
    // closes it again), then come back the same way mid-stream.
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    let worst = terminal_max_distance(&page, 3).await;
    assert!(
        worst <= 60.0,
        "streaming after the phone-width switch-back left the terminal {worst}px from its bottom \
         (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    assert_never_painted_at_top(&page, "sme108F").await;
    assert!(
        wait_for_text(&page, "phone-200", Duration::from_secs(40)).await,
        "the phone-width command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // F4 — the other half of the sticky rule the fix must keep: scrolling up
    // mid-stream is a user gesture, and the follow must stop and stay put
    // (SME-83's semantics, now distinguished from a resize by *how* the
    // position moved rather than by merely being off the bottom).
    //
    // The gesture happens while nothing is arriving, and the streaming starts
    // after it. A `scrollTop` write and a follow-scroll inside the same frame
    // coalesce into ONE scroll event describing only the final position, so the
    // app can miss the scroll-up entirely — which is what this phase did, now
    // and then, at 50 ms a line. Quiet first keeps the check watching the follow
    // instead of racing the event loop.
    let fill_id = unique_id("cmd-updown-fill");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &fill_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 200); do echo \\\"updown-$i\\\"; done"}),
    )
    .await
    .expect("run_terminal_command to fill the terminal for the scroll-up check");
    assert!(
        wait_for_text(&page, "updown-200", Duration::from_secs(30)).await,
        "the fill command's output should arrive"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
    let up = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             b.scrollTop = b.scrollHeight - b.clientHeight - 400; \
             return Math.round(b.scrollHeight - b.scrollTop - b.clientHeight); })()",
        )
        .await
        .expect("scroll the terminal up 400px")
        .into_value::<f64>()
        .expect("a number");
    assert!((300.0..=500.0).contains(&up), "the scroll-up should have landed ~400px up, got {up}");
    // A beat for the app to take the scroll event in before anything else can
    // move the view.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stream_id = unique_id("cmd-updown-stream");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &stream_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 100); do echo \\\"stream-$i\\\"; sleep 0.1; done"}),
    )
    .await
    .expect("run_terminal_command to stream while the user reads");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let still = terminal_distance_from_bottom(&page).await;
    assert!(
        (300.0..=1e5).contains(&still),
        "output arrived while the user had scrolled up and yanked the view to the bottom: \
         {still}px from the bottom (was ~400px, scrollTop/scrollHeight/clientHeight: {}, scroll \
         events: {})",
        terminal_scroll_facts(&page).await,
        terminal_scroll_events(&page).await
    );
    // Back to the bottom is following again: scroll down, and the follow
    // resumes on the next line rather than needing the command to finish.
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         b.scrollTop = b.scrollHeight; })()",
    )
    .await
    .expect("scroll back to the bottom");
    tokio::time::sleep(Duration::from_millis(500)).await;
    settle_terminal_bottom(&page, 8).await;
    assert!(
        wait_for_text(&page, "stream-100", Duration::from_secs(60)).await,
        "the streaming command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // Phase G — the other direction on the same rule, which the first fix left
    // as it found it: growing the window *shrinks* the scroll range, so a reader
    // close enough to the bottom gets clamped down onto it, landing inside the
    // slack. Position alone then reads as "at the bottom", and the follow comes
    // back to life for someone who asked, a moment ago, not to be followed. A
    // clamp is geometry; only the user moving back down asks again. The gesture
    // happens while output is quiet, for the reason F4 gives, and the output
    // then comes one line every 200 ms so the reader's drift stays well under
    // the ~580 px the body's client height gains — or the resize wouldn't reach
    // them and the phase would prove nothing.
    let up = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             b.scrollTop = b.scrollHeight - b.clientHeight - 100; \
             return Math.round(b.scrollHeight - b.scrollTop - b.clientHeight); })()",
        )
        .await
        .expect("scroll the terminal up 100px to read")
        .into_value::<f64>()
        .expect("a number");
    assert!((60.0..=200.0).contains(&up), "the scroll-up should have landed ~100px up, got {up}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let restick_id = unique_id("cmd-restick");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &restick_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 100); do echo \\\"restick-$i\\\"; sleep 0.2; done"}),
    )
    .await
    .expect("run_terminal_command for the re-stick check");
    // That scroll-up really did turn the follow off: lines arrive and the view
    // drifts away from the bottom instead of being held on it. Without this the
    // check below couldn't tell "woken by the clamp" from "never switched off
    // at all".
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let reading = terminal_distance_from_bottom(&page).await;
    assert!(
        reading > 60.0,
        "output yanked the reader along before any resize: {reading}px from the bottom, it should \
         have kept drifting away (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    // Tag the body element first: a rebuilt body arrives with a brand-new
    // `StickyBottom`, which starts out following, whatever the one it replaced
    // had decided — so whether the resize replaced it decides what the check
    // below is actually measuring.
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         b.__sme108tag = 'before-grow'; })()",
    )
    .await
    .expect("tag the terminal body before the resize");
    // Grow the window by far more than the ~100 px they have drifted: the new
    // scroll range no longer reaches where they were, so the browser clamps them
    // down onto its bottom edge — once. A beat later they should already be back
    // above it, by about as many lines as arrived in that beat: the follow is
    // off, nothing scrolls them, and the output piles up below. That is *not*
    // what the browser does by default — Chrome's scroll anchoring keeps a
    // scroller that sits at its end pinned onto every new bottom, which looks
    // exactly like a follow and which no scroll logic can undo; the body's
    // `overflow-anchor: none` (assets/chat.css) is what stops it, and the growth
    // checked below is what it stops.
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("grow the window while they read");
    // The clamp lands within a frame or two of the grow on a healthy page, but
    // betting one fixed sleep on it makes this premise itself the flake — poll
    // for it, and only fail as a premise once the reader has had 3s to clamp.
    let clamp_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut after_grow = terminal_distance_from_bottom(&page).await;
    while after_grow > 40.0 && tokio::time::Instant::now() < clamp_deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        after_grow = terminal_distance_from_bottom(&page).await;
    }
    assert!(
        after_grow <= 40.0,
        "the window grow didn't reach the reader to clamp them onto the new bottom (they were \
         still {after_grow}px above it after 3s), so this phase measures nothing \
         (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    let same_element: bool = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             return !!b && b.__sme108tag === 'before-grow'; })()",
        )
        .await
        .expect("ask whether the resize kept the same terminal body element")
        .into_value()
        .expect("a bool");
    // Lines keep arriving: left alone, the view stays where the clamp put it and
    // the gap only grows. A follow woken by the clamp — or a browser pinning the
    // clamped scroller onto each new bottom — keeps it at 0 instead. Confirm the
    // command is still running first, or the growth below would only prove the
    // output had stopped.
    let still_streaming = db::terminal_command_is_running(t.pool, terminal)
        .await
        .expect("the re-stick command's running state")
        .is_some();
    assert!(
        still_streaming,
        "the re-stick command had already finished by the time of the resize, so nothing was \
         arriving to follow"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let drifted = terminal_distance_from_bottom(&page).await;
    assert!(
        drifted > after_grow + 60.0,
        "a window grow left the reader being followed after all: {drifted}px from the bottom \
         after 2.5s of streaming, when it was {after_grow}px at the start of that window — the \
         gap should have kept growing by a line every 200 ms (same element through the resize: \
         {same_element}; scrollTop/scrollHeight/clientHeight: {}, scroll events: {})",
        terminal_scroll_facts(&page).await,
        terminal_scroll_events(&page).await
    );
    // And the user asking to be back at the bottom does bring the follow back.
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         b.scrollTop = b.scrollHeight; })()",
    )
    .await
    .expect("scroll back to the bottom");
    settle_terminal_bottom(&page, 8).await;
    assert!(
        wait_for_text(&page, "restick-100", Duration::from_secs(60)).await,
        "the re-stick command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
}

/// Watch the sandbox terminal body's scroll position every 8 ms: finer than
/// a frame, so a rebuilt body that sat at its top for even a frame shows up
/// here — the settle loops can only measure the body some milliseconds after
/// it first appears, and can't tell "sat at its top" from "rescued a beat
/// later". Installing is idempotent; `set_scroll_mark` clears the samples, so
/// the recorder holds one phase's worth at a time. It also keeps the last 30
/// `scroll` events on the body, capture-phase (scroll doesn't bubble): the
/// exact numbers the sticky logic sees, which is how a follow that stopped
/// partway gets explained.
const TERMINAL_SCROLL_SAMPLER: &str = "(() => { if (!window.__sme108timer) { \
    window.__sme108timer = setInterval(() => { const b = document.querySelector('.task-terminal-body'); \
      if (!b) { return; } \
      (window.__sme108samples || []).push({t: Math.round(performance.now()), \
        top: Math.round(b.scrollTop), sh: Math.round(b.scrollHeight), ch: Math.round(b.clientHeight)}); \
      }, 8); \
    window.__sme108evts = []; \
    document.addEventListener('scroll', e => { const b = e.target; if (!b || !b.classList || \
      !b.classList.contains('task-terminal-body')) { return; } \
      window.__sme108evts.push({t: Math.round(performance.now()), top: Math.round(b.scrollTop), \
        sh: Math.round(b.scrollHeight), ch: Math.round(b.clientHeight), \
        d: Math.round(b.scrollHeight - b.scrollTop - b.clientHeight)}); \
      if (window.__sme108evts.length > 30) { window.__sme108evts.shift(); } }, true); } })()";

/// Start the recorder (if this is the phase that first needs it) and mark the
/// window at now, clearing what earlier phases recorded.
async fn set_scroll_mark(page: &chromiumoxide::Page, name: &str) {
    page.evaluate(TERMINAL_SCROLL_SAMPLER).await.expect("install the scroll sampler");
    page.evaluate(format!("(() => {{ window.__sme108samples = []; window['{name}'] = performance.now(); }})()"))
        .await
        .expect("set the scroll mark");
}

/// The frame recorder's verdict for one switch: across every frame since the
/// mark in which the terminal body overflowed, it must never have sat at its
/// top (scrollTop 0) — the ticket's capture saw exactly that.
async fn assert_never_painted_at_top(page: &chromiumoxide::Page, mark: &str) {
    let v: serde_json::Value = page
        .evaluate(
            format!(
                "(() => {{ const m = window['{mark}'] || 0; const s = (window.__sme108samples || \
                   []).filter(x => x.t >= m && x.sh > x.ch + 40); \
                   return {{ min: s.reduce((a, x) => Math.min(a, x.top), 1e9), n: s.length }}; }})()"
            ),
        )
        .await
        .expect("read the frame recorder")
        .into_value()
        .expect("an object");
    let min_top = v["min"].as_f64().unwrap_or(f64::NAN);
    let frames = v["n"].as_u64().unwrap_or(0);
    assert!(
        frames > 0,
        "the frame recorder saw no overflowing frame of the terminal body since {mark}"
    );
    assert!(
        min_top > 0.0,
        "the terminal body was painted at its top: the lowest scrollTop across {frames} \
         overflowing frames since {mark} was {min_top}px"
    );
}

/// How far the terminal body is scrolled from its bottom, in pixels; a huge
/// number when there is none, so a settle loop keeps waiting for the body.
async fn terminal_distance_from_bottom(page: &chromiumoxide::Page) -> f64 {
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         return b ? b.scrollHeight - b.scrollTop - b.clientHeight : 1e18; })()",
    )
    .await
    .expect("read the terminal's scroll position")
    .into_value()
    .expect("a number")
}

/// The terminal body's scrollTop/scrollHeight/clientHeight, rounded, joined —
/// for failure messages.
async fn terminal_scroll_facts(page: &chromiumoxide::Page) -> String {    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         return b ? [b.scrollTop, b.scrollHeight, b.clientHeight].map(Math.round).join('/') : 'no \
         body'; })()",
    )
    .await
    .expect("read the terminal's scroll facts")
    .into_value::<String>()
    .expect("a string")
}

/// The last up-to-30 `scroll` events the terminal body fired, with the exact
/// numbers the sticky logic saw at each: which event demoted `stuck` (a
/// distance over the 32 px slack) is in here.
async fn terminal_scroll_events(page: &chromiumoxide::Page) -> String {
    page.evaluate(
        "(() => JSON.stringify(window.__sme108evts || []))()",
    )
    .await
    .expect("read the scroll event ring")
    .into_value::<String>()
    .expect("a string")
}

/// The terminal following its last line: the distance from the bottom settling
/// to within a pixel and holding — two reads 300 ms apart that agree, as the
/// transcript's settle does.
async fn settle_terminal_bottom(page: &chromiumoxide::Page, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut last = f64::NAN;
    loop {
        let distance = terminal_distance_from_bottom(page).await;
        if distance <= 1.0 && distance == last {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the terminal never followed its last line: still {distance}px from the bottom \
             (scrollTop/scrollHeight/clientHeight: {}, scroll events: {})",
            terminal_scroll_facts(page).await,
            terminal_scroll_events(page).await
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The largest distance-from-bottom the terminal body shows over `secs` of
/// sampling at ~30 ms. A rebuilt body takes a beat to appear after a
/// switch-back resolves, so the window waits for it first, and a body that
/// vanishes mid-window counts as no sample rather than infinite drift.
async fn terminal_max_distance(page: &chromiumoxide::Page, secs: u64) -> f64 {
    let present_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if terminal_distance_from_bottom(page).await < 1e17 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < present_deadline,
            "the terminal body never came back after the switch"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut max = f64::NAN;
    loop {
        let distance = terminal_distance_from_bottom(page).await;
        if distance < 1e17 {
            max = if max.is_nan() { distance } else { max.max(distance) };
        }
        if tokio::time::Instant::now() >= deadline {
            assert!(!max.is_nan(), "the terminal body vanished from the page");
            return max;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Wait for `terminal_id`'s running command to finish server-side rather than
/// sleeping a guess: the tier waits on server state, not timing.
async fn wait_command_finished(pool: &sqlx::PgPool, terminal_id: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let running = db::terminal_command_is_running(pool, terminal_id)
            .await
            .expect("check the command's running state");
        if running.is_none() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the seeded command never finished running"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// SME-75: with the pointer over the transcript, a side panel appearing
/// doesn't move what's under it. In a wide window the panel sits beside the
/// chat and narrows it, which re-wraps long messages; in a narrow one it
/// sits above the chat and pushes the transcript's top edge down. The
/// transcript used to snap to its new bottom either way. After the pointer
/// leaves, it catches up to its bottom.
async fn scenario_pointer_keeps_text_still(t: &Scenario<'_>) {
    // Where the pointer rests, as a fraction of the transcript's height: high
    // up in the wide window, so a lot of text below it re-wraps when the
    // panel narrows the chat; low in the narrow one, where the panel takes
    // the top of the window.
    for (width, height, aim) in [(1400, 900, 0.25), (800, 900, 0.75)] {
        let conversation = t.conversation().await;
        for i in 0..30 {
            db::create_message(
                t.pool,
                conversation.id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &[anthropic::ContentBlock::Text {
                    text: format!("message {i}: {}", "a long line that wraps across the width of the chat column ".repeat(6)),
                }],
            )
            .await
            .expect("seed a message");
        }
        let page = t.tab("about:blank").await;
        page.execute(
            chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(width, height, 1.0, false),
        )
        .await
        .expect("set the window size");
        page.goto(t.url(&format!("conversation/{}", conversation.id)))
            .await
            .expect("open the conversation");
        wait_for_live_client(&page, conversation.id).await;
        wait_for_transcript_at_bottom(&page).await;
        let centre: Vec<f64> = page
            .evaluate(format!("(() => {{ const r = document.querySelector('.messages').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height * {aim}]; }})()"))
            .await
            .expect("read the transcript's box")
            .into_value()
            .expect("two numbers");
        // Two moves, so the page sees the pointer move over the transcript.
        for dx in [0.0, 1.0] {
            page.move_mouse(chromiumoxide::layout::Point::new(centre[0] + dx, centre[1]))
                .await
                .expect("move the mouse over the transcript");
        }
        let watch = format!(
            "(() => {{ window.__sme75 = document.elementFromPoint({}, {}); return window.__sme75 ? window.__sme75.getBoundingClientRect().top : null; }})()",
            centre[0] + 1.0,
            centre[1]
        );
        let before: f64 = page
            .evaluate(watch.as_str())
            .await
            .expect("read what's under the pointer")
            .into_value()
            .expect("an element under the pointer");
        sandbox::create_pod(t.pool, conversation.id, Default::default()).await.expect("create_pod");
        wait_for_element(&page, ".sandbox-panel", Duration::from_secs(30)).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let after: f64 = page
            .evaluate("window.__sme75.getBoundingClientRect().top")
            .await
            .expect("read where it is now")
            .into_value()
            .expect("a number");
        assert!(
            (after - before).abs() <= 2.0,
            "at {width}px the text under the pointer moved when the sandbox panel appeared: top {before} -> {after}"
        );
        // Short of its bottom with the text held still means a snap is
        // pending. Then a scroll that isn't the user's (no wheel, touch,
        // key or press: the transcript keeping the text under the pointer
        // still) mustn't count as the user leaving the bottom (SME-75 code
        // review). Where holding the text still ended at the bottom (the
        // panel above, in a narrow window), nothing is pending.
        if transcript_distance_from_bottom(&page).await > 1.0 {
            page.evaluate("(() => { document.querySelector('.messages').scrollTop -= 100; })()")
                .await
                .expect("scroll the transcript");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0))
            .await
            .expect("move the mouse off the transcript");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let distance = transcript_distance_from_bottom(&page).await;
        assert!(
            distance <= 1.0,
            "at {width}px, once the pointer left, the transcript should catch up to its bottom, but it's {distance}px away"
        );

        // But a scroll the user makes with a pending snap (a scrollbar
        // drag, or keys after clicking the text: neither is a wheel, touch
        // or key event on the transcript) is theirs: a press on the
        // transcript gives up the pending snap, so leaving doesn't pull
        // them back (SME-75 code review 2). A new terminal card grows the
        // sandbox panel, a layout change that arms the snap.
        page.move_mouse(chromiumoxide::layout::Point::new(centre[0], centre[1]))
            .await
            .expect("move the mouse back over the transcript");
        sandbox::create_terminal(t.pool, conversation.id).await.expect("create_terminal");
        tokio::time::sleep(Duration::from_millis(800)).await;
        page.click(chromiumoxide::layout::Point::new(centre[0], centre[1]))
            .await
            .expect("click the transcript's text");
        page.evaluate("(() => { document.querySelector('.messages').scrollTop -= 100; })()")
            .await
            .expect("scroll the transcript up");
        tokio::time::sleep(Duration::from_millis(300)).await;
        page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0))
            .await
            .expect("move the mouse off the transcript");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let distance = transcript_distance_from_bottom(&page).await;
        assert!(
            distance >= 90.0,
            "at {width}px, a scroll after clicking the text should stay where the user put it, but the transcript is {distance}px from its bottom"
        );
    }
}

/// SME-30: the model's replies render as markdown, and the reply is
/// untrusted. A heading, list, table, code block and link become those
/// elements; raw HTML (an `<img onerror>`, a `<script>`) and a
/// `javascript:` link stay text and never run; the user's own message
/// stays plain; a code block's copy button copies its code.
async fn scenario_markdown_reply(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    db::create_message(
        t.pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "Show **markdown** please".into() }],
    )
    .await
    .expect("seed the user's message");
    let reply = "## Plan\n\n- first\n- [x] done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() { let x = 1; }\n```\n\n\
        See [the docs](https://example.com/docs) or [this](javascript:window.__smeltPwned=2).\n\n\
        Inline <img src=x onerror=\"window.__smeltPwned=1\"> html.\n\n<script>window.__smeltPwned=3</script>\n";
    db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: reply.into() }])
        .await
        .expect("seed the reply");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    wait_for_element(&page, ".message-assistant .markdown h2", Duration::from_secs(10)).await;
    let facts: serde_json::Value = page
        .evaluate(
            "(() => { const md = document.querySelector('.message-assistant .markdown');               const link = md.querySelector('a');               return {                 heading: md.querySelector('h2')?.innerText,                 items: md.querySelectorAll('ul > li').length,                 checked: md.querySelector('li.md-task input[type=checkbox]')?.checked ?? null,                 cells: Array.from(md.querySelectorAll('td')).map(c => c.innerText),                 highlighted: md.querySelectorAll('pre.hl-code .hl-keyword, pre.hl-code .hl-storage').length,                 lang: md.querySelector('.md-code-lang')?.innerText,                 links: Array.from(md.querySelectorAll('a')).map(a => a.getAttribute('href')),                 target: link?.getAttribute('target'), rel: link?.getAttribute('rel'),                 imgs: md.querySelectorAll('img').length, scripts: md.querySelectorAll('script').length,                 text: md.innerText, pwned: window.__smeltPwned ?? null,                 user: document.querySelector('.message-user .message-text')?.innerText,                 userStrong: document.querySelectorAll('.message-user strong').length }; })()",
        )
        .await
        .expect("read the rendered reply")
        .into_value()
        .expect("facts");
    assert_eq!(facts["heading"], "Plan", "{facts}");
    assert_eq!(facts["items"], 2, "{facts}");
    assert_eq!(facts["checked"], true, "a checked task item: {facts}");
    assert_eq!(facts["cells"], serde_json::json!(["1", "2"]), "{facts}");
    assert!(facts["highlighted"].as_i64().unwrap_or(0) > 0, "the rust block should be highlighted: {facts}");
    assert_eq!(facts["lang"], "rust", "{facts}");
    assert_eq!(facts["links"], serde_json::json!(["https://example.com/docs"]), "only the https link is a link: {facts}");
    assert_eq!(facts["target"], "_blank", "{facts}");
    assert_eq!(facts["rel"], "noopener noreferrer", "{facts}");
    assert_eq!(facts["imgs"], 0, "raw HTML mustn't become an image: {facts}");
    assert_eq!(facts["scripts"], 0, "{facts}");
    assert!(facts["pwned"].is_null(), "the reply's raw HTML ran: {facts}");
    let text = facts["text"].as_str().unwrap_or_default();
    assert!(text.contains("<img src=x onerror=") && text.contains("<script>"), "raw HTML should show as text: {text}");
    assert_eq!(facts["user"], "Show **markdown** please", "the user's message stays plain: {facts}");
    assert_eq!(facts["userStrong"], 0, "{facts}");

    // The copy button hands the block's code to the clipboard.
    page.evaluate("navigator.clipboard.writeText = async text => { window.__smeltCopied = text; }")
        .await
        .expect("watch the clipboard");
    click_when_present(&page, ".md-code-copy", Duration::from_secs(5)).await;
    assert!(wait_for_text(&page, "Copied", Duration::from_secs(5)).await, "the button should say it copied");
    let copied: Option<String> = page
        .evaluate("window.__smeltCopied ?? null")
        .await
        .expect("read what was copied")
        .into_value()
        .expect("a string");
    assert_eq!(copied.as_deref(), Some("fn main() { let x = 1; }\n"));
}

/// SME-30: the streaming reply renders as markdown as it arrives. A code
/// fence that hasn't closed yet shows plain and is highlighted once it
/// closes, and the reply looks the same once it's saved.
async fn scenario_markdown_streaming(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    publish(conversation.id, ConversationEvent::ReplyReset {});
    let first = "Here is **bold** and code:\n\n```rust\nfn main() {\n";
    publish(conversation.id, ConversationEvent::ReplyDelta { text: first.into(), offset: 0 });
    wait_for_element(&page, ".message-streaming .markdown strong", Duration::from_secs(10)).await;
    let open_fence: serde_json::Value = page
        .evaluate(
            "(() => { const pre = document.querySelector('.message-streaming .markdown pre');               return { pre: !!pre, highlighted: pre?.classList.contains('hl-code') ?? null, code: pre?.innerText }; })()",
        )
        .await
        .expect("read the open fence")
        .into_value()
        .expect("facts");
    assert_eq!(open_fence["pre"], true, "the open fence should already be a code block: {open_fence}");
    assert_eq!(open_fence["highlighted"], false, "an open fence shows plain: {open_fence}");
    let rest = "}\n```\n\nDone.";
    publish(conversation.id, ConversationEvent::ReplyDelta { text: rest.into(), offset: first.len() });
    wait_for_element(&page, ".message-streaming .markdown pre.hl-code", Duration::from_secs(10)).await;
    let streamed: String = page
        .evaluate("document.querySelector('.message-streaming .markdown').innerHTML.replace(/ data-dioxus-id=\"\\d+\"/g, '')")
        .await
        .expect("read the streamed reply")
        .into_value()
        .expect("html");
    let full = format!("{first}{rest}");
    let saved = db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: full }])
        .await
        .expect("save the reply");
    publish(conversation.id, ConversationEvent::MessagesAppended { messages: vec![saved] });
    assert!(
        wait_for_count(&page, ".message-streaming", 0, Duration::from_secs(10)).await,
        "the saved reply should replace its streaming copy"
    );
    let saved_html: String = page
        .evaluate("document.querySelector('.message-assistant .markdown').innerHTML.replace(/ data-dioxus-id=\"\\d+\"/g, '')")
        .await
        .expect("read the saved reply")
        .into_value()
        .expect("html");
    // Dioxus's own element ids differ between the two mounts; the reply
    // itself mustn't.
    assert_eq!(saved_html, streamed, "the reply changed when it was saved");
}

/// SME-30: a long reply (about 30 KB, ten code blocks) keeps up while it
/// streams. The whole reply re-renders on every delta, so this bounds how
/// far the page falls behind: the last delta shows within 3 s of being
/// sent, on the debug bundle the tier serves.
async fn scenario_markdown_long_reply(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    let section = |i: usize| {
        format!(
            "### Part {i}\n\nSome **text** with `code` and a [link](https://example.com/{i}). {}\n\n- one\n- two\n\n```rust\nfn part_{i}() -> u32 {{\n    let x = {i};\n    x * 2\n}}\n```\n\n",
            "Filler words to make the reply long. ".repeat(60)
        )
    };
    let reply: String = (0..10).map(section).collect::<String>() + "THE-END-MARKER";
    publish(conversation.id, ConversationEvent::ReplyReset {});
    let chunk = reply.len().div_ceil(100);
    let mut offset = 0;
    let mut sent_last = tokio::time::Instant::now();
    while offset < reply.len() {
        let mut end = (offset + chunk).min(reply.len());
        while !reply.is_char_boundary(end) {
            end += 1;
        }
        publish(conversation.id, ConversationEvent::ReplyDelta { text: reply[offset..end].to_string(), offset });
        offset = end;
        sent_last = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        wait_for_text(&page, "THE-END-MARKER", Duration::from_secs(30)).await,
        "the end of a long streamed reply never showed"
    );
    let lag = sent_last.elapsed();
    println!("browser tier: markdown_long_reply: {} bytes in 100 deltas, end shown {lag:?} after the last", reply.len());
    let blocks: i64 = page
        .evaluate("document.querySelectorAll('.message-streaming pre.hl-code').length")
        .await
        .expect("count highlighted blocks")
        .into_value()
        .expect("a number");
    assert_eq!(blocks, 10, "every closed block should be highlighted");
    assert!(lag < Duration::from_secs(3), "the page fell {lag:?} behind a long streamed reply");
}

/// Streams a 100-delta reply into `conversation` (open in `page`, live)
/// with a turn running, then lets the working line tick twice. Returns how
/// long after the last delta its end showed, and the page's main-thread
/// script time over the whole stream, in seconds.
async fn stream_and_measure(page: &chromiumoxide::Page, conversation: i64, marker: &str) -> (Duration, f64) {
    use crate::events::{ConversationEvent, publish};
    use chromiumoxide::cdp::browser_protocol::performance::{EnableParams, Metric};
    page.execute(EnableParams::default()).await.expect("enable performance metrics");
    let script_seconds =
        |metrics: Vec<Metric>| metrics.iter().find(|m| m.name == "ScriptDuration").map(|m| m.value).unwrap_or(0.0);
    let before = script_seconds(page.metrics().await.expect("read metrics"));
    publish(conversation, ConversationEvent::TurnState { running: true });
    publish(conversation, ConversationEvent::ReplyReset {});
    let reply: String = (0..100).map(|i| format!("word{i} ")).collect::<String>() + marker;
    let chunk = reply.len().div_ceil(100);
    let mut offset = 0;
    let mut sent_last = tokio::time::Instant::now();
    while offset < reply.len() {
        let end = (offset + chunk).min(reply.len());
        publish(conversation, ConversationEvent::ReplyDelta { text: reply[offset..end].to_string(), offset });
        offset = end;
        sent_last = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(wait_for_text(page, marker, Duration::from_secs(60)).await, "the end of the streamed reply never showed");
    let lag = sent_last.elapsed();
    // The working line ticks once a second.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let script = script_seconds(page.metrics().await.expect("read metrics")) - before;
    publish(conversation, ConversationEvent::TurnState { running: false });
    (lag, script)
}

/// SME-57 (c): streaming a reply into a long conversation costs what the
/// reply costs, not what the conversation does. The same 100-delta reply
/// (and the working line's ticks) streams into a one-message conversation
/// and into one with 300 messages (markdown replies with code, tool rows);
/// the page's script time for the long one stays within 3x the short one's,
/// and the messages already shown keep their DOM nodes untouched. Before,
/// every delta and every tick re-rendered and re-parsed the whole
/// transcript, so the long one cost many times more (the page still kept
/// up: the cost is CPU, not lag).
async fn scenario_streaming_into_a_long_transcript(t: &Scenario<'_>) {
    let short = t.conversation().await;
    seed_user_message(t.pool, short.id, "One question.").await;
    let page = t.tab(t.url(&format!("conversation/{}", short.id))).await;
    wait_for_live_client(&page, short.id).await;
    let (short_lag, short_script) = stream_and_measure(&page, short.id, "SHORT-END-MARKER").await;

    let long = t.conversation().await;
    for i in 0..100 {
        seed_user_message(t.pool, long.id, &format!("Question {i}: what does part {i} do?")).await;
        let reply = format!(
            "### Part {i}\n\nIt **doubles** `x`. {}\n\n```rust\nfn part_{i}() -> u32 {{\n    let x = {i};\n    x * 2\n}}\n```\n",
            "Some words about it. ".repeat(20)
        );
        db::create_message(t.pool, long.id, "assistant", &[
            anthropic::ContentBlock::Text { text: reply },
            anthropic::ContentBlock::ToolUse {
                id: format!("toolu_long_{i}"),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": format!("/workspace/part_{i}.rs")}),
            },
        ])
        .await
        .expect("seed a reply");
        db::create_message(t.pool, long.id, "user", &[anthropic::ContentBlock::ToolResult {
            tool_use_id: format!("toolu_long_{i}"),
            content: format!("fn part_{i}() {{}}"),
            is_error: None,
        }])
        .await
        .expect("seed a tool result");
    }
    let page = t.tab(t.url(&format!("conversation/{}", long.id))).await;
    wait_for_live_client(&page, long.id).await;
    assert!(
        wait_for_count(&page, ".messages .message-assistant", 100, Duration::from_secs(30)).await,
        "the long conversation never showed"
    );
    // Tag the first reply's node and count every change under it.
    page.evaluate(
        "(() => { const m = document.querySelector('.messages .message-assistant'); m.__smeltTag = 'first'; \
          window.__smeltMutations = 0; \
          new MutationObserver(r => { window.__smeltMutations += r.length; }) \
            .observe(m, { subtree: true, childList: true, characterData: true, attributes: true }); })()",
    )
    .await
    .expect("tag the first reply");
    let (long_lag, long_script) = stream_and_measure(&page, long.id, "LONG-END-MARKER").await;
    let first: serde_json::Value = page
        .evaluate(
            "(() => ({ same: document.querySelector('.messages .message-assistant')?.__smeltTag === 'first', \
                       mutations: window.__smeltMutations }))()",
        )
        .await
        .expect("read the first reply")
        .into_value()
        .expect("facts");
    println!(
        "browser tier: streaming_into_a_long_transcript: script {short_script:.2} s short, {long_script:.2} s long \
         ({:.1}x); end shown {short_lag:?} / {long_lag:?} after the last delta; first reply {first}",
        long_script / short_script.max(0.001)
    );
    assert_eq!(first["same"], true, "the first reply's node was replaced: {first}");
    assert_eq!(first["mutations"], 0, "the first reply changed while another streamed: {first}");
    assert!(
        long_script <= 3.0 * short_script.max(0.05),
        "streaming into 300 messages took {long_script:.2} s of script, against {short_script:.2} s into one"
    );
}

/// SME-57 (d), from SME-58: a conversation in the sidebar opens from the
/// keyboard. Each row's title is a link and a tab stop that Enter or Space
/// opens; the row itself isn't one, so its Delete isn't nested inside the
/// link (a screen reader would read "Delete" as part of the link's name,
/// and some can't reach a button inside a link). Delete stays a tab stop
/// of its own: Enter on it arms it, without opening the conversation. The
/// rows took mouse clicks only.
async fn scenario_conversation_rows_by_keyboard(t: &Scenario<'_>) {
    let first = t.conversation().await;
    seed_user_message(t.pool, first.id, "the first row").await;
    let second = t.conversation().await;
    seed_user_message(t.pool, second.id, "the second row").await;
    let page = t.tab(t.url(&format!("conversation/{}", first.id))).await;
    wait_for_live_client(&page, first.id).await;
    let row = |id: i64| format!("[data-conversation-id=\"{id}\"]");
    let title = |id: i64| format!("{} .conversation-title", row(id));
    let roles: serde_json::Value = page
        .evaluate(format!(
            "(() => {{ const r = document.querySelector('{}'), t = r.querySelector('.conversation-title'); \
               return {{ row: r.getAttribute('role') || 'none', title: t.getAttribute('role') || 'none', tab: t.tabIndex }}; }})()",
            row(second.id).replace('\'', "\\'")
        ))
        .await
        .expect("read the row's roles")
        .into_value()
        .expect("facts");
    assert_eq!(roles["title"], "link", "a row's title should be its link: {roles}");
    assert_eq!(roles["row"], "none", "the row around the link and its Delete shouldn't be a link too: {roles}");
    assert!(roles["tab"].as_i64().is_some_and(|t| t >= 0), "the title should be a tab stop: {roles}");
    let at = |id: i64| format!("/conversation/{id}");
    let url_now = |page: &chromiumoxide::Page| {
        let page = page.clone();
        async move { page.url().await.ok().flatten().unwrap_or_default() }
    };
    let wait_for_url = |page: &chromiumoxide::Page, suffix: String| {
        let page = page.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if page.url().await.ok().flatten().is_some_and(|u| u.ends_with(&suffix)) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }
    };
    let second_row = wait_for_element(&page, &title(second.id), Duration::from_secs(10)).await;
    second_row.focus().await.expect("focus the second row");
    second_row.press_key("Enter").await.expect("press Enter");
    assert!(wait_for_url(&page, at(second.id)).await, "Enter on a row should open it: at {}", url_now(&page).await);
    let first_row = wait_for_element(&page, &title(first.id), Duration::from_secs(10)).await;
    first_row.focus().await.expect("focus the first row");
    first_row.press_key(" ").await.expect("press Space");
    assert!(wait_for_url(&page, at(first.id)).await, "Space on a row should open it: at {}", url_now(&page).await);
    let delete = wait_for_element(&page, &format!("{} .delete-conversation", row(second.id)), Duration::from_secs(10)).await;
    delete.focus().await.expect("focus the second row's Delete");
    delete.press_key("Enter").await.expect("press Enter on Delete");
    wait_for_element(&page, &format!("{} .delete-conversation.confirm", row(second.id)), Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let url = url_now(&page).await;
    assert!(url.ends_with(&at(first.id)), "Enter on Delete should arm it, not open its row: at {url}");
}

/// SME-57 (d), from SME-58: the message box has a label, not only a
/// placeholder, which disappears as soon as anything is typed.
async fn scenario_message_box_label(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_element(&page, CHAT_INPUT, Duration::from_secs(10)).await;
    let label: serde_json::Value = page
        .evaluate(format!(
            "(() => {{ const i = document.querySelector('{}'); \
               return {{ labels: i.labels ? i.labels.length : 0, text: i.labels && i.labels[0] ? i.labels[0].textContent.trim() : null }}; }})()",
            CHAT_INPUT.replace('\'', "\\'")
        ))
        .await
        .expect("read the message box's label")
        .into_value()
        .expect("facts");
    assert_eq!(label["labels"], 1, "the message box should have one label: {label}");
    assert!(label["text"].as_str().is_some_and(|t| !t.is_empty()), "its label should say something: {label}");
}

/// SME-57 (d), from SME-58: an error line on the chat page is announced:
/// it uses `pages::ErrorText` (`role="alert"`) like the other pages, not a
/// bare `p.error`.
async fn scenario_chat_errors_are_alerts(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    publish(conversation.id, ConversationEvent::NotificationDeliveryFailed { detail: "NOTICE-ALERT-MARKER".into() });
    publish(conversation.id, ConversationEvent::TurnError { message: "TURN-ALERT-MARKER".into() });
    for marker in ["NOTICE-ALERT-MARKER", "TURN-ALERT-MARKER"] {
        assert!(wait_for_text(&page, marker, Duration::from_secs(10)).await, "{marker} never showed");
        let line: serde_json::Value = page
            .evaluate(format!(
                "(() => {{ const e = [...document.querySelectorAll('.error')].find(e => e.innerText.includes('{marker}')); \
                   return {{ found: !!e, role: e ? (e.getAttribute('role') || 'none') : 'none' }}; }})()"
            ))
            .await
            .expect("read the error line's role")
            .into_value()
            .expect("facts");
        assert_eq!(line["found"], true, "no .error line has {marker}: {line}");
        assert_eq!(line["role"], "alert", "the error line with {marker} should be an alert: {line}");
    }
}

/// SME-57 (d), from SME-58: a message that can't be read shows as an
/// error, in the error colours, not as an ordinary bubble (its class,
/// `message-error`, had no CSS rule).
async fn scenario_unreadable_message(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    sqlx::query("INSERT INTO messages (conversation_id, role, content) VALUES ($1, 'assistant', 'not json')")
        .bind(conversation.id)
        .execute(t.pool)
        .await
        .expect("seed an unreadable message");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_element(&page, ".message-error", Duration::from_secs(10)).await;
    let colours: serde_json::Value = page
        .evaluate(
            "(() => { const probe = document.createElement('p'); probe.className = 'error'; document.body.appendChild(probe); \
               const want = getComputedStyle(probe).color; probe.remove(); \
               return { want, got: getComputedStyle(document.querySelector('.message-error')).color }; })()",
        )
        .await
        .expect("read the colours")
        .into_value()
        .expect("colours");
    assert_eq!(colours["got"], colours["want"], "an unreadable message should be in the error colour: {colours}");
}

/// A server that answers every request after `delay` with an SVG image of
/// `height` pixels, so an image in a reply grows its bubble after the
/// reply has rendered. Returns its address.
async fn serve_late_image(delay: Duration, height: u32) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the late image");
    let address = format!("http://{}/", listener.local_addr().expect("the late image's address"));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
                tokio::time::sleep(delay).await;
                let body = format!(
                    "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='{height}'><rect width='300' height='{height}' fill='#88c'/></svg>"
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: image/svg+xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, format!("{head}{body}").as_bytes()).await;
            });
        }
    });
    address
}

/// SME-30: an image in a reply loads after the reply has rendered and
/// grows it. With the transcript stuck to its bottom, it stays at its
/// bottom; with the pointer over the transcript, the text under the
/// pointer doesn't move (SME-75's rule for anything that moves the bottom
/// that isn't new content).
async fn scenario_markdown_late_image(t: &Scenario<'_>) {
    for pointer in [false, true] {
        let image = serve_late_image(Duration::from_secs(2), 400).await;
        let conversation = t.conversation().await;
        for i in 0..20 {
            db::create_message(
                t.pool,
                conversation.id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &[anthropic::ContentBlock::Text { text: format!("filler message {i}") }],
            )
            .await
            .expect("seed a message");
        }
        db::create_message(
            t.pool,
            conversation.id,
            "assistant",
            &[anthropic::ContentBlock::Text { text: format!("A picture:\n\n![late]({image})\n\nThe end.") }],
        )
        .await
        .expect("seed the reply with an image");
        let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
        wait_for_live_client(&page, conversation.id).await;
        wait_for_element(&page, ".markdown img.md-image", Duration::from_secs(10)).await;
        let loaded: bool = page
            .evaluate("document.querySelector('.markdown img.md-image').complete")
            .await
            .expect("read the image")
            .into_value()
            .expect("a bool");
        assert!(!loaded, "the image should still be loading, or this checks nothing");
        wait_for_transcript_at_bottom(&page).await;
        let mut before = None;
        if pointer {
            let centre: Vec<f64> = page
                .evaluate("(() => { const r = document.querySelector('.messages').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height * 0.3]; })()")
                .await
                .expect("read the transcript's box")
                .into_value()
                .expect("two numbers");
            for dx in [0.0, 1.0] {
                page.move_mouse(chromiumoxide::layout::Point::new(centre[0] + dx, centre[1]))
                    .await
                    .expect("move the mouse over the transcript");
            }
            let top: f64 = page
                .evaluate(format!(
                    "(() => {{ window.__sme30 = document.elementFromPoint({}, {}); return window.__sme30.getBoundingClientRect().top; }})()",
                    centre[0] + 1.0,
                    centre[1]
                ))
                .await
                .expect("read what's under the pointer")
                .into_value()
                .expect("a number");
            before = Some(top);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let complete: bool = page
                .evaluate("(() => { const i = document.querySelector('.markdown img.md-image'); return i.complete && i.naturalHeight > 0; })()")
                .await
                .expect("read the image")
                .into_value()
                .expect("a bool");
            if complete {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the late image never loaded");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        if let Some(before) = before {
            let after: f64 = page
                .evaluate("window.__sme30.getBoundingClientRect().top")
                .await
                .expect("read where it is now")
                .into_value()
                .expect("a number");
            assert!(
                (after - before).abs() <= 2.0,
                "the text under the pointer moved when the image loaded: top {before} -> {after}"
            );
            page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0))
                .await
                .expect("move the mouse off the transcript");
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        let distance = transcript_distance_from_bottom(&page).await;
        assert!(
            distance <= 1.0,
            "pointer {pointer}: the transcript should be at its bottom once the image has loaded, but it's {distance}px away"
        );
    }
}

/// Waits until `.messages` has settled at its bottom: the same distance,
/// within a pixel of it, on two reads 300 ms apart.
async fn wait_for_transcript_at_bottom(page: &chromiumoxide::Page) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = f64::NAN;
    loop {
        let distance = transcript_distance_from_bottom(page).await;
        if distance <= 1.0 && distance == last {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the transcript never settled at its bottom: {distance}px away"
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// SME-57 code review: the address bar's navigation belongs to the chat
/// panel, not to the browsing panel. A session that closes while a
/// navigation is loading unmounts the panel; when the browser opens again
/// in that conversation, its address bar must still take an address.
async fn scenario_address_bar_after_session_closes(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    crate::browsing::open_session_with_guard(conversation.id, |_| true)
        .await
        .expect("open a browsing session");
    // A page that answers after 3 s: the navigation is still loading when
    // the session closes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a slow page");
    let address = format!("http://{}/", listener.local_addr().expect("the slow page's address"));
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = tokio::io::AsyncWriteExt::write_all(
                    &mut socket,
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 11\r\n\r\n<p>slow</p>",
                )
                .await;
            });
        }
    });
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    let input = wait_for_element(&page, ".browsing-address-input", Duration::from_secs(10)).await;
    input.focus().await.expect("focus the address bar");
    input.type_str(&address).await.expect("type the slow page's address");
    input.press_key("Enter").await.expect("go");
    let loading = wait_for_count(&page, ".browsing-address-input[disabled]", 1, Duration::from_secs(5)).await;
    crate::browsing::close_session(conversation.id).await.expect("close the browsing session");
    let closed = wait_for_count(&page, ".browsing-panel", 0, Duration::from_secs(5)).await;
    // Past the slow page's answer: the navigation has ended either way.
    tokio::time::sleep(Duration::from_secs(4)).await;
    crate::browsing::open_session_with_guard(conversation.id, |_| true)
        .await
        .expect("open the browsing session again");
    let reopened = wait_for_count(&page, ".browsing-address-input", 1, Duration::from_secs(10)).await;
    let disabled: bool = page
        .evaluate("document.querySelector('.browsing-address-input')?.disabled ?? true")
        .await
        .expect("read the address bar")
        .into_value()
        .expect("a bool");
    // Closed before the checks, so a failing one doesn't leave the session open.
    crate::browsing::close_session(conversation.id).await.expect("close the browsing session");
    server.abort();
    assert!(loading, "the address bar should show the navigation loading");
    assert!(closed, "the browsing panel should go when the session closes");
    assert!(reopened, "the browsing panel should come back when the session reopens");
    assert!(!disabled, "the address bar is still disabled after the session closed mid-navigation and reopened");
}

/// SME-82: the context bar and its detail view work from the
/// keyboard. The bar was a clickable div (no focus), the view had no dialog
/// role and ignored Escape, and its "×" had no name.
async fn scenario_context_from_the_keyboard(t: &Scenario<'_>) {
    let keyboard_conversation = t.conversation().await;
    db::create_message(
        t.pool,
        keyboard_conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "hello".to_string() }],
    )
    .await
    .expect("seed a message");
    db::upsert_conversation_usage(
        t.pool,
        keyboard_conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 40_000,
            output_tokens: 10_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let keyboard_page = t.tab(t.url(&format!("conversation/{}", keyboard_conversation.id))).await;
    assert!(
        wait_for_text(&keyboard_page, "25% of context", Duration::from_secs(10)).await,
        "the context bar should show"
    );
    wait_for_live_client(&keyboard_page, keyboard_conversation.id).await;
    let focused_is = |selector: &str| format!("document.activeElement === document.querySelector({selector:?})");
    let bar = wait_for_element(&keyboard_page, ".context-usage-bar", Duration::from_secs(5)).await;
    bar.focus().await.expect("focus the context bar");
    let bar_focused: bool = keyboard_page
        .evaluate(focused_is(".context-usage-bar"))
        .await
        .expect("read the focus")
        .into_value()
        .expect("a bool");
    assert!(bar_focused, "the context bar should take keyboard focus");
    bar.press_key("Enter").await.expect("press Enter on the bar");
    assert!(
        wait_for_count(&keyboard_page, ".context-detail-overlay [role=dialog]", 1, Duration::from_secs(5)).await,
        "Enter on the bar should open the detail view as a dialog"
    );
    // Focus moves in a task the page spawns, so wait for it.
    assert!(
        wait_for_focus(&keyboard_page, ".context-detail-close", Duration::from_secs(5)).await,
        "opening the detail view should focus its close button"
    );
    let close_name: String = keyboard_page
        .evaluate("document.querySelector('.context-detail-close').getAttribute('aria-label') || ''")
        .await
        .expect("read the close button")
        .into_value()
        .expect("a string");
    assert_eq!(close_name, "Close", "the close button should be named Close");
    let close = wait_for_element(&keyboard_page, ".context-detail-close", Duration::from_secs(5)).await;
    close.press_key("Escape").await.expect("press Escape");
    assert!(
        wait_for_count(&keyboard_page, ".context-detail-overlay", 0, Duration::from_secs(5)).await,
        "Escape should close the detail view"
    );
    assert!(
        wait_for_focus(&keyboard_page, ".context-usage-bar", Duration::from_secs(5)).await,
        "closing the detail view should put focus back on the bar"
    );

    // Escape still closes the view after a click on its own text, which
    // moved focus to the page body, outside the view's key handler
    // (SME-82 code review).
    bar.press_key("Enter").await.expect("press Enter on the bar");
    assert!(
        wait_for_text(&keyboard_page, "Tools (", Duration::from_secs(10)).await,
        "the detail view should load"
    );
    click_when_present(&keyboard_page, ".context-detail-panel h3", Duration::from_secs(5)).await;
    bar.press_key("Escape").await.expect("press Escape");
    assert!(
        wait_for_count(&keyboard_page, ".context-detail-overlay", 0, Duration::from_secs(3)).await,
        "Escape should close the detail view after a click on its text"
    );
    // Enter goes to whatever has focus, and focus returns to the bar in
    // a task the page spawns.
    assert!(
        wait_for_focus(&keyboard_page, ".context-usage-bar", Duration::from_secs(5)).await,
        "closing the detail view should put focus back on the bar"
    );

    // Tab doesn't walk out of a view that says it's modal (SME-82 code
    // review).
    bar.press_key("Enter").await.expect("press Enter on the bar");
    assert!(
        wait_for_focus(&keyboard_page, ".context-detail-close", Duration::from_secs(5)).await,
        "opening the detail view again should focus its close button"
    );
    let mut tabbed_out = None;
    for step in 1..=20 {
        bar.press_key("Tab").await.expect("press Tab");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let inside: bool = keyboard_page
            .evaluate("(() => { const o = document.querySelector('.context-detail-overlay'); return !!o && o.contains(document.activeElement); })()")
            .await
            .expect("read the focus")
            .into_value()
            .expect("a bool");
        if !inside {
            let on: String = keyboard_page
                .evaluate("(document.activeElement && (document.activeElement.className || document.activeElement.tagName)) || 'nothing'")
                .await
                .expect("read the focus")
                .into_value()
                .unwrap_or_default();
            tabbed_out = Some(format!("Tab {step} moved focus to {on}"));
            break;
        }
    }
    assert!(tabbed_out.is_none(), "Tab should stay in the detail view: {}", tabbed_out.unwrap_or_default());
}

/// Waits up to `timeout` for `selector`'s element to have focus. Focus
/// moves the page makes in a spawned task land a moment after the change
/// that asked for them.
async fn wait_for_focus(page: &chromiumoxide::Page, selector: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let probe = format!("document.activeElement === document.querySelector({selector:?})");
    loop {
        let focused: bool = page
            .evaluate(probe.as_str())
            .await
            .ok()
            .and_then(|value| value.into_value().ok())
            .unwrap_or(false);
        if focused {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How far `.messages` is scrolled from its bottom, in pixels.
async fn transcript_distance_from_bottom(page: &chromiumoxide::Page) -> f64 {
    page.evaluate("(() => { const m = document.querySelector('.messages'); return m ? m.scrollHeight - m.scrollTop - m.clientHeight : -1; })()")
        .await
        .expect("read the transcript's scroll position")
        .into_value()
        .expect("a number")
}

/// The test's own provider (the slow mock), which every conversation it
/// creates runs on.
static MOCK_PROVIDER: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// The model `MOCK_PROVIDER` serves (any name does: the mock ignores it).
const MOCK_MODEL: &str = "mock-model";

/// Makes the page's next DELETE request fail as a server error would;
/// every other request goes through.
const FAIL_NEXT_DELETE: &str = "(() => { const real = window.fetch; let armed = true; \
     window.fetch = function (input, init) { \
       const method = ((init && init.method) || (input && input.method) || 'GET').toUpperCase(); \
       if (armed && method === 'DELETE') { armed = false; return Promise.resolve(new Response('refused by the browser tier', { status: 500 })); } \
       return real.apply(this, arguments); }; })()";

/// Scenario 31 (SME-58): each settings page's two-step delete arms on the
/// first click and deletes on the second (they share `TwoStepButton`), and
/// the MCP header inputs say what they are to a screen reader, not only in
/// a placeholder.
async fn scenario_settings_two_step(t: &Scenario<'_>) {
    let pool = t.pool;
    let mcp = db::create_mcp_server_config(
        pool,
        &unique_id("two-step"),
        "http://127.0.0.1:9/mcp",
        &[("X-Existing".to_string(), "v".to_string())].into_iter().collect(),
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create an MCP server");
    let page = t.tab(t.url(&format!("mcp-servers/{}", mcp.id))).await;
    wait_for_resource(&page, &format!("/api/mcp-servers/{}", mcp.id)).await;
    click_when_present(&page, ".mcp-add-header", Duration::from_secs(5)).await;
    wait_for_element(&page, ".mcp-header-name", Duration::from_secs(5)).await;
    let labels: Vec<String> = page
        .evaluate(
            "Array.from(document.querySelectorAll('.mcp-header-row input')).map(i => i.getAttribute('aria-label') || '')",
        )
        .await
        .expect("read the header inputs' labels")
        .into_value()
        .expect("strings");
    assert!(
        labels.len() >= 3 && labels.iter().all(|label| !label.is_empty()),
        "every header input should have an aria-label: {labels:?}"
    );
    two_step_delete(&page, ".mcp-delete").await;
    assert!(
        wait_until(|| async { matches!(db::get_mcp_server_config(pool, mcp.id).await, Ok(None)) }, Duration::from_secs(10)).await,
        "the MCP server should be deleted"
    );

    let server = db::create_language_server(
        pool,
        &crate::models::LanguageServerConfig {
            name: unique_id("two-step"),
            image: "rust:1".to_string(),
            command: "true".to_string(),
            memory_limit: "1Gi".to_string(),
            cpu_limit: "1".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create a language server");
    let page = t.tab(t.url(&format!("language-servers/{}", server.id))).await;
    wait_for_resource(&page, &format!("/api/language-servers/{}", server.id)).await;
    two_step_delete(&page, ".mcp-delete-server").await;
    assert!(
        wait_until(|| async { matches!(db::get_language_server(pool, server.id).await, Ok(None)) }, Duration::from_secs(10)).await,
        "the language server should be deleted"
    );

    let provider = db::create_inference_provider(pool, &unique_id("two-step"), "anthropic", "http://127.0.0.1:9", "api_key", "key")
        .await
        .expect("create a provider");
    let page = t.tab(t.url(&format!("providers/{}", provider.id))).await;
    wait_for_resource(&page, &format!("/api/providers/{}", provider.id)).await;
    two_step_delete(&page, ".mcp-delete-server").await;
    assert!(
        wait_until(|| async { matches!(db::get_inference_provider(pool, provider.id).await, Ok(None)) }, Duration::from_secs(10)).await,
        "the provider should be deleted"
    );
}

/// Clicks a two-step button at `selector` once (it must arm: `confirm`
/// class, a "Confirm" label) and again (to confirm).
async fn two_step_delete(page: &chromiumoxide::Page, selector: &str) {
    wait_for_stable(page, selector, tokio::time::Instant::now() + Duration::from_secs(10)).await;
    click_when_present(page, selector, Duration::from_secs(5)).await;
    let armed = format!("{selector}.confirm");
    wait_for_element(page, &armed, Duration::from_secs(5)).await;
    let label: String = page
        .evaluate(format!("document.querySelector({armed:?}).innerText"))
        .await
        .expect("read the armed label")
        .into_value()
        .expect("text");
    assert!(label.starts_with("Confirm"), "an armed button should ask to confirm: {label:?}");
    click_when_present(page, &armed, Duration::from_secs(5)).await;
}

/// Polls `check` every 200ms until it holds or `timeout` passes.
async fn wait_until<F, Fut>(check: F, timeout: Duration) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Scenario 30 (SME-81): an error says what went wrong, not dioxus's
/// "error running server function: … (details: None)" wrapper, and the
/// sidebar's error goes once a later sidebar action succeeds.
async fn scenario_error_text(t: &Scenario<'_>) {
    // Through the list, whose own fetch shows the page is hydrated: typed
    // into the server-rendered form before that, the text never reaches
    // the form's state.
    let volume_page = t.tab(t.url("sandbox-volumes")).await;
    wait_for_resource(&volume_page, "/api/sandbox-volumes").await;
    click_when_present(&volume_page, ".sandbox-volumes-new-link", Duration::from_secs(5)).await;
    let name_field = wait_for_element(&volume_page, "#sandbox-volume-new-name", Duration::from_secs(10)).await;
    name_field.focus().await.expect("focus the volume name");
    name_field.type_str("scenario-30-root").await.expect("type the volume name");
    let path_field = wait_for_element(&volume_page, "#sandbox-volume-new-mount-path", Duration::from_secs(5)).await;
    path_field.focus().await.expect("focus the mount path");
    path_field.type_str("/").await.expect("type the mount path");
    click_when_present(&volume_page, ".sandbox-volume-add-form button[type=submit]", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&volume_page, "can't be mounted over the root directory", Duration::from_secs(10)).await,
        "the volume form should say why `/` was refused"
    );
    let shown: String = volume_page
        .evaluate("document.querySelector('.sandbox-volume-add-form .error').innerText")
        .await
        .expect("read the error")
        .into_value()
        .expect("text");
    assert!(
        !shown.contains("error running server function") && !shown.contains("details:"),
        "the error should be the server's message alone: {shown:?}"
    );

    // Two conversations to delete from the sidebar: the first delete fails,
    // the second succeeds. A success that navigates (New conversation)
    // remounts the sidebar and hides the bug; deleting a row that isn't
    // open stays on the page.
    let refused = t.conversation().await;
    let deleted = t.conversation().await;
    let watched = t.conversation().await;
    let sidebar = t.tab(t.url(&format!("conversation/{}", watched.id))).await;
    wait_for_live_client(&sidebar, watched.id).await;
    let delete_button = |id: i64| format!(".conversation-item[data-conversation-id='{id}'] .delete-conversation");
    wait_for_stable(&sidebar, &delete_button(refused.id), tokio::time::Instant::now() + Duration::from_secs(10)).await;
    // The next DELETE (the first conversation delete) fails as a server
    // error would; everything else goes through.
    sidebar.evaluate(FAIL_NEXT_DELETE).await.expect("make the next DELETE fail");
    click_when_present(&sidebar, &delete_button(refused.id), Duration::from_secs(5)).await;
    click_when_present(&sidebar, &format!("{}.confirm", delete_button(refused.id)), Duration::from_secs(5)).await;
    wait_for_element(&sidebar, ".sidebar-body > .error", Duration::from_secs(10)).await;
    // A message arriving in the open conversation reloads the list (as a
    // running turn's every reply does); that reload succeeding isn't the
    // failed Delete succeeding, so the error stays (SME-81 review).
    let lists_before = list_requests(&sidebar).await;
    let arrived = db::create_message(
        t.pool,
        watched.id,
        "assistant",
        &[anthropic::ContentBlock::Text { text: "scenario 30 reload".to_string() }],
    )
    .await
    .expect("save a message");
    crate::events::publish(watched.id, crate::events::ConversationEvent::MessagesAppended { messages: vec![arrived] });
    assert!(
        wait_for_text(&sidebar, "scenario 30 reload", Duration::from_secs(10)).await,
        "the open conversation should show the new message"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while list_requests(&sidebar).await <= lists_before {
        assert!(tokio::time::Instant::now() < deadline, "the new message should reload the conversation list");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The reload's result is applied by an effect after the request ends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kept: bool = sidebar
        .evaluate("!!document.querySelector('.sidebar-body > .error')")
        .await
        .expect("look for the sidebar error")
        .into_value()
        .expect("bool");
    assert!(kept, "a list reload shouldn't clear a failed Delete's error");
    click_when_present(&sidebar, &delete_button(deleted.id), Duration::from_secs(5)).await;
    click_when_present(&sidebar, &format!("{}.confirm", delete_button(deleted.id)), Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&sidebar, &format!(".conversation-item[data-conversation-id='{}']", deleted.id), 0, Duration::from_secs(10)).await,
        "the second delete should go through"
    );
    let still_shown: bool = sidebar
        .evaluate("!!document.querySelector('.sidebar-body > .error')")
        .await
        .expect("look for the sidebar error")
        .into_value()
        .expect("bool");
    assert!(!still_shown, "a later success should clear the sidebar's error");

    // The same on a settings page: a failed volume delete's error goes once
    // another delete succeeds (SME-81 review 2).
    let refused_volume = db::create_sandbox_volume(t.pool, &unique_id("refused"), "/home/sandbox/refused")
        .await
        .expect("create a volume")
        .id;
    let deleted_volume = db::create_sandbox_volume(t.pool, &unique_id("deleted"), "/home/sandbox/deleted")
        .await
        .expect("create a volume")
        .id;
    let volumes = t.tab(t.url("sandbox-volumes")).await;
    wait_for_resource(&volumes, "/api/sandbox-volumes").await;
    let volume_delete = |id: i64| format!(".sandbox-volume-row[data-volume-id='{id}'] .sandbox-volume-delete");
    wait_for_stable(&volumes, &volume_delete(refused_volume), tokio::time::Instant::now() + Duration::from_secs(10)).await;
    volumes.evaluate(FAIL_NEXT_DELETE).await.expect("make the next DELETE fail");
    click_when_present(&volumes, &volume_delete(refused_volume), Duration::from_secs(5)).await;
    click_when_present(&volumes, &format!("{}.confirm", volume_delete(refused_volume)), Duration::from_secs(5)).await;
    wait_for_element(&volumes, ".sandbox-volumes-page .error", Duration::from_secs(10)).await;
    click_when_present(&volumes, &volume_delete(deleted_volume), Duration::from_secs(5)).await;
    click_when_present(&volumes, &format!("{}.confirm", volume_delete(deleted_volume)), Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&volumes, &format!(".sandbox-volume-row[data-volume-id='{deleted_volume}']"), 0, Duration::from_secs(10)).await,
        "the second volume delete should go through"
    );
    let still_shown: bool = volumes
        .evaluate("!!document.querySelector('.sandbox-volumes-page .error')")
        .await
        .expect("look for the volume error")
        .into_value()
        .expect("bool");
    let _ = db::delete_sandbox_volume(t.pool, refused_volume).await;
    assert!(!still_shown, "a later volume delete succeeding should clear the failed one's error");
}

/// How many times `page` has fetched the conversation list.
async fn list_requests(page: &chromiumoxide::Page) -> usize {
    page.evaluate("performance.getEntriesByType('resource').filter(e => e.name.endsWith('/api/conversations')).length")
        .await
        .expect("read resource timings")
        .into_value()
        .expect("a count")
}

async fn new_conversation(
    pool: &sqlx::PgPool,
    created: &std::sync::Mutex<Vec<i64>>,
) -> crate::models::Conversation {
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    created.lock().expect("the conversation list lock").push(conversation.id);
    let provider = *MOCK_PROVIDER.get().expect("the mock provider is saved first");
    db::set_conversation_model(pool, conversation.id, provider, MOCK_MODEL)
        .await
        .expect("put the conversation on the mock provider");
    conversation
}

/// Ids of every sandbox pod belonging to `conversations`, read before
/// they're removed (the rows go with the conversation).
async fn sandbox_pod_ids(pool: &sqlx::PgPool, conversations: &[i64]) -> Vec<i64> {
    let mut ids = Vec::new();
    for &conversation in conversations {
        let rows = db::list_sandbox_pods(pool, conversation).await.unwrap_or_default();
        ids.extend(rows.into_iter().map(|row| row.id));
    }
    ids
}

/// Removes `conversations` the way deleting one in the app does: its
/// sandbox pods first, then the conversation itself (its messages, todos,
/// terminals and so on go with it).
async fn remove_conversations(pool: &sqlx::PgPool, conversations: &[i64]) {
    for &conversation in conversations {
        sandbox::teardown_conversation(conversation, &[]).await;
        if let Err(e) = db::delete_conversation(pool, conversation).await {
            eprintln!("failed to delete test conversation {conversation}: {e}");
        }
    }
}

/// What the test left behind: conversations still in the database and
/// sandbox pods still in the cluster, described for the failure message.
async fn find_leftovers(
    pool: &sqlx::PgPool,
    conversations: &[i64],
    pod_ids: &[i64],
) -> Vec<String> {
    let mut leftovers: Vec<String> = db::list_conversations(pool)
        .await
        .expect("list conversations")
        .into_iter()
        .filter(|c| conversations.contains(&c.id))
        .map(|c| format!("conversation {} in the database", c.id))
        .collect();
    // A deleted pod can linger briefly while it terminates.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for &pod_id in pod_ids {
        while sandbox::pod_exists(pod_id).await {
            if tokio::time::Instant::now() >= deadline {
                leftovers.push(format!("sandbox pod {pod_id} in the cluster"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    leftovers
}
