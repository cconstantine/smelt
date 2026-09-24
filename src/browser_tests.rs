//! The one comprehensive browser test tier — started for `sandbox-visibility`
//! (see `docs/projects/completed/20260815-sandbox-visibility.md`), extended for
//! `auto-compaction`'s context-usage indicator/detail view and compaction
//! divider (see `docs/projects/plans/auto-compaction.md`) — per
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

        let router = crate::build_router();
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

    /// Best-effort, called explicitly at the end of the test rather than via
    /// `Drop` (which can't `.await`) — same "explicit cleanup after the
    /// test body, not guaranteed on a panic" shape
    /// `test_terminal_lifecycle_end_to_end`'s own cleanup already accepts.
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
/// id, not title: titles repeat across runs against the same database, and
/// the sidebar doesn't refresh a title after the page loads.
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

    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let conversation = db::create_conversation(pool).await.expect("create conversation");

        // --- Scenario 1: cold-load panel population, one pod, two terminals
        // in it. A conversation has at most one live pod now (see
        // docs/projects/plans/file-tools.md's "One pod per conversation"),
        // so there's no tab bar to click through — both terminals render
        // straight through as soon as the panel loads. ---
        sandbox::create_pod(pool, conversation.id, None, None).await.expect("create_pod");
        let terminal_a1 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a1)");
        let terminal_a2 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a2)");

        let page = harness.browser.new_page(&harness.base_url).await.expect("open the app");
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
        // docs/projects/plans/auto-compaction.md. A separate conversation,
        // seeded directly (db::create_message/upsert_conversation_usage)
        // rather than sent through the model — same "bypass the model,
        // verify the DOM" shape every scenario above already uses; a real
        // Anthropic call needs credentials this test environment (and CI)
        // doesn't have. ---
        let context_conversation = db::create_conversation(pool)
            .await
            .expect("create context-usage conversation");
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
        // docs/projects/plans/todo-list-tool.md. ---
        let todo_conversation = db::create_conversation(pool)
            .await
            .expect("create todo conversation");
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
        let streaming = db::create_conversation(pool).await.expect("create conversation A");
        let other = db::create_conversation(pool).await.expect("create conversation B");
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
    })
    .await;

    harness.shutdown().await;
    outcome.expect("browser test should complete within the timeout, not hang");
}
