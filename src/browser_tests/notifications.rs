//! SME-71: desktop notifications when a conversation needs the user.
//!
//! `Runs::Alone`: the leader holds the app-wide stream, so another lane's
//! turn ends would land in this scenario's notification counts and titles;
//! and it's the only scenario that touches a browser-wide setting (the
//! notifications switch and the permission behind it).
//!
//! The `Notification` constructor is a stub (`evaluate_on_new_document`,
//! every tab): headless shell grants no real permission and shows nothing
//! a test can see, so the stub records every construction — heading, body,
//! tag, `renotify` — and counts `close()`s, exactly where the plan put it.
//! Focus and visibility are the stub's too: leadership and the title count
//! must not depend on which headless tab the browser thinks is active, so
//! `__smeltSetFocus` sets what `hasFocus`/`visibilityState` answer and
//! dispatches the `focus`/`visibilitychange` events smelt's notifier
//! listens for (the plan's sanctioned forcing of the focus path; the real
//! focus changes belong to the hands-on check).

use super::*;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;

/// The stub: a `Notification` class recording constructions, and the
/// focus/visibility overrides. Every tab gets it before smelt's code runs
/// (and again on every new document, so reloads and navigations keep it).
const NOTIFICATION_STUB: &str = r#"
(() => {
  if (window.__smeltNotifierStubbed) return;
  window.__smeltNotifierStubbed = true;
  window.__notifications = [];
  window.__unhandled = [];
  window.addEventListener("unhandledrejection", (e) => window.__unhandled.push(String(e.reason)));
  class FakeNotification {
    static permission = "granted";
    static async requestPermission() { return "granted"; }
    constructor(heading, options = {}) {
      this.heading = heading;
      this.body = (options && options.body) || "";
      this.tag = (options && options.tag) || "";
      this.renotify = !!(options && options.renotify);
      this.closed = 0;
      this.onclick = null;
      window.__notifications.push(this);
    }
    close() { this.closed += 1; }
  }
  window.Notification = FakeNotification;
  const realHasFocus = document.hasFocus.bind(document);
  Object.defineProperty(document, "hasFocus", {
    configurable: true,
    value: () => (window.__smeltFocus === undefined ? realHasFocus() : window.__smeltFocus),
  });
  const realVisible = Object.getOwnPropertyDescriptor(Document.prototype, "visibilityState").get.bind(document);
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => (window.__smeltVisible === undefined ? realVisible() : (window.__smeltVisible ? "visible" : "hidden")),
  });
  window.__smeltSetFocus = (focus, visible) => {
    window.__smeltFocus = focus;
    window.__smeltVisible = visible;
    document.dispatchEvent(new Event("visibilitychange"));
    if (focus) window.dispatchEvent(new Event("focus"));
  };
})();
"#;

/// One recorded construction, as the stub saved it.
#[derive(Debug, serde::Deserialize)]
struct Recorded {
    heading: String,
    body: String,
    tag: String,
    renotify: bool,
    closed: i64,
}

/// A tab with the stub in: installed for every document the tab loads,
/// and on the one that just loaded, before its WASM has booted.
pub(super) async fn stubbed_tab(t: &Scenario<'_>, url: impl Into<String>) -> chromiumoxide::Page {
    let page = t.tab(url).await;
    page.evaluate_on_new_document(
        chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams::new(NOTIFICATION_STUB.to_string()),
    )
    .await
    .expect("stub notifications on every new document");
    page.evaluate(NOTIFICATION_STUB).await.expect("stub notifications now");
    page
}

/// What the tab's stub has recorded.
async fn recorded(page: &chromiumoxide::Page) -> Vec<Recorded> {
    let json: String = page
        .evaluate("JSON.stringify(window.__notifications)")
        .await
        .expect("read the stub's records")
        .into_value()
        .expect("a string");
    serde_json::from_str(&json).expect("the stub's records")
}

/// How many notifications `page` has recorded with `tag`.
pub(super) async fn tag_count(page: &chromiumoxide::Page, tag: &str) -> usize {
    recorded(page).await.iter().filter(|n| n.tag == tag).count()
}

/// The promise rejections nothing handled in this document (SME-386 F9):
/// each is a `pageerror` in a real page.
pub(super) async fn unhandled_rejections(page: &chromiumoxide::Page) -> Vec<String> {
    page.evaluate("window.__unhandled")
        .await
        .expect("read the stub's rejections")
        .into_value()
        .expect("a list of strings")
}

/// Waits until `page` has recorded `count` notifications with `tag`.
pub(super) async fn wait_tag_count(page: &chromiumoxide::Page, tag: &str, count: usize, timeout: Duration) -> bool {
    let tag = tag.to_string();
    wait_until(|| async { recorded(page).await.iter().filter(|n| n.tag == tag).count() == count }, timeout).await
}

/// Waits until the last notification recorded for `tag` with `heading`
/// has had `close()` called.
pub(super) async fn wait_last_closed(page: &chromiumoxide::Page, tag: &str, heading: &str, timeout: Duration) -> bool {
    let (tag, heading) = (tag.to_string(), heading.to_string());
    wait_until(
        || async {
            recorded(page)
                .await
                .into_iter()
                .filter(|n| n.tag == tag && n.heading == heading)
                .last()
                .is_some_and(|n| n.closed > 0)
        },
        timeout,
    )
    .await
}

/// What the tab says for focus and visibility, and the events smelt's
/// notifier listens for follow at once.
pub(super) async fn set_focus(page: &chromiumoxide::Page, focused: bool, visible: bool) {
    page.evaluate(format!("window.__smeltSetFocus({focused}, {visible})"))
        .await
        .expect("set the tab's focus");
}

/// The sidebar button: label, disabled, `aria-pressed`.
async fn toggle_state(page: &chromiumoxide::Page) -> (String, bool, bool) {
    let parts: Vec<serde_json::Value> = page
        .evaluate(
            "(() => { const b = document.querySelector('.notifications-toggle'); if (!b) return ['', true, false]; \
             return [b.textContent, b.disabled, b.getAttribute('aria-pressed') === 'true']; })()",
        )
        .await
        .expect("read the toggle")
        .into_value()
        .expect("label, disabled, pressed");
    (
        parts[0].as_str().unwrap_or_default().to_string(),
        parts[1].as_bool().unwrap_or(true),
        parts[2].as_bool().unwrap_or(false),
    )
}

/// Waits for the button to read `label` and returns disabled/pressed then.
async fn wait_toggle_label(page: &chromiumoxide::Page, label: &str, timeout: Duration) -> (bool, bool) {
    let want = label.to_string();
    let found = wait_until(|| async { toggle_state(page).await.0 == want }, timeout).await;
    assert!(found, "the toggle never read {want:?}");
    let (_, disabled, pressed) = toggle_state(page).await;
    (disabled, pressed)
}

/// Waits until the tab's app is showing `path` — in-app navigation, the
/// way `click_conversation` checks its own click landed.
pub(super) async fn wait_pathname(page: &chromiumoxide::Page, path: &str, timeout: Duration) -> bool {
    let want = path.to_string();
    wait_until(
        || async {
            let current: String = page
                .evaluate("location.pathname")
                .await
                .ok()
                .and_then(|v| v.into_value().ok())
                .unwrap_or_default();
            current == want
        },
        timeout,
    )
    .await
}

/// Waits for the tab's `document.title`.
pub(super) async fn wait_title(page: &chromiumoxide::Page, title: &str, timeout: Duration) -> bool {
    let want = title.to_string();
    wait_until(
        || async {
            let got: String = page
                .evaluate("document.title")
                .await
                .ok()
                .and_then(|v| v.into_value().ok())
                .unwrap_or_default();
            got == want
        },
        timeout,
    )
    .await
}

/// One question's `ask_user` input, as the model would send it.
fn ask_input(question: &str, header: &str) -> serde_json::Value {
    serde_json::json!({"questions": [
        {"question": question, "header": header,
         "options": [{"label": "Yes (Recommended)"}, {"label": "No", "description": "keep it"}]}
    ]})
}

/// The favicon the tab currently names (the href string only — the tier
/// serves stylesheets, not the svgs).
async fn favicon_href(page: &chromiumoxide::Page) -> String {
    page.evaluate("document.querySelector(\"link[rel='icon']\")?.href ?? ''")
        .await
        .expect("read the favicon link")
        .into_value()
        .expect("a string")
}

/// Waits until a turn the trigger started has actually streamed its reply
/// and ended — `wait_until_idle` alone can't say that: with no stream
/// open yet — the moment between the trigger and its request arriving —
/// it is instantly true, and the scenario then arms the mock for the
/// turn *after* this one, which this one takes instead. `before` is
/// `chunks_sent()` taken before the trigger; the turn has to have made
/// its request (chunks moved), finished streaming it (none open), and
/// released its in-flight count.
async fn wait_turn_ended(t: &Scenario<'_>, conversation_id: i64, before: usize) -> bool {
    wait_until(
        || async {
            t.mock.chunks_sent() > before
                && t.mock.open() == 0
                && !crate::turn::turn_running(conversation_id)
        },
        Duration::from_secs(20),
    )
    .await
}

/// SME-71: one leader notifies, clicks open conversations, answered and
/// stale ones close, reloads don't re-announce, the switch silences, and
/// where notifications can't work at all — plain HTTP — the title count
/// still does the job.
pub(super) async fn scenario_desktop_notifications(t: &Scenario<'_>) {
    let pool = t.pool;
    // Replies arrive at once: every wait here is on a card, a notification
    // or a title, never on a reply trickling.
    let _hurry = t.mock.hurry();
    let home = t.conversation().await;
    let asking = t.conversation().await;
    // Step 4b's asked-elsewhere conversation: created before the tabs
    // so every tab's first sidebar load already carries it — this is a
    // direct db create, and no event fires to refresh a live sidebar
    // for it, which the sidebar click then depends on.
    let watched = t.conversation().await;
    let asking_tag = format!("smelt-conversation-{}", asking.id);
    let home_tag = format!("smelt-conversation-{}", home.id);
    let watched_tag = format!("smelt-conversation-{}", watched.id);

    // Tab A on a conversation, tab B on a settings page: both take part in
    // the election, only one of them notifies.
    let a = stubbed_tab(t, t.url(&format!("conversation/{}", home.id))).await;
    wait_for_live_client(&a, home.id).await;
    // The plan's spike, kept: headless shell must really have Web Locks
    // (a secure-context-only API on a secure-context origin), and the
    // stub must be the `Notification` before smelt's code runs.
    let spike: serde_json::Value = a
        .evaluate("({ locks: !!navigator.locks, notification: typeof Notification })")
        .await
        .expect("spike the shell")
        .into_value()
        .expect("an object");
    assert_eq!(
        spike,
        serde_json::json!({ "locks": true, "notification": "function" }),
        "no Web Locks, or the stub lost the race to the WASM"
    );
    set_focus(&a, true, true).await;
    let b = stubbed_tab(t, t.url("providers")).await;
    wait_for_resource(&b, "/api/providers").await;
    set_focus(&b, false, false).await;

    // 1. The sidebar button turns it on: permission asked in the click
    //    (the stub grants), the setting stored, one confirmation shown.
    wait_toggle_label(&a, "Turn on notifications", Duration::from_secs(10)).await;
    click_when_present(&a, ".notifications-toggle", Duration::from_secs(5)).await;
    let (disabled, pressed) = wait_toggle_label(&a, "Notifications on", Duration::from_secs(10)).await;
    assert!(!disabled && pressed, "the button reads on and pressed");
    let stored: Option<String> = a
        .evaluate("localStorage.getItem(\"smelt.notifications\")")
        .await
        .expect("read the setting")
        .into_value()
        .expect("a string or null");
    assert_eq!(stored.as_deref(), Some("on"), "the setting is stored for the browser");
    let hello = recorded(&a).await;
    assert_eq!(
        hello.first().map(|n| (n.heading.as_str(), n.body.as_str(), n.tag.as_str())),
        Some(("smelt", "Notifications are on", "smelt-hello")),
        "one confirmation notification"
    );
    for tab in [&a, &b] {
        tab.evaluate("window.__notifications.length = 0").await.expect("clear the records");
    }

    // 2. A turn ending on a question notifies — exactly one browser, one
    //    tab, and it says what the card says.
    let _asked1 = t.mock.ask_user_next(&unique_id("toolu_ask"), &ask_input("Delete the build directory?", "Delete"));
    let c = stubbed_tab(t, t.url(&format!("conversation/{}", asking.id))).await;
    wait_for_live_client(&c, asking.id).await;
    set_focus(&c, false, true).await;
    send_from(&c, "tidy the build").await;
    let shown1 = wait_tag_count(&a, &asking_tag, 1, Duration::from_secs(20)).await;
    assert!(shown1, "the leader showed the question");
    // Settle, then count: the leader hears its own event twice (relay and
    // leader stream), and every tab hears the relay.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let shown = recorded(&a).await;
    assert_eq!(shown.len(), 1, "one notification, not a stack");
    assert!(recorded(&b).await.is_empty() && recorded(&c).await.is_empty(), "only the leader constructs");
    let title = db::get_conversation_title(pool, asking.id)
        .await
        .expect("read the title")
        .expect("the message titled it");
    assert_eq!(shown[0].heading, "smelt has a question", "the heading names what happened");
    assert_eq!(shown[0].body, format!("{title} — Delete"), "the body names the conversation and the question");
    assert!(shown[0].renotify, "renotify so a repeat re-alerts");

    // 3. Clicking it opens the conversation in the leader's tab, and
    //    closes the notification.
    let click = format!(
        "(() => {{ const n = window.__notifications.find(n => n.tag === {}); \
         if (!n || !n.onclick) return false; n.onclick(); return true; }})()",
        serde_json::json!(&asking_tag)
    );
    assert!(
        a.evaluate(click).await.expect("click the notification").into_value::<bool>().expect("a bool"),
        "the recorded notification had an onclick"
    );
    assert!(
        wait_pathname(&a, &format!("/conversation/{}", asking.id), Duration::from_secs(5)).await,
        "the click opened the conversation in the leader's tab"
    );
    assert!(
        wait_last_closed(&a, &asking_tag, "smelt has a question", Duration::from_secs(5)).await,
        "clicking closes it"
    );

    // 4. The user answers on the other tab's card. With the leader's tab
    //    unfocused again (they clicked away), a second question shows
    //    again — and answering it closes that notification.
    set_focus(&a, false, true).await;
    assert!(wait_for_count(&c, ".question-card", 1, Duration::from_secs(10)).await, "the card waits in the other tab");
    let chunks_answered = t.mock.chunks_sent();
    click_when_present(&c, ".question-option", Duration::from_secs(5)).await;
    assert!(wait_turn_ended(t, asking.id, chunks_answered).await, "the answer's reply ended");

    let _asked2 = t.mock.ask_user_next(&unique_id("toolu_ask"), &ask_input("Commit the result?", "Commit"));
    send_from(&c, "ask me again").await;
    let shown2 = wait_until(
        || async {
            recorded(&a).await.iter().filter(|n| n.tag == asking_tag && n.heading == "smelt has a question").count() == 2
        },
        Duration::from_secs(20),
    )
    .await;
    // Same event on both paths — one notification, however many ways it
    // arrived.
    assert!(
        shown2,
        "the second question showed no notification; a_recs: {:?}",
        recorded(&a).await.iter().map(|n| (&n.tag, &n.heading)).collect::<Vec<_>>()
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    let shown = recorded(&a).await;
    assert_eq!(
        shown.iter().filter(|n| n.heading == "smelt has a question").count(),
        2,
        "one notification per question, not one per delivery"
    );
    assert!(wait_for_count(&c, ".question-card", 1, Duration::from_secs(10)).await, "the second card");
    let chunks_answered = t.mock.chunks_sent();
    click_when_present(&c, ".question-option", Duration::from_secs(5)).await;
    assert!(
        wait_last_closed(&a, &asking_tag, "smelt has a question", Duration::from_secs(10)).await,
        "answering elsewhere closes the open notification"
    );
    assert!(wait_turn_ended(t, asking.id, chunks_answered).await, "the answer's turn ended");

    // 4b. The looked-at close for an event the leader was focused and
    //     visible when it arrived (review 1, Medium): focused and visible
    //     the count entry is skipped by design and the notification is
    //     owed all the same — so the open set holds what the count set
    //     does not, and opening that conversation here must still close
    //     it. Every step above led unfocused, which is why the old `&&`
    //     chain's short-circuit survived the green runs.
    for tab in [&a, &b, &c] {
        tab.evaluate("window.__notifications.length = 0").await.expect("clear the records");
    }
    set_focus(&a, true, true).await; // the leader, looking at another conversation
    let _asked_watched = t.mock.ask_user_next(&unique_id("toolu_ask"), &ask_input("Archive the logs?", "Archive"));
    click_conversation(&c, watched.id).await;
    send_from(&c, "peek at the logs").await;
    assert!(
        wait_tag_count(&a, &watched_tag, 1, Duration::from_secs(20)).await,
        "the focused, visible leader showed the other conversation's question"
    );
    click_conversation(&a, watched.id).await; // now the leader looks at it
    assert!(
        wait_last_closed(&a, &watched_tag, "smelt has a question", Duration::from_secs(5)).await,
        "opening the conversation closes its notification even when the leader was focused and visible for it"
    );
    // Answer it on C so nothing hangs open (suppressed in A: it is looked
    // at), and send the leader back to asking — the reload step below
    // reloads it there.
    assert!(wait_for_count(&c, ".question-card", 1, Duration::from_secs(10)).await, "the card waits in C");
    let chunks_watched = t.mock.chunks_sent();
    click_when_present(&c, ".question-option", Duration::from_secs(5)).await;
    assert!(wait_turn_ended(t, watched.id, chunks_watched).await, "the answer's turn ended");
    click_conversation(&a, asking.id).await;

    // 5. Reload the leading tab. Its lock passes to a queued tab, nothing
    //    is re-announced, and the model's `notify_user` elsewhere
    //    (SME-377) still gives exactly one notification, from the one tab
    //    that leads, saying the model's message.
    a.reload().await.expect("reload the leader");
    wait_for_live_client(&a, asking.id).await;
    click_conversation(&c, home.id).await;
    let notify = |message: &str| serde_json::json!({ "message": message });
    let _notified = t.mock.tool_call_next("notify_user", &unique_id("toolu_notify"), &notify("Wrapped up"));
    send_from(&c, "wrap up").await;
    assert!(t.mock.wait_until_tool_call_sent(Duration::from_secs(15)).await, "the notifying turn ended");
    let (leader_after, understudy) = if wait_tag_count(&b, &home_tag, 1, Duration::from_secs(15)).await {
        (&b, &c)
    } else {
        // The hand-over may have landed on either queued tab; say which.
        assert!(wait_tag_count(&c, &home_tag, 1, Duration::from_secs(10)).await, "some tab took over the lock");
        (&c, &b)
    };
    tokio::time::sleep(Duration::from_millis(600)).await;
    let shown = recorded(leader_after).await;
    assert_eq!(shown.len(), 1, "one notification, from one tab");
    assert_eq!(shown[0].heading, "smelt: wrap up", "the conversation's title heads it");
    assert_eq!(shown[0].body, "Wrapped up", "the model's message is the body");
    // Across all tabs, not just the leader's own: with the old leader
    // gone, two tabs each believing it leads would both show it — the
    // one-notification property is what the leader
    // is for, so the queued tab that didn't take the lock must record
    // nothing (review 1, Low).
    assert!(recorded(understudy).await.is_empty(), "only the one new leader constructed");
    // The reload announced nothing new: tab A's records died with its
    // page and nothing came back for the answered question.
    assert!(recorded(&a).await.is_empty(), "the reload didn't re-announce anything");
    // 5b. The user's complaint (SME-377): a turn that only replies, with
    //     everything else as it was for the notification above, notifies
    //     nothing anywhere.
    send_from(&c, "thanks").await;
    assert!(t.mock.wait_until_idle(Duration::from_secs(15)).await, "the plain turn ended");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(recorded(leader_after).await.len(), 1, "a plain reply adds no notification");
    assert!(recorded(understudy).await.is_empty() && recorded(&a).await.is_empty(), "nor on any other tab");

    // 6. Turning the switch off: the label flips, and a notifying turn
    //    records nothing anywhere.
    for tab in [&a, &b, &c] {
        tab.evaluate("window.__notifications.length = 0").await.expect("clear the records");
    }
    click_when_present(&a, ".notifications-toggle", Duration::from_secs(5)).await;
    let (disabled, _) = wait_toggle_label(&a, "Turn on notifications", Duration::from_secs(10)).await;
    assert!(!disabled);
    let _notified = t.mock.tool_call_next("notify_user", &unique_id("toolu_notify"), &notify("All done"));
    send_from(&c, "and done").await;
    assert!(t.mock.wait_until_tool_call_sent(Duration::from_secs(15)).await, "the turn ended");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    for tab in [&a, &b, &c] {
        assert!(recorded(tab).await.is_empty(), "off is off: nothing is constructed");
    }

    // 7. Plain HTTP: not a secure context, so notifications can't work —
    //    the button says why, none are constructed, and the title count
    //    does the job instead. The turn itself is started from the
    //    127.0.0.1 tab (SME-146: writes from the plain-HTTP name can be
    //    refused).
    let port = t.harness.base_url.trim_end_matches('/').rsplit(':').next().unwrap_or_default().to_string();
    let d = stubbed_tab(t, format!("http://{INSECURE_HOST}:{port}/conversation/{}", home.id)).await;
    wait_for_live_client(&d, home.id).await;
    set_focus(&d, false, true).await;
    let (disabled, pressed) =
        wait_toggle_label(&d, "Desktop notifications need HTTPS", Duration::from_secs(10)).await;
    assert!(disabled && !pressed, "the plain-HTTP button is disabled and unpressed");
    let hint: String = d
        .evaluate("document.querySelector('.notifications-toggle').title")
        .await
        .expect("read the hint")
        .into_value()
        .expect("a string");
    assert!(hint.contains("https://localhost:8443"), "the hint names the one origin that works: {hint}");
    assert!(recorded(&d).await.is_empty(), "no Notification is even constructed here");

    // A plain reply counts nothing (SME-377): only the model's
    // notify_user, below, owes the user anything.
    send_from(&c, "and again").await;
    assert!(t.mock.wait_until_idle(Duration::from_secs(15)).await, "the plain turn ended");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(wait_title(&d, "smelt", Duration::from_secs(1)).await, "a plain reply adds nothing to the count");
    let _notified = t.mock.tool_call_next("notify_user", &unique_id("toolu_notify"), &notify("Ready again"));
    send_from(&c, "one more").await;
    assert!(t.mock.wait_until_tool_call_sent(Duration::from_secs(15)).await, "the notifying turn ended");
    assert!(wait_title(&d, "(1) smelt", Duration::from_secs(15)).await, "the unfocused tab counts in its title");
    assert!(favicon_href(&d).await.contains("favicon-dot"), "the favicon swaps with the count");
    assert!(recorded(&d).await.is_empty(), "still no Notification over plain HTTP");
    set_focus(&d, true, true).await;
    assert!(wait_title(&d, "smelt", Duration::from_secs(5)).await, "the focused tab has nothing to count");
    let back = favicon_href(&d).await;
    assert!(!back.contains("favicon-dot"), "the favicon swaps back: {back}");

    // 8. The button in the phone drawer, and the widest label wrapping
    //    inside the sidebar — on D, which always shows its widest one.
    d.execute(SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true)).await.expect("emulate a phone");
    click_when_present(&d, ".sidebar-toggle", Duration::from_secs(5)).await;
    assert!(
        wait_until(
            || async {
                d.evaluate("document.querySelector('.notifications-toggle').offsetParent !== null")
                    .await
                    .ok()
                    .and_then(|v| v.into_value::<bool>().ok())
                    == Some(true)
            },
            Duration::from_secs(5)
        )
        .await,
        "the button sits in the opened sidebar list on a phone"
    );
    let phone: Vec<f64> = d
        .evaluate(
            "(() => { const r = document.querySelector('.notifications-toggle').getBoundingClientRect(); \
             return [r.right, document.documentElement.scrollWidth - document.documentElement.clientWidth]; })()",
        )
        .await
        .expect("measure on a phone")
        .into_value()
        .expect("numbers");
    assert!(phone[0] <= 390.0, "the button is on screen at 390 px: {phone:?}");
    assert!(phone[1] <= 0.0, "no sideways scroll with the button at 390 px: {phone:?}");
    d.execute(SetDeviceMetricsOverrideParams::new(721, 900, 1.0, false)).await.expect("emulate a small laptop");
    let laptop: Vec<f64> = d
        .evaluate(
            "(() => { const b = document.querySelector('.notifications-toggle').getBoundingClientRect(); \
             const s = document.querySelector('.sidebar').getBoundingClientRect(); \
             return [b.right, s.right, document.documentElement.scrollWidth - document.documentElement.clientWidth]; })()",
        )
        .await
        .expect("measure at 721 px")
        .into_value()
        .expect("numbers");
    assert!(laptop[0] <= laptop[1] + 1.0, "the widest label wraps inside the sidebar: {laptop:?}");
    assert!(laptop[2] <= 0.0, "no sideways scroll at 721 px: {laptop:?}");
}
