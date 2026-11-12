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
/// practice it has been the transcript's background. A click step that
/// fails is read against the watcher too, and counts as landed when the
/// click reached its target: the selector of a two-step button matches it
/// armed too, so a retry would confirm what that click armed (SME-505).
pub(super) async fn click_when_present(page: &chromiumoxide::Page, selector: &str, timeout: Duration) {
    click_when_present_with(page, selector, timeout, real_click).await;
}

/// `click_when_present`, with its click step given: a scenario can stand in
/// a step that fails after its click went through (SME-505).
pub(super) async fn click_when_present_with(page: &chromiumoxide::Page, selector: &str, timeout: Duration, click: ClickStep) {
    let deadline = tokio::time::Instant::now() + timeout;
    // Best effort: a page that can't take it still gets its click.
    let _ = page.evaluate(RECORDER).await;
    let watch = format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return false; \
         window.__smeltClickTarget = el; window.__smeltClick = 'no click seen'; \
         if (!window.__smeltClickWatcher) {{ window.__smeltClickWatcher = true; \
         document.addEventListener('click', e => {{ const t = window.__smeltClickTarget; \
         window.__smeltClick = t && (t === e.target || t.contains(e.target)) ? 'hit' : 'clicked ' + (e.target.className || e.target.tagName); }}, true); }} \
         return true; }})()"
    );
    let outcome = "typeof window.__smeltClick === 'string' ? window.__smeltClick : 'navigated'";
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
                Ok(element) => {
                    // A click step that fails may have pressed and released
                    // already, after the watcher saw the click: that click
                    // landed, and a retry would repeat it, confirming a
                    // two-step button it armed (SME-505). So its failure is
                    // read against the watcher too.
                    let failed = click(&element).await.err();
                    match page.evaluate(outcome).await {
                        // The page went away mid-read: the click did something.
                        Err(_) => return,
                        Ok(value) => {
                            let result: String = value.into_value().unwrap_or_default();
                            if result == "hit" || result == "navigated" {
                                return;
                            }
                            last = match failed {
                                Some(e) => format!("the click failed: {e} ({result})"),
                                None => result,
                            };
                        }
                    }
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let held = page_state(page).await;
            panic!("clicking {selector} didn't land on it: {last}; the page: {held}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Records, once per page, the last 30 things a failed click or wait should
/// be read against (SME-505): every mouse press, release and click with
/// its target and row, every button's class change, every conversation row
/// added or removed (a row moved is both), and every in-app navigation.
const RECORDER: &str = "(() => { if (window.__smeltLog) return true; const log = window.__smeltLog = []; const t0 = performance.now(); \
     const put = s => { log.push(Math.round(performance.now() - t0) + 'ms ' + s); if (log.length > 30) log.shift(); }; \
     const row = el => { const r = el && el.closest && el.closest('[data-conversation-id]'); return r ? r.dataset.conversationId : 'no row'; }; \
     const name = el => (typeof el.className === 'string' && el.className) || el.tagName; \
     for (const type of ['mousedown', 'mouseup', 'click']) document.addEventListener(type, e => put(type + ' on ' + name(e.target) + ' in ' + row(e.target)), true); \
     new MutationObserver(ms => { for (const m of ms) { \
       if (m.type === 'attributes') { if (m.target.tagName === 'BUTTON') put('class ' + m.target.className + ' in ' + row(m.target)); continue; } \
       for (const n of m.removedNodes) if (n.nodeType === 1 && n.dataset && n.dataset.conversationId) put('row ' + n.dataset.conversationId + ' removed'); \
       for (const n of m.addedNodes) if (n.nodeType === 1 && n.dataset && n.dataset.conversationId) put('row ' + n.dataset.conversationId + ' added'); } }) \
       .observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ['class'] }); \
     const push = history.pushState; history.pushState = function () { put('pushState ' + arguments[2]); return push.apply(this, arguments); }; \
     return true; })()";

/// What the page held when a click or wait gave up, as JSON: where the tab
/// was, which rows had an armed two-step button, the sidebar's error, and
/// `RECORDER`'s log. Each names a different cause of a missing button: a
/// navigation, a confirm that already happened (no row armed, perhaps an
/// error), a click that never reached the page (no click logged).
pub(super) async fn page_state(page: &chromiumoxide::Page) -> String {
    let read = "JSON.stringify({ url: location.href, \
         armed: [...document.querySelectorAll('.confirm')].map(b => { const r = b.closest('[data-conversation-id]'); return r ? r.dataset.conversationId : b.className; }), \
         sidebar_error: (document.querySelector('.sidebar-body > .error') || {}).innerText ?? null, \
         log: window.__smeltLog || 'nothing recorded: no click_when_present on this page' })";
    // Bounded: a hung renderer may be why the click or wait failed, and an
    // unbounded read would hold its failure up to chromiumoxide's 30 s, past
    // a scenario's limit, losing the message (review 1, L1).
    match tokio::time::timeout(Duration::from_secs(2), page.evaluate(read)).await {
        Ok(Ok(value)) => value.into_value().unwrap_or_else(|e| format!("unreadable: {e}")),
        Ok(Err(e)) => format!("couldn't read it: {e}"),
        Err(_) => "page state unavailable: the page didn't answer within 2 s".to_string(),
    }
}

/// `wait_for_stable`'s failure: what kept `selector` from settling
/// (`state`), then `page_state`. Its own function so a scenario can check
/// it without a panic, whose text would sit in every tier log in a flake's
/// signature form (SME-505 review 1, M2).
pub(super) async fn settle_failure(page: &chromiumoxide::Page, selector: &str, state: &str) -> String {
    let held = page_state(page).await;
    format!("{selector} never settled to be clicked: {state}; the page: {held}")
}

/// Waits until `selector`'s element exists and has the same box on two
/// reads 150 ms apart, without scrolling anything itself (that would both
/// hide movement and change the page: scrolling the transcript turns off
/// its stick-to-bottom). When its centre is on screen, it must also be the
/// element there (or contain it), so nothing covers it. Returns its box;
/// panics at `deadline` naming what kept it from settling, and what the
/// page held then (`page_state`).
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
        if tokio::time::Instant::now() >= deadline {
            panic!("{}", settle_failure(page, selector, &state).await);
        }
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

/// A click step that misses once: the first time it changes its target's
/// class for a reason of its own and clicks the page's body instead, so
/// the click is seen to land elsewhere; after that it clicks for real.
fn miss_once_then_click(element: &chromiumoxide::Element) -> BoxFuture<'_, Result<(), String>> {
    Box::pin(async move {
        let decoy = "function() { if (window.__smeltMissedOnce) return false; window.__smeltMissedOnce = true; \
             this.classList.add('changed-for-another-reason'); document.body.click(); return true; }";
        let missed = element.call_js_fn(decoy, false).await.map_err(|e| e.to_string())?;
        if missed.result.value == Some(serde_json::Value::Bool(true)) {
            return Ok(());
        }
        element.click().await.map(|_| ()).map_err(|e| e.to_string())
    })
}

/// SME-505: a click that reached its target isn't clicked again, even when
/// its click step reports an error; one seen to land elsewhere still is,
/// whatever its target did meanwhile (review 1, M1). A sidebar
/// Delete arms on its first click and deletes on its second, and the same
/// selector matches it armed or not; so a click whose report is lost (here
/// an error after the press and release) and is retried confirms the
/// delete, and the scenario's own confirm then finds no armed button:
/// "….confirm never settled to be clicked: missing", main's CI run
/// 43219768271. That such a retry happened there is not shown: the run's
/// log has only that line. This shows the path exists, and closes it.
pub(super) async fn scenario_click_not_repeated(t: &Scenario<'_>) {
    let missed = t.conversation().await;
    let target = t.conversation().await;
    let open = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", open.id))).await;
    wait_for_live_client(&page, open.id).await;
    // A click seen to land elsewhere is retried, even when its target's
    // class changes meanwhile for another reason (SME-505 review 1, M1).
    let missed_delete = format!(".conversation-item[data-conversation-id='{}'] .delete-conversation", missed.id);
    click_when_present_with(&page, &missed_delete, Duration::from_secs(5), miss_once_then_click).await;
    wait_for_element(&page, &format!("{missed_delete}.confirm"), Duration::from_secs(5)).await;
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
    // A wait that never settles says what the page held, so the next red
    // tells its causes apart (SME-505): where the tab was, which row was
    // armed, the sidebar's error, and the clicks and changes before it.
    // Read without a panic: one would print its text, in SME-505's
    // signature form, into every green tier log (review 1, M2).
    let gone = format!(".conversation-item[data-conversation-id='{}'] .no-such-button", target.id);
    let why = settle_failure(&page, &gone, "missing").await;
    for told in [
        format!("{gone} never settled to be clicked: missing; the page: "),
        format!("\"url\":\"{}\"", t.url(&format!("conversation/{}", open.id))),
        format!("\"armed\":[\"{}\"]", target.id),
        "\"sidebar_error\":null".to_string(),
        format!("click on two-step-text in {}", target.id),
        format!("class delete-conversation confirm in {}", target.id),
    ] {
        assert!(why.contains(&told), "the failure should say {told}: {why}");
    }
    click_when_present(&page, &armed, Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&page, &format!(".conversation-item[data-conversation-id='{}']", target.id), 0, Duration::from_secs(10)).await,
        "the confirm should delete the conversation"
    );
    // A page too busy to answer doesn't hold up the failure it would
    // annotate (review 1, L1): its state is given up on after 2 s.
    page.evaluate("setTimeout(() => { const end = Date.now() + 4000; while (Date.now() < end) {} }, 0)")
        .await
        .expect("keep the page busy");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let asked = tokio::time::Instant::now();
    let held = page_state(&page).await;
    let took = asked.elapsed();
    assert!(
        took < Duration::from_millis(3000) && held.starts_with("page state unavailable"),
        "a busy page's state should be given up on after 2 s: {held:?} after {took:?}"
    );
}
