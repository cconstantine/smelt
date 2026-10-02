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
//! Deliberately one test, not several: every scenario runs sequentially
//! inside it, sharing one browser/server/`MANAGER` instance for its whole
//! duration — more than one `#[tokio::test]` here touching `sandbox::init()`/
//! `db::init()` would risk the same `OnceLock`-across-separate-runtimes
//! hazard `docs/testing.md` documents for `PgPool`, the same reasoning
//! `sandbox-terminal`'s own real-cluster test already applied. Every
//! scenario bypasses the model entirely (seeding state directly via `db`/
//! `anthropic::tools`, never a real `send_message`) — this tier verifies
//! the browser/live-event pipeline and DOM rendering, not tool-selection or
//! compaction-trigger *logic* (already covered by `api::chat`'s own
//! mock-upstream tests), and this test environment (like CI) has no real
//! Anthropic credentials to make a live call with anyway.

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
        let stylesheet_url = {
            use dioxus::prelude::*;
            asset!("/assets/chat.css").to_string()
        };
        let stylesheet = std::fs::read_to_string(repo_root.join("assets/chat.css"))
            .expect("read assets/chat.css");
        let router = crate::build_router().route(
            &stylesheet_url,
            axum::routing::get(move || {
                let stylesheet = stylesheet.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "text/css")], stylesheet) }
            }),
        );
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

/// How many requests the page has made for a URL ending in `suffix`.
async fn resource_count(page: &chromiumoxide::Page, suffix: &str) -> usize {
    page.evaluate(format!(
        "performance.getEntriesByType('resource').filter(e => e.name.endsWith({suffix:?})).length"
    ))
    .await
    .expect("read resource timings")
    .into_value()
    .expect("a count")
}

/// Whether the page is asking for a reload (SME-43).
async fn banner_shown(page: &chromiumoxide::Page) -> bool {
    page.evaluate("!!document.querySelector('.stale-bundle-banner')")
        .await
        .expect("look for the reload banner")
        .into_value()
        .expect("a bool")
}

/// Waits until the page's WASM client has hydrated and is live. After
/// subscribing to the conversation's live events, the client pulls a
/// one-shot snapshot of each panel, `get_browsing_state` last; that
/// request having completed is the signal. (The event stream itself never
/// completes, so it never shows up in resource timings.) Until then the
/// server-rendered page accepts typing with no handlers attached, so
/// input is silently lost.
async fn wait_for_live_client(page: &chromiumoxide::Page, conversation_id: i64) {
    let last_pull = format!("/api/conversations/{conversation_id}/browsing");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let live: bool = page
            .evaluate(format!(
                "performance.getEntriesByType('resource').some(e => e.name.endsWith({last_pull:?}))"
            ))
            .await
            .expect("read resource timings")
            .into_value()
            .expect("a bool");
        if live {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page's client never finished loading ({last_pull} never completed)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
/// mid-stream. Returns its address; the test saves it as a provider every
/// conversation it creates uses (`MOCK_PROVIDER`).
async fn start_slow_mock_upstream() -> std::net::SocketAddr {
    fn event(name: &str, data: &str) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }
    let router = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
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
            let body = futures_util::StreamExt::then(futures_util::stream::iter(chunks), |chunk| async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, std::io::Error>(chunk)
            });
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
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

#[tokio::test]
#[ignore]
async fn test_end_to_end_browser_scenarios() {
    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("migrations should apply");
    sandbox::init().await;

    let harness = BrowserTestHarness::start().await;
    // The model every conversation the test creates runs on: a provider of
    // the test's own, so the dev database's providers and default are left
    // alone (SME-72). Removed with the conversations at the end.
    let mock_addr = start_slow_mock_upstream().await;
    let mock_provider = db::create_inference_provider(
        pool,
        &format!("browser tier mock {}", std::process::id()),
        "anthropic",
        &format!("http://{mock_addr}"),
        "api_key",
        "test-key",
    )
    .await
    .expect("save the mock provider");
    MOCK_PROVIDER.set(mock_provider.id).expect("the mock provider is set once");
    // Every conversation a scenario creates, so they (and their sandbox
    // pods) can be removed afterwards — this runs against the real dev
    // database and cluster, so leftovers show up in the app's own sidebar
    // and pile up pods until new ones stop starting.
    let created = std::sync::Mutex::new(Vec::new());

    // `catch_unwind` so a failing scenario still gets cleaned up after;
    // its panic is re-raised once that's done.
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(240), async {
        let conversation = new_conversation(pool, &created).await;

        // --- Scenario 1: cold-load panel population, one pod, two terminals
        // in it. A conversation has at most one live pod now (see
        // SME-11's "One pod per conversation"),
        // so there's no tab bar to click through — both terminals render
        // straight through as soon as the panel loads. ---
        sandbox::create_pod(pool, conversation.id, Default::default()).await.expect("create_pod");
        let terminal_a1 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a1)");
        let terminal_a2 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a2)");

        let page = harness.browser.new_page(&harness.base_url).await.expect("open the app");
        // Every scenario below assumes the page is styled; a layout check
        // on an unstyled page proves nothing (SME-40 F17: the stylesheet
        // was a 404 in this tier, so every page ran unstyled).
        let stylesheet_status: u16 = page
            .evaluate("fetch(document.querySelector('link[rel=stylesheet]').href).then(r => r.status)")
            .await
            .expect("fetch the stylesheet")
            .into_value()
            .expect("a status code");
        assert_eq!(stylesheet_status, 200, "the page's stylesheet doesn't load");
        // Freshly created conversation sorts first (most-recently-updated) —
        // clicking its sidebar entry, same as a real user, though a direct
        // `/conversation/{id}` URL would work too now that routing exists.
        click_when_present(&page, ".conversation-item", Duration::from_secs(10)).await;

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

        page.goto(&harness.base_url).await.expect("reload the app");
        click_when_present(&page, ".conversation-item", Duration::from_secs(10)).await;
        assert!(
            wait_for_text(&page, &format!("terminal {terminal_a1}"), Duration::from_secs(10)).await,
            "the fresh page load's snapshot pull should reconstruct the terminal"
        );
        assert!(
            wait_for_text(&page, "done_after_reload", Duration::from_secs(15)).await,
            "live updates should resume after the reload, not just the pre-reload snapshot"
        );

        // Best-effort teardown of what this test created.
        let _ = sandbox::terminate_terminal(pool, terminal_a1).await;
        let _ = sandbox::terminate_pod(pool, conversation.id).await;

        // --- Scenario 5: the always-visible context-usage indicator and
        // its click-through detail view — see
        // SME-18. A separate conversation,
        // seeded directly (db::create_message/upsert_conversation_usage)
        // rather than sent through the model — same "bypass the model,
        // verify the DOM" shape every scenario above already uses; a real
        // Anthropic call needs credentials this test environment (and CI)
        // doesn't have. ---
        let context_conversation = new_conversation(pool, &created).await;
        db::create_message(
            pool,
            context_conversation.id,
            "user",
            &[anthropic::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        )
        .await
        .expect("seed a message");
        db::upsert_conversation_usage(
            pool,
            context_conversation.id,
            &anthropic::TokenUsage {
                input_tokens: 40_000,
                output_tokens: 10_000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        )
        .await
        .expect("seed usage");

        let context_page = harness
            .browser
            .new_page(&format!(
                "{}conversation/{}",
                harness.base_url, context_conversation.id
            ))
            .await
            .expect("open the context-usage conversation");
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

        // --- Scenario 6: a compaction event renders as a distinct,
        // collapsed-by-default divider — not an ordinary chat bubble —
        // and expands to reveal the real summary text on click. Seeded
        // directly (a real trigger/summarization round trip is already
        // covered by api::chat's own mock-upstream integration test; this
        // tier's job is the DOM, not the backend logic). ---
        db::create_message(
            pool,
            context_conversation.id,
            "assistant",
            &[anthropic::ContentBlock::CompactionSummary {
                summary: "the user said hello; nothing else happened".to_string(),
                covers_through_message_id: 1,
            }],
        )
        .await
        .expect("seed a compaction summary message");

        context_page
            .goto(&format!(
                "{}conversation/{}",
                harness.base_url, context_conversation.id
            ))
            .await
            .expect("reload to see the newly seeded message");
        wait_for_live_client(&context_page, context_conversation.id).await;
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

        // --- Scenario 7: the todo panel — cold-load population from a
        // seeded list, then a live, full-replace update with no reload,
        // via the real todowrite tool (not a hand-built event) — see
        // SME-20. ---
        let todo_conversation = new_conversation(pool, &created).await;
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

        let todo_page = harness
            .browser
            .new_page(&format!(
                "{}conversation/{}",
                harness.base_url, todo_conversation.id
            ))
            .await
            .expect("open the todo conversation");
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

        // --- Scenario 8: a reply streaming into one conversation stays in
        // that conversation. Switching to another mid-stream must leave the
        // other one usable (not disabled while the first one's reply is in
        // flight) and must never show the first one's reply there; going
        // back shows the finished reply, once. The model is a slow mock
        // upstream, so the switch lands mid-stream. ---
        let streaming = new_conversation(pool, &created).await;
        let other = new_conversation(pool, &created).await;
        db::create_message(
            pool,
            other.id,
            "user",
            &[anthropic::ContentBlock::Text { text: "seeded message in B".to_string() }],
        )
        .await
        .expect("seed B so it has a clickable title");

        let chat = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, streaming.id))
            .await
            .expect("open conversation A");
        wait_for_live_client(&chat, streaming.id).await;
        let input = wait_for_element(&chat, CHAT_INPUT, Duration::from_secs(10)).await;
        input.focus().await.expect("focus the message box");
        input.type_str("hello from A").await.expect("type into A");
        input.press_key("Enter").await.expect("send in A");
        assert!(
            wait_for_text(&chat, "zebra0", Duration::from_secs(10)).await,
            "A's reply should start streaming"
        );

        click_conversation(&chat, other.id).await;
        assert!(
            wait_for_text(&chat, "seeded message in B", Duration::from_secs(5)).await,
            "should now be showing B"
        );
        for moment in ["right after switching", "a second later"] {
            let b = view_state(&chat).await;
            assert!(b.input_enabled, "B's message box is disabled {moment}: {b:?}");
            assert!(!b.streaming_bubble, "B shows a streaming bubble {moment}: {b:?}");
            assert!(!b.shows_reply, "B shows A's reply {moment}: {b:?}");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        // Let A's reply finish while B is still on screen.
        tokio::time::sleep(Duration::from_secs(5)).await;
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

        // --- Scenario 9: a reply the watching tab didn't ask for — a
        // background task's notification, another tab's send — still
        // reaches it live, with no reload. ---
        let watcher = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, other.id))
            .await
            .expect("open conversation B in a watching tab");
        wait_for_live_client(&watcher, other.id).await;
        crate::api::chat::run_turn(
            pool,
            other.id,
            anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "sent from somewhere else".to_string(),
                }],
            },
            None,
        )
        .await
        .expect("a turn run outside the watching tab should succeed");
        assert!(
            wait_for_text(&watcher, "zebra24", Duration::from_secs(10)).await,
            "the watching tab never showed a reply it didn't send itself"
        );

        // --- Scenario 10: a conversation that doesn't exist says so,
        // rather than offering a chat box whose send fails with a raw
        // database error. ---
        let missing = harness
            .browser
            .new_page(format!("{}conversation/987654321", harness.base_url))
            .await
            .expect("open a conversation that doesn't exist");
        assert!(
            wait_for_text(&missing, "doesn't exist", Duration::from_secs(10)).await,
            "a missing conversation should say it doesn't exist"
        );
        assert!(
            missing.find_element(CHAT_INPUT).await.is_err(),
            "a missing conversation shouldn't offer a message box"
        );

        // --- Scenario 11: a failed background notification belongs to its
        // conversation — switching away clears it. ---
        crate::events::publish(
            other.id,
            crate::events::ConversationEvent::NotificationDeliveryFailed {
                detail: "scenario 11 failure".to_string(),
            },
        );
        assert!(
            wait_for_text(&watcher, "scenario 11 failure", Duration::from_secs(10)).await,
            "the watching tab should show B's notification failure"
        );
        click_conversation(&watcher, streaming.id).await;
        assert!(
            wait_for_text(&watcher, "hello from A", Duration::from_secs(10)).await,
            "switching to A should show A's messages"
        );
        assert!(
            wait_for_text_gone(&watcher, "scenario 11 failure", Duration::from_secs(2)).await,
            "B's notification failure followed the tab to A"
        );

        // Every open smelt tab holds one always-open event stream, and over
        // plain HTTP/1.1 the browser allows only 6 connections per host
        // across all tabs: a 7th stream-holding tab never finishes loading.
        // Close the tabs the scenarios above are done with.
        for tab in [chat, watcher, missing] {
            tab.close().await.expect("close a finished tab");
        }

        // --- Scenario 12: pods. A pod created in one conversation shows up
        // as a dot in another tab's sidebar, live; the pods page lists it;
        // Stop there removes the row, and the dot goes away, live. ---
        let with_pod = new_conversation(pool, &created).await;
        db::create_message(
            pool,
            with_pod.id,
            "user",
            &[anthropic::ContentBlock::Text { text: "scenario 12".to_string() }],
        )
        .await
        .expect("title it");
        let sidebar_tab = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, other.id))
            .await
            .expect("open a conversation without a pod");
        wait_for_live_client(&sidebar_tab, other.id).await;
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

        let pods_page = harness
            .browser
            .new_page(format!("{}pods", harness.base_url))
            .await
            .expect("open the pods page");
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
        for tab in [sidebar_tab, pods_page, page, todo_page, context_page] {
            tab.close().await.expect("close a finished tab");
        }

        // --- Scenario 13: stopping a turn. The model (still the slow mock
        // from scenario 8) is mid-reply; Stop shows in the sending tab and
        // in a second tab watching the same conversation; stopping ends
        // the reply for good, says "Stopped.", and leaves the chat usable. ---
        let to_stop = new_conversation(pool, &created).await;
        let url = format!("{}conversation/{}", harness.base_url, to_stop.id);
        let sender = harness.browser.new_page(url.clone()).await.expect("open the sending tab");
        let observer = harness.browser.new_page(url).await.expect("open a watching tab");
        wait_for_live_client(&sender, to_stop.id).await;
        wait_for_live_client(&observer, to_stop.id).await;
        let input = wait_for_element(&sender, CHAT_INPUT, Duration::from_secs(10)).await;
        input.focus().await.expect("focus the message box");
        input.type_str("please stop me").await.expect("type");
        input.press_key("Enter").await.expect("send");
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
        // The mock would have finished its reply 5s after it started.
        tokio::time::sleep(Duration::from_secs(6)).await;
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
        for tab in [sender, observer] {
            tab.close().await.expect("close a finished tab");
        }

        // --- Scenario 14: replies travel on the conversation stream, so
        // every tab sees one live, a reload mid-reply keeps the text so
        // far, and the sender sees its own message once. And five tabs, a
        // streaming reply and a Stop from another tab fit under the
        // browser's 6-connections-per-host limit: a tab holds one
        // connection even while a reply streams, which leaves the sixth for
        // ordinary requests (a tab's own loading needs one too). When the
        // reply had its own stream, it took the sixth, and Stop queued. ---
        let shared = new_conversation(pool, &created).await;
        let url = format!("{}conversation/{}", harness.base_url, shared.id);
        let mut tabs = Vec::new();
        for _ in 0..5 {
            let tab = harness.browser.new_page(url.clone()).await.expect("open a tab");
            wait_for_live_client(&tab, shared.id).await;
            tabs.push(tab);
        }
        let input = wait_for_element(&tabs[0], CHAT_INPUT, Duration::from_secs(10)).await;
        input.focus().await.expect("focus the message box");
        input.type_str("stream to everyone").await.expect("type");
        input.press_key("Enter").await.expect("send");
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
        for tab in tabs {
            tab.close().await.expect("close a finished tab");
        }

        // --- Scenario 15: with a browsing session open, the chat stays
        // usable at a laptop width (this browser is 1400x900). The live
        // frame used to be a fixed 1280px that couldn't shrink, which
        // squeezed the messages and input to 48px and made the page
        // scroll sideways (SME-40 F2). ---
        let browsing = new_conversation(pool, &created).await;
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
        let page = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, browsing.id))
            .await
            .expect("open the browsing conversation");
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
        assert!(layout[0] >= 300.0, "the messages are too narrow to use: {layout:?}");
        assert!(layout[1] >= 150.0, "the message box is too narrow to use: {layout:?}");
        assert!(layout[2] <= 0.0, "the page scrolls sideways: {layout:?}");
        assert!(layout[3] <= layout[4], "the frame overflows its panel: {layout:?}");
        assert!(layout[5] <= layout[4], "the address bar overflows its panel: {layout:?}");
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
        assert!(panels[0] >= 400.0, "the live browser is too small to use: {panels:?}");
        assert!(panels[1] >= 400.0, "the sandbox panel is too narrow to read: {panels:?}");
        assert!(panels[2] >= 240.0, "the sandbox panel should be visible without scrolling: {panels:?}");
        crate::browsing::close_session(browsing.id).await.expect("close the browsing session");
        page.close().await.expect("close the browsing tab");

        // --- Scenario 16: every settings page is reachable from the
        // sidebar. Sandbox volumes had no link anywhere; the only way in
        // was typing the URL (SME-40 F7). ---
        let page = harness.browser.new_page(&harness.base_url).await.expect("open the app");
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
        let page = harness.browser.new_page(&harness.base_url).await.expect("open the app");
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
        page.close().await.expect("close the tab");

        // --- Scenario 17: a phone-width window. The sidebar kept its
        // 272px, the chat got about 100px and the page scrolled sideways
        // (SME-40 F8). With `mobile` on, a page without a viewport meta tag
        // lays out at 980px, so this also checks the tag is there. ---
        let phone_conversation = new_conversation(pool, &created).await;
        let page = harness.browser.new_page("about:blank").await.expect("open a tab");
        page.execute(
            chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
        )
        .await
        .expect("emulate a phone");
        page.goto(format!("{}conversation/{}", harness.base_url, phone_conversation.id))
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
        page.close().await.expect("close the tab");

        // --- Scenario 18: a URL that isn't a page says so, with a way
        // back. It used to show the router's raw "Failed to parse route"
        // dump (SME-40 F10). ---
        for path in ["nope", "conversation/abc"] {
            let page = harness.browser.new_page(format!("{}{path}", harness.base_url)).await.expect("open a bad URL");
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

        // --- Scenario 19: a tool call reads as one compact line that says
        // what it did, with its result folded in; a failed one is open. Each
        // call and each result used to be its own card of raw JSON, 24 of
        // them for a three-line answer (SME-41 D2). ---
        let tools = new_conversation(pool, &created).await;
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
        db::create_message(pool, tools.id, "assistant", &[
            call("toolu_ok", "run_terminal_command", serde_json::json!({"command": "ls /tmp", "terminal_id": 1})),
            call("toolu_bad", "read_file", serde_json::json!({"path": "/nope.txt"})),
        ]).await.expect("seed the calls");
        db::create_message(pool, tools.id, "user", &[
            result("toolu_ok", "command sent (id: abc)", false),
            result("toolu_bad", "No such file or directory", true),
        ]).await.expect("seed the results");
        let page = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, tools.id))
            .await
            .expect("open the conversation");
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
        page.close().await.expect("close the tab");

        // --- Scenario 20: dark mode follows the system setting. There was
        // none: a dark-mode system got a bright white page (SME-41 D5). ---
        let page = harness.browser.new_page("about:blank").await.expect("open a tab");
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
        page.goto(format!("{}conversation/{}", harness.base_url, tools.id)).await.expect("open the conversation");
        wait_for_live_client(&page, tools.id).await;
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
        page.close().await.expect("close the tab");

        // --- Scenario 21: one primary action per form, and intro text
        // lines up with its heading. Every button in the MCP form was
        // solid black, "Remove" included, and intro text sat 24px in from
        // the heading (SME-41 D6). ---
        let page = harness.browser.new_page(format!("{}mcp-servers", harness.base_url)).await.expect("open MCP servers");
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
        page.goto(format!("{}mcp-servers/new", harness.base_url)).await.expect("open the new-server form");
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
        page.close().await.expect("close the tab");

        // --- Scenario 22: a new conversation says what smelt can do and
        // offers example asks; one fills the message box without sending.
        // It was a blank screen (SME-41 D12). ---
        let empty = new_conversation(pool, &created).await;
        let page = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, empty.id))
            .await
            .expect("open the empty conversation");
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
        page.close().await.expect("close the tab");

        // --- Scenario 23 (SME-42): a dev server in the sandbox, end to end.
        // It's bound to 127.0.0.1 inside the pod, as dev servers are by
        // default. The model's own browser loads it at localhost; the model
        // shares a preview; the link shows up in the sandbox panel live,
        // opens the same server in a tab of its own, and survives a reload. ---
        let serving = new_conversation(pool, &created).await;
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
        let page = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, serving.id))
            .await
            .expect("open the serving conversation");
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
        let preview_tab = harness.browser.new_page(link.clone()).await.expect("open the preview");
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
        crate::browsing::close_session(serving.id).await.expect("close the browsing session");
        let seen = seen.expect("the model's browser should load the container's server");
        assert!(seen.text.contains("Hello from a Docker container"), "got {:?}", seen.text);

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
        let preview_tab = harness.browser.new_page(link).await.expect("open the container's preview");
        assert!(
            wait_for_text(&preview_tab, "Hello from a Docker container", Duration::from_secs(10)).await,
            "the container's preview link should show its server"
        );
        preview_tab.close().await.expect("close the preview tab");

        // --- A repo's AGENTS.md waits for the user's trust (SME-32): when
        // the model asks to load one from an unknown remote, the chat shows
        // the file with Trust / Don't trust, and trusting loads exactly
        // that file. The model the decision wakes is the mock upstream. ---
        let trusting = new_conversation(pool, &created).await;
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
        let trust_page = harness
            .browser
            .new_page(&format!("{}conversation/{}", harness.base_url, trusting.id))
            .await
            .expect("open the trust conversation");
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
        trust_page.close().await.expect("close the trust tab");

        // --- SME-51 B11: switching conversations resets what belonged to
        // the one left: its context detail view doesn't stay open over the
        // next conversation. ---
        let leaving = new_conversation(pool, &created).await;
        let arriving = new_conversation(pool, &created).await;
        db::create_message(pool, leaving.id, "user", &[anthropic::ContentBlock::Text { text: "hello".to_string() }])
            .await
            .expect("seed a message");
        db::upsert_conversation_usage(
            pool,
            leaving.id,
            &anthropic::TokenUsage { input_tokens: 1_000, output_tokens: 100, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        )
        .await
        .expect("seed usage");
        let switching = harness
            .browser
            .new_page(&format!("{}conversation/{}", harness.base_url, leaving.id))
            .await
            .expect("open the first conversation");
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
        switching.close().await.expect("close the switching tab");

        // --- Scenario 24 (SME-72): the model picker above the message box
        // says which model the conversation runs on, and choosing another
        // in one tab shows in every tab on it, with no reload. ---
        let picking = new_conversation(pool, &created).await;
        let provider_name = db::get_inference_provider(pool, *MOCK_PROVIDER.get().expect("the mock provider"))
            .await
            .expect("read the mock provider")
            .expect("the mock provider exists")
            .name;
        let first_tab = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, picking.id))
            .await
            .expect("open the picker's conversation");
        let second_tab = harness
            .browser
            .new_page(format!("{}conversation/{}", harness.base_url, picking.id))
            .await
            .expect("open it in a second tab");
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
        first_tab.close().await.expect("close the first picker tab");
        second_tab.close().await.expect("close the second picker tab");

        // --- Scenario 25 (SME-76): an OAuth server takes extra headers
        // too, such as GitHub's `X-MCP-Toolsets` (which turns on the tools
        // that read CI logs). The edit page only showed its header editor
        // to static-header servers. ---
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
        let edit = harness
            .browser
            .new_page(format!("{}mcp-servers/{}", harness.base_url, oauth_server.id))
            .await
            .expect("open the OAuth server's edit page");
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
        assert_eq!(
            saved.extra_headers.0.get("X-MCP-Toolsets").map(String::as_str),
            Some("repos,actions"),
            "the header should be saved: {:?}",
            saved.extra_headers.0.keys().collect::<Vec<_>>()
        );
        assert_eq!(saved.auth_mode, "oauth", "saving headers keeps the server on OAuth");
        edit.close().await.expect("close the edit page");
        db::delete_mcp_server_config(pool, oauth_server.id).await.expect("delete the OAuth server");

        // --- Scenario 26 (SME-43): a tab running an older bundle than the
        // server. (a) An event type the bundle doesn't know is skipped: the
        // stream stays up (one snapshot pull, not a second after a
        // reconnect), and the next event still arrives live. It also proves
        // the server is newer, so the tab asks for a reload. ---
        let stale = new_conversation(pool, &created).await;
        let stale_url = format!("{}conversation/{}", harness.base_url, stale.id);
        let snapshot_pull = format!("/api/conversations/{}/browsing", stale.id);
        let stale_tab = harness.browser.new_page(stale_url.as_str()).await.expect("open the stale tab");
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
            resource_count(&stale_tab, &snapshot_pull).await,
            1,
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
        let tab = harness.browser.new_page(stale_url.as_str()).await.expect("open a tab");
        wait_for_live_client(&tab, stale.id).await;
        assert!(!banner_shown(&tab).await, "no reload banner before the server changes");
        crate::api::version::test_override::set(Some("a-newer-build"));
        crate::events::forget(stale.id);
        assert!(
            wait_for_text(&tab, "smelt was updated", Duration::from_secs(10)).await,
            "a reconnect to a newer server should ask for a reload"
        );
        let pods_tab = harness
            .browser
            .new_page(format!("{}pods", harness.base_url))
            .await
            .expect("open the Sandboxes page");
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
        tab.close().await.expect("close the tab");
        page.close().await.expect("close the tab");
    })))
    .await;

    // Cleanup and the leftover check both run before `harness.shutdown()`.
    // The server's handlers run on dioxus's own worker runtimes, and
    // database and cluster connections they opened stay tied to those
    // runtimes. Once the harness shuts down, using one either fails ("A
    // Tokio 1.x context was found, but it is being shutdown", "runtime
    // dropped the dispatch task") or hangs forever (SME-39).
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
    match outcome {
        Err(panic) => std::panic::resume_unwind(panic),
        Ok(timed) => timed.expect("browser test should complete within the timeout, not hang"),
    }
    assert!(leftovers.is_empty(), "the test left things behind: {leftovers:?}");
}

/// The test's own provider (the slow mock), which every conversation it
/// creates runs on.
static MOCK_PROVIDER: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// The model `MOCK_PROVIDER` serves (any name does: the mock ignores it).
const MOCK_MODEL: &str = "mock-model";

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
