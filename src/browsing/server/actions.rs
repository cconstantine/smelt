//! The page actions on an open session: read the page, navigate, click,
//! fill and go back, each settling before it reads the page again
//! (`act_and_settle`). Split from `server.rs` along its own section.

use super::*;
use crate::fetch_guard;
use chromiumoxide::cdp::browser_protocol::page::{
    EventFrameStartedLoading, EventLoadEventFired, GetNavigationHistoryParams,
    NavigateToHistoryEntryParams,
};

pub(super) async fn extract_page_state(page: &Page) -> Result<PageState, String> {
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
pub(super) async fn find_tagged_element(
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

pub(super) const SELECT_CONTENTS_FN: &str = "function() {
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
        // Through CDP's own history, not `history.back()` evaluated in
        // the page: a back navigation can commit before
        // `Runtime.evaluate` answers, and the evaluate then fails with
        // "Inspected target navigated or closed" though the page did go
        // back (SME-342). With no earlier entry it does nothing, as
        // `history.back()` does.
        let history = page
            .execute(GetNavigationHistoryParams::default())
            .await
            .map_err(|e| format!("failed to read the session's history: {e}"))?;
        let previous = usize::try_from(history.result.current_index - 1)
            .ok()
            .and_then(|i| history.result.entries.get(i));
        let Some(previous) = previous else { return Ok(()) };
        page.execute(NavigateToHistoryEntryParams::new(previous.id))
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
pub(super) async fn act_and_settle(
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
pub(super) fn parse_elements(json: &str) -> Result<Vec<PageElement>, String> {
    serde_json::from_str(json)
        .map_err(|e| format!("failed to parse extracted elements: {e}"))
}
