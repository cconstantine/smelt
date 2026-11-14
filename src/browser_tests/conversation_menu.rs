//! SME-513: the conversation header's ☰ menu, end to end, on a laptop
//! and on a phone: opening and closing it, each header shape's items, and
//! each item's action.

use super::*;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;

/// The open menu's items, as `label|role|aria-checked|aria-disabled`.
async fn menu_items(page: &chromiumoxide::Page) -> Vec<String> {
    page.evaluate(
        "[...document.querySelectorAll('.conversation-menu [role^=menuitem]')].map(b => \
         [b.innerText.replace(/\\s+/g, ' ').trim(), b.getAttribute('role'), \
          b.getAttribute('aria-checked') ?? '', b.getAttribute('aria-disabled') ?? ''].join('|'))",
    )
    .await
    .expect("read the menu")
    .into_value()
    .expect("strings")
}

/// Whether the focused element matches `selector`.
async fn focused(page: &chromiumoxide::Page, selector: &str) -> bool {
    js_bool(page, &format!("document.activeElement?.matches({selector:?}) ?? false")).await
}

/// Opens `id` in a new tab at `width`×`height`, live.
async fn open_at(t: &Scenario<'_>, id: i64, (width, height, mobile): (i64, i64, bool)) -> chromiumoxide::Page {
    let page = t.tab("about:blank").await;
    page.execute(SetDeviceMetricsOverrideParams::new(width, height, 2.0, mobile))
        .await
        .expect("set the window's size");
    page.goto(t.url(&format!("conversation/{id}"))).await.expect("open the conversation");
    wait_for_live_client(&page, id).await;
    page
}

/// The menu opened, read and closed both ways, at one size: its button's
/// attributes, its items in order (S2), Escape back onto ☰, and the
/// backdrop. On a phone, its box is on screen, above the composer, and
/// each item is at least 40 px tall.
async fn open_and_close(page: &chromiumoxide::Page, id: i64, (width, _, mobile): (i64, i64, bool)) {
    let button = format!(".conversation-header[data-conversation-id='{id}'] .conversation-menu-button");
    wait_for_element(page, &button, Duration::from_secs(10)).await;
    assert!(
        js_bool(
            page,
            &format!(
                "(() => {{ const b = document.querySelector({button:?}); \
                 return b.getAttribute('aria-haspopup') === 'menu' && b.getAttribute('aria-expanded') === 'false'; }})()"
            )
        )
        .await,
        "{width}px: ☰ is a closed menu button"
    );
    open_conversation_menu(page, id).await;
    assert!(js_bool(page, &format!("document.querySelector({button:?}).getAttribute('aria-expanded') === 'true'")).await);
    // Archive and Delete wait for the tree to have the row (S1 to S2).
    let s2 = ["Show thinking|menuitemcheckbox|false|", "Compact|menuitem||", "Archive|menuitem||", "Delete|menuitem||"];
    assert!(
        wait_until(|| async { menu_items(page).await == s2 }, Duration::from_secs(10)).await,
        "{width}px: a top-level conversation's items, in order: {:?}",
        menu_items(page).await
    );
    assert!(focused(page, ".conversation-menu-thinking").await, "{width}px: the click focuses the first item");
    if mobile {
        let measured: Vec<f64> = page
            .evaluate(
                "(() => { const m = document.querySelector('.conversation-menu').getBoundingClientRect(); \
                 const c = document.querySelector('.composer').getBoundingClientRect(); \
                 const h = [...document.querySelectorAll('.conversation-menu-item')].map(b => b.getBoundingClientRect().height); \
                 return [m.left, m.right, m.bottom, c.top, Math.min(...h)]; })()",
            )
            .await
            .expect("measure the menu")
            .into_value()
            .expect("numbers");
        assert!(measured[0] >= 0.0 && measured[1] <= width as f64, "{width}px: the menu is on screen: {measured:?}");
        assert!(measured[2] <= measured[3], "{width}px: the menu is above the composer: {measured:?}");
        assert!(measured[4] >= 40.0, "{width}px: each item is a finger's height: {measured:?}");
    }
    press_escape(page, ".conversation-menu-thinking").await;
    assert!(wait_for_count(page, ".conversation-menu", 0, Duration::from_secs(5)).await, "{width}px: Escape closes it");
    assert!(focused(page, ".conversation-menu-button").await, "{width}px: Escape puts focus back on ☰");
    // Review 1, A-L4: with the menu already open, the arrows on ☰ move
    // into it, and Escape on ☰ closes it.
    open_conversation_menu(page, id).await;
    press_key(page, ".conversation-menu-button", "ArrowUp").await;
    assert!(
        wait_until(|| focused(page, ".conversation-menu-delete"), Duration::from_secs(5)).await,
        "{width}px: ArrowUp on an open ☰ focuses the last item"
    );
    press_key(page, ".conversation-menu-button", "ArrowDown").await;
    assert!(
        wait_until(|| focused(page, ".conversation-menu-thinking"), Duration::from_secs(5)).await,
        "{width}px: ArrowDown on an open ☰ focuses the first item"
    );
    press_key(page, ".conversation-menu-button", "Escape").await;
    assert!(wait_for_count(page, ".conversation-menu", 0, Duration::from_secs(5)).await, "{width}px: Escape on an open ☰ closes it");
    open_conversation_menu(page, id).await;
    click_when_present(page, ".conversation-menu-backdrop", Duration::from_secs(5)).await;
    assert!(wait_for_count(page, ".conversation-menu", 0, Duration::from_secs(5)).await, "{width}px: a click outside closes it");
}

/// Show thinking: the seeded reply's thinking shows, and the item's
/// label and `aria-checked` flip; then it hides again.
async fn show_thinking(page: &chromiumoxide::Page, id: i64) {
    assert!(wait_for_text(page, "the seeded answer", Duration::from_secs(10)).await);
    assert!(wait_for_count(page, ".thinking-block", 0, Duration::from_secs(2)).await, "thinking starts hidden");
    open_conversation_menu(page, id).await;
    click_when_present(page, ".conversation-menu-thinking", Duration::from_secs(5)).await;
    assert!(wait_for_count(page, ".conversation-menu", 0, Duration::from_secs(5)).await, "Show thinking closes the menu");
    assert!(
        wait_until(|| focused(page, ".conversation-menu-button"), Duration::from_secs(5)).await,
        "Show thinking puts focus back on ☰ (review 1, A-L4)"
    );
    assert!(wait_for_count(page, ".thinking-block", 1, Duration::from_secs(5)).await, "the thinking shows");
    open_conversation_menu(page, id).await;
    assert_eq!(menu_items(page).await[0], "Hide thinking|menuitemcheckbox|true|");
    click_when_present(page, ".conversation-menu-thinking", Duration::from_secs(5)).await;
    assert!(wait_for_count(page, ".thinking-block", 0, Duration::from_secs(5)).await, "and hides again");
}

/// Compact: the item opens the popover with its note focused; while a
/// turn runs it's unavailable and says why.
async fn compact(page: &chromiumoxide::Page, id: i64) {
    open_conversation_menu(page, id).await;
    click_when_present(page, ".conversation-menu-compact", Duration::from_secs(5)).await;
    wait_for_element(page, ".compact-popover", Duration::from_secs(5)).await;
    assert!(focused(page, ".compact-note").await, "the popover opens with its note focused");
    press_escape(page, ".compact-note").await;
    assert!(wait_for_count(page, ".compact-popover", 0, Duration::from_secs(5)).await);

    send_from(page, "keep talking").await;
    assert!(wait_for_count(page, ".stop-turn", 1, Duration::from_secs(10)).await, "the turn runs");
    open_conversation_menu(page, id).await;
    assert_eq!(menu_items(page).await[1], "Compact once the turn ends|menuitem||true", "unavailable, saying why");
    // A click on an unavailable item does nothing.
    click_when_present(page, ".conversation-menu-compact", Duration::from_secs(5)).await;
    assert!(wait_for_count(page, ".compact-popover", 0, Duration::from_secs(1)).await, "no popover while a turn runs");
    press_escape(page, ".conversation-menu-compact").await;
    click_when_present(page, ".stop-turn", Duration::from_secs(5)).await;
    assert!(wait_for_count(page, ".stop-turn", 0, Duration::from_secs(10)).await, "the turn ends");
}

/// Archive: the label flips, and back.
async fn archive(page: &chromiumoxide::Page, id: i64) {
    for (from, to) in [("Archive", "Unarchive"), ("Unarchive", "Archive")] {
        open_conversation_menu(page, id).await;
        assert_eq!(menu_items(page).await[2], format!("{from}|menuitem||"));
        click_when_present(page, ".conversation-menu-archive", Duration::from_secs(5)).await;
        assert!(wait_for_count(page, ".conversation-menu", 0, Duration::from_secs(5)).await, "{from} closes the menu");
        assert!(
            wait_until(|| focused(page, ".conversation-menu-button"), Duration::from_secs(5)).await,
            "{from} puts focus back on ☰ (review 1, A-L4)"
        );
        open_conversation_menu(page, id).await;
        let flipped = wait_until(
            || async { menu_items(page).await.get(2).is_some_and(|s| s.starts_with(&format!("{to}|"))) },
            Duration::from_secs(10),
        )
        .await;
        assert!(flipped, "{from} flips to {to}: {:?}", menu_items(page).await);
        press_escape(page, ".conversation-menu-archive").await;
    }
}

/// An agent's header (S3): Show thinking, Compact and Delete, no Archive.
async fn agent_shape(page: &chromiumoxide::Page, agent: i64) {
    open_conversation_menu(page, agent).await;
    let labels = || async {
        menu_items(page).await.iter().map(|s| s.split('|').next().unwrap_or("").to_string()).collect::<Vec<_>>()
    };
    assert!(
        wait_until(|| async { labels().await == ["Show thinking", "Compact", "Delete"] }, Duration::from_secs(10)).await,
        "an agent has no Archive: {:?}",
        labels().await
    );
    press_escape(page, ".conversation-menu-thinking").await;
}

/// Delete: the first click arms it in place, the menu staying open (Q2),
/// and a list refetch leaves it armed (SME-505's signature was an armed
/// Delete vanishing under one); the confirm drops the row and lands on
/// `lands` (the parent for an agent, Home for a root).
async fn delete(page: &chromiumoxide::Page, id: i64, lands: &str) {
    open_conversation_menu(page, id).await;
    let delete = menu_delete(id);
    click_when_present(page, &delete, Duration::from_secs(10)).await;
    let armed = format!("{delete}.confirm");
    wait_for_element(page, &armed, Duration::from_secs(5)).await;
    assert!(js_bool(page, "!!document.querySelector('.conversation-menu[role=menu]')").await, "arming leaves the menu open");
    let before = list_requests(page).await;
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    assert!(
        wait_until(|| async { list_requests(page).await > before }, Duration::from_secs(10)).await,
        "ConversationsChanged refetches the list"
    );
    // The refetch's result is applied by an effect after the request ends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(js_bool(page, &format!("!!document.querySelector({armed:?})")).await, "a refetch leaves Delete armed");
    click_when_present(page, &armed, Duration::from_secs(5)).await;
    assert!(
        wait_for_count(page, &format!(".conversation-item[data-conversation-id='{id}']"), 0, Duration::from_secs(10)).await,
        "the confirm drops the row"
    );
    let at_lands = format!("location.pathname === {lands:?}");
    assert!(
        wait_until(|| js_bool(page, &at_lands), Duration::from_secs(10)).await,
        "deleting lands on {lands}"
    );
    // Landing on its parent, the header stays, and focus is back on ☰
    // (review 1, A-L4); Home has no header to hold it.
    if lands != "/" {
        assert!(
            wait_until(|| focused(page, ".conversation-menu-button"), Duration::from_secs(5)).await,
            "Delete's confirm puts focus back on ☰"
        );
    }
}

/// Holds the page's next DELETE reply back 3 s after the server has
/// answered: by then the deleted conversation's stream has ended and its
/// resubscribe come back "not found", which unmounts the header.
const DELAY_NEXT_DELETE_REPLY: &str = "(() => { const real = window.fetch; let armed = true; \
     window.fetch = function (input, init) { \
       const method = ((init && init.method) || (input && input.method) || 'GET').toUpperCase(); \
       const reply = real.apply(this, arguments); \
       if (!armed || method !== 'DELETE') return reply; \
       armed = false; \
       return reply.then(r => new Promise(done => setTimeout(() => done(r), 3000))); }; })()";

/// Review 1, A-M1: a Delete whose reply lands after the header has gone
/// (the conversation's own stream said "not found" first) still lands
/// Home: the delete isn't the header's to cancel.
async fn delete_with_a_late_reply(t: &Scenario<'_>) {
    let root = t.conversation().await;
    let page = open_at(t, root.id, (1400, 900, false)).await;
    open_conversation_menu(&page, root.id).await;
    page.evaluate(DELAY_NEXT_DELETE_REPLY).await.expect("hold the next DELETE's reply");
    let delete = menu_delete(root.id);
    click_when_present(&page, &delete, Duration::from_secs(10)).await;
    click_when_present(&page, &format!("{delete}.confirm"), Duration::from_secs(5)).await;
    assert!(
        wait_until(|| js_bool(&page, "location.pathname === '/'"), Duration::from_secs(15)).await,
        "a Delete whose reply comes after the header unmounted still lands Home"
    );
    page.close().await.expect("close the tab");
}

/// SME-513: the ☰ menu at 1400×900 and at 390×844 (a phone).
pub(super) async fn scenario_conversation_menu(t: &Scenario<'_>) {
    for size in [(1400, 900, false), (390, 844, true)] {
        let root = t.conversation().await;
        seed_user_message(t.pool, root.id, "a question").await;
        db::create_message(
            t.pool,
            root.id,
            "assistant",
            &[
                anthropic::ContentBlock::Thinking { thinking: "a seeded thought".to_string(), signature: "s".to_string() },
                anthropic::ContentBlock::Text { text: "the seeded answer".to_string() },
            ],
        )
        .await
        .expect("seed a reply with thinking");
        let agent = seed_agent(t.pool, root.id, "an agent", None).await;

        let page = open_at(t, root.id, size).await;
        open_and_close(&page, root.id, size).await;
        show_thinking(&page, root.id).await;
        archive(&page, root.id).await;
        compact(&page, root.id).await;
        page.close().await.expect("close the tab");

        // Then Delete, the agent first: it lands on its root, which is
        // then deleted in turn and lands Home.
        let page = open_at(t, agent, size).await;
        agent_shape(&page, agent).await;
        delete(&page, agent, &format!("/conversation/{}", root.id)).await;
        wait_for_live_client(&page, root.id).await;
        delete(&page, root.id, "/").await;
        page.close().await.expect("close the tab");
    }
    delete_with_a_late_reply(t).await;
}
