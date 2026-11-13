//! SME-386: the notifier's leadership, on settings pages, where only the
//! leader's own app stream reaches a tab — N2's focus steal. Uses the
//! `Notification` stub and the focus override of `notifications.rs`.
//!
//! F1 and F2 too: the notifier across a client-side route change.
//!
//! `Runs::Alone`, as `desktop_notifications`: the leader's app-wide
//! stream hears every lane's turns, and the setting is browser-wide.

use super::*;

/// Whether `page`'s notifier holds the lock now.
async fn leads(page: &chromiumoxide::Page) -> bool {
    page.evaluate("!!(window.__smeltNotifier && window.__smeltNotifier.leader)")
        .await
        .ok()
        .and_then(|v| v.into_value::<bool>().ok())
        == Some(true)
}

/// How many requests for the notifier's lock wait in this browser (the
/// lock manager is per origin, so any tab answers for all of them).
async fn queued_requests(page: &chromiumoxide::Page) -> usize {
    page.evaluate("navigator.locks.query().then(q => q.pending.filter(r => r.name === 'smelt-notifier').length)")
        .await
        .expect("query the lock manager")
        .into_value::<usize>()
        .expect("a count")
}

/// Opens a stubbed settings tab with notifications on: the setting is
/// the browser's (localStorage), set before the page boots, which reads
/// it then. A settings page has no sidebar button to click.
async fn settings_tab_notifying(t: &Scenario<'_>) -> chromiumoxide::Page {
    let page = stubbed_tab(t, t.url("providers")).await;
    page.evaluate("localStorage.setItem('smelt.notifications', 'on')").await.expect("turn the setting on");
    page.reload().await.expect("boot with the setting on");
    wait_for_resource(&page, "/api/providers").await;
    page
}

pub(super) async fn scenario_notifier_leadership(t: &Scenario<'_>) {
    // Two settings tabs: neither has a conversation stream, so what each
    // does as leader rides its own app stream alone.
    let a = settings_tab_notifying(t).await;
    assert!(wait_until(|| leads(&a), Duration::from_secs(10)).await, "the first tab leads");
    let b = settings_tab_notifying(t).await;
    set_focus(&b, false, false).await;
    assert!(
        wait_until(|| async { queued_requests(&a).await == 1 }, Duration::from_secs(10)).await,
        "the second tab queues behind the first"
    );

    // N2: focusing B takes the lock from A at once (`steal: true`), and
    // focusing it again queues nothing more: one waiting request, A's.
    for _ in 0..3 {
        set_focus(&b, true, true).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(wait_until(|| leads(&b), Duration::from_secs(5)).await, "focus moved the leader to B");
    assert!(!leads(&a).await, "A stopped leading");
    assert_eq!(queued_requests(&b).await, 1, "A queues once, and B's repeated focus queued nothing");
    // F9: dropping B's queued request for the steal is no page error.
    assert_eq!(unhandled_rejections(&b).await, Vec::<String>::new(), "B's steal raised an uncaught error");
    // And back: focusing A takes it again.
    set_focus(&b, false, false).await;
    set_focus(&a, true, true).await;
    assert!(wait_until(|| leads(&a), Duration::from_secs(5)).await, "focus moved the leader back to A");
    assert!(!leads(&b).await, "B stopped leading");
    assert_eq!(queued_requests(&a).await, 1, "B queued once");

    // N3: a conversation deleted while it waits on the user takes its
    // notification with it — the delete's `ConversationsChanged` makes the
    // leader revalidate, and the gone conversation answers `None`.
    let gone = t.conversation().await;
    let gone_tag = format!("smelt-conversation-{}", gone.id);
    set_focus(&a, false, false).await;
    crate::turn::publish_a_test_spell(gone.id, crate::events::Attention::Failed);
    assert!(wait_tag_count(&a, &gone_tag, 1, Duration::from_secs(10)).await, "the leader showed the failure");
    crate::api::chat::delete_conversation(gone.id).await.expect("delete the conversation");
    assert!(
        wait_last_closed(&a, &gone_tag, "smelt: the turn failed", Duration::from_secs(10)).await,
        "deleting the conversation closes its notification"
    );
    set_focus(&a, true, true).await;

    // N4: a leader on a settings page has no conversation stream, so once
    // demoted nothing would revalidate what it showed: it closes them.
    let held = t.conversation().await;
    let held_tag = format!("smelt-conversation-{}", held.id);
    set_focus(&a, false, false).await;
    crate::turn::publish_a_test_spell(held.id, crate::events::Attention::Failed);
    assert!(wait_tag_count(&a, &held_tag, 1, Duration::from_secs(10)).await, "the leader showed the failure");
    set_focus(&b, true, true).await;
    assert!(wait_until(|| leads(&b), Duration::from_secs(5)).await, "focus moved the leader to B");
    assert!(
        wait_last_closed(&a, &held_tag, "smelt: the turn failed", Duration::from_secs(5)).await,
        "the demoted leader closes what it can no longer revalidate"
    );
    crate::turn::forget_conversation_lock(held.id);
    set_focus(&b, false, false).await;
    set_focus(&a, true, true).await;
    assert!(wait_until(|| leads(&a), Duration::from_secs(5)).await, "A leads again, to turn the setting off");

    for tab in [&a, &b] {
        assert_eq!(unhandled_rejections(tab).await, Vec::<String>::new(), "every lock move stays quiet");
    }

    // Leave the browser's setting off, as the next scenario expects.
    a.evaluate("localStorage.removeItem('smelt.notifications')").await.expect("turn the setting off");
}

/// Opens a stubbed tab on a conversation with notifications on, out of
/// focus, leading.
async fn conversation_tab_notifying(t: &Scenario<'_>, id: i64) -> chromiumoxide::Page {
    let page = stubbed_tab(t, t.url(&format!("conversation/{id}"))).await;
    wait_for_live_client(&page, id).await;
    page.evaluate("localStorage.setItem('smelt.notifications', 'on')").await.expect("turn the setting on");
    page.reload().await.expect("boot with the setting on");
    wait_for_live_client(&page, id).await;
    assert!(wait_until(|| leads(&page), Duration::from_secs(10)).await, "the tab leads");
    set_focus(&page, false, false).await;
    page
}

/// Whether a spell owing the user on `id` makes `page` show a new
/// notification. The stream a new leader opens has no catch-up, so a
/// publish can come before it connects: this publishes again until one
/// shows, each a new generation, as the user's send would start.
async fn spell_notifies(page: &chromiumoxide::Page, id: i64) -> bool {
    let tag = format!("smelt-conversation-{id}");
    let before = tag_count(page, &tag).await;
    for _ in 0..6 {
        crate::turn::new_turn_generation(id);
        crate::turn::publish_a_test_spell(id, crate::events::Attention::CutOff);
        if wait_until(|| async { tag_count(page, &tag).await > before }, Duration::from_millis(1500)).await {
            return true;
        }
    }
    false
}

/// The sidebar's Settings link, then Settings' link to the providers
/// page, and that page having loaded: two client-side route changes.
async fn to_providers(page: &chromiumoxide::Page) {
    click_when_present(page, ".sidebar-settings-link", Duration::from_secs(10)).await;
    assert!(wait_pathname(page, "/settings", Duration::from_secs(10)).await, "the sidebar link didn't open /settings");
    click_when_present(page, ".settings-index .providers-link", Duration::from_secs(10)).await;
    assert!(wait_pathname(page, "/providers", Duration::from_secs(10)).await, "Settings' link didn't open /providers");
    wait_for_element(page, ".settings-home-link", Duration::from_secs(10)).await;
    // Whatever the route change was going to do to the notifier, done.
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// The browser's Back from `to_providers`' page to `path`: twice, by way
/// of Settings (SME-512), and whatever it did settled.
async fn browser_back(page: &chromiumoxide::Page, path: &str) {
    page.evaluate("history.back()").await.expect("go back");
    assert!(wait_pathname(page, "/settings", Duration::from_secs(10)).await, "Back didn't return to /settings");
    page.evaluate("history.back()").await.expect("go back again");
    assert!(wait_pathname(page, path, Duration::from_secs(10)).await, "Back didn't return to {path}");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// The settings page's own link back to Home, and it settled.
async fn back_link_home(page: &chromiumoxide::Page) {
    click_when_present(page, ".settings-home-link", Duration::from_secs(10)).await;
    assert!(wait_pathname(page, "/", Duration::from_secs(10)).await, "the back link didn't open Home");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// F1: a tab that went to a settings page and back still notifies. It
/// used to keep the lock, say "Notifications on", and notify nothing.
pub(super) async fn scenario_notifier_after_navigation(t: &Scenario<'_>) {
    let shown = t.conversation().await;
    let other = t.conversation().await;
    let page = conversation_tab_notifying(t, shown.id).await;
    assert!(spell_notifies(&page, other.id).await, "a fresh load notifies");

    to_providers(&page).await;
    assert!(spell_notifies(&page, other.id).await, "a settings page notifies");
    browser_back(&page, &format!("/conversation/{}", shown.id)).await;
    assert!(leads(&page).await, "the tab still holds the lock");
    assert!(spell_notifies(&page, other.id).await, "the tab notifies after a settings page and the browser's Back");

    to_providers(&page).await;
    back_link_home(&page).await;
    assert!(leads(&page).await, "the tab still holds the lock");
    assert!(spell_notifies(&page, other.id).await, "the tab notifies after a settings page and the link to Home");
    assert_eq!(unhandled_rejections(&page).await, Vec::<String>::new(), "route changes stay quiet");

    crate::turn::forget_conversation_lock(other.id);
    page.evaluate("localStorage.removeItem('smelt.notifications')").await.expect("turn the setting off");
}

/// The state a route change must leave alone, read from the tab: its
/// notifier instance (`mark` is set on it by the scenario), how many
/// notifications it tracks, and its title.
async fn assert_state_kept(page: &chromiumoxide::Page, other: &chromiumoxide::Page, tracked: usize, title: &str, leg: &str) {
    let facts: serde_json::Value = page
        .evaluate("({ mark: window.__smeltNotifier.mark ?? null, tracked: window.__smeltNotifier.notifications.size, title: document.title })")
        .await
        .expect("read the notifier")
        .into_value()
        .expect("an object");
    assert_eq!(
        facts,
        serde_json::json!({ "mark": "first", "tracked": tracked, "title": title }),
        "{leg}: the route change forgot the tab's notifier"
    );
    assert!(leads(page).await, "{leg}: the tab lost the lock");
    assert!(!leads(other).await, "{leg}: leadership moved with no focus change");
}

/// F2: a route change keeps the tab's one notifier, so its open
/// notifications stay tracked (and close when they go stale), its count
/// stays, and it stays the leader. A route used to be a new notifier.
pub(super) async fn scenario_notifier_keeps_state_across_navigation(t: &Scenario<'_>) {
    const HEADING: &str = "smelt: the reply was cut off";
    let shown = t.conversation().await;
    let first = t.conversation().await;
    let clicked = t.conversation().await;
    let demoted = t.conversation().await;
    let first_tag = format!("smelt-conversation-{}", first.id);
    let clicked_tag = format!("smelt-conversation-{}", clicked.id);
    let demoted_tag = format!("smelt-conversation-{}", demoted.id);
    let shown_path = format!("/conversation/{}", shown.id);
    let clicked_path = format!("/conversation/{}", clicked.id);

    let a = conversation_tab_notifying(t, shown.id).await;
    let b = settings_tab_notifying(t).await;
    set_focus(&b, false, false).await;
    assert!(
        wait_until(|| async { queued_requests(&a).await == 1 }, Duration::from_secs(10)).await,
        "the second tab queues behind the first"
    );
    assert!(spell_notifies(&a, first.id).await, "the leader notifies");
    assert!(wait_title(&a, "(1) smelt", Duration::from_secs(5)).await, "the count shows");
    a.evaluate("window.__smeltNotifier.mark = 'first'").await.expect("mark the notifier");

    // Every kind of route change the notifier used to fall to.
    to_providers(&a).await;
    assert_state_kept(&a, &b, 1, "(1) smelt", "conversation to settings").await;
    browser_back(&a, &shown_path).await;
    assert_state_kept(&a, &b, 1, "(1) smelt", "settings to conversation").await;
    to_providers(&a).await;
    back_link_home(&a).await;
    assert_state_kept(&a, &b, 1, "(1) smelt", "settings to Home").await;

    // What it tracked before the route changes still closes when stale.
    assert!(spell_notifies(&a, clicked.id).await, "Home's leader notifies");
    assert!(wait_title(&a, "(2) smelt", Duration::from_secs(5)).await, "the count grows");
    crate::api::chat::delete_conversation(first.id).await.expect("delete the conversation");
    assert!(
        wait_last_closed(&a, &first_tag, HEADING, Duration::from_secs(10)).await,
        "a notification from before the route changes closes when it goes stale"
    );
    assert!(wait_title(&a, "(1) smelt", Duration::from_secs(5)).await, "and the count drops");

    // A click from Home opens the conversation and closes the notification.
    a.evaluate(format!(
        "window.__notifications.filter(n => n.tag === {clicked_tag:?}).at(-1).onclick()"
    ))
    .await
    .expect("click the notification");
    assert!(wait_pathname(&a, &clicked_path, Duration::from_secs(10)).await, "the click opened the conversation");
    assert!(wait_last_closed(&a, &clicked_tag, HEADING, Duration::from_secs(10)).await, "the click closed it");
    assert!(wait_title(&a, "smelt", Duration::from_secs(5)).await, "and the count empties");

    // Demoted on a conversation, a tab keeps what it showed: the
    // conversation's stream revalidates it. Leaving for a page with no
    // stream, nothing would, so it closes them (N4's rule, on a route
    // change).
    assert!(spell_notifies(&a, demoted.id).await, "the leader notifies");
    assert!(wait_title(&a, "(1) smelt", Duration::from_secs(5)).await, "the count shows");
    let shown_before = tag_count(&a, &demoted_tag).await;
    set_focus(&b, true, true).await;
    assert!(wait_until(|| leads(&b), Duration::from_secs(5)).await, "focus moved the leader to B");
    assert!(wait_until(|| async { !leads(&a).await }, Duration::from_secs(5)).await, "A stopped leading");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(tag_count(&a, &demoted_tag).await, shown_before, "the demotion showed nothing more");
    assert!(wait_title(&a, "(1) smelt", Duration::from_secs(1)).await, "demoted on a conversation, it keeps the count");
    to_providers(&a).await;
    assert!(
        wait_last_closed(&a, &demoted_tag, HEADING, Duration::from_secs(5)).await,
        "leaving the conversation closes what only its stream could revalidate"
    );
    assert!(wait_title(&a, "smelt", Duration::from_secs(5)).await, "and the count goes with them");

    crate::turn::forget_conversation_lock(demoted.id);
    crate::turn::forget_conversation_lock(clicked.id);
    for tab in [&a, &b] {
        assert_eq!(unhandled_rejections(tab).await, Vec::<String>::new(), "route changes stay quiet");
    }
    a.evaluate("localStorage.removeItem('smelt.notifications')").await.expect("turn the setting off");
}

/// How many notifications `page`'s notifier tracks now.
async fn tracked(page: &chromiumoxide::Page) -> usize {
    page.evaluate("window.__smeltNotifier.notifications.size")
        .await
        .expect("read the notifier")
        .into_value()
        .expect("a count")
}

/// F2 through a real route to not leading: the setting turned off lets
/// go of the lock for good. On the conversation the tab keeps what it
/// showed, which its stream revalidates; leaving it, nothing would, so
/// they close. The toggle is on Settings (SME-512), so it's turned off
/// from a second tab there, and the conversation's tab hears it as a
/// `storage` event.
pub(super) async fn scenario_notifier_setting_off_then_leaving(t: &Scenario<'_>) {
    const HEADING: &str = "smelt: the reply was cut off";
    let shown = t.conversation().await;
    let other = t.conversation().await;
    let tag = format!("smelt-conversation-{}", other.id);
    let page = conversation_tab_notifying(t, shown.id).await;
    assert!(spell_notifies(&page, other.id).await, "the leader notifies");
    assert!(wait_title(&page, "(1) smelt", Duration::from_secs(5)).await, "the count shows");

    // The setting is already on in this browser: B boots with it.
    let settings = stubbed_tab(t, t.url("settings")).await;
    set_focus(&settings, false, false).await;
    wait_toggle_label(&settings, "Notifications on", Duration::from_secs(10)).await;
    click_when_present(&settings, ".notifications-toggle", Duration::from_secs(5)).await;
    wait_toggle_label(&settings, "Turn on notifications", Duration::from_secs(10)).await;
    assert!(wait_until(|| async { !leads(&page).await }, Duration::from_secs(5)).await, "the setting off let the lock go");
    assert!(!leads(&settings).await, "and the Settings tab doesn't take it");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(tracked(&page).await, 1, "on the conversation the tab keeps what it showed");
    assert!(wait_title(&page, "(1) smelt", Duration::from_secs(1)).await, "and its count");

    to_providers(&page).await;
    assert!(wait_last_closed(&page, &tag, HEADING, Duration::from_secs(5)).await, "leaving the conversation closes it");
    assert_eq!(tracked(&page).await, 0, "and forgets it");
    assert!(wait_title(&page, "smelt", Duration::from_secs(5)).await, "and the count goes with it");
    assert_eq!(unhandled_rejections(&page).await, Vec::<String>::new(), "route changes stay quiet");
    crate::turn::forget_conversation_lock(other.id);
}

/// Review round 1's L1: an event already in the leader's queue when it's
/// demoted. Nothing but the leader's own stream carried it, and the next
/// leader's stream has no catch-up, so if this tab drops it, no tab shows
/// it. It shows here, then closes on the next message, as anything a
/// demoted tab holds away from a conversation does. Ticks are held so the
/// event waits in the queue until the demotion's own message wakes the loop.
pub(super) async fn scenario_notifier_event_queued_at_demotion(t: &Scenario<'_>) {
    const HEADING: &str = "smelt: the reply was cut off";
    let warm = t.conversation().await;
    let queued = t.conversation().await;
    let tag = format!("smelt-conversation-{}", queued.id);
    let a = settings_tab_notifying(t).await;
    assert!(wait_until(|| leads(&a), Duration::from_secs(10)).await, "the first tab leads");
    set_focus(&a, false, false).await;
    let b = settings_tab_notifying(t).await;
    set_focus(&b, false, false).await;
    assert!(
        wait_until(|| async { queued_requests(&a).await == 1 }, Duration::from_secs(10)).await,
        "the second tab queues behind the first"
    );
    assert!(spell_notifies(&a, warm.id).await, "A's stream is live");

    a.evaluate("window.__smeltTicksPaused = true").await.expect("hold A's ticks");
    tokio::time::sleep(Duration::from_millis(400)).await;
    crate::turn::new_turn_generation(queued.id);
    crate::turn::publish_a_test_spell(queued.id, crate::events::Attention::CutOff);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(tag_count(&a, &tag).await, 0, "the event waits in A's queue");

    set_focus(&b, true, true).await;
    assert!(wait_until(|| leads(&b), Duration::from_secs(5)).await, "focus moved the leader to B");
    let shown_somewhere = wait_until(|| async { tag_count(&a, &tag).await == 1 }, Duration::from_secs(3)).await;
    assert_eq!(tag_count(&b, &tag).await, 0, "B's stream has no catch-up");
    assert!(shown_somewhere, "the event reached no tab");

    a.evaluate("window.__smeltTicksPaused = false").await.expect("let A's ticks go");
    assert!(wait_last_closed(&a, &tag, HEADING, Duration::from_secs(5)).await, "the demoted tab closes it");
    crate::turn::forget_conversation_lock(warm.id);
    crate::turn::forget_conversation_lock(queued.id);
    set_focus(&b, false, false).await;
    a.evaluate("localStorage.removeItem('smelt.notifications')").await.expect("turn the setting off");
}
