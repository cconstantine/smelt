use super::*;

/// Scenarios 1-4: a session opens and navigates, a second open and a bad scheme are refused, and every action errors once it's closed.
pub(super) async fn open_navigate_close(conversation_id: i64) {
    // --- Scenario 1: open a session, navigate, read back real
    // rendered text and a real extracted element list. ---
    let (url, _server) = start_test_server(
        "<html><body><h1>Browsing session page</h1>\
                 <a href=\"#\">a link</a></body></html>",
    )
    .await;
    open_session(conversation_id, None)
        .await
        .expect("open_session should succeed");
    let state = navigate(conversation_id, &url)
        .await
        .expect("navigate should succeed");
    assert!(
        state.text.contains("Browsing session page"),
        "expected the rendered text, got: {:?}",
        state.text
    );
    assert!(
        state.elements.iter().any(|e| e.label == "a link"),
        "expected the link to be extracted, got: {:?}",
        state.elements
    );

    // --- Scenario 2: opening a second session for the same
    // conversation is refused (mirrors create_pod). ---
    let second_open =
        open_session(conversation_id, None).await;
    assert!(
        second_open.is_err(),
        "expected a second open_session for the same conversation to be refused"
    );

    // --- Scenario 3: the URL checks run on every call, not just
    // the first one after a session opens — a non-http scheme is
    // refused on a later `navigate` too. ---
    let blocked = navigate(conversation_id, "file:///etc/passwd").await;
    assert!(
        blocked.is_err(),
        "expected a later navigate on an already-open session to still \
                 reject a non-http scheme, got: {blocked:?}"
    );

    close_session(conversation_id)
        .await
        .expect("close_session should succeed");

    // --- Scenario 4: after closing, every action errors clearly
    // rather than reusing a stale/closed page. ---
    let after_close = navigate(conversation_id, &url).await;
    assert!(
        after_close.is_err(),
        "expected navigate to fail once the session is closed"
    );
}

/// Scenarios 5-8: click (navigating and not), fill and go_back, on a fresh session.
pub(super) async fn click_fill_go_back(conversation_id: i64) {
    // --- Scenarios 5-8: click (navigating and non-navigating),
    // fill, go_back — a fresh session against a small multi-page
    // fixture. ---
    let (base, _server) = start_interactive_test_server().await;
    open_session(conversation_id, None)
        .await
        .expect("re-open_session should succeed");
    let state = navigate(conversation_id, &base)
        .await
        .expect("navigate to start page should succeed");
    let link_index = state
        .elements
        .iter()
        .find(|e| e.label == "Go to page 2")
        .map(|e| e.index)
        .expect("the nav link should be in the extracted elements");
    let toggle_index = state
        .elements
        .iter()
        .find(|e| e.label == "Toggle")
        .map(|e| e.index)
        .expect("the toggle button should be in the extracted elements");
    let input_index = state
        .elements
        .iter()
        .find(|e| e.tag == "input")
        .map(|e| e.index)
        .expect("the input should be in the extracted elements");

    // Scenario 5 (riskiest assumption #2, navigating case):
    // clicking a real link navigates, and the returned state
    // reflects the *new* page, not the old one.
    let start = tokio::time::Instant::now();
    let clicked = click(conversation_id, link_index)
        .await
        .expect("clicking the nav link should succeed");
    assert!(
        clicked.url.ends_with("/page2"),
        "expected the URL to reflect the navigation, got: {}",
        clicked.url
    );
    assert!(
        clicked.text.contains("Page two"),
        "expected page2's content, got: {:?}",
        clicked.text
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a real navigating click should resolve promptly via wait_for_navigation, \
                 not wait out the full settle timeout"
    );

    // Scenario 6 (riskiest assumption #2, non-navigating case): a
    // JS-only toggle click doesn't navigate, but its effect still
    // shows up in the re-extracted state, and it doesn't take
    // unreasonably long either (bounded by ACTION_SETTLE_TIMEOUT).
    navigate(conversation_id, &base)
        .await
        .expect("navigate back to the start page should succeed");
    let start = tokio::time::Instant::now();
    let toggled = click(conversation_id, toggle_index)
        .await
        .expect("clicking the toggle should succeed");
    assert!(
        toggled.text.contains("toggled"),
        "expected the JS toggle's effect in the re-extracted text, got: {:?}",
        toggled.text
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a non-navigating click should be bounded by the settle timeout, not hang"
    );

    // Scenario 7: fill types into the input and the page's own JS
    // mirrors it — proves fill delivers real keyboard input, not
    // just a value assignment nothing on the page reacts to.
    let filled = fill(conversation_id, input_index, "hello browsing")
        .await
        .expect("fill should succeed");
    assert!(
        filled.text.contains("hello browsing"),
        "expected the filled value mirrored into the page, got: {:?}",
        filled.text
    );

    // Scenario 8: go_back returns to the previous page in
    // history.
    navigate(conversation_id, &format!("{base}/page2"))
        .await
        .expect("navigate to page2 should succeed");
    let back = go_back(conversation_id)
        .await
        .expect("go_back should succeed");
    assert!(
        back.text.contains("Start page") || back.text.contains("Toggle"),
        "expected go_back to land on the start page, got: {:?}",
        back.text
    );
}

/// Scenario 9: screencast frames arrive for a viewer.
pub(super) async fn screencast_frames(conversation_id: i64) {
    // --- Scenario 9 (riskiest assumption #3): screencast frames
    // actually keep flowing once acked, not just a single frame
    // then a stall — and the subscribe/unsubscribe lifecycle
    // (starting and fully stopping the real CDP screencast) can
    // run more than once cleanly. Needs a page with continuous
    // visual change (a static page may only ever produce one
    // frame, which wouldn't distinguish "ack works" from "nothing
    // new to send anyway"). ---
    navigate_to_animated_page(conversation_id).await;

    {
        let mut sub =
            subscribe_frames(conversation_id).expect("subscribe_frames should succeed");
        let mut received = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while received < 3 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(3), sub.next_frame()).await {
                Ok(Some(frame)) => {
                    assert!(!frame.data.is_empty(), "expected non-empty frame data");
                    received += 1;
                }
                Ok(None) => panic!("the session closed mid-stream"),
                Err(_) => break,
            }
        }
        assert!(
            received >= 2,
            "expected acking to keep frames flowing (got {received} frame(s)) — \
                     a stall after 1 would mean the ack isn't actually unblocking more"
        );
    } // `sub` drops here — the last viewer leaving stops the screencast.

    // Let the stop land, then confirm a second subscription gets
    // genuinely new frames (the screencast restarted, not just the
    // cached last frame).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut second_sub = subscribe_frames(conversation_id)
        .expect("a second subscribe_frames should succeed");
    expect_fresh_frame(&mut second_sub).await;
    drop(second_sub);
}
