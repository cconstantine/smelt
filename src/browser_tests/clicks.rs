//! Clicking in the tier: `click_when_present` and the `wait_for_stable`
//! it waits on (moved here from `harness.rs` for SME-505).

use super::*;

use futures_util::future::BoxFuture;

/// One click on `element`, as `click_when_present` makes it: chromiumoxide
/// scrolls it into view, reads its position, then presses and releases the
/// mouse there.
pub(super) type ClickStep = for<'a> fn(&'a chromiumoxide::Element) -> BoxFuture<'a, Result<(), String>>;

fn real_click(element: &chromiumoxide::Element) -> BoxFuture<'_, Result<(), String>> {
    Box::pin(async move { element.click().await.map(|_| ()).map_err(|e| e.to_string()) })
}

/// Clicks `selector` once it exists and has stopped moving, and makes sure
/// the click landed on it, retrying until `timeout` if it didn't.
///
/// A chromiumoxide click scrolls the element into view, reads its position,
/// then presses and releases the mouse there, in separate round trips. In
/// the first moments after a load the chat page keeps re-scrolling its
/// transcript to the bottom as data arrives, so a click aimed at an element
/// that moves in between lands on something else. On SME-68 that was 8
/// clicks in 40 on the compaction divider, throttled: the press hit
/// `.messages` and nothing opened. So this waits until the element is still
/// (`wait_for_stable`), clicks, then checks the resulting `click` event's
/// target was that very element (held by reference, so a list re-sorting
/// meanwhile doesn't confuse it): a miss is tried again. A click that
/// loaded another page takes the check with it, and counts as landed. A
/// miss that hit something with its own action does that action too; in
/// practice it has been the transcript's background. A click not seen to
/// land whose target has changed its class anyway counts as landed, and
/// isn't repeated: the selector of a two-step button matches it armed too,
/// so a retry would confirm what the lost click armed (SME-505).
pub(super) async fn click_when_present(page: &chromiumoxide::Page, selector: &str, timeout: Duration) {
    click_when_present_with(page, selector, timeout, real_click).await;
}

/// `click_when_present`, with its click step given: a scenario can stand in
/// a step that fails after its click went through (SME-505).
pub(super) async fn click_when_present_with(page: &chromiumoxide::Page, selector: &str, timeout: Duration, click: ClickStep) {
    let deadline = tokio::time::Instant::now() + timeout;
    let watch = format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return false; \
         window.__smeltClickTarget = el; window.__smeltClick = 'no click seen'; window.__smeltClickBefore = el.className; \
         if (!window.__smeltClickWatcher) {{ window.__smeltClickWatcher = true; \
         document.addEventListener('click', e => {{ const t = window.__smeltClickTarget; \
         window.__smeltClick = t && (t === e.target || t.contains(e.target)) ? 'hit' : 'clicked ' + (e.target.className || e.target.tagName); }}, true); }} \
         return true; }})()"
    );
    let outcome = "typeof window.__smeltClick === 'string' ? window.__smeltClick : 'navigated'";
    // Whether the target, still in the page, has another class than when
    // the click was aimed at it: the click did something to it.
    let changed = "(() => { const t = window.__smeltClickTarget; \
         return !!t && t.isConnected && t.className !== window.__smeltClickBefore; })()";
    let mut last = String::new();
    loop {
        wait_for_stable(page, selector, deadline).await;
        let watching = match page.evaluate(watch.as_str()).await {
            Ok(value) => value.into_value::<bool>().unwrap_or(false),
            Err(e) => {
                last = format!("couldn't watch the click: {e}");
                false
            }
        };
        if watching {
            match page.find_element(selector).await {
                Err(e) => last = format!("gone before the click: {e}"),
                Ok(element) => match click(&element).await {
                    Err(e) => last = format!("the click failed: {e}"),
                    Ok(()) => match page.evaluate(outcome).await {
                        // The page went away mid-read: the click did something.
                        Err(_) => return,
                        Ok(value) => {
                            let result: String = value.into_value().unwrap_or_default();
                            if result == "hit" || result == "navigated" {
                                return;
                            }
                            last = result;
                        }
                    },
                },
            }
            // Not seen to land, yet its target changed: it did land, and a
            // retry would do it again. A two-step button armed by a click
            // whose report was lost would be confirmed by the retry
            // (SME-505). The page renders a click's change a little after
            // it, so this watches for one for a moment before retrying.
            if target_changed(page, changed).await {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "clicking {selector} didn't land on it: {last}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Whether `changed` (`click_when_present_with`'s check) turns true within
/// half a second: a click's effect is rendered a few milliseconds to
/// about 90 ms after it on the tier's long sidebar.
async fn target_changed(page: &chromiumoxide::Page, changed: &str) -> bool {
    let until = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        if page.evaluate(changed).await.ok().and_then(|v| v.into_value::<bool>().ok()) == Some(true) {
            return true;
        }
        if tokio::time::Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until `selector`'s element exists and has the same box on two
/// reads 150 ms apart, without scrolling anything itself (that would both
/// hide movement and change the page: scrolling the transcript turns off
/// its stick-to-bottom). When its centre is on screen, it must also be the
/// element there (or contain it), so nothing covers it. Returns its box;
/// panics at `deadline` naming what kept it from settling.
pub(super) async fn wait_for_stable(page: &chromiumoxide::Page, selector: &str, deadline: tokio::time::Instant) -> String {
    let probe = format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return 'missing'; \
         const r = el.getBoundingClientRect(); if (r.width === 0 || r.height === 0) return 'hidden'; \
         const x = r.x + r.width / 2, y = r.y + r.height / 2; \
         if (x >= 0 && y >= 0 && x < innerWidth && y < innerHeight) {{ const hit = document.elementFromPoint(x, y); \
         if (!hit || !(hit === el || el.contains(hit))) return 'covered by ' + (hit ? (hit.className || hit.tagName) : 'nothing'); }} \
         return [r.x, r.y, r.width, r.height].map(Math.round).join(','); }})()"
    );
    let mut last = String::new();
    loop {
        let state: String = match page.evaluate(probe.as_str()).await {
            Ok(value) => value.into_value().unwrap_or_default(),
            Err(e) => format!("couldn't read it: {e}"),
        };
        if state.contains(',') && !state.starts_with("couldn't") && state == last {
            return state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{selector} never settled to be clicked: {state}"
        );
        last = state;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// A click step whose click goes through and then reports an error, as a
/// CDP call failing after the mouse was pressed and released would.
fn click_then_fail(element: &chromiumoxide::Element) -> BoxFuture<'_, Result<(), String>> {
    Box::pin(async move {
        element.click().await.map_err(|e| e.to_string())?;
        Err("forced after the click went through (SME-505)".to_string())
    })
}

/// SME-505: a click that changed its target isn't clicked again. A sidebar
/// Delete arms on its first click and deletes on its second, and the same
/// selector matches it armed or not; so a click whose report is lost (here
/// an error after the press and release) and is retried confirms the
/// delete, and the scenario's own confirm then finds no armed button:
/// "….confirm never settled to be clicked: missing", main's CI run
/// 43219768271. That such a retry happened there is not shown: the run's
/// log has only that line. This shows the path exists, and closes it.
pub(super) async fn scenario_click_not_repeated(t: &Scenario<'_>) {
    let target = t.conversation().await;
    let open = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", open.id))).await;
    wait_for_live_client(&page, open.id).await;
    let delete = format!(".conversation-item[data-conversation-id='{}'] .delete-conversation", target.id);
    click_when_present_with(&page, &delete, Duration::from_secs(5), click_then_fail).await;
    // Armed, and still armed: the arm wasn't repeated into a confirm.
    let armed = format!("{delete}.confirm");
    wait_for_element(&page, &armed, Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still_armed: bool = page
        .evaluate(format!("!!document.querySelector({armed:?})"))
        .await
        .expect("look for the armed Delete")
        .into_value()
        .expect("bool");
    assert!(still_armed, "the arm was clicked again, and the retry confirmed the delete");
    click_when_present(&page, &armed, Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&page, &format!(".conversation-item[data-conversation-id='{}']", target.id), 0, Duration::from_secs(10)).await,
        "the confirm should delete the conversation"
    );
}
