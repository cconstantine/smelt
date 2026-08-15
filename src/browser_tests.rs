//! The one comprehensive browser test for `sandbox-visibility` — see
//! `docs/projects/completed/20260815-sandbox-visibility.md`. Runs the real app in-process
//! (no `lib.rs` exists, so an external `tests/` integration test couldn't
//! reach `db`/`sandbox`/`anthropic::tools` at all — see the plan's "How")
//! against a real headless `chrome-headless-shell`, driven over CDP via
//! `chromiumoxide` (no `chromedriver` to download/manage). `#[ignore]`d by
//! default: needs `scripts/browser-check/setup.sh` run first, and a real
//! Postgres + k3s cluster reachable the same way every other real-cluster
//! test in this codebase already assumes.
//!
//! Deliberately one test, not several: every scenario in the plan's
//! `Verification` checklist runs sequentially inside it, sharing one
//! browser/server/`MANAGER` instance for its whole duration — more than one
//! `#[tokio::test]` here touching `sandbox::init()` would risk the same
//! `OnceLock`-across-separate-runtimes hazard `docs/testing.md` documents
//! for `PgPool`, the same reasoning `sandbox-terminal`'s own real-cluster
//! test already applied.

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures_util::StreamExt;

use crate::{anthropic, db, sandbox};

const CHROME_BINARY: &str =
    ".browser-check-cache/chrome/chrome-headless-shell-linux64/chrome-headless-shell";
const LIB_DIR: &str = ".browser-check-cache/libs/usr/lib/x86_64-linux-gnu";

struct BrowserTestHarness {
    browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
    base_url: String,
}

impl BrowserTestHarness {
    async fn start() -> Self {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let chrome_binary = repo_root.join(CHROME_BINARY);
        if !chrome_binary.is_file() {
            panic!(
                "chrome-headless-shell not found at {} — run scripts/browser-check/setup.sh first",
                chrome_binary.display()
            );
        }
        let lib_dir = repo_root.join(LIB_DIR);
        // chrome-headless-shell needs its bundled shared libraries (nss,
        // atk, dbus, X11, mesa, ...) — not installed system-wide, same
        // requirement scripts/browser-check/browser_check.py already has.
        // chromiumoxide's builder has no direct env-var hook, so this sets
        // it for the whole test process; the child process it spawns
        // inherits it.
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        // SAFETY: this test binary is single-threaded at this point (no
        // other threads have been spawned yet that could race a concurrent
        // std::env read) — see the harness's own doc comment on why this is
        // the one test in this module.
        unsafe {
            std::env::set_var("LD_LIBRARY_PATH", format!("{}:{}/dri:{existing}", lib_dir.display(), lib_dir.display()));
        }

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
        // SAFETY: same single-threaded-at-startup reasoning as the
        // LD_LIBRARY_PATH set above.
        unsafe {
            std::env::set_var("DIOXUS_PUBLIC_PATH", &public_path);
        }

        let router = crate::build_router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind a test-local port");
        let port = listener.local_addr().expect("listener should have a local address").port();
        let server_task = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("test server error");
        });

        let config = BrowserConfig::builder()
            .chrome_executable(&chrome_binary)
            .no_sandbox()
            .arg("--disable-gpu")
            .window_size(1400, 900)
            .build()
            .expect("valid chrome-headless-shell launch config");
        let (browser, mut handler) = Browser::launch(config).await.expect("chrome-headless-shell should launch");
        let handler_task = tokio::spawn(async move {
            while handler.next().await.is_some() {}
        });

        Self { browser, handler_task, server_task, base_url: format!("http://127.0.0.1:{port}/") }
    }

    /// Best-effort, called explicitly at the end of the test rather than via
    /// `Drop` (which can't `.await`) — same "explicit cleanup after the
    /// test body, not guaranteed on a panic" shape
    /// `test_terminal_lifecycle_end_to_end`'s own cleanup already accepts.
    async fn shutdown(mut self) {
        let _ = self.browser.close().await;
        let _ = self.browser.wait().await;
        self.handler_task.abort();
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
        assert!(tokio::time::Instant::now() < deadline, "{selector} never appeared");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Clicks the first element matching `selector` whose text contains
/// `text` — for the sandbox panel's pod tabs, which share one class
/// (`.sandbox-pod-tab`) and are only distinguished by their label, so a
/// plain CSS selector (what `click_when_present` uses) can't target one
/// specifically. JS-`click()`ed via `evaluate` rather than chromiumoxide's
/// own element click, same bounded-retry shape as `click_when_present`.
async fn click_containing_text(page: &chromiumoxide::Page, selector: &str, text: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    let js = format!(
        "(() => {{ const el = [...document.querySelectorAll({selector:?})].find(e => e.textContent.includes({text:?})); if (el) {{ el.click(); return true; }} return false; }})()"
    );
    loop {
        if let Ok(result) = page.evaluate(js.as_str()).await {
            if let Ok(true) = result.into_value::<bool>() {
                return;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "no {selector} containing {text:?} ever appeared");
        tokio::time::sleep(Duration::from_millis(200)).await;
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
async fn test_sandbox_panel_reflects_live_state_end_to_end() {
    let pool = db::init().await;
    sqlx::migrate!().run(pool).await.expect("migrations should apply");
    sandbox::init().await;

    let harness = BrowserTestHarness::start().await;

    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let conversation = db::create_conversation(pool).await.expect("create conversation");

        // --- Scenario 1: cold-load panel population, two pods, two terminals in one ---
        let pod_a = sandbox::create_pod(pool, conversation.id).await.expect("create_pod (a)");
        let pod_b = sandbox::create_pod(pool, conversation.id).await.expect("create_pod (b)");
        let terminal_a1 = sandbox::create_terminal(pool, pod_a).await.expect("create_terminal (a1)");
        let terminal_a2 = sandbox::create_terminal(pool, pod_a).await.expect("create_terminal (a2)");
        let terminal_b1 = sandbox::create_terminal(pool, pod_b).await.expect("create_terminal (b1)");

        let page = harness.browser.new_page(&harness.base_url).await.expect("open the app");
        // Freshly created conversation sorts first (most-recently-updated) —
        // clicking its sidebar entry, same as a real user, though a direct
        // `/conversation/{id}` URL would work too now that routing exists.
        click_when_present(&page, ".conversation-item", Duration::from_secs(10)).await;

        // Two or more pods means a tab bar (see .sandbox-pod-tabs) — only
        // the active tab's terminals render at once, defaulting to the
        // first pod. Both tab labels should be visible regardless; only
        // pod_a's own terminals should be, until pod_b's tab is clicked.
        for text in [format!("pod {pod_a}"), format!("pod {pod_b}"), format!("terminal {terminal_a1}"), format!("terminal {terminal_a2}")]
        {
            assert!(
                wait_for_text(&page, &text, Duration::from_secs(10)).await,
                "cold snapshot should render {text}"
            );
        }

        click_containing_text(&page, ".sandbox-pod-tab", &format!("pod {pod_b}"), Duration::from_secs(10)).await;
        assert!(
            wait_for_text(&page, &format!("terminal {terminal_b1}"), Duration::from_secs(10)).await,
            "clicking pod_b's tab should reveal its terminal"
        );

        // Scenario 2 onward only touches pod_a's terminals — switch back to
        // its tab so their output is actually in the DOM to assert on.
        click_containing_text(&page, ".sandbox-pod-tab", &format!("pod {pod_a}"), Duration::from_secs(10)).await;
        assert!(
            wait_for_text(&page, &format!("terminal {terminal_a1}"), Duration::from_secs(10)).await,
            "switching back to pod_a's tab should show its terminals again"
        );

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
        let _ = sandbox::terminate_terminal(pool, terminal_b1).await;
        let _ = sandbox::terminate_pod(pool, pod_a).await;
        let _ = sandbox::terminate_pod(pool, pod_b).await;
    })
    .await;

    harness.shutdown().await;
    outcome.expect("browser test should complete within the timeout, not hang");
}
