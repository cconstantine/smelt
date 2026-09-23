//! Interactive web browsing: a persistent browsing *session* per
//! conversation — open a page, read it, click/fill/navigate across
//! several tool calls, instead of `webfetch`'s fresh-page-per-call shape.
//! Plus a live panel: the user can watch and interact with the same real
//! page the model is browsing. See docs/projects/plans/web-browsing.md.
//!
//! `PageElement`/`PageState`/`BrowserFrame`/`BrowserInputEvent` are
//! ungated — they cross the client/server boundary as server-function
//! payloads (`api::browsing`'s frame stream and input endpoint), so the
//! `web` build needs them too. Everything else (the actual
//! `chromiumoxide`-driven session logic) lives in the `server`-only
//! nested module, re-exported — same shape `anthropic::tools` already
//! uses for the same reason.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageElement {
    pub index: usize,
    pub tag: String,
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PageState {
    pub url: String,
    pub text: String,
    pub truncated: bool,
    pub elements: Vec<PageElement>,
}

/// One live-panel frame — a base64-encoded JPEG straight off the wire
/// (CDP's own `screencastFrame.data` is already base64; `chromiumoxide`'s
/// `Binary` type is a thin wrapper around that same string, not
/// separately decoded — confirmed against the vendored
/// `chromiumoxide_types` source — so no decode/re-encode round trip is
/// needed, just embed it directly as a `data:image/jpeg;base64,...` URI
/// on the frontend).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserFrame {
    pub data: String,
}

/// A live-panel input event, forwarded from the user's own browser to the
/// real session page. Mouse events carry coordinates in the *frame's own*
/// pixel space (the frontend translates an on-screen click position back
/// to frame pixels before sending) and dispatch as real
/// `Input.dispatchMouseEvent` commands. `TypeText` uses `Input.insertText`
/// (simple, but bypasses `keydown`/`keyup` listeners — acceptable for
/// ordinary typing) rather than simulating a `dispatchKeyEvent` per
/// character, which would need a full keycode table for arbitrary
/// characters; `PressKey` is for the small set of *named* keys
/// (Enter, Backspace, ...) a page's `keydown` handler might actually care
/// about, which does need a real `dispatchKeyEvent` — see
/// `server::named_key_event_fields`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum BrowserInputEvent {
    MouseMove { x: f64, y: f64 },
    MouseDown { x: f64, y: f64 },
    MouseUp { x: f64, y: f64 },
    Wheel { x: f64, y: f64, delta_x: f64, delta_y: f64 },
    TypeText { text: String },
    PressKey { key: String },
}

#[cfg(feature = "server")]
mod server {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;

    use chromiumoxide::Page;
    use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
        DispatchMouseEventType, InsertTextParams,
    };
    use chromiumoxide::cdp::browser_protocol::page::{
        EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat,
        StartScreencastParams, StopScreencastParams,
    };
    use futures_util::StreamExt;
    use tokio::sync::broadcast;

    use super::{BrowserFrame, BrowserInputEvent, PageElement, PageState};
    use crate::fetch_guard;

    /// Bounds page navigation+load, same reasoning `webfetch::NAV_TIMEOUT`
    /// already established — "bound the boundaries" (development-process.md).
    const NAV_TIMEOUT: Duration = Duration::from_secs(20);
    /// Same truncation cap `webfetch`/`http_request` already use.
    const MAX_TEXT_CHARS: usize = 20_000;
    /// Upper bound on how long `click`/`fill` wait to "settle" before
    /// extracting the resulting page state — see `settle_after_action`'s
    /// own doc comment for why a click has no built-in "wait until done"
    /// signal the way `goto` does. This is a real tax paid on *every*
    /// action that doesn't navigate (the common case — a toggle, a tab
    /// switch, an in-place form update), so it's deliberately short; a
    /// navigating click still resolves as soon as `wait_for_navigation`
    /// actually fires, regardless of this bound. Value chosen empirically
    /// against real pages during implementation, not guessed — see the
    /// plan's "Riskiest assumptions."
    const ACTION_SETTLE_TIMEOUT: Duration = Duration::from_millis(600);
    /// Screencast frame bounds — deliberately conservative (bandwidth over
    /// smoothness), tunable once the panel is actually running against a
    /// real page; see the plan's "Exact frame quality/size/rate defaults."
    const SCREENCAST_MAX_WIDTH: i64 = 1280;
    const SCREENCAST_MAX_HEIGHT: i64 = 800;
    const SCREENCAST_QUALITY: i64 = 60;
    /// Small on purpose — frames are ephemeral, "latest wins" data; a slow
    /// subscriber should drop old frames rather than backing up a large
    /// buffer of stale ones.
    const FRAME_CHANNEL_CAPACITY: usize = 4;

    /// Tags every interactive element it finds with `data-smelt-el="N"`
    /// (so `click`/`fill` can reference one precisely by re-querying that
    /// attribute, rather than trusting index-into-a-fresh-query-result
    /// order to stay stable) and returns a JSON-stringified array
    /// describing them — stringified deliberately, not returned as a
    /// plain JS array, so the actual parsing (`parse_elements`) happens
    /// in Rust and is unit-testable on its own, the same "split pure
    /// logic from browser glue" shape this codebase already uses
    /// elsewhere.
    const ELEMENT_EXTRACTION_SCRIPT: &str = r#"
(() => {
    const interactive = document.querySelectorAll(
        'a, button, input, select, textarea, [role="button"], [onclick]'
    );
    const results = [];
    interactive.forEach((el, i) => {
        el.setAttribute('data-smelt-el', String(i));
        const tag = el.tagName.toLowerCase();
        let kind = tag;
        if (tag === 'a') kind = 'link';
        else if (tag === 'button' || el.getAttribute('role') === 'button') kind = 'button';
        const label = (
            el.innerText || el.value || el.getAttribute('aria-label')
            || el.getAttribute('placeholder') || el.getAttribute('name') || ''
        ).trim().slice(0, 200);
        results.push({index: i, tag, kind, label});
    });
    return JSON.stringify(results);
})()
"#;

    static SESSIONS: LazyLock<Mutex<HashMap<i64, Session>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    struct Session {
        page: Page,
        intercept_task: tokio::task::JoinHandle<()>,
        frame_tx: broadcast::Sender<BrowserFrame>,
        frame_subscriber_count: usize,
        screencast_task: Option<tokio::task::JoinHandle<()>>,
    }

    /// Opens a browsing session for `conversation_id` — refuses if one is
    /// already open (mirrors `create_pod`'s own "refuses if a pod already
    /// exists" precedent; `close_session` first). Always uses the real,
    /// strict `fetch_guard::is_safe_fetch_addr` — see
    /// `open_session_with_guard` for why a test-only seam exists at all.
    pub async fn open_session(conversation_id: i64) -> Result<(), String> {
        open_session_with_guard(conversation_id, fetch_guard::is_safe_fetch_addr).await
    }

    /// The real implementation behind `open_session`, parameterized on the
    /// address-safety predicate — same reason `webfetch::fetch_with_guard`
    /// has this seam: every locally-reachable address in this dev
    /// container is either loopback or RFC1918-private, so exercising
    /// "does a real navigation actually get through" needs a relaxed
    /// variant in tests, while the real `open_session` above always uses
    /// the strict guard.
    async fn open_session_with_guard(
        conversation_id: i64,
        is_addr_allowed: fn(IpAddr) -> bool,
    ) -> Result<(), String> {
        if SESSIONS.lock().unwrap().contains_key(&conversation_id) {
            return Err(
                "a browsing session is already open for this conversation — call \
                 close_browser_session first"
                    .to_string(),
            );
        }
        let browser = crate::webfetch::shared_browser().await?;
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("failed to open a page: {e}"))?;
        // Pins this page's viewport to exactly the screencast's own
        // max dimensions (device_scale_factor 1, no mobile emulation) —
        // otherwise every screencast frame's actual pixel size depends on
        // the shared browser's own launch-time window size scaled down to
        // fit maxWidth/maxHeight, which the live panel would have to
        // discover per frame to translate a click position back to real
        // page coordinates correctly. Pinning it here means the panel can
        // just assume a fixed, known frame size.
        page.execute(SetDeviceMetricsOverrideParams::new(
            SCREENCAST_MAX_WIDTH,
            SCREENCAST_MAX_HEIGHT,
            1.0,
            false,
        ))
        .await
        .map_err(|e| format!("failed to set the session's viewport: {e}"))?;
        let intercept_task =
            fetch_guard::spawn_request_interceptor(&page, is_addr_allowed).await?;
        let (frame_tx, _) = broadcast::channel(FRAME_CHANNEL_CAPACITY);
        SESSIONS.lock().unwrap().insert(
            conversation_id,
            Session {
                page,
                intercept_task,
                frame_tx,
                frame_subscriber_count: 0,
                screencast_task: None,
            },
        );
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::BrowsingSessionUpdate { open: true },
        );
        Ok(())
    }

    /// Closes `conversation_id`'s browsing session — a no-op (not an
    /// error) if none is open, matching `terminate_pod`'s own "already
    /// gone is fine" precedent. Also stops the screencast (if any
    /// live-panel viewer was watching) — there's no subscriber left to
    /// notify, just real CDP/process state to tear down.
    pub async fn close_session(conversation_id: i64) -> Result<(), String> {
        let session = SESSIONS.lock().unwrap().remove(&conversation_id);
        if let Some(session) = session {
            session.intercept_task.abort();
            if let Some(task) = session.screencast_task {
                task.abort();
            }
            let _ = session.page.close().await;
            crate::events::publish(
                conversation_id,
                crate::events::ConversationEvent::BrowsingSessionUpdate { open: false },
            );
        }
        Ok(())
    }

    /// A live handle on `conversation_id`'s frame stream — subscribing
    /// (`subscribe_frames`) starts the real CDP screencast if this is the
    /// first viewer; dropping this (when the panel closes or the browser
    /// tab disconnects) stops it again if this was the last one. The
    /// actual unsubscribe is async (it may send `Page.stopScreencast`),
    /// which a plain `Drop` impl can't `.await` — so `Drop` here spawns a
    /// detached task to do it, the same "best-effort cleanup, fire and
    /// forget" shape this codebase already uses for other non-critical
    /// teardown.
    pub struct FrameSubscription {
        pub receiver: broadcast::Receiver<BrowserFrame>,
        conversation_id: i64,
    }

    impl Drop for FrameSubscription {
        fn drop(&mut self) {
            let conversation_id = self.conversation_id;
            tokio::spawn(async move {
                unsubscribe_frames(conversation_id).await;
            });
        }
    }

    /// Subscribes to `conversation_id`'s live-panel frame stream — errors
    /// if no session is open. Starts the real screencast
    /// (`Page.startScreencast`) the moment the *first* subscriber
    /// attaches, not when the session itself opens, so a session nobody's
    /// watching never pays continuous encoding/bandwidth cost.
    pub fn subscribe_frames(conversation_id: i64) -> Result<FrameSubscription, String> {
        let mut sessions = SESSIONS.lock().unwrap();
        let session = sessions
            .get_mut(&conversation_id)
            .ok_or_else(|| "no browser session is open for this conversation".to_string())?;
        let receiver = session.frame_tx.subscribe();
        session.frame_subscriber_count += 1;
        if session.frame_subscriber_count == 1 {
            let page = session.page.clone();
            let frame_tx = session.frame_tx.clone();
            session.screencast_task = Some(tokio::spawn(async move {
                run_screencast(page, frame_tx).await;
            }));
        }
        Ok(FrameSubscription {
            receiver,
            conversation_id,
        })
    }

    /// Decrements `conversation_id`'s subscriber count and, if it just
    /// hit zero, stops the real screencast — both the local polling task
    /// and the actual CDP `Page.stopScreencast` command, so Chrome stops
    /// encoding frames nobody's reading. A no-op if the session itself is
    /// already gone (closed out from under a still-live subscription).
    async fn unsubscribe_frames(conversation_id: i64) {
        let (should_stop, page) = {
            let mut sessions = SESSIONS.lock().unwrap();
            let Some(session) = sessions.get_mut(&conversation_id) else {
                return;
            };
            session.frame_subscriber_count = session.frame_subscriber_count.saturating_sub(1);
            if session.frame_subscriber_count == 0 {
                if let Some(task) = session.screencast_task.take() {
                    task.abort();
                }
                (true, Some(session.page.clone()))
            } else {
                (false, None)
            }
        };
        if should_stop {
            if let Some(page) = page {
                let _ = page.execute(StopScreencastParams::default()).await;
            }
        }
    }

    /// Drives the real screencast: starts it, then forwards every frame
    /// onto `frame_tx` and acks it (`Page.screencastFrameAck`) so Chrome
    /// keeps sending more — CDP stops sending new frames until the
    /// previous one is acked. Ends (and lets the screencast trail off)
    /// when the event stream itself ends, e.g. the page closes.
    async fn run_screencast(page: Page, frame_tx: broadcast::Sender<BrowserFrame>) {
        let mut frames = match page.event_listener::<EventScreencastFrame>().await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("browsing: failed to listen for screencast frames: {e}");
                return;
            }
        };
        let start = StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(SCREENCAST_QUALITY)
            .max_width(SCREENCAST_MAX_WIDTH)
            .max_height(SCREENCAST_MAX_HEIGHT)
            .build();
        if let Err(e) = page.execute(start).await {
            tracing::warn!("browsing: failed to start screencast: {e}");
            return;
        }
        while let Some(event) = frames.next().await {
            let _ = frame_tx.send(BrowserFrame {
                data: String::from(event.data.clone()),
            });
            if let Err(e) = page
                .execute(ScreencastFrameAckParams::new(event.session_id))
                .await
            {
                tracing::warn!("browsing: failed to ack screencast frame: {e}");
            }
        }
    }

    /// Whether a browsing session is currently open for `conversation_id`
    /// — for the panel's initial load
    /// (`api::browsing::get_browsing_state`), so it can show an idle
    /// state instead of trying to subscribe to a frame stream that
    /// doesn't exist yet.
    pub fn is_session_open(conversation_id: i64) -> bool {
        SESSIONS.lock().unwrap().contains_key(&conversation_id)
    }

    fn live_page(conversation_id: i64) -> Result<Page, String> {
        SESSIONS
            .lock()
            .unwrap()
            .get(&conversation_id)
            .map(|s| s.page.clone())
            .ok_or_else(|| {
                "no browser session is open for this conversation — call \
                 open_browser_session first"
                    .to_string()
            })
    }

    /// `(key, code, windows_virtual_key_code)` for the named keys
    /// `PressKey` supports — real, standard values (the same ones any
    /// real keyboard sends), not placeholders. Deliberately a small,
    /// explicit set rather than a full keyboard layout table — covers the
    /// common cases a form or a keyboard-driven page actually listens
    /// for.
    fn named_key_event_fields(key: &str) -> Result<(&'static str, &'static str, i64), String> {
        match key {
            "Enter" => Ok(("Enter", "Enter", 13)),
            "Backspace" => Ok(("Backspace", "Backspace", 8)),
            "Tab" => Ok(("Tab", "Tab", 9)),
            "Escape" => Ok(("Escape", "Escape", 27)),
            "Delete" => Ok(("Delete", "Delete", 46)),
            "ArrowUp" => Ok(("ArrowUp", "ArrowUp", 38)),
            "ArrowDown" => Ok(("ArrowDown", "ArrowDown", 40)),
            "ArrowLeft" => Ok(("ArrowLeft", "ArrowLeft", 37)),
            "ArrowRight" => Ok(("ArrowRight", "ArrowRight", 39)),
            other => Err(format!("unsupported named key: {other:?}")),
        }
    }

    /// Forwards a live-panel input event to `conversation_id`'s session
    /// page.
    pub async fn send_input(conversation_id: i64, event: BrowserInputEvent) -> Result<(), String> {
        let page = live_page(conversation_id)?;
        match event {
            BrowserInputEvent::MouseMove { x, y } => {
                dispatch_mouse(&page, DispatchMouseEventType::MouseMoved, x, y, None, None).await
            }
            BrowserInputEvent::MouseDown { x, y } => {
                dispatch_mouse(
                    &page,
                    DispatchMouseEventType::MousePressed,
                    x,
                    y,
                    Some(1),
                    None,
                )
                .await
            }
            BrowserInputEvent::MouseUp { x, y } => {
                dispatch_mouse(
                    &page,
                    DispatchMouseEventType::MouseReleased,
                    x,
                    y,
                    Some(1),
                    None,
                )
                .await
            }
            BrowserInputEvent::Wheel {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                dispatch_mouse(
                    &page,
                    DispatchMouseEventType::MouseWheel,
                    x,
                    y,
                    None,
                    Some((delta_x, delta_y)),
                )
                .await
            }
            BrowserInputEvent::TypeText { text } => page
                .execute(InsertTextParams::new(text))
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to send input: {e}")),
            BrowserInputEvent::PressKey { key } => {
                let (key_value, code, windows_virtual_key_code) = named_key_event_fields(&key)?;
                for r#type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
                    let params = DispatchKeyEventParams::builder()
                        .r#type(r#type)
                        .key(key_value)
                        .code(code)
                        .windows_virtual_key_code(windows_virtual_key_code)
                        .build()
                        .map_err(|e| format!("failed to build key event: {e}"))?;
                    page.execute(params)
                        .await
                        .map_err(|e| format!("failed to send key event: {e}"))?;
                }
                Ok(())
            }
        }
    }

    async fn dispatch_mouse(
        page: &Page,
        r#type: DispatchMouseEventType,
        x: f64,
        y: f64,
        click_count: Option<i64>,
        wheel_delta: Option<(f64, f64)>,
    ) -> Result<(), String> {
        let mut builder = DispatchMouseEventParams::builder()
            .r#type(r#type)
            .x(x)
            .y(y)
            .button(chromiumoxide::cdp::browser_protocol::input::MouseButton::Left);
        if let Some(click_count) = click_count {
            builder = builder.click_count(click_count);
        }
        if let Some((delta_x, delta_y)) = wheel_delta {
            builder = builder.delta_x(delta_x).delta_y(delta_y);
        }
        let params = builder
            .build()
            .map_err(|e| format!("failed to build mouse event: {e}"))?;
        page.execute(params)
            .await
            .map(|_| ())
            .map_err(|e| format!("failed to send mouse event: {e}"))
    }

    async fn extract_page_state(page: &Page) -> Result<PageState, String> {
        let url = page
            .url()
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let text_value = page
            .evaluate("document.body.innerText")
            .await
            .map_err(|e| format!("failed to read page content: {e}"))?;
        let text: String = text_value
            .into_value()
            .map_err(|e| format!("failed to read page content: {e}"))?;
        let (text, truncated) = fetch_guard::truncate(text, MAX_TEXT_CHARS);

        let elements_value = page
            .evaluate(ELEMENT_EXTRACTION_SCRIPT)
            .await
            .map_err(|e| format!("failed to extract page elements: {e}"))?;
        let elements_json: String = elements_value
            .into_value()
            .map_err(|e| format!("failed to extract page elements: {e}"))?;
        let elements = parse_elements(&elements_json)?;

        Ok(PageState {
            url,
            text,
            truncated,
            elements,
        })
    }

    /// Navigates `conversation_id`'s live session to `url` and returns
    /// the resulting page's state.
    pub async fn navigate(conversation_id: i64, url: &str) -> Result<PageState, String> {
        let page = live_page(conversation_id)?;
        tokio::time::timeout(NAV_TIMEOUT, page.goto(url))
            .await
            .map_err(|_| format!("timed out loading {url}"))?
            .map_err(|e| format!("failed to load {url}: {e}"))?;
        extract_page_state(&page).await
    }

    /// Re-extracts the live session's current page state without taking
    /// any action — for re-checking after a user's own live-panel
    /// interaction, or just to get a fresh element list.
    pub async fn read(conversation_id: i64) -> Result<PageState, String> {
        let page = live_page(conversation_id)?;
        extract_page_state(&page).await
    }

    /// Finds the element `browser_click`/`browser_fill` were told to act
    /// on — shared lookup so both give the same clear error for an
    /// unknown index.
    async fn find_tagged_element(
        page: &Page,
        element_index: usize,
    ) -> Result<chromiumoxide::Element, String> {
        page.find_element(format!("[data-smelt-el=\"{element_index}\"]"))
            .await
            .map_err(|_| {
                format!(
                    "no element with index {element_index} — call browser_read or \
                     browser_navigate first to get a current element list"
                )
            })
    }

    /// Clicks the element at `element_index` (from the most recent
    /// navigate/click/fill/read response's `elements` list) and returns
    /// the resulting page state.
    pub async fn click(conversation_id: i64, element_index: usize) -> Result<PageState, String> {
        let page = live_page(conversation_id)?;
        let element = find_tagged_element(&page, element_index).await?;
        element
            .click()
            .await
            .map_err(|e| format!("failed to click element {element_index}: {e}"))?;
        settle_after_action(&page).await;
        extract_page_state(&page).await
    }

    /// Types `value` into the element at `element_index` and returns the
    /// resulting page state. Does not submit — a separate `click` on a
    /// submit control is a deliberate, explicit second step (matches how
    /// a person actually fills out a form), not implicit in `fill`.
    pub async fn fill(
        conversation_id: i64,
        element_index: usize,
        value: &str,
    ) -> Result<PageState, String> {
        let page = live_page(conversation_id)?;
        let element = find_tagged_element(&page, element_index).await?;
        // `type_str` needs the element focused first — confirmed against
        // the vendored source's own doc example, which always chains
        // `.click().await?.type_str(...)`; without it, real keyboard
        // events go nowhere and nothing on the page reacts. Found the
        // hard way: the first real test run of `fill` typed into a field
        // with no visible effect at all, since nothing had focus.
        element
            .focus()
            .await
            .map_err(|e| format!("failed to focus element {element_index}: {e}"))?;
        element
            .type_str(value)
            .await
            .map_err(|e| format!("failed to fill element {element_index}: {e}"))?;
        extract_page_state(&page).await
    }

    /// Navigates back in the session's history and returns the resulting
    /// page state.
    pub async fn go_back(conversation_id: i64) -> Result<PageState, String> {
        let page = live_page(conversation_id)?;
        page.evaluate("history.back()")
            .await
            .map_err(|e| format!("failed to go back: {e}"))?;
        settle_after_action(&page).await;
        extract_page_state(&page).await
    }

    /// Waits for the action that just happened to "settle" before the
    /// caller extracts the resulting page state. `goto` conveniently
    /// resolves only once its own navigation is fully loaded (see
    /// `webfetch`'s own doc comment on that), but a click/fill/back has
    /// no equivalent signal when it *doesn't* trigger a navigation (a
    /// JS-driven tab switch, an expand/collapse toggle, ...) — there's
    /// nothing to wait for in that case beyond giving the page a moment
    /// to react. Races `page.wait_for_navigation()` against a fixed
    /// timeout and takes whichever finishes first: a real navigation
    /// resolves it immediately; no navigation just falls through to the
    /// timeout, which is also the (bounded) cost of every non-navigating
    /// action. Confirmed against real pages during implementation — see
    /// the plan's "Riskiest assumptions."
    async fn settle_after_action(page: &Page) {
        let _ = tokio::time::timeout(ACTION_SETTLE_TIMEOUT, page.wait_for_navigation()).await;
    }

    /// Parses the JSON array a page-evaluated element-extraction script
    /// returns (see `ELEMENT_EXTRACTION_SCRIPT`) into `PageElement`s —
    /// pure, so it's TDD-able without a real page in the loop. A
    /// malformed/missing field is a real bug in the extraction script,
    /// not a value worth guessing at, so this surfaces the parse error
    /// rather than silently dropping the element.
    fn parse_elements(json: &str) -> Result<Vec<PageElement>, String> {
        serde_json::from_str(json)
            .map_err(|e| format!("failed to parse extracted elements: {e}"))
    }

    /// Real, `chrome-headless-shell`-backed scenarios — **not** its own
    /// `#[tokio::test]`. `open_session`/`navigate` run on the *same*
    /// shared `chromiumoxide::Browser` static `webfetch.rs` owns
    /// (`crate::webfetch::shared_browser`), and `docs/testing.md`'s
    /// generalized `OnceLock`/`OnceCell`-across-separate-runtimes hazard
    /// means only *one* `#[tokio::test]` function in the whole binary can
    /// touch that static — `webfetch::browser_tests::test_fetch_scenarios`
    /// already is that one function, so these scenarios run as a plain
    /// async fn it calls into, in the same runtime, rather than a second
    /// `#[tokio::test]` that would race it.
    #[cfg(all(test, feature = "browser-test"))]
    pub(crate) mod browser_tests {
        use super::*;

        /// Same seam `webfetch::browser_tests::allow_loopback_too`
        /// already establishes, same reason: every locally-reachable
        /// address in this dev container is either loopback or
        /// RFC1918-private.
        fn allow_loopback_too(addr: IpAddr) -> bool {
            fetch_guard::is_safe_fetch_addr(addr) || addr.is_loopback()
        }

        async fn start_test_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
            let router = axum::Router::new().route(
                "/",
                axum::routing::get(move || async move { axum::response::Html(body) }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a test-local port");
            let port = listener.local_addr().expect("local addr").port();
            let task = tokio::spawn(async move {
                axum::serve(listener, router)
                    .await
                    .expect("test server error");
            });
            (format!("http://127.0.0.1:{port}/"), task)
        }

        /// A start page exercising a navigating link, a non-navigating
        /// (JS-only) toggle, and a fillable input with a live mirror —
        /// plus a second page the link leads to, for
        /// `click`/`fill`/`go_back`.
        async fn start_interactive_test_server() -> (String, tokio::task::JoinHandle<()>) {
            let router = axum::Router::new()
                .route(
                    "/",
                    axum::routing::get(|| async {
                        axum::response::Html(
                            "<html><body>\
                             <h1>Start page</h1>\
                             <a href=\"/page2\">Go to page 2</a>\
                             <button onclick=\"document.getElementById('toggle-out').innerText='toggled'\">Toggle</button>\
                             <div id=\"toggle-out\">not toggled</div>\
                             <input type=\"text\" oninput=\"document.getElementById('mirror').innerText=this.value\">\
                             <div id=\"mirror\"></div>\
                             </body></html>",
                        )
                    }),
                )
                .route(
                    "/page2",
                    axum::routing::get(|| async {
                        axum::response::Html("<html><body><h1>Page two</h1></body></html>")
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a test-local port");
            let port = listener.local_addr().expect("local addr").port();
            let task = tokio::spawn(async move {
                axum::serve(listener, router)
                    .await
                    .expect("test server error");
            });
            (format!("http://127.0.0.1:{port}"), task)
        }

        pub(crate) async fn run_session_scenarios() {
            let conversation_id: i64 = 900_001;

            // --- Scenario 1: open a session, navigate, read back real
            // rendered text and a real extracted element list. ---
            let (url, _server) = start_test_server(
                "<html><body><h1>Browsing session page</h1>\
                 <a href=\"#\">a link</a></body></html>",
            )
            .await;
            open_session_with_guard(conversation_id, allow_loopback_too)
                .await
                .expect("open_session should succeed");
            let state = navigate(conversation_id, &url)
                .await
                .expect("navigate should succeed");
            assert!(
                state.text.contains("Browsing session page"),
                "expected the rendered text, got: {:?}",
                state.text
            );
            assert!(
                state.elements.iter().any(|e| e.label == "a link"),
                "expected the link to be extracted, got: {:?}",
                state.elements
            );

            // --- Scenario 2: opening a second session for the same
            // conversation is refused (mirrors create_pod). ---
            let second_open =
                open_session_with_guard(conversation_id, allow_loopback_too).await;
            assert!(
                second_open.is_err(),
                "expected a second open_session for the same conversation to be refused"
            );

            // --- Scenario 3 (riskiest assumption #1): interception
            // enabled once when the session opened still applies on a
            // *later*, separate navigate call on the same page — not
            // just within the one `goto` a single `webfetch` call
            // makes. ---
            let blocked = navigate(conversation_id, "http://169.254.169.254/").await;
            assert!(
                blocked.is_err(),
                "expected a later navigate on an already-open session to still be \
                 SSRF-guarded, got: {blocked:?}"
            );

            close_session(conversation_id)
                .await
                .expect("close_session should succeed");

            // --- Scenario 4: after closing, every action errors clearly
            // rather than reusing a stale/closed page. ---
            let after_close = navigate(conversation_id, &url).await;
            assert!(
                after_close.is_err(),
                "expected navigate to fail once the session is closed"
            );

            // --- Scenarios 5-8: click (navigating and non-navigating),
            // fill, go_back — a fresh session against a small multi-page
            // fixture. ---
            let (base, _server) = start_interactive_test_server().await;
            open_session_with_guard(conversation_id, allow_loopback_too)
                .await
                .expect("re-open_session should succeed");
            let state = navigate(conversation_id, &base)
                .await
                .expect("navigate to start page should succeed");
            let link_index = state
                .elements
                .iter()
                .find(|e| e.label == "Go to page 2")
                .map(|e| e.index)
                .expect("the nav link should be in the extracted elements");
            let toggle_index = state
                .elements
                .iter()
                .find(|e| e.label == "Toggle")
                .map(|e| e.index)
                .expect("the toggle button should be in the extracted elements");
            let input_index = state
                .elements
                .iter()
                .find(|e| e.tag == "input")
                .map(|e| e.index)
                .expect("the input should be in the extracted elements");

            // Scenario 5 (riskiest assumption #2, navigating case):
            // clicking a real link navigates, and the returned state
            // reflects the *new* page, not the old one.
            let start = tokio::time::Instant::now();
            let clicked = click(conversation_id, link_index)
                .await
                .expect("clicking the nav link should succeed");
            assert!(
                clicked.url.ends_with("/page2"),
                "expected the URL to reflect the navigation, got: {}",
                clicked.url
            );
            assert!(
                clicked.text.contains("Page two"),
                "expected page2's content, got: {:?}",
                clicked.text
            );
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "a real navigating click should resolve promptly via wait_for_navigation, \
                 not wait out the full settle timeout"
            );

            // Scenario 6 (riskiest assumption #2, non-navigating case): a
            // JS-only toggle click doesn't navigate, but its effect still
            // shows up in the re-extracted state, and it doesn't take
            // unreasonably long either (bounded by ACTION_SETTLE_TIMEOUT).
            navigate(conversation_id, &base)
                .await
                .expect("navigate back to the start page should succeed");
            let start = tokio::time::Instant::now();
            let toggled = click(conversation_id, toggle_index)
                .await
                .expect("clicking the toggle should succeed");
            assert!(
                toggled.text.contains("toggled"),
                "expected the JS toggle's effect in the re-extracted text, got: {:?}",
                toggled.text
            );
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "a non-navigating click should be bounded by the settle timeout, not hang"
            );

            // Scenario 7: fill types into the input and the page's own JS
            // mirrors it — proves fill delivers real keyboard input, not
            // just a value assignment nothing on the page reacts to.
            let filled = fill(conversation_id, input_index, "hello browsing")
                .await
                .expect("fill should succeed");
            assert!(
                filled.text.contains("hello browsing"),
                "expected the filled value mirrored into the page, got: {:?}",
                filled.text
            );

            // Scenario 8: go_back returns to the previous page in
            // history.
            navigate(conversation_id, &format!("{base}/page2"))
                .await
                .expect("navigate to page2 should succeed");
            let back = go_back(conversation_id)
                .await
                .expect("go_back should succeed");
            assert!(
                back.text.contains("Start page") || back.text.contains("Toggle"),
                "expected go_back to land on the start page, got: {:?}",
                back.text
            );

            // --- Scenario 9 (riskiest assumption #3): screencast frames
            // actually keep flowing once acked, not just a single frame
            // then a stall — and the subscribe/unsubscribe lifecycle
            // (starting and fully stopping the real CDP screencast) can
            // run more than once cleanly. Needs a page with continuous
            // visual change (a static page may only ever produce one
            // frame, which wouldn't distinguish "ack works" from "nothing
            // new to send anyway"). ---
            navigate_to_animated_page(conversation_id).await;

            {
                let mut sub =
                    subscribe_frames(conversation_id).expect("subscribe_frames should succeed");
                let mut received = 0;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while received < 3 && tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout(Duration::from_secs(3), sub.receiver.recv()).await
                    {
                        Ok(Ok(frame)) => {
                            assert!(!frame.data.is_empty(), "expected non-empty frame data");
                            received += 1;
                        }
                        Ok(Err(e)) => panic!("frame channel error: {e}"),
                        Err(_) => break,
                    }
                }
                assert!(
                    received >= 2,
                    "expected acking to keep frames flowing (got {received} frame(s)) — \
                     a stall after 1 would mean the ack isn't actually unblocking more"
                );
            } // `sub` drops here — spawns the async unsubscribe/stop-screencast cleanup.

            // Give the fire-and-forget unsubscribe cleanup a moment to
            // run, then confirm a second subscription still works
            // cleanly (the screencast can be stopped and restarted, not
            // just started once).
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut second_sub = subscribe_frames(conversation_id)
                .expect("a second subscribe_frames should succeed");
            let second_frame =
                tokio::time::timeout(Duration::from_secs(5), second_sub.receiver.recv())
                    .await
                    .expect("should receive a frame within the timeout")
                    .expect("frame channel should not error");
            assert!(!second_frame.data.is_empty());
            drop(second_sub);

            // --- Scenario 10: live-panel input forwarding (`send_input`)
            // — a real mouse click at real coordinates on a real button,
            // and real typed text into a real input, both dispatched the
            // same way a live-panel viewer's own mouse/keyboard would
            // be. ---
            let (base, _server) = start_interactive_test_server().await;
            navigate(conversation_id, &base)
                .await
                .expect("navigate for input scenario should succeed");
            let page = live_page(conversation_id).expect("session should still be live");
            let toggle_element = page
                .find_element("button")
                .await
                .expect("should find the toggle button");
            let point = toggle_element
                .clickable_point()
                .await
                .expect("should get a clickable point for the button");
            send_input(
                conversation_id,
                BrowserInputEvent::MouseMove {
                    x: point.x,
                    y: point.y,
                },
            )
            .await
            .expect("mouse move should succeed");
            send_input(
                conversation_id,
                BrowserInputEvent::MouseDown {
                    x: point.x,
                    y: point.y,
                },
            )
            .await
            .expect("mouse down should succeed");
            send_input(
                conversation_id,
                BrowserInputEvent::MouseUp {
                    x: point.x,
                    y: point.y,
                },
            )
            .await
            .expect("mouse up should succeed");
            let after_click = read(conversation_id)
                .await
                .expect("read after input should succeed");
            assert!(
                after_click.text.contains("toggled"),
                "expected the real mouse click to trigger the toggle, got: {:?}",
                after_click.text
            );

            let input_element = page
                .find_element("input")
                .await
                .expect("should find the input");
            let input_point = input_element
                .clickable_point()
                .await
                .expect("should get a clickable point for the input");
            send_input(
                conversation_id,
                BrowserInputEvent::MouseDown {
                    x: input_point.x,
                    y: input_point.y,
                },
            )
            .await
            .expect("mouse down on input should succeed");
            send_input(
                conversation_id,
                BrowserInputEvent::MouseUp {
                    x: input_point.x,
                    y: input_point.y,
                },
            )
            .await
            .expect("mouse up on input should succeed");
            send_input(
                conversation_id,
                BrowserInputEvent::TypeText {
                    text: "typed via panel".to_string(),
                },
            )
            .await
            .expect("type text should succeed");
            let after_type = read(conversation_id)
                .await
                .expect("read after typing should succeed");
            assert!(
                after_type.text.contains("typed via panel"),
                "expected the real click-to-focus plus typed text to reach the input, got: {:?}",
                after_type.text
            );

            close_session(conversation_id)
                .await
                .expect("final close_session should succeed");
        }

        /// Navigates to a page whose content changes continuously (a
        /// `setInterval`-driven counter) — screencast only sends a new
        /// frame when something visually changes, so a static page can't
        /// distinguish "ack is working" from "nothing new to send
        /// anyway."
        async fn navigate_to_animated_page(conversation_id: i64) {
            let (animated_url, _server) = start_test_server(
                "<html><body><div id=\"counter\">0</div><script>
                    let n = 0;
                    setInterval(() => { document.getElementById('counter').innerText = String(++n); }, 100);
                </script></body></html>",
            )
            .await;
            navigate(conversation_id, &animated_url)
                .await
                .expect("navigate to the animated page should succeed");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_parse_elements_parses_a_well_formed_list() {
            let json = r#"[
                {"index": 0, "tag": "a", "kind": "link", "label": "Home"},
                {"index": 1, "tag": "input", "kind": "input", "label": "Search"}
            ]"#;
            let elements = parse_elements(json).expect("should parse");
            assert_eq!(
                elements,
                vec![
                    PageElement {
                        index: 0,
                        tag: "a".to_string(),
                        kind: "link".to_string(),
                        label: "Home".to_string(),
                    },
                    PageElement {
                        index: 1,
                        tag: "input".to_string(),
                        kind: "input".to_string(),
                        label: "Search".to_string(),
                    },
                ]
            );
        }

        #[test]
        fn test_parse_elements_parses_an_empty_list() {
            assert_eq!(parse_elements("[]").expect("should parse"), vec![]);
        }

        #[test]
        fn test_parse_elements_rejects_malformed_json() {
            assert!(parse_elements("not json").is_err());
        }

        #[test]
        fn test_parse_elements_rejects_an_element_missing_a_field() {
            assert!(parse_elements(r#"[{"index": 0, "tag": "a"}]"#).is_err());
        }

        #[test]
        fn test_named_key_event_fields_accepts_a_known_key() {
            let (key, code, windows_virtual_key_code) =
                named_key_event_fields("Enter").expect("Enter should be known");
            assert_eq!(key, "Enter");
            assert_eq!(code, "Enter");
            assert_eq!(windows_virtual_key_code, 13);
        }

        #[test]
        fn test_named_key_event_fields_rejects_an_unknown_key() {
            assert!(named_key_event_fields("F13").is_err());
        }
    }
}

#[cfg(feature = "server")]
pub use server::{
    click, close_session, fill, go_back, is_session_open, navigate, open_session, read,
    send_input, subscribe_frames,
};

#[cfg(all(test, feature = "browser-test"))]
pub(crate) use server::browser_tests;
