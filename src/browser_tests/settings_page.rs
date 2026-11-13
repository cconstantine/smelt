//! The Settings index and the sidebar's bottom-left link to it (SME-512).

use super::*;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;

/// Each settings index the Settings page lists, as `(path, its h1)`, in
/// the page's order.
const SETTINGS_PAGES: [(&str, &str); 7] = [
    ("/providers", "Model providers"),
    ("/mcp-servers", "MCP servers"),
    ("/pods", "Sandboxes"),
    ("/sandbox-volumes", "Sandbox volumes"),
    ("/git", "Git"),
    ("/secrets", "Secrets"),
    ("/language-servers", "Language servers"),
];

/// A string `js` evaluates to on `page`, or "" when it can't.
async fn read_string(page: &chromiumoxide::Page, js: &str) -> String {
    page.evaluate(js).await.ok().and_then(|v| v.into_value().ok()).unwrap_or_default()
}

/// Waits until `page`'s `h1` reads `text`.
async fn wait_h1(page: &chromiumoxide::Page, text: &str) -> bool {
    let want = text.to_string();
    wait_until(
        || async { read_string(page, "(document.querySelector('h1') || {}).innerText || ''").await.trim() == want },
        Duration::from_secs(10),
    )
    .await
}

/// Scenario: `/settings` lists every settings page with a line each, and
/// holds the notifications toggle. Each entry opens its page, whose
/// breadcrumb leads back to Settings and to the conversations.
pub(super) async fn scenario_settings_page(t: &Scenario<'_>) {
    let page = t.tab(t.url("settings")).await;
    wait_for_element(&page, ".settings-index", Duration::from_secs(10)).await;
    assert!(wait_h1(&page, "Settings").await, "/settings has no Settings heading");
    let hrefs = read_string(
        &page,
        "Array.from(document.querySelectorAll('.settings-entry a')).map(a => a.getAttribute('href')).join('|')",
    )
    .await;
    let want: Vec<&str> = SETTINGS_PAGES.iter().map(|(path, _)| *path).collect();
    assert_eq!(hrefs, want.join("|"), "the Settings page's links");
    // Named the way a user thinks of them: "Sandboxes", not "Pods"
    // (SME-41 D11), and each says what it holds.
    let entries: Vec<(String, String)> = page
        .evaluate(
            "Array.from(document.querySelectorAll('.settings-entry')).map(e => \
             [e.querySelector('a').innerText, (e.querySelector('p') || {}).innerText || ''])",
        )
        .await
        .expect("read the entries")
        .into_value()
        .expect("pairs of strings");
    for ((label, description), (_, h1)) in entries.iter().zip(SETTINGS_PAGES) {
        assert_eq!(label, h1, "an entry's label is its page's name");
        assert!(!description.trim().is_empty(), "{label} has no description");
    }
    assert!(!entries.iter().any(|(label, _)| label.contains("Pods")), "entries: {entries:?}");
    assert_eq!(
        read_string(&page, "document.querySelector('.settings-browser .notifications-toggle') ? 'yes' : 'no'").await,
        "yes",
        "the notifications toggle sits on the Settings page, under This browser"
    );
    assert_eq!(
        read_string(&page, "document.querySelector('.settings-back-link') ? 'yes' : 'no'").await,
        "no",
        "the index's breadcrumb has no link to itself"
    );

    for (path, h1) in SETTINGS_PAGES {
        click_when_present(&page, &format!(".settings-entry a[href='{path}']"), Duration::from_secs(10)).await;
        assert!(wait_pathname(&page, path, Duration::from_secs(10)).await, "Settings' {h1} link didn't open {path}");
        assert!(wait_h1(&page, h1).await, "{path} didn't show its {h1} heading");
        assert_eq!(
            read_string(&page, "(document.querySelector('.settings-home-link') || {}).getAttribute?.('href') || ''").await,
            "/",
            "{path}'s breadcrumb leads to the conversations"
        );
        click_when_present(&page, ".settings-back-link", Duration::from_secs(10)).await;
        assert!(wait_pathname(&page, "/settings", Duration::from_secs(10)).await, "{path}'s breadcrumb didn't lead to Settings");
        wait_for_element(&page, ".settings-index", Duration::from_secs(10)).await;
    }
    // And from a page on to the conversations.
    click_when_present(&page, ".settings-entry a[href='/git']", Duration::from_secs(10)).await;
    assert!(wait_pathname(&page, "/git", Duration::from_secs(10)).await, "the Git link didn't open /git");
    click_when_present(&page, ".settings-home-link", Duration::from_secs(10)).await;
    assert!(wait_pathname(&page, "/", Duration::from_secs(10)).await, "the breadcrumb didn't lead to the conversations");
    wait_for_element(&page, ".new-conversation", Duration::from_secs(10)).await;
    page.close().await.expect("close the tab");

    // Enough conversations that the list scrolls, whatever else is in it.
    let mut last = None;
    for i in 0..30 {
        let conversation = t.conversation().await;
        seed_user_message(t.pool, conversation.id, &format!("settings_page row {i}")).await;
        last = Some(conversation.id);
    }
    sidebar_link_on_desktop(t, last.expect("a conversation")).await;
    sidebar_link_on_a_phone(t).await;
}

/// Reads `js` (an array of numbers) on `page`.
async fn numbers(page: &chromiumoxide::Page, js: &str) -> Vec<f64> {
    page.evaluate(js).await.expect("measure").into_value().expect("numbers")
}

/// The sidebar's Settings link sits at its bottom left on a laptop, and
/// stays there while the list scrolls under New conversation.
async fn sidebar_link_on_desktop(t: &Scenario<'_>, conversation: i64) {
    let page = t.tab("about:blank").await;
    page.execute(SetDeviceMetricsOverrideParams::new(1280, 600, 1.0, false)).await.expect("a short laptop window");
    page.goto(t.url(&format!("conversation/{conversation}"))).await.expect("open a conversation");
    wait_for_live_client(&page, conversation).await;
    let link = ".sidebar .sidebar-settings-link";
    wait_for_element(&page, link, Duration::from_secs(10)).await;
    let measure = "(() => { const b = s => document.querySelector(s).getBoundingClientRect(); \
         const l = b('.sidebar-settings-link'), s = b('.sidebar'), n = b('.new-conversation'), \
         list = b('.conversation-list'), sc = document.querySelector('.sidebar-scroll'); \
         return [l.left - s.left, s.bottom - l.bottom, l.top, l.bottom, innerHeight, \
         sc.scrollHeight - sc.clientHeight, list.top - n.bottom, n.top]; })()";
    let before = numbers(&page, measure).await;
    assert!(before[5] > 0.0, "the list should overflow, to test the long case: {before:?}");
    assert!((0.0..=24.0).contains(&before[0]), "the Settings link isn't at the sidebar's left: {before:?}");
    assert!((0.0..=24.0).contains(&before[1]), "the Settings link isn't at the sidebar's bottom: {before:?}");
    assert!(before[2] >= 0.0 && before[3] <= before[4], "the Settings link is off screen: {before:?}");
    // The list starts right under New conversation: no settings block
    // between them.
    assert!(before[6] <= 60.0, "the list starts {}px below New conversation", before[6]);
    page.evaluate("(() => { const sc = document.querySelector('.sidebar-scroll'); sc.scrollTop = sc.scrollHeight; })()")
        .await
        .expect("scroll the list");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = numbers(&page, measure).await;
    assert_eq!(
        (before[2], before[7]),
        (after[2], after[7]),
        "scrolling the list moved the Settings link or New conversation: {before:?} {after:?}"
    );
    click_when_present(&page, link, Duration::from_secs(10)).await;
    assert!(wait_pathname(&page, "/settings", Duration::from_secs(10)).await, "the Settings link didn't open /settings");
    wait_for_element(&page, ".settings-index", Duration::from_secs(10)).await;
    page.close().await.expect("close the tab");
}

/// On a phone the link hides with the folded list, and is the opened
/// drawer's last row.
async fn sidebar_link_on_a_phone(t: &Scenario<'_>) {
    let page = t.tab("about:blank").await;
    page.execute(SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true)).await.expect("emulate a phone");
    page.goto(t.url("")).await.expect("open Home");
    wait_for_element(&page, ".sidebar-toggle", Duration::from_secs(10)).await;
    let link = ".sidebar .sidebar-settings-link";
    wait_for_element(&page, link, Duration::from_secs(10)).await;
    assert_eq!(
        read_string(&page, "document.querySelector('.sidebar-settings-link').offsetParent === null ? 'hidden' : 'shown'").await,
        "hidden",
        "the folded sidebar shows its Settings link"
    );
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    let tab = &page;
    let shown = wait_until(
        || async { read_string(tab, "document.querySelector('.sidebar-settings-link').offsetParent === null ? '' : 'yes'").await == "yes" },
        Duration::from_secs(5),
    )
    .await;
    assert!(shown, "the opened drawer has no Settings link");
    let drawer = numbers(
        &page,
        "(() => { const l = document.querySelector('.sidebar-settings-link').getBoundingClientRect(); \
         const sc = document.querySelector('.sidebar-scroll').getBoundingClientRect(); \
         return [l.right, innerWidth, l.top - sc.bottom, \
         document.documentElement.scrollWidth - document.documentElement.clientWidth]; })()",
    )
    .await;
    assert!(drawer[0] <= drawer[1], "the Settings link runs past a phone's screen: {drawer:?}");
    assert!(drawer[2] >= 0.0, "the Settings link isn't below the list: {drawer:?}");
    assert!(drawer[3] <= 0.0, "the opened drawer scrolls sideways: {drawer:?}");
    click_when_present(&page, link, Duration::from_secs(5)).await;
    assert!(wait_pathname(&page, "/settings", Duration::from_secs(10)).await, "the phone's Settings link didn't open /settings");
    wait_for_element(&page, ".settings-index .notifications-toggle", Duration::from_secs(10)).await;
    let fit = numbers(
        &page,
        "[document.documentElement.scrollWidth - document.documentElement.clientWidth, \
         document.querySelector('.notifications-toggle').getBoundingClientRect().right, innerWidth]",
    )
    .await;
    assert!(fit[0] <= 0.0, "/settings scrolls sideways on a phone: {fit:?}");
    assert!(fit[1] <= fit[2], "the notifications toggle runs past a phone's screen: {fit:?}");
    page.close().await.expect("close the tab");
}
