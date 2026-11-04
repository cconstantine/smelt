//! SME-386: the notifier's leadership, on settings pages, where only the
//! leader's own app stream reaches a tab — N2's focus steal. Uses the
//! `Notification` stub and the focus override of `notifications.rs`.
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

    // Leave the browser's setting off, as the next scenario expects.
    a.evaluate("localStorage.removeItem('smelt.notifications')").await.expect("turn the setting off");
}
