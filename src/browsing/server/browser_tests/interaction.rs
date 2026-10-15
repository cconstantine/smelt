use super::*;

/// Scenarios 17-19b: only http and https, what's typed into a password field, fill replacing a value, and named keys keeping their modifiers.
pub(super) async fn schemes_passwords_and_fields(base: &str, conversation_id: i64) {
    // --- Scenario 17: only http/https can be navigated to — a
    // data: URL never reaches the network, so the request
    // interceptor never sees it and can't be what refuses it. ---
    let data_nav =
        navigate(conversation_id, "data:text/html,<h1>DATA-SCHEME-LOADED</h1>").await;
    assert!(
        data_nav.is_err(),
        "expected a data: URL to be refused, got: {data_nav:?}"
    );

    // --- Scenario 18: what someone types into a password field
    // (say, logging in through the live panel) never shows up in
    // the element list sent to the model. ---
    navigate(conversation_id, &format!("{base}/form-fields"))
        .await
        .expect("navigate to the form-fields page should succeed");
    let page = live_page(conversation_id).expect("session should still be live");
    let pw = page.find_element("#pw").await.expect("should find #pw");
    let pw_point = pw.clickable_point().await.expect("clickable point for #pw");
    for event in [
        BrowserInputEvent::MouseDown { x: pw_point.x, y: pw_point.y },
        BrowserInputEvent::MouseUp { x: pw_point.x, y: pw_point.y },
        BrowserInputEvent::TypeText { text: "hunter2secret".to_string() },
    ] {
        send_input(conversation_id, event).await.expect("input should succeed");
    }
    let after_password = read(conversation_id).await.expect("read should succeed");
    assert!(
        !format!("{after_password:?}").contains("hunter2secret"),
        "a typed password leaked into the page state: {:?}",
        after_password.elements
    );

    // --- Scenario 19: fill replaces a field's existing value
    // rather than appending to it, including filling it with
    // nothing. ---
    let prefilled_index = after_password
        .elements
        .iter()
        .find(|e| e.label == "1")
        .map(|e| e.index)
        .expect("the prefilled input should be in the element list");
    fill(conversation_id, prefilled_index, "5")
        .await
        .expect("fill should succeed");
    assert_eq!(field_value(&page, "#prefilled").await, "5");
    let state = read(conversation_id).await.expect("read should succeed");
    let prefilled_index = state
        .elements
        .iter()
        .find(|e| e.label == "5")
        .map(|e| e.index)
        .expect("the filled input should be in the element list");
    fill(conversation_id, prefilled_index, "")
        .await
        .expect("filling with nothing should succeed");
    assert_eq!(field_value(&page, "#prefilled").await, "");

    // --- Scenario 19b: named keys keep their modifiers — Shift+Tab
    // moves focus backwards, Ctrl+Backspace deletes a word. ---
    let state = read(conversation_id).await.expect("read should succeed");
    let prefilled_index = state
        .elements
        .iter()
        .find(|e| e.tag == "input" && e.label.is_empty())
        .map(|e| e.index)
        .expect("the emptied input should be in the element list");
    fill(conversation_id, prefilled_index, "hello world")
        .await
        .expect("fill should succeed");
    let press = |key: &str, modifiers: i64| BrowserInputEvent::PressKey {
        key: key.to_string(),
        modifiers,
    };
    send_input(conversation_id, press("Backspace", 2))
        .await
        .expect("ctrl+backspace should succeed");
    assert_eq!(field_value(&page, "#prefilled").await, "hello ");
    send_input(conversation_id, press("Tab", 0)).await.expect("tab should succeed");
    send_input(conversation_id, press("Tab", 8))
        .await
        .expect("shift+tab should succeed");
    let focused: String = page
        .evaluate("document.activeElement.id")
        .await
        .expect("evaluate should succeed")
        .into_value()
        .expect("id should be a string");
    assert_eq!(focused, "prefilled", "Shift+Tab should move focus back");
}

/// Scenarios 22-25: a click waits for what it set off, fill takes any text, no popup tabs, and every URL change is reported.
pub(super) async fn clicks_fills_popups_and_urls(base: &str, conversation_id: i64) {
    // --- Scenario 22: a click waits for what it set off. A delayed
    // update (a fetch, a tab switch) shows up in the returned
    // state, and a click that navigates returns the *new* page even
    // when that page is slow to respond. ---
    let state = navigate(conversation_id, &format!("{base}/delayed"))
        .await
        .expect("navigate to the delayed page should succeed");
    let index_of = |state: &PageState, label: &str| {
        state
            .elements
            .iter()
            .find(|e| e.label == label)
            .map(|e| e.index)
            .unwrap_or_else(|| panic!("no element labelled {label:?}: {:?}", state.elements))
    };
    let after_later = click(conversation_id, index_of(&state, "Later"))
        .await
        .expect("clicking Later should succeed");
    assert!(
        after_later.text.contains("delayed-done"),
        "expected the click's delayed update in the returned state, got: {:?}",
        after_later.text
    );
    let after_slow = click(conversation_id, index_of(&after_later, "Slow link"))
        .await
        .expect("clicking the slow link should succeed");
    assert!(
        after_slow.url.ends_with("/slow") && after_slow.text.contains("Slow page"),
        "expected the slow page after a navigating click, got {} / {:?}",
        after_slow.url,
        after_slow.text
    );

    // --- Scenario 23: fill takes any text, not just what a US
    // keyboard can type — accents, CJK, emoji, and newlines in a
    // textarea. ---
    let state = navigate(conversation_id, &format!("{base}/form-fields"))
        .await
        .expect("navigate to the form-fields page should succeed");
    let page = live_page(conversation_id).expect("session should still be live");
    let international = "Zürich 東京 🎉";
    fill(conversation_id, index_of(&state, "1"), international)
        .await
        .expect("filling non-ASCII text should succeed");
    assert_eq!(field_value(&page, "#prefilled").await, international);
    let state = read(conversation_id).await.expect("read should succeed");
    let notes_index = state
        .elements
        .iter()
        .find(|e| e.tag == "textarea")
        .map(|e| e.index)
        .expect("the textarea should be in the element list");
    fill(conversation_id, notes_index, "line one\nline two")
        .await
        .expect("filling multi-line text should succeed");
    assert_eq!(field_value(&page, "#notes").await, "line one\nline two");

    // --- Scenario 24: a session opens no popup tab, and the pages
    // behind its `window.open` button and target=_blank link are
    // never requested at all. ---
    let (escape_url, hits, _escape_server) = start_popup_test_server().await;
    let state = navigate(conversation_id, &escape_url)
        .await
        .expect("navigate to the popup page should succeed");
    let open_pages = || async {
        crate::webfetch::shared_browser()
            .await
            .expect("shared browser")
            .pages()
            .await
            .expect("list pages")
            .len()
    };
    let pages_before = open_pages().await;
    click(conversation_id, index_of(&state, "Popup"))
        .await
        .expect("clicking the popup button should succeed");
    let state = read(conversation_id).await.expect("read should succeed");
    click(conversation_id, index_of(&state, "Blank link"))
        .await
        .expect("clicking the target=_blank link should succeed");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let hits = hits.lock().unwrap().clone();
    assert!(
        hits.is_empty(),
        "a blocked popup still requested its page: {hits:?}"
    );
    assert_eq!(
        open_pages().await,
        pages_before,
        "a popup tab was opened in the shared browser"
    );

    // --- Scenario 25: the session reports every URL change as it
    // happens — the model navigating, a link click, and an in-page
    // change (`pushState`) that never loads a new document — so the
    // live panel's address bar can follow along. ---
    let mut events = crate::events::subscribe(conversation_id);
    let start_url = format!("{base}/");
    navigate(conversation_id, &start_url)
        .await
        .expect("navigate should succeed");
    expect_url_event(&mut events, &start_url).await;
    assert_eq!(current_url(conversation_id).as_deref(), Some(start_url.as_str()));
    let state = read(conversation_id).await.expect("read should succeed");
    click(conversation_id, index_of(&state, "Go to page 2"))
        .await
        .expect("clicking the page 2 link should succeed");
    expect_url_event(&mut events, &format!("{base}/page2")).await;
    live_page(conversation_id)
        .expect("session should still be live")
        .evaluate("history.pushState({}, '', '/pushed')")
        .await
        .expect("pushState should succeed");
    expect_url_event(&mut events, &format!("{base}/pushed")).await;
    assert_eq!(
        current_url(conversation_id),
        Some(format!("{base}/pushed"))
    );
    // A refused URL isn't loaded at all: the error says why, and the
    // session stays on the page it was on. It used to load and be
    // blocked mid-way, leaving Chrome's blank error page
    // (`chrome-error://chromewebdata/`) as the current page, which
    // the model then read (SME-40 F11).
    let refused = "file:///etc/passwd";
    let error = navigate(conversation_id, refused)
        .await
        .expect_err("a non-http scheme is refused before anything loads");
    assert!(error.contains("unsupported scheme"), "the refusal should say why: {error}");
    assert_eq!(current_url(conversation_id), Some(format!("{base}/pushed")));
    let still_here = read(conversation_id).await.expect("read after a refusal");
    assert_eq!(still_here.url, format!("{base}/pushed"), "the session left its page");

    close_session(conversation_id)
        .await
        .expect("close_session should succeed");
}
