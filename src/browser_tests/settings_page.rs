//! The Settings index and the sidebar's bottom-left link to it (SME-512).

use super::*;

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
}
