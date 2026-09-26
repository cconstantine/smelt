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
        // while first running this test, not anticipated in the plan.
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
            base_url: format!("http://127.0.0.1:{port}/"),
        }
    }

    /// Called explicitly at the end of the test rather than via `Drop`
    /// (which can't `.await`); the test runs its scenarios under
    /// `catch_unwind`, so this still runs when one fails.
    async fn shutdown(mut self) {
        let _ = self.browser.close().await;

        self.server_task.abort();
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

/// Polls for `selector` to exist and clicks it — `find_element` doesn't
/// itself wait/retry, and the sidebar's conversation list only appears once
/// `get_conversations` resolves after hydration.
async fn click_when_present(page: &chromiumoxide::Page, selector: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(element) = page.find_element(selector).await {
            if element.click().await.is_ok() {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{selector} never appeared"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
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
/// mid-stream. Points `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY` at it; hold
/// `anthropic::test_support::lock_anthropic_base_url` while it's in use.
async fn start_slow_mock_upstream() {
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
    // SAFETY: callers hold the process-wide ANTHROPIC_BASE_URL lock, which
    // is what every test touching these variables coordinates on.
    unsafe {
        std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
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
    // Every conversation a scenario creates, so they (and their sandbox
    // pods) can be removed afterwards — this runs against the real dev
    // database and cluster, so leftovers show up in the app's own sidebar
    // and pile up pods until new ones stop starting.
    let created = std::sync::Mutex::new(Vec::new());

    // `catch_unwind` so a failing scenario still gets cleaned up after;
    // its panic is re-raised once that's done.
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(180), async {
        let conversation = new_conversation(pool, &created).await;

        // --- Scenario 1: cold-load panel population, one pod, two terminals
        // in it. A conversation has at most one live pod now (see
        // SME-11's "One pod per conversation"),
        // so there's no tab bar to click through — both terminals render
        // straight through as soon as the panel loads. ---
        sandbox::create_pod(pool, conversation.id, None, None).await.expect("create_pod");
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
        let _anthropic = anthropic::test_support::lock_anthropic_base_url();
        start_slow_mock_upstream().await;
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
        let pod_id = sandbox::create_pod(pool, with_pod.id, None, None).await.expect("create_pod");
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
        let before = (element_box(&pods_page, &stop).await, element_box(&pods_page, &neighbour).await);
        wait_for_element(&pods_page, &stop, Duration::from_secs(5)).await.click().await.expect("arm stop");
        let confirm = format!("{row} .pod-stop.confirm");
        wait_for_element(&pods_page, &confirm, Duration::from_secs(5)).await;
        let after = (element_box(&pods_page, &confirm).await, element_box(&pods_page, &neighbour).await);
        assert_eq!(
            before, after,
            "arming Stop must not move or resize the button or its neighbours (button, cell to its left)"
        );
        wait_for_element(&pods_page, &confirm, Duration::from_secs(5))
            .await
            .click()
            .await
            .expect("confirm stop");
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
        wait_for_element(&sender, ".stop-turn", Duration::from_secs(5))
            .await
            .click()
            .await
            .expect("click Stop");
        assert!(
            wait_for_text(&sender, "Stopped.", Duration::from_secs(5)).await,
            "stopping should say so"
        );
        assert!(
            wait_for_count(&sender, ".stop-turn", 0, Duration::from_secs(5)).await
                && wait_for_count(&observer, ".stop-turn", 0, Duration::from_secs(5)).await,
            "Stop should go away in every tab once the turn has ended"
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
        wait_for_element(&tabs[4], ".stop-turn", Duration::from_secs(5))
            .await
            .click()
            .await
            .expect("click Stop in another tab");
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
        crate::browsing::open_session(browsing.id).await.expect("open a browsing session");
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
                 w('.browsing-panel-frame-wrap'), w('.browsing-panel')]; })()",
            )
            .await
            .expect("measure the layout")
            .into_value()
            .expect("numbers");
        assert!(layout[0] >= 300.0, "the messages are too narrow to use: {layout:?}");
        assert!(layout[1] >= 150.0, "the message box is too narrow to use: {layout:?}");
        assert!(layout[2] <= 0.0, "the page scrolls sideways: {layout:?}");
        assert!(layout[3] <= layout[4], "the frame overflows its panel: {layout:?}");
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
        wait_for_element(&page, delete, Duration::from_secs(10)).await;
        let before = element_box(&page, delete).await;
        page.find_element(delete).await.expect("find Delete").click().await.expect("arm Delete");
        wait_for_element(&page, ".conversation-item .delete-conversation.confirm", Duration::from_secs(5)).await;
        let after = element_box(&page, ".conversation-item .delete-conversation.confirm").await;
        assert_eq!(before.2, after.2, "arming the sidebar's Delete changed its width");
        page.close().await.expect("close the tab");
        let page = harness.browser.new_page(&harness.base_url).await.expect("open the app");
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
    let leftovers = find_leftovers(pool, &created, &pod_ids).await;
    harness.shutdown().await;
    match outcome {
        Err(panic) => std::panic::resume_unwind(panic),
        Ok(timed) => timed.expect("browser test should complete within the timeout, not hang"),
    }
    assert!(leftovers.is_empty(), "the test left things behind: {leftovers:?}");
}

async fn new_conversation(
    pool: &sqlx::PgPool,
    created: &std::sync::Mutex<Vec<i64>>,
) -> crate::models::Conversation {
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    created.lock().expect("the conversation list lock").push(conversation.id);
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
        sandbox::teardown_conversation(pool, conversation).await;
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
