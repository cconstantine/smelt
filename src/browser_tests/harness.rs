use super::*;

pub(super) struct BrowserTestHarness {
    pub(super) browser: Browser,
    server_task: tokio::task::JoinHandle<()>,
    preview_task: tokio::task::JoinHandle<()>,
    pub(super) base_url: String,
}

impl BrowserTestHarness {
    pub(super) async fn start() -> Self {
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
                (asset!("/assets/css/01-base.css").to_string(), "assets/css/01-base.css"),
                (asset!("/assets/css/02-transcript.css").to_string(), "assets/css/02-transcript.css"),
                (asset!("/assets/css/03-panels.css").to_string(), "assets/css/03-panels.css"),
                (asset!("/assets/css/04-phone-and-composer.css").to_string(), "assets/css/04-phone-and-composer.css"),
                (asset!("/assets/css/05-mcp.css").to_string(), "assets/css/05-mcp.css"),
                (asset!("/assets/css/06-volumes-and-context.css").to_string(), "assets/css/06-volumes-and-context.css"),
                (asset!("/assets/css/07-pages.css").to_string(), "assets/css/07-pages.css"),
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

        let browser = launch_browser().await;

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
    /// A Chrome of its own for each further scenario lane (SME-341's speed
    /// pass): tabs in different lanes never share a renderer, so one lane's
    /// busy page can't throttle another's timers or frames.
    pub(super) async fn another_browser(&self) -> Browser {
        launch_browser().await
    }

    pub(super) async fn shutdown(mut self) {
        let _ = self.browser.close().await;

        self.server_task.abort();
        self.preview_task.abort();
    }
}

/// Same launcher as the app's shared browser, so this Chrome can't outlive
/// the test process either. `smelt-http.test` reaches the tier's server over
/// plain HTTP from an origin that isn't a secure context, as a LAN address
/// does (SME-105's copy fallback).
async fn launch_browser() -> Browser {
    crate::headless_chrome::launch(&[format!("--host-resolver-rules=MAP {INSECURE_HOST} 127.0.0.1")])
        .await
        .expect("chrome-headless-shell should launch")
}

/// Polls `document.body.innerText` for `needle` up to `timeout` — the same
/// bounded-retry shape `poll_until_finished` already uses in
/// `sandbox.rs`'s own integration test, applied to DOM content instead of a
/// DB row.
pub(super) async fn wait_for_text(page: &chromiumoxide::Page, needle: &str, timeout: Duration) -> bool {
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
pub(super) fn unique_id(label: &str) -> String {
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
pub(super) async fn click_when_present(page: &chromiumoxide::Page, selector: &str, timeout: Duration) {
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
pub(super) async fn wait_for_stable(page: &chromiumoxide::Page, selector: &str, deadline: tokio::time::Instant) -> String {
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

pub(super) const CHAT_INPUT: &str = "input[placeholder=\"Type a message...\"]";

/// What the chat view shows, for scenario 8.
#[derive(Debug, serde::Deserialize)]
pub(super) struct ViewState {
    pub(super) input_enabled: bool,
    pub(super) streaming_bubble: bool,
    pub(super) shows_reply: bool,
}

pub(super) async fn view_state(page: &chromiumoxide::Page) -> ViewState {
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

pub(super) async fn wait_for_element(
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
pub(super) async fn element_box(page: &chromiumoxide::Page, selector: &str) -> (i64, i64, i64, i64) {
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
pub(super) async fn wait_for_count(page: &chromiumoxide::Page, selector: &str, count: usize, timeout: Duration) -> bool {
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
pub(super) async fn wait_for_resource(page: &chromiumoxide::Page, suffix: &str) {
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
pub(super) async fn banner_shown(page: &chromiumoxide::Page) -> bool {
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
pub(super) async fn wait_for_live_client(page: &chromiumoxide::Page, conversation_id: i64) {
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
pub(super) async fn live_pulls(page: &chromiumoxide::Page, conversation_id: i64) -> Option<u32> {
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
pub(super) async fn click_conversation(page: &chromiumoxide::Page, id: i64) {
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

pub(super) async fn wait_for_text_gone(page: &chromiumoxide::Page, needle: &str, timeout: Duration) -> bool {
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

/// The test's own provider (the slow mock), which every conversation it
/// creates runs on.
pub(super) static MOCK_PROVIDER: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// The model lane 0's conversations run on. Each lane of the mock (see
/// `MockUpstream`) answers its own model name, and lane 0 answers this one.
pub(super) const MOCK_MODEL: &str = "mock-model";

/// How many times `page` has fetched the conversation list (the tree, since
/// SME-246).
pub(super) async fn list_requests(page: &chromiumoxide::Page) -> usize {
    page.evaluate("performance.getEntriesByType('resource').filter(e => e.name.endsWith('/api/conversations/tree')).length")
        .await
        .expect("read resource timings")
        .into_value()
        .expect("a count")
}

pub(super) async fn new_conversation(
    pool: &sqlx::PgPool,
    created: &std::sync::Mutex<Vec<i64>>,
    model: &str,
) -> crate::models::Conversation {
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    created.lock().expect("the conversation list lock").push(conversation.id);
    let provider = *MOCK_PROVIDER.get().expect("the mock provider is saved first");
    db::set_conversation_model(pool, conversation.id, provider, model)
        .await
        .expect("put the conversation on the mock provider");
    conversation
}

/// Ids of every sandbox pod belonging to `conversations`, read before
/// they're removed (the rows go with the conversation).
pub(super) async fn sandbox_pod_ids(pool: &sqlx::PgPool, conversations: &[i64]) -> Vec<i64> {
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
pub(super) async fn remove_conversations(pool: &sqlx::PgPool, conversations: &[i64]) {
    for &conversation in conversations {
        sandbox::teardown_conversation(pool, conversation, &[]).await;
        if let Err(e) = db::delete_conversation(pool, conversation).await {
            eprintln!("failed to delete test conversation {conversation}: {e}");
        }
    }
}

/// What the test left behind: conversations still in the database and
/// sandbox pods still in the cluster, described for the failure message.
pub(super) async fn find_leftovers(
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

/// Ends a command's pauses early: a scenario's command that has to keep
/// running (or keep trickling output) while the scenario checks something
/// skips its `sleep`s once `/home/sandbox/.stop-<name>` exists, and the
/// scenario calls this once its checks are done. The command still ends the
/// way it would have, without the scenario waiting out a fixed timer
/// (SME-341's speed pass).
pub(super) async fn stop_trickling(t: &Scenario<'_>, conversation_id: i64, name: &str) {
    anthropic::tools::execute(
        t.pool,
        conversation_id,
        &unique_id("stop-trickle"),
        "write_file",
        &serde_json::json!({"path": format!("/home/sandbox/.stop-{name}"), "content": ""}),
    )
    .await
    .expect("write the burst's stop file");
}
