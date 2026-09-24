//! Interactive web browsing: a persistent browsing *session* per
//! conversation — open a page, read it, click/fill/navigate across
//! several tool calls, instead of `webfetch`'s fresh-page-per-call shape.
//! Plus a live panel: the user can watch and interact with the same real
//! page the model is browsing. See
//! docs/projects/completed/20260922-web-browsing.md.
//!
//! `BrowserFrame`/`BrowserInputEvent` are ungated — they cross the
//! client/server boundary as server-function payloads (`api::browsing`'s
//! frame stream and input endpoint), so the `web` build needs them too.
//! Everything else (the actual `chromiumoxide`-driven session logic) lives
//! in the `server`-only nested module, re-exported — same shape
//! `anthropic::tools` already uses for the same reason.

use serde::{Deserialize, Serialize};

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
/// `server::named_key_event_fields`. `MouseMove` carries whether the
/// viewer's left button is actually held (read off the real DOM event, not
/// tracked server-side, so a button released outside the panel can't leave
/// the page stuck mid-drag) — a plain hover must reach the page as a move
/// with no buttons down, not a drag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum BrowserInputEvent {
    MouseMove { x: f64, y: f64, left_held: bool },
    MouseDown { x: f64, y: f64 },
    MouseUp { x: f64, y: f64 },
    Wheel { x: f64, y: f64, delta_x: f64, delta_y: f64 },
    TypeText { text: String },
    /// `modifiers` is CDP's bitmask (Alt=1, Ctrl=2, Meta=4, Shift=8), so
    /// Shift+Tab, Shift+Arrow and Ctrl+Backspace do what they would on a
    /// real keyboard.
    PressKey { key: String, modifiers: i64 },
}

#[cfg(feature = "server")]
mod server {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;

    use chromiumoxide::Page;
    use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
        DispatchMouseEventType, InsertTextParams,
    };
    use chromiumoxide::cdp::browser_protocol::page::{
        EventFrameNavigated, EventFrameStartedLoading, EventLoadEventFired,
        EventNavigatedWithinDocument, EventScreencastFrame,
        ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
        StopScreencastParams,
    };
    use futures_util::{Stream, StreamExt};
    use tokio::sync::watch;

    use serde::{Deserialize, Serialize};

    use super::{BrowserFrame, BrowserInputEvent};
    use crate::fetch_guard;

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

    /// Bounds page navigation+load, same reasoning `webfetch::NAV_TIMEOUT`
    /// already established — "bound the boundaries" (development-process.md).
    const NAV_TIMEOUT: Duration = Duration::from_secs(20);
    /// Same truncation cap `webfetch`/`http_request` already use.
    const MAX_TEXT_CHARS: usize = 20_000;
    /// How long `click`/`go_back` watch for the action to start a
    /// navigation — and, when it doesn't, how long they wait for in-page
    /// updates it set off (a fetch, a tab switch) before reading the page.
    /// A real cost on every non-navigating action, so kept short; a
    /// navigation that does start gets up to `NAV_TIMEOUT` to load.
    const ACTION_SETTLE_TIMEOUT: Duration = Duration::from_millis(600);
    /// Screencast frame bounds — deliberately conservative (bandwidth over
    /// smoothness), tunable once the panel is actually running against a
    /// real page; see the plan's "Exact frame quality/size/rate defaults."
    const SCREENCAST_MAX_WIDTH: i64 = 1280;
    const SCREENCAST_MAX_HEIGHT: i64 = 800;
    const SCREENCAST_QUALITY: i64 = 60;

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
        // A password field's value is never a label: it would go to the
        // model (and into the saved conversation) with every page read.
        const secret = tag === 'input' && (el.getAttribute('type') || '').toLowerCase() === 'password';
        const label = (
            el.innerText || (secret ? '' : el.value) || el.getAttribute('aria-label')
            || el.getAttribute('placeholder') || el.getAttribute('name') || ''
        ).trim().slice(0, 200);
        results.push({index: i, tag, kind, label});
    });
    return JSON.stringify(results);
})()
"#;

    static SESSIONS: LazyLock<Mutex<HashMap<i64, Session>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Held across the whole of `open_session` and `close_session`, so an
    /// open's "is one already open?" check and its eventual insert can't
    /// interleave with another open, and a close issued mid-open waits for
    /// that open to finish rather than missing it. Opens and closes are
    /// rare, so one lock for all conversations is fine.
    static SESSION_LIFECYCLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Distinguishes one open of a conversation's session from the next,
    /// so a `FrameSubscription` outliving the session it came from can't
    /// touch a later session's viewer count.
    static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

    struct Session {
        id: u64,
        page: Page,
        intercept_task: tokio::task::JoinHandle<()>,
        /// The latest frame, as a template for new viewers' receivers — a
        /// `watch` channel rather than a broadcast one, because Chrome only
        /// sends a frame when something on screen changes: a viewer joining
        /// a running screencast on a still page would otherwise get
        /// nothing. The sender lives in `run_screencast`.
        latest_frame: watch::Receiver<Option<BrowserFrame>>,
        frame_subscriber_count: usize,
        /// Whether anyone is watching — `run_screencast` (one task for the
        /// session's whole life) starts/stops the real screencast as this
        /// flips, so start and stop commands are always issued in order
        /// from one place and can't race each other.
        want_screencast: watch::Sender<bool>,
        screencast_task: tokio::task::JoinHandle<()>,
        /// The page's URL as of its latest navigation — kept current by
        /// `url_task` (`watch_url`).
        current_url: String,
        url_task: tokio::task::JoinHandle<()>,
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
        let _lifecycle = SESSION_LIFECYCLE.lock().await;
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
        let intercept_task = match configure_session_page(&page, is_addr_allowed).await {
            Ok(task) => task,
            Err(e) => {
                let _ = page.close().await;
                return Err(e);
            }
        };
        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let url_task = match watch_url(conversation_id, session_id, &page).await {
            Ok(task) => task,
            Err(e) => {
                intercept_task.abort();
                let _ = page.close().await;
                return Err(e);
            }
        };
        let (frame_tx, latest_frame) = watch::channel(None);
        let (want_screencast, want_rx) = watch::channel(false);
        let screencast_task = tokio::spawn(run_screencast(page.clone(), frame_tx, want_rx));
        SESSIONS.lock().unwrap().insert(
            conversation_id,
            Session {
                id: session_id,
                current_url: "about:blank".to_string(),
                url_task,
                page,
                intercept_task,
                latest_frame,
                frame_subscriber_count: 0,
                want_screencast,
                screencast_task,
            },
        );
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::BrowsingSessionUpdate { open: true },
        );
        Ok(())
    }

    /// Everything a fresh session page needs before it's handed out —
    /// pinned viewport plus SSRF interception. Split out so
    /// `open_session_with_guard` can close the page if any step fails,
    /// rather than leaking it in the shared browser.
    async fn configure_session_page(
        page: &Page,
        is_addr_allowed: fn(IpAddr) -> bool,
    ) -> Result<tokio::task::JoinHandle<()>, String> {
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
        fetch_guard::spawn_request_interceptor(page, is_addr_allowed).await
    }

    /// Closes `conversation_id`'s browsing session — a no-op (not an
    /// error) if none is open, matching `terminate_pod`'s own "already
    /// gone is fine" precedent. Also stops the screencast (if any
    /// live-panel viewer was watching) — there's no subscriber left to
    /// notify, just real CDP/process state to tear down.
    pub async fn close_session(conversation_id: i64) -> Result<(), String> {
        let _lifecycle = SESSION_LIFECYCLE.lock().await;
        let session = SESSIONS.lock().unwrap().remove(&conversation_id);
        if let Some(session) = session {
            session.intercept_task.abort();
            session.screencast_task.abort();
            session.url_task.abort();
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
    /// tab disconnects) stops it again if this was the last one.
    pub struct FrameSubscription {
        receiver: watch::Receiver<Option<BrowserFrame>>,
        _viewer: ViewerGuard,
    }

    impl FrameSubscription {
        /// Starts from whatever frame the session already has, so the first
        /// `next_frame` returns it straight away.
        fn new(mut receiver: watch::Receiver<Option<BrowserFrame>>, viewer: ViewerGuard) -> Self {
            if receiver.borrow().is_some() {
                receiver.mark_changed();
            }
            Self {
                receiver,
                _viewer: viewer,
            }
        }

        /// The next frame this viewer hasn't seen — always the newest one,
        /// however many were produced since the last call, so a slow viewer
        /// skips ahead instead of falling behind. `None` once the session
        /// closes.
        pub async fn next_frame(&mut self) -> Option<BrowserFrame> {
            loop {
                self.receiver.changed().await.ok()?;
                if let Some(frame) = self.receiver.borrow_and_update().clone() {
                    return Some(frame);
                }
            }
        }
    }

    /// Counts as one viewer of one specific session for as long as it
    /// lives. Unsubscribing is synchronous (a counter plus a `watch`
    /// flip that `run_screencast` acts on), so this needs no spawned
    /// cleanup task.
    struct ViewerGuard {
        conversation_id: i64,
        session_id: u64,
    }

    impl Drop for ViewerGuard {
        fn drop(&mut self) {
            unsubscribe_frames(self.conversation_id, self.session_id);
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
        let receiver = session.latest_frame.clone();
        session.frame_subscriber_count += 1;
        if session.frame_subscriber_count == 1 {
            session.want_screencast.send_replace(true);
        }
        Ok(FrameSubscription::new(
            receiver,
            ViewerGuard {
                conversation_id,
                session_id: session.id,
            },
        ))
    }

    /// Drops one viewer from the session it was counted against; the last
    /// one leaving tells `run_screencast` to stop, so Chrome stops encoding
    /// frames nobody's reading. A no-op if that session is gone — including
    /// when a *newer* session has since been opened for the same
    /// conversation, whose viewers this one was never part of.
    fn unsubscribe_frames(conversation_id: i64, session_id: u64) {
        let mut sessions = SESSIONS.lock().unwrap();
        let Some(session) = sessions.get_mut(&conversation_id) else {
            return;
        };
        if session.id != session_id {
            return;
        }
        session.frame_subscriber_count = session.frame_subscriber_count.saturating_sub(1);
        if session.frame_subscriber_count == 0 {
            session.want_screencast.send_replace(false);
        }
    }

    /// The live panel's frame stream for one subscription. Frames are
    /// produced only as the stream is pulled, and each pull gets the newest
    /// frame, so a slow reader never builds a backlog. The stream owns the
    /// subscription — dropping it (the SSE connection closing) is what
    /// drops the viewer.
    pub fn frame_stream(
        subscription: FrameSubscription,
    ) -> impl Stream<Item = Result<BrowserFrame, axum::BoxError>> + Send + 'static {
        futures_util::stream::unfold(subscription, |mut sub| async move {
            sub.next_frame().await.map(|frame| (Ok(frame), sub))
        })
    }

    /// Runs for the session's whole life, starting the real screencast
    /// whenever `want` turns true and stopping it (`Page.stopScreencast`)
    /// whenever it turns false. While running, forwards every frame onto
    /// `frame_tx` and acks it (`Page.screencastFrameAck`) so Chrome keeps
    /// sending more — CDP stops sending new frames until the previous one is
    /// acked. Issuing every start/stop from this one task is what keeps a
    /// stop meant for a departed viewer from landing after a new viewer's
    /// start.
    async fn run_screencast(
        page: Page,
        frame_tx: watch::Sender<Option<BrowserFrame>>,
        mut want: watch::Receiver<bool>,
    ) {
        let mut frames = match page.event_listener::<EventScreencastFrame>().await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("browsing: failed to listen for screencast frames: {e}");
                return;
            }
        };
        loop {
            if want.wait_for(|wanted| *wanted).await.is_err() {
                return;
            }
            let start = StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(SCREENCAST_QUALITY)
                .max_width(SCREENCAST_MAX_WIDTH)
                .max_height(SCREENCAST_MAX_HEIGHT)
                .build();
            if let Err(e) = page.execute(start).await {
                tracing::warn!("browsing: failed to start screencast: {e}");
                // Retry on the next viewer rather than giving up for the
                // rest of the session.
                if want.wait_for(|wanted| !*wanted).await.is_err() {
                    return;
                }
                continue;
            }
            loop {
                tokio::select! {
                    changed = want.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if !*want.borrow_and_update() {
                            break;
                        }
                    }
                    frame = frames.next() => {
                        let Some(event) = frame else { return };
                        frame_tx.send_replace(Some(BrowserFrame {
                            data: String::from(event.data.clone()),
                        }));
                        if let Err(e) = page
                            .execute(ScreencastFrameAckParams::new(event.session_id))
                            .await
                        {
                            tracing::warn!("browsing: failed to ack screencast frame: {e}");
                        }
                    }
                }
            }
            if let Err(e) = page.execute(StopScreencastParams::default()).await {
                tracing::warn!("browsing: failed to stop screencast: {e}");
            }
        }
    }

    /// Follows the page's main-frame URL, recording each change and
    /// publishing it as a `BrowsingUrlUpdate`: full navigations
    /// (`frameNavigated`) and in-page ones that load no new document
    /// (`navigatedWithinDocument` — `pushState`, `#fragment` links), which
    /// single-page apps rely on. The listeners are attached before this
    /// returns, so no navigation after it can be missed.
    async fn watch_url(
        conversation_id: i64,
        session_id: u64,
        page: &Page,
    ) -> Result<tokio::task::JoinHandle<()>, String> {
        let listen_error = |e| format!("failed to watch the page's URL: {e}");
        let mut main_frame = page.mainframe().await.map_err(listen_error)?;
        let mut navigated = page
            .event_listener::<EventFrameNavigated>()
            .await
            .map_err(listen_error)?;
        let mut within_document = page
            .event_listener::<EventNavigatedWithinDocument>()
            .await
            .map_err(listen_error)?;
        Ok(tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = navigated.next() => {
                        let Some(event) = event else { return };
                        if event.frame.parent_id.is_some() {
                            continue;
                        }
                        main_frame = Some(event.frame.id.clone());
                        // A failed load lands on Chrome's own error page
                        // (`chrome-error://…`); report what was asked for
                        // instead, as a browser's address bar does.
                        let url = match &event.frame.unreachable_url {
                            Some(unreachable) => unreachable.clone(),
                            None => format!(
                                "{}{}",
                                event.frame.url,
                                event.frame.url_fragment.as_deref().unwrap_or_default()
                            ),
                        };
                        record_url(conversation_id, session_id, url);
                    }
                    event = within_document.next() => {
                        let Some(event) = event else { return };
                        if main_frame.as_ref() == Some(&event.frame_id) {
                            record_url(conversation_id, session_id, event.url.clone());
                        }
                    }
                }
            }
        }))
    }

    fn record_url(conversation_id: i64, session_id: u64, url: String) {
        {
            let mut sessions = SESSIONS.lock().unwrap();
            let Some(session) = sessions.get_mut(&conversation_id) else {
                return;
            };
            if session.id != session_id || session.current_url == url {
                return;
            }
            session.current_url = url.clone();
        }
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::BrowsingUrlUpdate { url },
        );
    }

    /// The session page's current URL, or `None` if no session is open.
    pub fn current_url(conversation_id: i64) -> Option<String> {
        SESSIONS
            .lock()
            .unwrap()
            .get(&conversation_id)
            .map(|s| s.current_url.clone())
    }

    /// What someone typed into the live panel's address bar, as a URL: a
    /// bare host (`example.com`) gets `https://`. Whether the result is a
    /// URL smelt will actually load is `navigate`'s call, not this one's.
    pub fn normalize_address(input: &str) -> Result<String, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("enter an address".to_string());
        }
        if input.contains("://") {
            Ok(input.to_string())
        } else {
            Ok(format!("https://{input}"))
        }
    }

    #[cfg(all(test, feature = "browser-test"))]
    fn frame_subscriber_count(conversation_id: i64) -> usize {
        SESSIONS
            .lock()
            .unwrap()
            .get(&conversation_id)
            .map_or(0, |s| s.frame_subscriber_count)
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

    /// CDP key-event fields for one named key `PressKey` supports.
    #[derive(Debug, PartialEq)]
    struct NamedKey {
        key: &'static str,
        code: &'static str,
        windows_virtual_key_code: i64,
        /// What the key types, if anything. Chrome only runs a key's
        /// default text action — Enter's implicit form submit or textarea
        /// newline — when the keydown carries it, the same way
        /// Puppeteer's US layout sends Enter as `text: "\r"`.
        text: Option<&'static str>,
    }

    /// Real, standard values for the named keys `PressKey` supports (the
    /// same ones any real keyboard sends), not placeholders. Deliberately a
    /// small, explicit set rather than a full keyboard layout table —
    /// covers the common cases a form or a keyboard-driven page actually
    /// listens for.
    fn named_key_event_fields(key: &str) -> Result<NamedKey, String> {
        let (key, windows_virtual_key_code, text) = match key {
            "Enter" => ("Enter", 13, Some("\r")),
            "Backspace" => ("Backspace", 8, None),
            "Tab" => ("Tab", 9, None),
            "Escape" => ("Escape", 27, None),
            "Delete" => ("Delete", 46, None),
            "ArrowUp" => ("ArrowUp", 38, None),
            "ArrowDown" => ("ArrowDown", 40, None),
            "ArrowLeft" => ("ArrowLeft", 37, None),
            "ArrowRight" => ("ArrowRight", 39, None),
            other => return Err(format!("unsupported named key: {other:?}")),
        };
        Ok(NamedKey {
            key,
            code: key,
            windows_virtual_key_code,
            text,
        })
    }

    /// Forwards a live-panel input event to `conversation_id`'s session
    /// page.
    pub async fn send_input(conversation_id: i64, event: BrowserInputEvent) -> Result<(), String> {
        let page = live_page(conversation_id)?;
        let params = match event {
            BrowserInputEvent::TypeText { text } => {
                return page
                    .execute(InsertTextParams::new(text))
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("failed to send input: {e}"));
            }
            BrowserInputEvent::PressKey { key, modifiers } => {
                for params in key_event_params(&key, modifiers)? {
                    page.execute(params)
                        .await
                        .map_err(|e| format!("failed to send key event: {e}"))?;
                }
                return Ok(());
            }
            mouse => mouse_event_params(&mouse),
        };
        page.execute(params)
            .await
            .map(|_| ())
            .map_err(|e| format!("failed to send mouse event: {e}"))
    }

    /// The keydown/keyup pair for a named key. A key that types something
    /// (Enter) goes down as `keyDown` with its text so Chrome runs its
    /// default action; one that doesn't goes down as `rawKeyDown`, the
    /// same split Puppeteer makes.
    fn key_event_params(key: &str, modifiers: i64) -> Result<Vec<DispatchKeyEventParams>, String> {
        let named = named_key_event_fields(key)?;
        let down_type = if named.text.is_some() {
            DispatchKeyEventType::KeyDown
        } else {
            DispatchKeyEventType::RawKeyDown
        };
        let mut down = DispatchKeyEventParams::builder()
            .r#type(down_type)
            .key(named.key)
            .code(named.code)
            .windows_virtual_key_code(named.windows_virtual_key_code)
            .modifiers(modifiers);
        if let Some(text) = named.text {
            down = down.text(text).unmodified_text(text);
        }
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(named.key)
            .code(named.code)
            .windows_virtual_key_code(named.windows_virtual_key_code)
            .modifiers(modifiers);
        [down, up]
            .into_iter()
            .map(|b| b.build().map_err(|e| format!("failed to build key event: {e}")))
            .collect()
    }

    /// CDP params for a mouse-shaped input event. `button` is the button
    /// this event is *about* (pressed or released — none for a move or
    /// wheel); `buttons` is the bitmask held *after* it, which is what a
    /// page's `event.buttons` reports. Getting these wrong turns a plain
    /// hover into a drag. Only ever called with a mouse event — the key
    /// events are handled before `send_input` reaches here.
    fn mouse_event_params(event: &BrowserInputEvent) -> DispatchMouseEventParams {
        use chromiumoxide::cdp::browser_protocol::input::MouseButton;
        const LEFT: i64 = 1;
        let (r#type, x, y, button, buttons, click_count, delta) = match *event {
            BrowserInputEvent::MouseMove { x, y, left_held } => (
                DispatchMouseEventType::MouseMoved,
                x,
                y,
                if left_held { MouseButton::Left } else { MouseButton::None },
                if left_held { LEFT } else { 0 },
                None,
                None,
            ),
            BrowserInputEvent::MouseDown { x, y } => (
                DispatchMouseEventType::MousePressed,
                x,
                y,
                MouseButton::Left,
                LEFT,
                Some(1),
                None,
            ),
            BrowserInputEvent::MouseUp { x, y } => (
                DispatchMouseEventType::MouseReleased,
                x,
                y,
                MouseButton::Left,
                0,
                Some(1),
                None,
            ),
            BrowserInputEvent::Wheel {
                x,
                y,
                delta_x,
                delta_y,
            } => (
                DispatchMouseEventType::MouseWheel,
                x,
                y,
                MouseButton::None,
                0,
                None,
                Some((delta_x, delta_y)),
            ),
            BrowserInputEvent::TypeText { .. } | BrowserInputEvent::PressKey { .. } => {
                unreachable!("key events are dispatched before mouse_event_params")
            }
        };
        let mut builder = DispatchMouseEventParams::builder()
            .r#type(r#type)
            .x(x)
            .y(y)
            .button(button)
            .buttons(buttons);
        if let Some(click_count) = click_count {
            builder = builder.click_count(click_count);
        }
        if let Some((delta_x, delta_y)) = delta {
            builder = builder.delta_x(delta_x).delta_y(delta_y);
        }
        builder.build().expect("type, x and y are always set")
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
        // The request interceptor only sees loads that touch the network, so
        // it can't stop a `data:` (or similar) URL — check the scheme here.
        fetch_guard::parse_fetch_target(url)?;
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
        act_and_settle(&page, async {
            element
                .click()
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to click element {element_index}: {e}"))
        })
        .await?;
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
        // Inserted text and key events go to whatever has focus, so focus the
        // element first — without it, nothing on the page reacts.
        element
            .focus()
            .await
            .map_err(|e| format!("failed to focus element {element_index}: {e}"))?;
        // Select what's already there so typing replaces it rather than
        // inserting next to it — done with real key events, not by assigning
        // `.value`, which frameworks like React don't notice.
        element
            .call_js_fn(SELECT_CONTENTS_FN, false)
            .await
            .map_err(|e| format!("failed to clear element {element_index}: {e}"))?;
        // `Input.insertText`, not `type_str`: `type_str` presses one key per
        // character from a US-keyboard table and fails on anything else
        // (accents, CJK, emoji, newlines).
        if value.is_empty() {
            for params in key_event_params("Delete", 0)? {
                page.execute(params)
                    .await
                    .map_err(|e| format!("failed to clear element {element_index}: {e}"))?;
            }
        } else {
            page.execute(InsertTextParams::new(value))
                .await
                .map_err(|e| format!("failed to fill element {element_index}: {e}"))?;
        }
        extract_page_state(&page).await
    }

    const SELECT_CONTENTS_FN: &str = "function() {
        if (typeof this.select === 'function') {
            this.select();
        } else if (this.isContentEditable) {
            const range = document.createRange();
            range.selectNodeContents(this);
            const selection = window.getSelection();
            selection.removeAllRanges();
            selection.addRange(range);
        }
    }";

    /// Navigates back in the session's history and returns the resulting
    /// page state.
    pub async fn go_back(conversation_id: i64) -> Result<PageState, String> {
        let page = live_page(conversation_id)?;
        act_and_settle(&page, async {
            page.evaluate("history.back()")
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to go back: {e}"))
        })
        .await?;
        extract_page_state(&page).await
    }

    /// Runs `action`, then waits for what it set off before the caller
    /// reads the page. There's no single "done" signal for a click: it may
    /// start a navigation (possibly to a slow page), update the page later
    /// (a fetch, a timer), or do nothing. So: listen for the main frame to
    /// start loading, from *before* the action (a navigation can start
    /// before the click call even returns). If one starts within
    /// `ACTION_SETTLE_TIMEOUT`, wait for that page's load event (bounded by
    /// `NAV_TIMEOUT`); if not, the settle window itself has given in-page
    /// updates time to land. `wait_for_navigation` can't do this — it
    /// returns at once whenever the current page is already loaded, which
    /// right after a click it almost always is.
    async fn act_and_settle(
        page: &Page,
        action: impl std::future::Future<Output = Result<(), String>>,
    ) -> Result<(), String> {
        let main_frame = page
            .mainframe()
            .await
            .map_err(|e| format!("failed to find the page's main frame: {e}"))?;
        let mut started = page
            .event_listener::<EventFrameStartedLoading>()
            .await
            .map_err(|e| format!("failed to watch for navigation: {e}"))?;
        let mut loaded = page
            .event_listener::<EventLoadEventFired>()
            .await
            .map_err(|e| format!("failed to watch for page loads: {e}"))?;
        action.await?;
        let navigation_started = tokio::time::timeout(ACTION_SETTLE_TIMEOUT, async {
            while let Some(event) = started.next().await {
                if Some(&event.frame_id) == main_frame.as_ref() {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if navigation_started {
            let _ = tokio::time::timeout(NAV_TIMEOUT, loaded.next()).await;
        }
        Ok(())
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
                )
                .route(
                    "/delayed",
                    axum::routing::get(|| async {
                        axum::response::Html(
                            "<html><body>\
                             <button onclick=\"setTimeout(() => { document.getElementById('late').innerText = 'delayed-done'; }, 300)\">Later</button>\
                             <div id=\"late\">waiting</div>\
                             <a href=\"/slow\">Slow link</a>\
                             </body></html>",
                        )
                    }),
                )
                .route(
                    "/slow",
                    axum::routing::get(|| async {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        axum::response::Html("<html><body><h1>Slow page</h1></body></html>")
                    }),
                )
                .route(
                    "/form-fields",
                    axum::routing::get(|| async {
                        axum::response::Html(
                            "<html><body>\
                             <input id=\"prefilled\" type=\"text\" value=\"1\">\
                             <input id=\"pw\" type=\"password\">\
                             <textarea id=\"notes\"></textarea>\
                             </body></html>",
                        )
                    }),
                )
                .route(
                    "/input-events",
                    axum::routing::get(|| async {
                        axum::response::Html(
                            "<html><body>\
                             <div id=\"buttons-out\">no move yet</div>\
                             <form onsubmit=\"event.preventDefault(); document.getElementById('submit-out').innerText='submitted';\">\
                             <input id=\"q\" type=\"text\">\
                             </form>\
                             <div id=\"submit-out\">not sent</div>\
                             <script>document.addEventListener('mousemove', e => { document.getElementById('buttons-out').innerText = 'buttons=' + e.buttons; });</script>\
                             </body></html>",
                        )
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
                    match tokio::time::timeout(Duration::from_secs(3), sub.next_frame()).await {
                        Ok(Some(frame)) => {
                            assert!(!frame.data.is_empty(), "expected non-empty frame data");
                            received += 1;
                        }
                        Ok(None) => panic!("the session closed mid-stream"),
                        Err(_) => break,
                    }
                }
                assert!(
                    received >= 2,
                    "expected acking to keep frames flowing (got {received} frame(s)) — \
                     a stall after 1 would mean the ack isn't actually unblocking more"
                );
            } // `sub` drops here — the last viewer leaving stops the screencast.

            // Let the stop land, then confirm a second subscription gets
            // genuinely new frames (the screencast restarted, not just the
            // cached last frame).
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut second_sub = subscribe_frames(conversation_id)
                .expect("a second subscribe_frames should succeed");
            expect_fresh_frame(&mut second_sub).await;
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
                    left_held: false,
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

            // --- Scenario 11: a plain hover isn't a drag — a mouse move
            // with no button held reaches the page with `buttons == 0`. ---
            navigate(conversation_id, &format!("{base}/input-events"))
                .await
                .expect("navigate to the input-events page should succeed");
            send_input(
                conversation_id,
                BrowserInputEvent::MouseMove { x: 200.0, y: 300.0, left_held: false },
            )
            .await
            .expect("mouse move should succeed");
            let after_hover = read(conversation_id).await.expect("read should succeed");
            assert!(
                after_hover.text.contains("buttons=0"),
                "expected a plain hover to report no held buttons, got: {:?}",
                after_hover.text
            );

            // --- Scenario 12: Enter in a form's text field submits it,
            // the same as a real keyboard's Enter would. ---
            let page = live_page(conversation_id).expect("session should still be live");
            let field = page.find_element("#q").await.expect("should find #q");
            let field_point = field.clickable_point().await.expect("clickable point for #q");
            for event in [
                BrowserInputEvent::MouseDown { x: field_point.x, y: field_point.y },
                BrowserInputEvent::MouseUp { x: field_point.x, y: field_point.y },
                BrowserInputEvent::PressKey { key: "Enter".to_string(), modifiers: 0 },
            ] {
                send_input(conversation_id, event).await.expect("input should succeed");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            let after_enter = read(conversation_id).await.expect("read should succeed");
            assert!(
                after_enter.text.contains("submitted"),
                "expected Enter to submit the form, got: {:?}",
                after_enter.text
            );

            // --- Scenario 13: a viewer leaving and another arriving right
            // away (a panel re-render, a quick conversation switch back)
            // never leaves the new viewer without frames, whatever the
            // timing between the two. ---
            navigate_to_animated_page(conversation_id).await;
            for delay_ms in [0, 1, 5, 20, 50] {
                let mut sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
                expect_fresh_frame(&mut sub).await;
                drop(sub);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let mut last_sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
            expect_fresh_frame(&mut last_sub).await;
            drop(last_sub);

            // --- Scenario 14: a subscription left over from a *closed*
            // session can't affect the next session opened for the same
            // conversation when it finally drops. ---
            let stale_sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
            close_session(conversation_id).await.expect("close should succeed");
            open_session_with_guard(conversation_id, allow_loopback_too)
                .await
                .expect("re-open should succeed");
            navigate_to_animated_page(conversation_id).await;
            let mut fresh_sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
            expect_fresh_frame(&mut fresh_sub).await;
            drop(stale_sub);
            tokio::time::sleep(Duration::from_millis(300)).await;
            expect_fresh_frame(&mut fresh_sub).await;
            drop(fresh_sub);

            // --- Scenario 15: the live panel's frame stream releases its
            // viewer as soon as the stream is dropped (the SSE connection
            // closing), so a departed viewer doesn't keep the screencast
            // running for the rest of the session. ---
            assert_eq!(frame_subscriber_count(conversation_id), 0);
            let mut stream = Box::pin(frame_stream(
                subscribe_frames(conversation_id).expect("subscribe should succeed"),
            ));
            assert_eq!(frame_subscriber_count(conversation_id), 1);
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("the stream should yield a frame")
                .expect("the stream should not end")
                .expect("frames are never errors");
            drop(stream);
            assert_eq!(frame_subscriber_count(conversation_id), 0);

            close_session(conversation_id)
                .await
                .expect("close_session should succeed");

            // --- Scenario 16: two opens racing for the same conversation
            // — exactly one wins; the other is refused rather than
            // silently replacing (and leaking) the first. ---
            let (first, second) = tokio::join!(
                open_session_with_guard(conversation_id, allow_loopback_too),
                open_session_with_guard(conversation_id, allow_loopback_too),
            );
            assert_eq!(
                [first.is_ok(), second.is_ok()].iter().filter(|ok| **ok).count(),
                1,
                "expected exactly one of two racing opens to succeed, got {first:?} / {second:?}"
            );

            close_session(conversation_id)
                .await
                .expect("close_session should succeed");

            open_session_with_guard(conversation_id, allow_loopback_too)
                .await
                .expect("open for the remaining scenarios should succeed");

            // --- Scenario 17: only http/https can be navigated to — a
            // data: URL never reaches the network, so the request
            // interceptor never sees it and can't be what refuses it. ---
            let data_nav =
                navigate(conversation_id, "data:text/html,<h1>DATA-SCHEME-LOADED</h1>").await;
            assert!(
                data_nav.is_err(),
                "expected a data: URL to be refused, got: {data_nav:?}"
            );

            // --- Scenario 18: what someone types into a password field
            // (say, logging in through the live panel) never shows up in
            // the element list sent to the model. ---
            navigate(conversation_id, &format!("{base}/form-fields"))
                .await
                .expect("navigate to the form-fields page should succeed");
            let page = live_page(conversation_id).expect("session should still be live");
            let pw = page.find_element("#pw").await.expect("should find #pw");
            let pw_point = pw.clickable_point().await.expect("clickable point for #pw");
            for event in [
                BrowserInputEvent::MouseDown { x: pw_point.x, y: pw_point.y },
                BrowserInputEvent::MouseUp { x: pw_point.x, y: pw_point.y },
                BrowserInputEvent::TypeText { text: "hunter2secret".to_string() },
            ] {
                send_input(conversation_id, event).await.expect("input should succeed");
            }
            let after_password = read(conversation_id).await.expect("read should succeed");
            assert!(
                !format!("{after_password:?}").contains("hunter2secret"),
                "a typed password leaked into the page state: {:?}",
                after_password.elements
            );

            // --- Scenario 19: fill replaces a field's existing value
            // rather than appending to it, including filling it with
            // nothing. ---
            let prefilled_index = after_password
                .elements
                .iter()
                .find(|e| e.label == "1")
                .map(|e| e.index)
                .expect("the prefilled input should be in the element list");
            fill(conversation_id, prefilled_index, "5")
                .await
                .expect("fill should succeed");
            assert_eq!(field_value(&page, "#prefilled").await, "5");
            let state = read(conversation_id).await.expect("read should succeed");
            let prefilled_index = state
                .elements
                .iter()
                .find(|e| e.label == "5")
                .map(|e| e.index)
                .expect("the filled input should be in the element list");
            fill(conversation_id, prefilled_index, "")
                .await
                .expect("filling with nothing should succeed");
            assert_eq!(field_value(&page, "#prefilled").await, "");

            // --- Scenario 19b: named keys keep their modifiers — Shift+Tab
            // moves focus backwards, Ctrl+Backspace deletes a word. ---
            let state = read(conversation_id).await.expect("read should succeed");
            let prefilled_index = state
                .elements
                .iter()
                .find(|e| e.tag == "input" && e.label.is_empty())
                .map(|e| e.index)
                .expect("the emptied input should be in the element list");
            fill(conversation_id, prefilled_index, "hello world")
                .await
                .expect("fill should succeed");
            let press = |key: &str, modifiers: i64| BrowserInputEvent::PressKey {
                key: key.to_string(),
                modifiers,
            };
            send_input(conversation_id, press("Backspace", 2))
                .await
                .expect("ctrl+backspace should succeed");
            assert_eq!(field_value(&page, "#prefilled").await, "hello ");
            send_input(conversation_id, press("Tab", 0)).await.expect("tab should succeed");
            send_input(conversation_id, press("Tab", 8))
                .await
                .expect("shift+tab should succeed");
            let focused: String = page
                .evaluate("document.activeElement.id")
                .await
                .expect("evaluate should succeed")
                .into_value()
                .expect("id should be a string");
            assert_eq!(focused, "prefilled", "Shift+Tab should move focus back");

            // --- Scenario 20: a second viewer joining a screencast that's
            // already running gets a frame straight away, even on a page
            // where nothing is changing (Chrome only sends a new frame
            // when something on screen changes). ---
            // page2 has no inputs, so no blinking caret keeps frames coming.
            navigate(conversation_id, &format!("{base}/page2"))
                .await
                .expect("navigate to a static page should succeed");
            let mut first_viewer = subscribe_frames(conversation_id).expect("subscribe should succeed");
            tokio::time::timeout(Duration::from_secs(5), first_viewer.next_frame())
                .await
                .expect("the first viewer should get a frame")
                .expect("the session should still be open");
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let mut second_viewer = subscribe_frames(conversation_id).expect("subscribe should succeed");
            let joined = tokio::time::timeout(Duration::from_secs(3), second_viewer.next_frame()).await;
            assert!(
                matches!(joined, Ok(Some(_))),
                "a viewer joining a running screencast on a static page got no frame: {joined:?}"
            );
            drop(first_viewer);
            drop(second_viewer);

            close_session(conversation_id)
                .await
                .expect("close_session should succeed");

            // --- Scenario 21: a close that arrives while an open is still
            // in progress (deleting the conversation mid-open) wins —
            // the session doesn't reappear once the open finishes. ---
            let (opened, _) = tokio::join!(
                open_session_with_guard(conversation_id, allow_loopback_too),
                async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    close_session(conversation_id).await
                },
            );
            opened.expect("the open itself should succeed");
            assert!(
                current_url(conversation_id).is_none(),
                "a close issued during an open left the session running"
            );
            close_session(conversation_id)
                .await
                .expect("close_session should succeed");

            open_session_with_guard(conversation_id, allow_loopback_too)
                .await
                .expect("open for the remaining scenarios should succeed");

            // --- Scenario 22: a click waits for what it set off. A delayed
            // update (a fetch, a tab switch) shows up in the returned
            // state, and a click that navigates returns the *new* page even
            // when that page is slow to respond. ---
            let state = navigate(conversation_id, &format!("{base}/delayed"))
                .await
                .expect("navigate to the delayed page should succeed");
            let index_of = |state: &PageState, label: &str| {
                state
                    .elements
                    .iter()
                    .find(|e| e.label == label)
                    .map(|e| e.index)
                    .unwrap_or_else(|| panic!("no element labelled {label:?}: {:?}", state.elements))
            };
            let after_later = click(conversation_id, index_of(&state, "Later"))
                .await
                .expect("clicking Later should succeed");
            assert!(
                after_later.text.contains("delayed-done"),
                "expected the click's delayed update in the returned state, got: {:?}",
                after_later.text
            );
            let after_slow = click(conversation_id, index_of(&after_later, "Slow link"))
                .await
                .expect("clicking the slow link should succeed");
            assert!(
                after_slow.url.ends_with("/slow") && after_slow.text.contains("Slow page"),
                "expected the slow page after a navigating click, got {} / {:?}",
                after_slow.url,
                after_slow.text
            );

            // --- Scenario 23: fill takes any text, not just what a US
            // keyboard can type — accents, CJK, emoji, and newlines in a
            // textarea. ---
            let state = navigate(conversation_id, &format!("{base}/form-fields"))
                .await
                .expect("navigate to the form-fields page should succeed");
            let page = live_page(conversation_id).expect("session should still be live");
            let international = "Zürich 東京 🎉";
            fill(conversation_id, index_of(&state, "1"), international)
                .await
                .expect("filling non-ASCII text should succeed");
            assert_eq!(field_value(&page, "#prefilled").await, international);
            let state = read(conversation_id).await.expect("read should succeed");
            let notes_index = state
                .elements
                .iter()
                .find(|e| e.tag == "textarea")
                .map(|e| e.index)
                .expect("the textarea should be in the element list");
            fill(conversation_id, notes_index, "line one\nline two")
                .await
                .expect("filling multi-line text should succeed");
            assert_eq!(field_value(&page, "#notes").await, "line one\nline two");

            // --- Scenario 24: nothing a page does can reach an address the
            // guard refuses — not a popup (window.open or a target=_blank
            // link), not a WebSocket, not a service worker. The forbidden
            // server listens on this machine's own private address, which
            // even the test's loopback-allowing guard refuses. ---
            let forbidden = start_forbidden_server().await;
            let (escape_url, _escape_server) = start_escape_test_server(&forbidden).await;
            let state = navigate(conversation_id, &escape_url)
                .await
                .expect("navigate to the escape page should succeed");
            let open_pages = || async {
                crate::webfetch::shared_browser()
                    .await
                    .expect("shared browser")
                    .pages()
                    .await
                    .expect("list pages")
                    .len()
            };
            let pages_before = open_pages().await;
            click(conversation_id, index_of(&state, "Popup"))
                .await
                .expect("clicking the popup button should succeed");
            let state = read(conversation_id).await.expect("read should succeed");
            click(conversation_id, index_of(&state, "Blank link"))
                .await
                .expect("clicking the target=_blank link should succeed");
            tokio::time::sleep(Duration::from_secs(3)).await;
            let hits = forbidden.hits.lock().unwrap().clone();
            assert!(
                hits.is_empty(),
                "a page reached the forbidden address {}: {hits:?}",
                forbidden.addr
            );
            assert_eq!(
                open_pages().await,
                pages_before,
                "a popup tab was opened in the shared browser"
            );

            // --- Scenario 25: the session reports every URL change as it
            // happens — the model navigating, a link click, and an in-page
            // change (`pushState`) that never loads a new document — so the
            // live panel's address bar can follow along. ---
            let mut events = crate::events::subscribe(conversation_id);
            let start_url = format!("{base}/");
            navigate(conversation_id, &start_url)
                .await
                .expect("navigate should succeed");
            expect_url_event(&mut events, &start_url).await;
            assert_eq!(current_url(conversation_id).as_deref(), Some(start_url.as_str()));
            let state = read(conversation_id).await.expect("read should succeed");
            click(conversation_id, index_of(&state, "Go to page 2"))
                .await
                .expect("clicking the page 2 link should succeed");
            expect_url_event(&mut events, &format!("{base}/page2")).await;
            live_page(conversation_id)
                .expect("session should still be live")
                .evaluate("history.pushState({}, '', '/pushed')")
                .await
                .expect("pushState should succeed");
            expect_url_event(&mut events, &format!("{base}/pushed")).await;
            assert_eq!(
                current_url(conversation_id),
                Some(format!("{base}/pushed"))
            );
            // A refused load shows Chrome's own error page, whose URL is
            // `chrome-error://…` — the address bar should keep showing
            // what was asked for, as a real browser does.
            let refused = "http://169.254.169.254/";
            assert!(navigate(conversation_id, refused).await.is_err());
            expect_url_event(&mut events, refused).await;
            assert_eq!(current_url(conversation_id).as_deref(), Some(refused));

            close_session(conversation_id)
                .await
                .expect("final close_session should succeed");
        }

        /// A server on this machine's own private (non-loopback) address,
        /// logging every request that reaches it — anything logged got past
        /// the address guard.
        pub(crate) struct ForbiddenServer {
            pub(crate) addr: std::net::SocketAddr,
            pub(crate) hits: std::sync::Arc<Mutex<Vec<String>>>,
            _task: tokio::task::JoinHandle<()>,
        }

        pub(crate) async fn start_forbidden_server() -> ForbiddenServer {
            use axum::extract::ws::WebSocketUpgrade;
            let ip = {
                // Picks the outward-facing interface; UDP connect sends nothing.
                let probe = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind a probe socket");
                probe.connect("10.255.255.255:1").expect("route a probe socket");
                probe.local_addr().expect("probe address").ip()
            };
            assert!(
                !ip.is_loopback() && !allow_loopback_too(ip),
                "this test needs a private, non-loopback address the guard refuses; got {ip}"
            );
            let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
            let record = |hits: std::sync::Arc<Mutex<Vec<String>>>, what: &'static str| {
                move || async move {
                    hits.lock().unwrap().push(what.to_string());
                    "reached"
                }
            };
            let ws_hits = hits.clone();
            let router = axum::Router::new()
                .route("/popup", axum::routing::get(record(hits.clone(), "window.open popup")))
                .route("/blank", axum::routing::get(record(hits.clone(), "target=_blank link")))
                .route("/sw-hit", axum::routing::get(record(hits.clone(), "service worker fetch")))
                .route(
                    "/ws",
                    axum::routing::get(move |ws: WebSocketUpgrade| async move {
                        ws_hits.lock().unwrap().push("websocket".to_string());
                        ws.on_upgrade(|_socket| async {})
                    }),
                );
            let listener = tokio::net::TcpListener::bind((ip, 0))
                .await
                .expect("bind on the private address");
            let addr = listener.local_addr().expect("local addr");
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.expect("forbidden server error");
            });
            ForbiddenServer {
                addr,
                hits,
                _task: task,
            }
        }

        /// A page (on an allowed loopback address) that tries every way out
        /// to `forbidden` it can: a WebSocket and a service worker as soon
        /// as it loads, plus a window.open button and a target=_blank link.
        pub(crate) async fn start_escape_test_server(
            forbidden: &ForbiddenServer,
        ) -> (String, tokio::task::JoinHandle<()>) {
            let target = forbidden.addr;
            let page: &'static str = Box::leak(
                format!(
                    "<html><body>\
                     <button onclick=\"window.open('http://{target}/popup')\">Popup</button>\
                     <a href=\"http://{target}/blank\" target=\"_blank\">Blank link</a>\
                     <script>\
                     try {{ new WebSocket('ws://{target}/ws'); }} catch (e) {{}}\
                     if (navigator.serviceWorker) {{ navigator.serviceWorker.register('/sw.js'); }}\
                     </script>\
                     </body></html>"
                )
                .into_boxed_str(),
            );
            let worker: &'static str = Box::leak(
                format!(
                    "self.addEventListener('install', e => e.waitUntil(\
                     fetch('http://{target}/sw-hit').catch(() => {{}})));"
                )
                .into_boxed_str(),
            );
            let router = axum::Router::new()
                .route("/", axum::routing::get(move || async move { axum::response::Html(page) }))
                .route(
                    "/sw.js",
                    axum::routing::get(move || async move {
                        ([(axum::http::header::CONTENT_TYPE, "application/javascript")], worker)
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a test-local port");
            let port = listener.local_addr().expect("local addr").port();
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.expect("test server error");
            });
            (format!("http://127.0.0.1:{port}/"), task)
        }

        /// Waits for a `BrowsingUrlUpdate` carrying exactly `url`, skipping
        /// any other events (and URL updates for pages passed on the way).
        async fn expect_url_event(
            events: &mut tokio::sync::broadcast::Receiver<crate::events::ConversationEvent>,
            url: &str,
        ) {
            let found = tokio::time::timeout(Duration::from_secs(5), async {
                let mut seen = Vec::new();
                loop {
                    match events.recv().await {
                        Ok(crate::events::ConversationEvent::BrowsingUrlUpdate { url: got }) => {
                            if got == url {
                                return Ok(());
                            }
                            seen.push(got);
                        }
                        Ok(_) => {}
                        Err(e) => return Err(format!("event channel error: {e}; saw {seen:?}")),
                    }
                }
            })
            .await;
            match found {
                Ok(Ok(())) => {}
                Ok(Err(e)) => panic!("{e}"),
                Err(_) => panic!("no BrowsingUrlUpdate for {url} within 5s"),
            }
        }

        async fn field_value(page: &Page, selector: &str) -> String {
            page.evaluate(format!("document.querySelector('{selector}').value"))
                .await
                .expect("evaluate should succeed")
                .into_value()
                .expect("value should be a string")
        }

        /// Waits for a frame produced *after* this call — the current frame
        /// is marked seen first, so a stalled screencast can't pass on a
        /// stale one.
        async fn expect_fresh_frame(sub: &mut FrameSubscription) {
            sub.receiver.borrow_and_update();
            let frame = tokio::time::timeout(Duration::from_secs(5), sub.next_frame())
                .await
                .expect("expected a fresh frame within 5s — the screencast stalled")
                .expect("the session should still be open");
            assert!(!frame.data.is_empty());
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
            assert_eq!(
                named_key_event_fields("Enter").expect("Enter should be known"),
                NamedKey {
                    key: "Enter",
                    code: "Enter",
                    windows_virtual_key_code: 13,
                    text: Some("\r"),
                }
            );
        }

        #[test]
        fn test_named_key_event_fields_rejects_an_unknown_key() {
            assert!(named_key_event_fields("F13").is_err());
        }

        #[test]
        fn test_key_event_params_sends_enter_down_with_its_text() {
            let params = key_event_params("Enter", 0).expect("Enter should be known");
            assert_eq!(params.len(), 2);
            assert_eq!(params[0].r#type, DispatchKeyEventType::KeyDown);
            assert_eq!(params[0].text.as_deref(), Some("\r"));
            assert_eq!(params[1].r#type, DispatchKeyEventType::KeyUp);
            assert_eq!(params[1].text, None);
        }

        #[test]
        fn test_key_event_params_sends_a_non_typing_key_down_raw() {
            let params = key_event_params("Backspace", 0).expect("Backspace should be known");
            assert_eq!(params[0].r#type, DispatchKeyEventType::RawKeyDown);
            assert_eq!(params[0].text, None);
            assert_eq!(params[1].r#type, DispatchKeyEventType::KeyUp);
        }

        fn buttons_of(event: BrowserInputEvent) -> (Option<MouseButton>, Option<i64>) {
            let params = mouse_event_params(&event);
            (params.button, params.buttons)
        }

        use chromiumoxide::cdp::browser_protocol::input::MouseButton;

        #[test]
        fn test_mouse_event_params_hover_holds_no_button() {
            assert_eq!(
                buttons_of(BrowserInputEvent::MouseMove { x: 1.0, y: 2.0, left_held: false }),
                (Some(MouseButton::None), Some(0))
            );
        }

        #[test]
        fn test_mouse_event_params_drag_holds_the_left_button() {
            assert_eq!(
                buttons_of(BrowserInputEvent::MouseMove { x: 1.0, y: 2.0, left_held: true }),
                (Some(MouseButton::Left), Some(1))
            );
        }

        #[test]
        fn test_mouse_event_params_press_and_release_name_the_left_button() {
            assert_eq!(
                buttons_of(BrowserInputEvent::MouseDown { x: 1.0, y: 2.0 }),
                (Some(MouseButton::Left), Some(1))
            );
            assert_eq!(
                buttons_of(BrowserInputEvent::MouseUp { x: 1.0, y: 2.0 }),
                (Some(MouseButton::Left), Some(0))
            );
        }

        #[test]
        fn test_mouse_event_params_wheel_holds_no_button() {
            let params = mouse_event_params(&BrowserInputEvent::Wheel {
                x: 1.0,
                y: 2.0,
                delta_x: 0.0,
                delta_y: 120.0,
            });
            assert_eq!((params.button, params.buttons), (Some(MouseButton::None), Some(0)));
            assert_eq!(params.delta_y, Some(120.0));
        }

        #[test]
        fn test_normalize_address_adds_https_to_a_bare_host() {
            assert_eq!(normalize_address("example.com").unwrap(), "https://example.com");
            assert_eq!(
                normalize_address("localhost:8080/a?b=1").unwrap(),
                "https://localhost:8080/a?b=1"
            );
        }

        #[test]
        fn test_normalize_address_keeps_an_explicit_scheme_and_trims() {
            assert_eq!(normalize_address("  http://example.com/x  ").unwrap(), "http://example.com/x");
            assert_eq!(normalize_address("https://example.com").unwrap(), "https://example.com");
        }

        #[test]
        fn test_normalize_address_rejects_nothing() {
            assert!(normalize_address("").is_err());
            assert!(normalize_address("   ").is_err());
        }

        #[test]
        fn test_key_event_params_carries_modifiers_on_both_events() {
            let params = key_event_params("Tab", 8).expect("Tab should be known");
            assert!(params.iter().all(|p| p.modifiers == Some(8)));
        }

        fn frame(data: &str) -> Option<BrowserFrame> {
            Some(BrowserFrame {
                data: data.to_string(),
            })
        }

        /// A subscription not counted against any real session (no
        /// session has this id), so dropping it touches nothing.
        fn detached_subscription(
            rx: watch::Receiver<Option<BrowserFrame>>,
        ) -> FrameSubscription {
            FrameSubscription::new(
                rx,
                ViewerGuard {
                    conversation_id: -1,
                    session_id: 0,
                },
            )
        }

        #[tokio::test]
        async fn test_a_new_viewer_gets_the_latest_frame_straight_away() {
            let (tx, rx) = watch::channel(None);
            tx.send_replace(frame("a"));
            tx.send_replace(frame("b"));
            let mut sub = detached_subscription(rx);
            let first = tokio::time::timeout(Duration::from_millis(100), sub.next_frame()).await;
            assert_eq!(first.expect("should not wait"), frame("b"));
        }

        #[tokio::test]
        async fn test_a_slow_viewer_skips_to_the_newest_frame() {
            let (tx, rx) = watch::channel(frame("a"));
            let mut sub = detached_subscription(rx);
            assert_eq!(sub.next_frame().await, frame("a"));
            tx.send_replace(frame("b"));
            tx.send_replace(frame("c"));
            assert_eq!(sub.next_frame().await, frame("c"));
        }

        #[tokio::test]
        async fn test_a_viewer_waits_while_there_is_no_frame_yet() {
            let (tx, rx) = watch::channel(None);
            let mut sub = detached_subscription(rx);
            let early = tokio::time::timeout(Duration::from_millis(50), sub.next_frame()).await;
            assert!(early.is_err(), "should still be waiting, got {early:?}");
            tx.send_replace(frame("a"));
            assert_eq!(sub.next_frame().await, frame("a"));
        }

        #[tokio::test]
        async fn test_the_frame_stream_ends_when_the_session_closes() {
            let (tx, rx) = watch::channel(frame("a"));
            let mut stream = Box::pin(frame_stream(detached_subscription(rx)));
            assert!(matches!(stream.next().await, Some(Ok(_))));
            drop(tx);
            assert!(stream.next().await.is_none());
        }
    }
}

#[cfg(feature = "server")]
pub use server::{
    click, close_session, current_url, fill, frame_stream, go_back, navigate,
    normalize_address, open_session, read, send_input, subscribe_frames,
};

#[cfg(all(test, feature = "browser-test"))]
pub(crate) use server::browser_tests;
