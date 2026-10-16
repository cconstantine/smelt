use super::*;

/// Scenarios 10-16: the live panel's input forwarding, hovers, Enter, viewers leaving and arriving, a closed session's leftovers, and two opens racing. Returns the interactive test server's address, which later scenarios use.
pub(super) async fn live_panel_input_and_viewers(conversation_id: i64) -> String {
    // --- Scenario 10: live-panel input forwarding (`send_input`)
    // — a real mouse click at real coordinates on a real button,
    // and real typed text into a real input, both dispatched the
    // same way a live-panel viewer's own mouse/keyboard would
    // be. ---
    let (base, _server) = start_interactive_test_server().await;
    navigate(conversation_id, &base)
        .await
        .expect("navigate for input scenario should succeed");
    let page = live_page(conversation_id).expect("session should still be live");
    let toggle_element = page
        .find_element("button")
        .await
        .expect("should find the toggle button");
    let point = toggle_element
        .clickable_point()
        .await
        .expect("should get a clickable point for the button");
    send_input(
        conversation_id,
        BrowserInputEvent::MouseMove {
            x: point.x,
            y: point.y,
            left_held: false,
        },
    )
    .await
    .expect("mouse move should succeed");
    send_input(
        conversation_id,
        BrowserInputEvent::MouseDown {
            x: point.x,
            y: point.y,
        },
    )
    .await
    .expect("mouse down should succeed");
    send_input(
        conversation_id,
        BrowserInputEvent::MouseUp {
            x: point.x,
            y: point.y,
        },
    )
    .await
    .expect("mouse up should succeed");
    let after_click = read(conversation_id)
        .await
        .expect("read after input should succeed");
    assert!(
        after_click.text.contains("toggled"),
        "expected the real mouse click to trigger the toggle, got: {:?}",
        after_click.text
    );

    let input_element = page
        .find_element("input")
        .await
        .expect("should find the input");
    let input_point = input_element
        .clickable_point()
        .await
        .expect("should get a clickable point for the input");
    send_input(
        conversation_id,
        BrowserInputEvent::MouseDown {
            x: input_point.x,
            y: input_point.y,
        },
    )
    .await
    .expect("mouse down on input should succeed");
    send_input(
        conversation_id,
        BrowserInputEvent::MouseUp {
            x: input_point.x,
            y: input_point.y,
        },
    )
    .await
    .expect("mouse up on input should succeed");
    send_input(
        conversation_id,
        BrowserInputEvent::TypeText {
            text: "typed via panel".to_string(),
        },
    )
    .await
    .expect("type text should succeed");
    let after_type = read(conversation_id)
        .await
        .expect("read after typing should succeed");
    assert!(
        after_type.text.contains("typed via panel"),
        "expected the real click-to-focus plus typed text to reach the input, got: {:?}",
        after_type.text
    );

    // --- Scenario 11: a plain hover isn't a drag — a mouse move
    // with no button held reaches the page with `buttons == 0`. ---
    navigate(conversation_id, &format!("{base}/input-events"))
        .await
        .expect("navigate to the input-events page should succeed");
    send_input(
        conversation_id,
        BrowserInputEvent::MouseMove { x: 200.0, y: 300.0, left_held: false },
    )
    .await
    .expect("mouse move should succeed");
    let after_hover = read(conversation_id).await.expect("read should succeed");
    assert!(
        after_hover.text.contains("buttons=0"),
        "expected a plain hover to report no held buttons, got: {:?}",
        after_hover.text
    );

    // --- Scenario 12: Enter in a form's text field submits it,
    // the same as a real keyboard's Enter would. ---
    let page = live_page(conversation_id).expect("session should still be live");
    let field = page.find_element("#q").await.expect("should find #q");
    let field_point = field.clickable_point().await.expect("clickable point for #q");
    for event in [
        BrowserInputEvent::MouseDown { x: field_point.x, y: field_point.y },
        BrowserInputEvent::MouseUp { x: field_point.x, y: field_point.y },
        BrowserInputEvent::PressKey { key: "Enter".to_string(), modifiers: 0 },
    ] {
        send_input(conversation_id, event).await.expect("input should succeed");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_enter = read(conversation_id).await.expect("read should succeed");
    assert!(
        after_enter.text.contains("submitted"),
        "expected Enter to submit the form, got: {:?}",
        after_enter.text
    );

    // --- Scenario 13: a viewer leaving and another arriving right
    // away (a panel re-render, a quick conversation switch back)
    // never leaves the new viewer without frames, whatever the
    // timing between the two. ---
    navigate_to_animated_page(conversation_id).await;
    for delay_ms in [0, 1, 5, 20, 50] {
        let mut sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
        expect_fresh_frame(&mut sub).await;
        drop(sub);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    let mut last_sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
    expect_fresh_frame(&mut last_sub).await;
    drop(last_sub);

    // --- Scenario 14: a subscription left over from a *closed*
    // session can't affect the next session opened for the same
    // conversation when it finally drops. ---
    let stale_sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
    close_session(conversation_id).await.expect("close should succeed");
    open_session(conversation_id, None)
        .await
        .expect("re-open should succeed");
    navigate_to_animated_page(conversation_id).await;
    let mut fresh_sub = subscribe_frames(conversation_id).expect("subscribe should succeed");
    expect_fresh_frame(&mut fresh_sub).await;
    drop(stale_sub);
    tokio::time::sleep(Duration::from_millis(300)).await;
    expect_fresh_frame(&mut fresh_sub).await;
    drop(fresh_sub);

    // --- Scenario 15: the live panel's frame stream releases its
    // viewer as soon as the stream is dropped (the SSE connection
    // closing), so a departed viewer doesn't keep the screencast
    // running for the rest of the session. ---
    assert_eq!(frame_subscriber_count(conversation_id), 0);
    let mut stream = Box::pin(frame_stream(
        subscribe_frames(conversation_id).expect("subscribe should succeed"),
    ));
    assert_eq!(frame_subscriber_count(conversation_id), 1);
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream should yield a frame")
        .expect("the stream should not end")
        .expect("frames are never errors");
    drop(stream);
    assert_eq!(frame_subscriber_count(conversation_id), 0);

    close_session(conversation_id)
        .await
        .expect("close_session should succeed");

    // --- Scenario 16: two opens racing for the same conversation
    // — exactly one wins; the other is refused rather than
    // silently replacing (and leaking) the first. ---
    let (first, second) = tokio::join!(
        open_session(conversation_id, None),
        open_session(conversation_id, None),
    );
    assert_eq!(
        [first.is_ok(), second.is_ok()].iter().filter(|ok| **ok).count(),
        1,
        "expected exactly one of two racing opens to succeed, got {first:?} / {second:?}"
    );

    close_session(conversation_id)
        .await
        .expect("close_session should succeed");

    open_session(conversation_id, None)
        .await
        .expect("open for the remaining scenarios should succeed");
    base
}

/// Scenarios 20-21: a second viewer joining a screencast, and a close arriving while an open is still starting.
pub(super) async fn second_viewer_and_close_race(base: &str, conversation_id: i64) {
    // --- Scenario 20: a second viewer joining a screencast that's
    // already running gets a frame straight away, even on a page
    // where nothing is changing (Chrome only sends a new frame
    // when something on screen changes). ---
    // page2 has no inputs, so no blinking caret keeps frames coming.
    navigate(conversation_id, &format!("{base}/page2"))
        .await
        .expect("navigate to a static page should succeed");
    let mut first_viewer = subscribe_frames(conversation_id).expect("subscribe should succeed");
    tokio::time::timeout(Duration::from_secs(5), first_viewer.next_frame())
        .await
        .expect("the first viewer should get a frame")
        .expect("the session should still be open");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut second_viewer = subscribe_frames(conversation_id).expect("subscribe should succeed");
    let joined = tokio::time::timeout(Duration::from_secs(3), second_viewer.next_frame()).await;
    assert!(
        matches!(joined, Ok(Some(_))),
        "a viewer joining a running screencast on a static page got no frame: {joined:?}"
    );
    drop(first_viewer);
    drop(second_viewer);

    close_session(conversation_id)
        .await
        .expect("close_session should succeed");

    // --- Scenario 21: a close that arrives while an open is still
    // in progress (deleting the conversation mid-open) wins —
    // the session doesn't reappear once the open finishes. ---
    let (opened, _) = tokio::join!(
        open_session(conversation_id, None),
        async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            close_session(conversation_id).await
        },
    );
    opened.expect("the open itself should succeed");
    assert!(
        current_url(conversation_id).is_none(),
        "a close issued during an open left the session running"
    );
    close_session(conversation_id)
        .await
        .expect("close_session should succeed");

    open_session(conversation_id, None)
        .await
        .expect("open for the remaining scenarios should succeed");
}
