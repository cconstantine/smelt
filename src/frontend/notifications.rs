//! SME-71: desktop notifications when a conversation needs the user.
//!
//! The server says a conversation went idle owing the user something
//! (`AppEvent::NeedsUser`); this file is everything a browser does with
//! that: the `Shell` layout that keeps the `Notifier` mounted on every
//! page, the leader election (one tab per browser holds the Web Lock and
//! does the notifying), the permission-on-click button in the sidebar,
//! and — where notifications can't work at all — the `(N) smelt` title
//! count every chat tab keeps for itself.
//!
//! Browser APIs come in through `document::eval` only (no new crate, the
//! way `clipboard.rs` works): one long-lived eval per tab is the channel
//! — commands go down with `Eval::send`, everything the browser observed
//! (its state, focus, clicks, lock changes) comes up as `dioxus.send`
//! messages, a tick every 300 ms keeping the leader's read of the setting,
//! and the permission fresh. The JS is deliberately dumb: it
//! reports and
//! constructs; every decision is one of the pure functions below, unit-
//! tested in the server build. `live.rs` feeds conversation-relayed
//! events in through the `NotifierFeed` context; the leader's own stream
//! arrives through the app-event queue.

// Most of this module — everything below the pure functions — runs only
// in the browser, so the server build sees most of it as dead.
#![cfg_attr(not(feature = "web"), allow(dead_code))]

use dioxus::prelude::*;

use crate::events::Attention;
use super::Route;

#[cfg(test)]
mod tests;

/// What `Notification.permission` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Permission {
    Granted,
    Denied,
    /// `"default"`: never asked, or the prompt was dismissed.
    NotAsked,
}

impl Permission {
    fn from_js(value: &str) -> Self {
        match value {
            "granted" => Permission::Granted,
            "denied" => Permission::Denied,
            _ => Permission::NotAsked,
        }
    }
}

/// What the sidebar button says and does, and whether the notifier runs
/// at all — the plan's mode table as data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotifyMode {
    /// Not a secure context, or no `Notification` or `navigator.locks`.
    Unavailable,
    /// `new Notification` threw (Chrome on Android).
    Unsupported,
    Blocked,
    Off,
    On,
}

impl NotifyMode {
    /// The button's label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            NotifyMode::Unavailable => "Desktop notifications need HTTPS",
            NotifyMode::Unsupported => "Notifications aren't supported in this browser",
            NotifyMode::Blocked => "Notifications blocked in site settings",
            NotifyMode::Off => "Turn on notifications",
            NotifyMode::On => "Notifications on",
        }
    }

    /// Only Off and On take a click.
    pub(crate) fn enabled(self) -> bool {
        matches!(self, NotifyMode::Off | NotifyMode::On)
    }

    /// The `title` the disabled Unavailable button carries, naming the
    /// one HTTPS origin that works as shipped.
    pub(crate) fn hint(self) -> Option<&'static str> {
        match self {
            NotifyMode::Unavailable => {
                Some("Notifications need a secure origin: https://localhost:8443 works as shipped")
            }
            _ => None,
        }
    }
}

/// The mode table: what the browser offers, what it allowed, what the
/// user set, and whether the constructor ever threw (`constructor_throws`
/// — learned by trying, never guessed). Unavailable outranks everything:
/// on plain HTTP the permission can well read "granted" from an earlier
/// secure visit, and none of it would work anyway.
pub(crate) fn notify_mode(
    secure_context: bool,
    has_notification: bool,
    has_locks: bool,
    permission: Permission,
    constructor_throws: bool,
    setting_on: bool,
) -> NotifyMode {
    if !secure_context || !has_notification || !has_locks {
        NotifyMode::Unavailable
    } else if constructor_throws {
        NotifyMode::Unsupported
    } else if permission == Permission::Denied {
        NotifyMode::Blocked
    } else if setting_on && permission == Permission::Granted {
        NotifyMode::On
    } else {
        NotifyMode::Off
    }
}

/// The leader's steps before showing (plan): the setting (off — another
/// tab may have turned it off), and watching this conversation with this
/// tab visible and focused (the user is right there). No kind has a
/// cooldown: the one there was held back the automatic Finished, which
/// SME-377 removed, and the model's `notify_user` is once per task. The
/// server's own re-check (`api::notifications`) comes after these; this
/// is everything in front of it.
pub(crate) fn should_notify(setting_on: bool, visible: bool, focused: bool, showing_this: bool) -> bool {
    if !setting_on {
        return false;
    }
    !(visible && focused && showing_this)
}

/// The notification's heading and body per kind: the heading names what
/// happened, the body names the conversation, plus the first question's
/// header where there is one. The model's own `notify_user` (SME-377)
/// puts the conversation in the heading and its `message` in the body,
/// falling back to the title when there's none.
pub(crate) fn notification_text(
    attention: Attention,
    title: &str,
    question_header: Option<&str>,
    message: Option<&str>,
) -> (String, String) {
    let heading = match attention {
        Attention::Question => "smelt has a question".to_string(),
        Attention::Failed => "smelt: the turn failed".to_string(),
        Attention::CutOff => "smelt: the reply was cut off".to_string(),
        Attention::Refused => "smelt: the model refused".to_string(),
        Attention::Notified => format!("smelt: {title}"),
    };
    let body = match (attention, question_header, message) {
        (Attention::Question, Some(header), _) if !header.is_empty() => format!("{title} — {header}"),
        (Attention::Notified, _, Some(message)) if !message.is_empty() => message.to_string(),
        _ => title.to_string(),
    };
    (heading, body)
}

/// The `tag`, so a second event for one conversation replaces its first
/// notification rather than stacking it (and so `renotify` works).
pub(crate) fn notification_tag(conversation_id: i64) -> String {
    format!("smelt-conversation-{conversation_id}")
}

/// The document title with the count of conversations that need the user
/// in front — the notification that works where notifications can't.
pub(crate) fn badge_title(count: usize) -> String {
    if count == 0 {
        "smelt".to_string()
    } else {
        format!("({count}) smelt")
    }
}

/// What `live.rs` hands the notifier through the `NotifierFeed` context:
/// a relayed `NeedsUser` this tab just saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NeedsUserEvent {
    pub(crate) conversation_id: i64,
    pub(crate) attention: Attention,
    pub(crate) generation: u64,
}

/// The feed `live.rs` writes and the `Notifier` reads: relayed
/// `NeedsUser`s for this tab's count (and, when this tab leads, for its
/// notifications), and a counter bumped on `TurnsChanged`/
/// `QuestionsChanged` to re-check everything already open.
#[derive(Clone, Copy)]
pub(crate) struct NotifierFeed {
    queue: Signal<Vec<NeedsUserEvent>>,
    revalidate: Signal<u64>,
}

impl NotifierFeed {
    /// The feed this tab's `Notifier` provides; `None` only above it in
    /// the tree, where there is nothing to feed.
    pub(crate) fn current() -> Option<NotifierFeed> {
        try_use_context::<NotifierFeed>()
    }

    pub(crate) fn needs_user(&self, event: NeedsUserEvent) {
        // `Signal` is `Copy`: this handle is a copy, and `.write()` is
        // taken on it, not on `self`.
        let mut queue = self.queue;
        let mut queue = queue.write();
        if queue.len() >= 256 {
            queue.remove(0);
        }
        queue.push(event);
    }

    pub(crate) fn bump_revalidate(&self) {
        let mut revalidate = self.revalidate;
        *revalidate.write() += 1;
    }
}

/// The button's and notifier's shared view: what mode the browser is in,
/// and the button's drained-by-the-loop click.
#[derive(Clone, Copy)]
struct NotifierStatus {
    mode: Signal<NotifyMode>,
    toggle: Signal<Option<ToggleRequest>>,
}

/// What the sidebar button clicked for.
#[derive(Clone, Copy, PartialEq)]
enum ToggleRequest {
    /// The click asked for permission: granted, or not (either way the
    /// loop re-probes; the answer moves the mode).
    On { granted: bool },
    Off,
}

/// The layout around every route (SME-71): the `Notifier` stays mounted
/// across navigation — one leader election and one eval channel per tab
/// — and every page, Home and the settings pages included, can be the
/// one that notifies.
#[component]
pub fn Shell() -> Element {
    // The feed and status live here, not in `Notifier`: a context reaches
    // only an ancestor's subtree, and the sidebar's `NotificationsToggle`
    // sits under `Outlet`, a sibling of `Notifier`. Provided by `Shell`,
    // every route below — and `Notifier` itself — can find them.
    let feed = use_context_provider(|| NotifierFeed {
        queue: Signal::new(Vec::new()),
        revalidate: Signal::new(0),
    });
    let status = use_context_provider(|| NotifierStatus {
        mode: Signal::new(NotifyMode::Off),
        toggle: Signal::new(None),
    });
    let _ = (feed, status);
    rsx! {
        Notifier {}
        Outlet::<Route> {}
    }
}

#[component]
fn Notifier() -> Element {
    let feed = use_context::<NotifierFeed>();
    let status = use_context::<NotifierStatus>();
    let app_queue: Signal<Vec<crate::events::AppEvent>> = use_context_provider(Signal::default);

    #[cfg(feature = "web")]
    {
        // Which conversation this tab shows, for "the user is looking at
        // it": the router read tracked in an effect, as `Chat`'s own.
        let mut shown = use_signal(|| None::<i64>);
        let router = router();
        use_effect(move || {
            shown.set(match router.current::<Route>() {
                Route::ConversationRoute { id } => Some(id),
                _ => None,
            });
        });
        let navigator = use_navigator();
        use_effect(move || {
            // The navigator arrives as a plain callback: `notifier` needs no
            // router type, and `Navigator` isn't nameable through the prelude.
            spawn(notifier(feed, status, app_queue, shown, move |id| {
                navigator.push(Route::ConversationRoute { id });
            }));
        });
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = (feed, status, app_queue);
    }

    rsx! {}
}

/// The sidebar's notifications button, among the settings links: the
/// one place the browser's permission prompt ever appears. Its label
/// follows the mode; only Off and On answer a click.
#[component]
pub(crate) fn NotificationsToggle() -> Element {
    let status = use_context::<NotifierStatus>();
    let mode = (status.mode)();
    let mut toggle = status.toggle;
    let label = mode.label();
    let hint = mode.hint();
    rsx! {
        button {
            class: "settings-link sandbox-volumes-link notifications-toggle",
            r#type: "button",
            disabled: !mode.enabled(),
            aria_pressed: (mode == NotifyMode::On).then_some("true"),
            title: hint,
            onclick: move |_| {
                #[cfg(feature = "web")]
                match mode {
                    NotifyMode::Off => {
                        // The prompt must live inside the click: dioxus-web
                        // runs handlers synchronously and starts an eval's
                        // script where it's made (`clipboard.rs`), so only
                        // the wait for its answer is spawned.
                        let answer = document::eval(PERMISSION_PROMPT_JS);
                        spawn(async move {
                            let granted = matches!(answer.await, Ok(serde_json::Value::String(s)) if s == "granted");
                            toggle.set(Some(ToggleRequest::On { granted }));
                        });
                    }
                    NotifyMode::On => toggle.set(Some(ToggleRequest::Off)),
                    _ => {}
                }
                #[cfg(not(feature = "web"))]
                let _ = &mut toggle;
            },
            "{label}"
        }
    }
}

/// The JS the toggle's click runs: the permission prompt itself, started
/// inside the gesture, its answer awaited by Rust.
#[cfg(feature = "web")]
const PERMISSION_PROMPT_JS: &str = "return await Notification.requestPermission();";

/// The tab's notifier agent: reports what it is and sees, holds or
/// releases the leader lock, and constructs and closes notifications —
/// every decision made by Rust. One per page load.
///
/// Commands down: `state`, `turnOn`, `setting {on}`, `steal`,
/// `notify {id, heading, body, tag}`, `close {id}`, `title {title, href}`.
/// Messages up: `state`, `tick`, `leader`, `notLeader`, `click {id}`.
#[cfg(feature = "web")]
const NOTIFIER_JS: &str = r#"
const KEY = "smelt.notifications";
const LOCK = "smelt-notifier";
const N = { notifications: new Map(), leader: false, pending: false, intentional: false, abort: null, throws: false, setting: false, dead: false, heldResolve: null };
// One notifier per page, newest wins: an early render-remount burst can
// boot this script several times before the page settles, and the older
// instances' lock requests (or even a granted `held` whose Rust task was
// cancelled under remount) would otherwise queue or hold behind a ghost
// forever. A new boot releases the previous instance's lock outright; the
// old loop quits at its next message and its catch neither posts nor
// re-requests (measured: 14 boots in ~350 ms on the providers page, 2 on
// a conversation page).
const prev = window.__smeltNotifier;
if (prev) {
  prev.dead = true;
  if (prev.heldResolve) { try { prev.heldResolve(); } catch (_) {} }
  if (prev.abort) { try { prev.abort.abort(); } catch (_) {} }
}
window.__smeltNotifier = N;
function post(m) { dioxus.send(m); }
function readSetting() { try { return localStorage.getItem(KEY) === "on"; } catch (_) { return false; } }
function writeSetting(on) { try { if (on) { localStorage.setItem(KEY, "on"); } else { localStorage.removeItem(KEY); } } catch (_) {} }
function state() {
  let permission = "default";
  const hasNotification = typeof Notification === "function";
  if (hasNotification) { permission = Notification.permission; }
  return { ev: "state", secure: !!self.isSecureContext, hasNotification, hasLocks: !!navigator.locks, permission, setting: N.setting, throws: N.throws };
}
function tick() { return { ev: "tick", setting: readSetting(), focused: document.hasFocus(), visible: document.visibilityState === "visible", permission: typeof Notification === "function" ? Notification.permission : "default" }; }
function releaseLock() {
  N.intentional = true;
  if (N.heldResolve) { N.heldResolve(); N.heldResolve = null; }
  if (N.abort) { N.abort.abort(); N.abort = null; }
  N.pending = false;
  const wasLeader = N.leader;
  N.leader = false;
  if (wasLeader) { post({ ev: "notLeader" }); }
}
function requestLock(steal) {
  if (!navigator.locks || N.leader) { return; }
  // A queued request cannot be promoted to a steal: the spec rejects
  // `signal` together with `steal` outright. Drop a queued one and ask
  // again — steal requests carry no signal, and are granted at once
  // (pre-empting the holder), so there is nothing to abort after that.
  const had = N.pending;
  if (N.abort) { N.intentional = true; N.abort.abort(); N.abort = null; N.pending = false; }
  else if (had && !steal) { return; }
  const ac = steal ? null : new AbortController();
  if (ac) { N.abort = ac; }
  N.pending = true;
  let resolveHeld;
  const held = new Promise((res, rej) => { resolveHeld = res; if (ac) { ac.signal.addEventListener("abort", rej); } });
  navigator.locks.request(LOCK, ac ? { signal: ac.signal } : {}, () => {
    if (N.dead) { throw new DOMException("replaced", "AbortError"); }
    N.pending = false; N.intentional = false; N.leader = true; N.heldResolve = resolveHeld;
    post({ ev: "leader" });
    return held;
  }).catch((e) => {
    if (N.dead) { return; }
    if (ac && N.abort !== ac) { return; }
    if (ac) { N.abort = null; }
    N.pending = false;
    const wasLeader = N.leader;
    N.leader = false;
    N.heldResolve = null;
    if (wasLeader) { post({ ev: "notLeader" }); }
    if (N.setting && !N.intentional) { requestLock(false); }
  });
}
N.setting = readSetting();
if (N.setting) { requestLock(false); }
// Turning the setting on makes every tab request the lock (plan): one already
// open queues behind the leader; turning it off releases the leader's.
window.addEventListener("focus", () => { if (N.dead) { return; } if (N.setting && !N.leader) { requestLock(true); } post(state()); });
document.addEventListener("visibilitychange", () => { if (N.dead) { return; } post(tick()); });
window.addEventListener("storage", (e) => { if (N.dead) { return; } if (!e.key || e.key === KEY) { N.setting = readSetting(); if (N.setting) { requestLock(false); } else { releaseLock(); } post(state()); } });
setInterval(() => { if (!N.dead) { post(tick()); } }, 300);
post(state());
for (;;) {
  const c = await dioxus.recv();
  if (N.dead) { break; }
  if (c.cmd === "state") { post(state()); }
  else if (c.cmd === "turnOn") {
    try {
      if (Notification.permission !== "granted") { post(state()); }
      else {
        new Notification("smelt", { body: "Notifications are on", tag: "smelt-hello" });
        N.setting = true; writeSetting(true); requestLock(document.hasFocus()); post(state());
      }
    } catch (_) { N.throws = true; post(state()); }
  }
  else if (c.cmd === "setting") { N.setting = !!c.on; writeSetting(!!c.on); if (c.on) { requestLock(false); } else { releaseLock(); } post(state()); }
  else if (c.cmd === "steal") { if (N.setting && !N.leader) { requestLock(true); } }
  else if (c.cmd === "notify") {
    if (!readSetting()) { post(state()); }
    else {
      try {
        const old = N.notifications.get(c.id);
        if (old) { try { old.close(); } catch (_) {} }
        const n = new Notification(c.heading, { body: c.body, tag: c.tag, renotify: true });
        // `onclick` so the browser-tier stub can fire a click through it.
        n.onclick = () => { window.focus(); post({ ev: "click", id: c.id }); };
        N.notifications.set(c.id, n);
      } catch (_) { N.throws = true; post(state()); }
    }
  }
  else if (c.cmd === "close") {
    const n = N.notifications.get(c.id);
    if (n) { N.notifications.delete(c.id); try { n.close(); } catch (_) {} }
  }
  else if (c.cmd === "title") {
    document.title = c.title;
    const l = document.querySelector("link[rel='icon']");
    if (l && c.href) { l.href = c.href; }
  }
}
"#;

/// What the page is, as its own JS last reported it: the mode's inputs
/// plus the visibility and clock the leader needs. Fresh to the last
/// `state` or `tick`; a notification's construction re-checks the setting
/// at the moment it would appear.
#[cfg(feature = "web")]
#[derive(Clone, Copy)]
struct Page {
    setting: bool,
    secure: bool,
    has_notification: bool,
    has_locks: bool,
    permission: Permission,
    throws: bool,
    visible: bool,
    focused: bool,
}

#[cfg(feature = "web")]
impl Page {
    fn mode(&self) -> NotifyMode {
        notify_mode(self.secure, self.has_notification, self.has_locks, self.permission, self.throws, self.setting)
    }
}

/// The tab's notifier: the JS half of the channel and every decision
/// made with its messages. One task owns all of it — the eval's receive
/// is the only thing it ever waits on, and the feed and app queues are
/// drained on every message — so no locks and no interleaving. Runs for
/// the life of the page.
#[cfg(feature = "web")]
async fn notifier(
    mut feed: NotifierFeed,
    mut status: NotifierStatus,
    mut app_queue: Signal<Vec<crate::events::AppEvent>>,
    shown: Signal<Option<i64>>,
    on_click: impl Fn(i64),
) {
    use std::collections::BTreeMap;

    let mut eval = document::eval(NOTIFIER_JS);
    // Eval is Copy; the sender and the receive loop below are its own
    // handles over the one JS script.
    let send = {
        let eval = eval;
        move |message: serde_json::Value| {
            let _ = eval.send(message);
        }
    };

    let mut page = Page {
        setting: false,
        secure: false,
        has_notification: false,
        has_locks: false,
        permission: Permission::NotAsked,
        throws: false,
        visible: true,
        focused: false,
    };
    let mut leader = false;
    let mut leader_task: Option<dioxus::prelude::dioxus_core::Task> = None;
    // Every conversation this tab knows needs the user, with the kind
    // and generation to re-check it with: the title count and, for the
    // leader, the notifications to revalidate. `open` marks which of them
    // this tab actually has a notification up for, with the event it was
    // shown for: a chat tab that also leads hears one event twice — its
    // own relay and the leader's stream — and one event shows one
    // notification.
    let mut needers: BTreeMap<i64, (Attention, u64)> = BTreeMap::new();
    let mut open: BTreeMap<i64, (Attention, u64)> = BTreeMap::new();
    let mut title_sent = String::new();
    let mut revalidate_seen: u64 = 0;
    let favicon_plain = asset!("/assets/favicon.svg").to_string();
    let favicon_dot = asset!("/assets/favicon-dot.svg").to_string();

    loop {
        let Ok(from_js) = eval.recv::<serde_json::Value>().await else { break };
        let event = from_js.get("ev").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();

        match event.as_str() {
            "state" => {
                page.secure = flag(&from_js, "secure");
                page.has_notification = flag(&from_js, "hasNotification");
                page.has_locks = flag(&from_js, "hasLocks");
                page.permission = from_js
                    .get("permission")
                    .and_then(serde_json::Value::as_str)
                    .map(Permission::from_js)
                    .unwrap_or(page.permission);
                page.throws = flag(&from_js, "throws");
                page.setting = flag(&from_js, "setting");
                set_mode(&mut status, page.mode());
            }
            "tick" => {
                page.setting = flag(&from_js, "setting");
                page.focused = flag(&from_js, "focused");
                page.visible = flag(&from_js, "visible");
                if let Some(permission) = from_js.get("permission").and_then(serde_json::Value::as_str) {
                    page.permission = Permission::from_js(permission);
                }
                set_mode(&mut status, page.mode());
            }
            "leader" => {
                leader = true;
                if leader_task.is_none() {
                    let queue = app_queue;
                    leader_task.replace(spawn(async move { leader_stream(queue).await; }));
                }
            }
            "notLeader" => {
                leader = false;
                // A demoted leader keeps the notifications it showed;
                // their revalidation now rides this tab's feed.
                if let Some(task) = leader_task.take() {
                    task.cancel();
                }
            }
            "click" => {
                if let Some(id) = num(&from_js, "id").map(|id| id as i64) {
                    on_click(id);
                    needers.remove(&id);
                    open.remove(&id);
                    send(serde_json::json!({ "cmd": "close", "id": id }));
                }
            }
            _ => continue,
        }

        // The toggle, if it was clicked since the last message. (Read
        // then clear as two statements: `peek`'s guard must be gone
        // before `set` borrows the signal again.)
        let request = *status.toggle.peek();
        if let Some(request) = request {
            status.toggle.set(None);
            match request {
                ToggleRequest::On { granted: true } => send(serde_json::json!({ "cmd": "turnOn" })),
                ToggleRequest::On { granted: false } => send(serde_json::json!({ "cmd": "state" })),
                ToggleRequest::Off => send(serde_json::json!({ "cmd": "setting", "on": false })),
            }
        }

        // Relayed `NeedsUser`s from this tab's conversation stream, and
        // app events from the leader's own stream. A conversation joins
        // the count only while the tab is in the background: focused and
        // visible, the user is reading it anyway.
        let pending: Vec<NeedsUserEvent> = feed.queue.write().drain(..).collect();
        for event in pending {
            if !(page.focused && page.visible) {
                needers.insert(event.conversation_id, (event.attention, event.generation));
            }
            if leader {
                notify_leader(&send, &page, &shown, &mut open, event).await;
            }
        }
        let queued: Vec<crate::events::AppEvent> = app_queue.write().drain(..).collect();
        for event in queued {
            match event {
                crate::events::AppEvent::NeedsUser { conversation_id, attention, generation } => {
                    let event = NeedsUserEvent { conversation_id, attention, generation };
                    if !(page.focused && page.visible) {
                        needers.insert(conversation_id, (attention, generation));
                    }
                    notify_leader(&send, &page, &shown, &mut open, event).await;
                }
                crate::events::AppEvent::TurnsChanged | crate::events::AppEvent::QuestionsChanged => {
                    revalidate_open(&send, &mut needers, &mut open).await;
                }
                crate::events::AppEvent::Unknown => *crate::frontend::STALE_BUNDLE.write() = true,
                _ => {}
            }
        }

        // Revalidation, the conversation stream's way too.
        let revalidate = *feed.revalidate.peek();
        if revalidate != revalidate_seen {
            revalidate_seen = revalidate;
            revalidate_open(&send, &mut needers, &mut open).await;
        }

        // This tab, looked at on a conversation: its count entry and any
        // notification of it are the user's now. Two independent
        // removals: a tab that was focused and visible when an event
        // arrived skipped the count entry by design and is owed the
        // notification anyway, so `open` can hold what `needers` lacks —
        // and the close rides the `open` removal alone. (The one `&&`
        // chain short-circuited on the missing count entry there, which
        // is the leader's normal state: review 1, Medium.)
        if page.focused && page.visible {
            if let Some(id) = *shown.peek() {
                needers.remove(&id);
                if open.remove(&id).is_some() {
                    send(serde_json::json!({ "cmd": "close", "id": id }));
                }
            }
        }

        // The title and favicon follow the count, which shows only while
        // the tab isn't looked at.
        let count = if page.focused && page.visible { 0 } else { needers.len() };
        let title = badge_title(count);
        if title != title_sent {
            title_sent = title.clone();
            send(serde_json::json!({
                "cmd": "title",
                "title": title,
                "href": if count == 0 { favicon_plain.as_str() } else { favicon_dot.as_str() },
            }));
        }
    }
}

#[cfg(feature = "web")]
fn flag(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// Moves the shared mode only when it actually changed: the loop hears
/// from the JS on every tick, and writing a `Signal` wakes its readers
/// whether or not the value moved — the button would re-render three
/// times a second for nothing.
#[cfg(feature = "web")]
fn set_mode(status: &mut NotifierStatus, mode: NotifyMode) {
    let current = *status.mode.peek();
    if current != mode {
        status.mode.set(mode);
    }
}

#[cfg(feature = "web")]
fn num(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

/// The leader's path for one event: the pure steps, then the server's
/// own "still true?" — and the notification, if both answer yes.
#[cfg(feature = "web")]
async fn notify_leader(
    send: &impl Fn(serde_json::Value),
    page: &Page,
    shown: &Signal<Option<i64>>,
    open: &mut std::collections::BTreeMap<i64, (Attention, u64)>,
    event: NeedsUserEvent,
) {
    let showing_this = *shown.peek() == Some(event.conversation_id);
    if !should_notify(page.setting, page.visible, page.focused, showing_this) {
        return;
    }
    // A denied or never-asked permission decides it here, in page, before
    // the round-trip: the constructor answers denied with a silent
    // no-display rather than a throw, so a leader whose permission the
    // user refused would otherwise spend a `get_attention` on every event
    // and accumulate `open` entries for notifications that never existed
    // (review 1). Leadership itself is untouched — one connection, and
    // the permission can flip back in site settings, which the tick
    // re-reads — and the sidebar button keeps the same disabled mode.
    if !matches!(page.mode(), NotifyMode::On) {
        return;
    }
    // The same event twice is one notification: on a chat tab that also
    // leads, the relay and the leader's stream both carry it.
    if open.get(&event.conversation_id) == Some(&(event.attention, event.generation)) {
        return;
    }
    let Ok(Some(details)) =
        crate::api::notifications::get_attention(event.conversation_id, event.generation, event.attention).await
    else {
        return;
    };
    let (heading, body) =
        // The server's kind, not the event's: a question resolved mid-spell
        // answers with the spell's notify_user instead (SME-377 review 1).
        notification_text(details.attention, &details.title, details.question_header.as_deref(), details.message.as_deref());
    send(serde_json::json!({
        "cmd": "notify",
        "id": event.conversation_id,
        "heading": heading,
        "body": body,
        "tag": notification_tag(event.conversation_id),
    }));
    open.insert(event.conversation_id, (event.attention, event.generation));
}

/// The leader's own stream: app events into the queue, reconnecting like
/// `use_pods_changed`; deliberately no catch-up (a waiting question
/// keeps its card and the sidebar's `?` — nothing is re-announced).
#[cfg(feature = "web")]
async fn leader_stream(mut app_queue: Signal<Vec<crate::events::AppEvent>>) {
    loop {
        if let Ok(mut events) = crate::api::pods::subscribe_app_events().await {
            crate::frontend::check_build_id().await;
            while let Some(Ok(event)) = events.recv().await {
                app_queue.write().push(event);
            }
        }
        gloo_timers::future::TimeoutFuture::new(1500).await;
    }
}

/// Every entry re-checked against the server on its own terms: answered
/// elsewhere, a newer turn, a deleted conversation — the notification
/// closes and the count drops. `needers` and `open` are checked with
/// *their own* kind and generation: a stale count entry must not close a
/// fresh notification of the same conversation (one event arrives twice,
/// relay and stream, and a `TurnsChanged` between the two would otherwise
/// clear the dedupe and let the second through — the third notification
/// the headless run measured).
#[cfg(feature = "web")]
async fn revalidate_open(
    send: &impl Fn(serde_json::Value),
    needers: &mut std::collections::BTreeMap<i64, (Attention, u64)>,
    open: &mut std::collections::BTreeMap<i64, (Attention, u64)>,
) {
    let ids: std::collections::BTreeSet<i64> = needers.keys().chain(open.keys()).copied().collect();
    for id in ids {
        // A failed request says nothing: a hiccup (dev-server restart,
        // a dropped connection) must not close a live notification or
        // empty the count. "Stale" is a fetched `None`; an error leaves
        // the entry, and the next `QuestionsChanged`/`TurnsChanged`
        // re-checks it (review 1).
        if let Some((attention, generation)) = needers.get(&id).copied() {
            let checked = crate::api::notifications::get_attention(id, generation, attention).await;
            if matches!(checked, Ok(None)) {
                needers.remove(&id);
            }
        }
        if let Some((attention, generation)) = open.get(&id).copied() {
            let checked = crate::api::notifications::get_attention(id, generation, attention).await;
            if matches!(checked, Ok(None)) {
                open.remove(&id);
                send(serde_json::json!({ "cmd": "close", "id": id }));
            }
        }
    }
}
