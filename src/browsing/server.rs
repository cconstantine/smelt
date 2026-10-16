use std::collections::HashMap;

use crate::egress_proxy::SandboxDial;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
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
/// already established — "bound the boundaries" (docs/process/rules-code.md).
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
/// real page; see SME-22's "Exact frame quality/size/rate defaults."
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

/// The session list, locked. Every production use goes through this,
/// never `SESSIONS.lock()`: a call that panicked while holding the
/// lock poisons it, and without recovery every later browsing call in
/// every conversation would panic too, until a restart (SME-119). No
/// critical section leaves the map in a state another call would trip
/// over (the worst is a viewer count one too high, so a screencast
/// runs until its session closes, or a leaked context), so carrying on
/// with it as it was left is safe. `clear_poison` makes the warning
/// once per panic, not once per call. A lock is only ever poisoned
/// where panics unwind (`dx serve`, the tests): the release profile has
/// `panic = "abort"`, so there any panic ends the process (SME-139).
fn sessions() -> MutexGuard<'static, HashMap<i64, Session>> {
    SESSIONS.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(
            "a browsing call panicked while holding the session list; carrying on with it as it was left"
        );
        SESSIONS.clear_poison();
        poisoned.into_inner()
    })
}

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
    /// This session's own browser context — see `open_session`.
    context: crate::webfetch::IsolatedContext,
    page: Page,
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
/// exists" precedent; `close_session` first). With `sandbox`, the
/// session's browser context sends `localhost`-style hosts and pod
/// container addresses to the conversation's sandbox (SME-42, SME-33).
pub async fn open_session(
    conversation_id: i64,
    sandbox: Option<SandboxDial>,
) -> Result<(), String> {
    let _lifecycle = SESSION_LIFECYCLE.lock().await;
    if sessions().contains_key(&conversation_id) {
        return Err(
            "a browsing session is already open for this conversation — call \
                 close_browser_session first"
                .to_string(),
        );
    }
    // Each session gets a browser context of its own, so no two
    // conversations ever share cookies, storage or service workers, and
    // closing the session deletes everything it stored.
    let (page, context) = crate::webfetch::new_isolated_page(sandbox).await?;
    if let Err(e) = configure_session_page(&page).await {
        crate::webfetch::dispose_isolated(context).await;
        return Err(e);
    }
    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let url_task = match watch_url(conversation_id, session_id, &page).await {
        Ok(task) => task,
        Err(e) => {
            crate::webfetch::dispose_isolated(context).await;
            return Err(e);
        }
    };
    let (frame_tx, latest_frame) = watch::channel(None);
    let (want_screencast, want_rx) = watch::channel(false);
    let screencast_task = tokio::spawn(run_screencast(page.clone(), frame_tx, want_rx));
    sessions().insert(
        conversation_id,
        Session {
            id: session_id,
            context,
            current_url: "about:blank".to_string(),
            url_task,
            page,
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

/// Everything a fresh session page needs before it's handed out: pin
/// the viewport. Split out so `open_session` can close the page if the
/// step fails, rather than leaking it in the shared browser.
async fn configure_session_page(page: &Page) -> Result<(), String> {
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
    .map(|_| ())
    .map_err(|e| format!("failed to set the session's viewport: {e}"))
}

/// Closes `conversation_id`'s browsing session — a no-op (not an
/// error) if none is open, matching `terminate_pod`'s own "already
/// gone is fine" precedent. Also stops the screencast (if any
/// live-panel viewer was watching) — there's no subscriber left to
/// notify, just real CDP/process state to tear down.
pub async fn close_session(conversation_id: i64) -> Result<(), String> {
    let _lifecycle = SESSION_LIFECYCLE.lock().await;
    let session = sessions().remove(&conversation_id);
    if let Some(session) = session {
        session.screencast_task.abort();
        session.url_task.abort();
        // Closes the page and deletes everything the session stored.
        crate::webfetch::dispose_isolated(session.context).await;
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
    let mut sessions = sessions();
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
    let mut sessions = sessions();
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
        let mut sessions = sessions();
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
    sessions()
        .get(&conversation_id)
        .map(|s| s.current_url.clone())
}

/// What someone typed into the live panel's address bar, as a URL. With
/// no scheme typed it picks one as Chromium's omnibox does (SME-184):
/// `http://` for an IP literal, an explicit port other than 443, a
/// single-label name or a local-only suffix (`localhost:5173`, a dev
/// server in the sandbox), `https://` for anything else (`example.com`),
/// and for input that doesn't parse. Whether the result is a URL smelt
/// will actually load is `navigate`'s call, not this one's.
pub fn normalize_address(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("enter an address".to_string());
    }
    if has_scheme(input) {
        return Ok(input.to_string());
    }
    let scheme = match url::Url::parse(&format!("http://{input}")) {
        Ok(url) if is_local_address(&url, input) => "http",
        _ => "https",
    };
    Ok(format!("{scheme}://{input}"))
}

/// Whether `input` starts with a scheme and `://`: a `://` later on (a
/// return URL in the query, review 1) doesn't count. A scheme is a letter
/// then letters, digits, `+`, `-` or `.`, as RFC 3986 has it.
fn has_scheme(input: &str) -> bool {
    input.split_once("://").is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    })
}

/// Whether Chromium would keep a typed `url` on plain http: an IP, a
/// typed port (443 excepted: there https is what can work), or a
/// name outside the public DNS. A short suffix list stands in for the
/// Public Suffix List; `.home.arpa` is home-only, and smelt has no
/// fallback to http if https fails, so it gets http too.
fn is_local_address(url: &url::Url, input: &str) -> bool {
    let name = match url.host() {
        Some(url::Host::Domain(name)) => name,
        Some(_) => return true,
        None => return false,
    };
    // `port()` hides the scheme's default port, so a typed :80 shows only
    // when parsed as https (review 1: https on port 80 can't load).
    let https_port = || url::Url::parse(&format!("https://{input}")).ok()?.port();
    if let Some(port) = url.port().or_else(https_port) {
        return port != 443;
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    const LOCAL: [&str; 6] = ["localhost", "local", "lan", "internal", "test", "home.arpa"];
    !name.contains('.')
        || LOCAL
            .iter()
            .any(|suffix| name.strip_suffix(suffix).is_some_and(|rest| rest.ends_with('.')))
}

#[cfg(all(test, feature = "browser-test"))]
fn frame_subscriber_count(conversation_id: i64) -> usize {
    sessions()
        .get(&conversation_id)
        .map_or(0, |s| s.frame_subscriber_count)
}

fn live_page(conversation_id: i64) -> Result<Page, String> {
    sessions()
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
#[expect(clippy::expect_used, reason = "the builder's required fields (type, x and y) are always set")]
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
    let page = live_page(conversation_id)?;
    // Checked before loading, so a bad URL leaves the session on its
    // current page instead of Chrome's blank error page, and the error
    // says why (SME-40 F11). Only the scheme is checked — addresses
    // aren't filtered any more (SME-155) — and a `data:` (or similar)
    // URL never reaches the network, so this pre-load check is what
    // refuses it.
    fetch_guard::parse_fetch_target(url).map_err(|e| format!("refused {url}: {e}"))?;
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
pub(crate) mod browser_tests;

#[cfg(test)]
mod tests;
