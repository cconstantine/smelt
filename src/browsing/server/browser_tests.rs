use super::*;

async fn start_test_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let router = axum::Router::new().route(
        "/",
        axum::routing::get(move || async move { axum::response::Html(body) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a test-local port");
    let port = listener.local_addr().expect("local addr").port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server error");
    });
    (format!("http://127.0.0.1:{port}/"), task)
}

/// A start page exercising a navigating link, a non-navigating
/// (JS-only) toggle, and a fillable input with a live mirror —
/// plus a second page the link leads to, for
/// `click`/`fill`/`go_back`.
async fn start_interactive_test_server() -> (String, tokio::task::JoinHandle<()>) {
    let router = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <h1>Start page</h1>\
                             <a href=\"/page2\">Go to page 2</a>\
                             <button onclick=\"document.getElementById('toggle-out').innerText='toggled'\">Toggle</button>\
                             <div id=\"toggle-out\">not toggled</div>\
                             <input type=\"text\" oninput=\"document.getElementById('mirror').innerText=this.value\">\
                             <div id=\"mirror\"></div>\
                             </body></html>",
                )
            }),
        )
        .route(
            "/page2",
            axum::routing::get(|| async {
                axum::response::Html("<html><body><h1>Page two</h1></body></html>")
            }),
        )
        .route(
            "/set-data",
            axum::routing::get(|| async {
                (
                    [(axum::http::header::SET_COOKIE, "smelt_probe=from-first; Path=/")],
                    axum::response::Html(
                        "<html><body>data set<script>\
                                 localStorage.setItem('smelt_probe', 'from-first');\
                                 </script></body></html>",
                    ),
                )
            }),
        )
        .route(
            "/show-data",
            axum::routing::get(|| async { axum::response::Html(SHOW_DATA_PAGE) }),
        )
        .route(
            "/delayed",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <button onclick=\"setTimeout(() => { document.getElementById('late').innerText = 'delayed-done'; }, 300)\">Later</button>\
                             <div id=\"late\">waiting</div>\
                             <a href=\"/slow\">Slow link</a>\
                             </body></html>",
                )
            }),
        )
        .route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                axum::response::Html("<html><body><h1>Slow page</h1></body></html>")
            }),
        )
        .route(
            "/form-fields",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <input id=\"prefilled\" type=\"text\" value=\"1\">\
                             <input id=\"pw\" type=\"password\">\
                             <textarea id=\"notes\"></textarea>\
                             </body></html>",
                )
            }),
        )
        .route(
            "/input-events",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <div id=\"buttons-out\">no move yet</div>\
                             <form onsubmit=\"event.preventDefault(); document.getElementById('submit-out').innerText='submitted';\">\
                             <input id=\"q\" type=\"text\">\
                             </form>\
                             <div id=\"submit-out\">not sent</div>\
                             <script>document.addEventListener('mousemove', e => { document.getElementById('buttons-out').innerText = 'buttons=' + e.buttons; });</script>\
                             </body></html>",
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a test-local port");
    let port = listener.local_addr().expect("local addr").port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server error");
    });
    (format!("http://127.0.0.1:{port}"), task)
}

pub(crate) async fn run_session_scenarios() {
    let conversation_id: i64 = 900_001;

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

    // --- Scenario 26: conversations share no browser data. A
    // cookie and a localStorage value set in one conversation's
    // session are visible to that session, invisible to another
    // conversation's, and gone once the session is closed. ---
    let first = conversation_id;
    let second = conversation_id + 1;
    open_session(first, None)
        .await
        .expect("open the first conversation's session");
    open_session(second, None)
        .await
        .expect("open the second conversation's session");
    navigate(first, &format!("{base}/set-data"))
        .await
        .expect("set data in the first session");
    let own = navigate(first, &format!("{base}/show-data"))
        .await
        .expect("read data in the first session");
    assert!(
        own.text.contains(SEES_FIRST_DATA),
        "the fixture should show the first session its own data, got: {:?}",
        own.text
    );
    let other = navigate(second, &format!("{base}/show-data"))
        .await
        .expect("read data in the second session");
    assert!(
        other.text.contains(SEES_NO_DATA),
        "another conversation's session saw the first one's data: {:?}",
        other.text
    );
    close_session(first).await.expect("close the first session");
    open_session(first, None)
        .await
        .expect("reopen the first conversation's session");
    let reopened = navigate(first, &format!("{base}/show-data"))
        .await
        .expect("read data in the reopened session");
    assert!(
        reopened.text.contains(SEES_NO_DATA),
        "a reopened session still had the closed session's data: {:?}",
        reopened.text
    );
    close_session(first).await.expect("close the first session");
    close_session(second).await.expect("close the second session");

    // --- SME-42: a session with a sandbox route loads `localhost`
    // from the sandbox — the navigation and the page's own requests
    // alike. ---
    let (sandbox_url, _sandbox_server) = start_test_server(
        "<html><body><div id=\"out\">pending</div><script>\
                 fetch('http://localhost:4400/').then(\
                   r => { document.getElementById('out').innerText = 'in-page fetch ' + r.status; },\
                   () => { document.getElementById('out').innerText = 'in-page fetch blocked'; });\
                 </script></body></html>",
    )
    .await;
    let sandbox_addr: std::net::SocketAddr = sandbox_url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .parse()
        .expect("a test server address");
    let dialed = std::sync::Arc::new(Mutex::new(Vec::new()));
    let routed: i64 = 900_101;
    open_session(
        routed,
        Some(crate::egress_proxy::dial_to(sandbox_addr, dialed.clone())),
    )
    .await
    .expect("open a routed session");
    let loaded = navigate(routed, "http://localhost:4400/").await;
    let mut text = loaded.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while loaded.is_ok() && text.contains("pending") && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        text = read(routed).await.map(|s| s.text).unwrap_or_default();
    }
    close_session(routed).await.expect("close the routed session");
    loaded.expect("a routed session should load localhost from the sandbox");
    assert!(text.contains("in-page fetch 200"), "the page's own request to localhost failed: {text:?}");
    assert!(dialed.lock().unwrap().iter().all(|&(_, p)| p == 4400), "unexpected ports: {dialed:?}");

    // --- SME-42 review: a page from another site, open in a routed
    // session, can't reach the sandbox — not by fetch, an image or a
    // WebSocket. 127.0.0.2 is loopback but not a sandbox address. ---
    let outside = tokio::net::TcpListener::bind("127.0.0.2:0").await.expect("bind 127.0.0.2");
    let outside_url = format!("http://{}/", outside.local_addr().expect("local addr"));
    let outside_page: &'static str = "<html><body><div id=\"out\">pending</div>\
                 <img src=\"http://localhost:4500/from-outside-img\">\
                 <script>\
                 fetch('http://localhost:4500/from-outside-fetch', {mode: 'no-cors', method: 'POST', body: 'x'});\
                 try { new WebSocket('ws://localhost:4500/from-outside-ws'); } catch (e) {}\
                 setTimeout(() => { document.getElementById('out').innerText = 'tried'; }, 1500);\
                 </script></body></html>";
    let _outside_server = tokio::spawn(async move {
        let router = axum::Router::new()
            .route("/", axum::routing::get(move || async move { axum::response::Html(outside_page) }));
        axum::serve(outside, router).await.expect("outside server");
    });
    let reached = std::sync::Arc::new(Mutex::new(Vec::new()));
    let guarded: i64 = 900_103;
    open_session(
        guarded,
        Some(crate::egress_proxy::dial_to(sandbox_addr, reached.clone())),
    )
    .await
    .expect("open a routed session");
    let loaded = navigate(guarded, &outside_url).await;
    let mut text = loaded.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while loaded.is_ok() && text.contains("pending") && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        text = read(guarded).await.map(|s| s.text).unwrap_or_default();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    close_session(guarded).await.expect("close the routed session");
    loaded.expect("the outside page should load");
    assert!(text.contains("tried"), "the outside page's script never ran: {text:?}");
    assert!(
        reached.lock().unwrap().is_empty(),
        "a page from another site reached the sandbox: {:?}",
        reached.lock().unwrap()
    );

    scenario_open_after_a_poisoned_session_lock().await;
}

/// SME-119: once a browsing call has panicked while holding the
/// session list, a real session still opens, loads, reads, takes a
/// live-panel viewer and closes: the paths (the insert, `navigate`'s
/// guard read) the unit test can't reach without Chrome.
async fn scenario_open_after_a_poisoned_session_lock() {
    let poisoner = std::thread::spawn(|| {
        let _held = SESSIONS.lock();
        panic!("a browsing call panics while holding the session list (deliberate, SME-119)");
    });
    assert!(poisoner.join().is_err(), "the poisoning thread should have panicked");
    assert!(SESSIONS.is_poisoned(), "the session list should be poisoned now");

    let conversation_id: i64 = 900_119;
    let (url, _server) =
        start_test_server("<html><body><h1>Opened after a poisoning</h1></body></html>").await;
    open_session(conversation_id, None)
        .await
        .expect("a session should open on a recovered lock");
    let state = navigate(conversation_id, &url)
        .await
        .expect("navigate should succeed on a recovered lock");
    assert!(
        state.text.contains("Opened after a poisoning"),
        "expected the rendered text, got: {:?}",
        state.text
    );
    let read_back = read(conversation_id).await.expect("read should succeed");
    assert!(read_back.text.contains("Opened after a poisoning"));
    let viewer = subscribe_frames(conversation_id).expect("a viewer should subscribe");
    assert_eq!(frame_subscriber_count(conversation_id), 1);
    drop(viewer);
    assert_eq!(frame_subscriber_count(conversation_id), 0);
    close_session(conversation_id)
        .await
        .expect("close_session should succeed");
    assert_eq!(current_url(conversation_id), None);
    assert!(!SESSIONS.is_poisoned(), "the lock should be healthy again");
}

/// Shows whatever cookie and localStorage value the page can see.
pub(crate) const SHOW_DATA_PAGE: &str = "<html><body><div id=\"out\"></div><script>\
             document.getElementById('out').innerText = 'cookie=[' + document.cookie + \
             '] storage=[' + (localStorage.getItem('smelt_probe') || '') + ']';\
             </script></body></html>";
pub(crate) const SEES_FIRST_DATA: &str = "cookie=[smelt_probe=from-first] storage=[from-first]";
pub(crate) const SEES_NO_DATA: &str = "cookie=[] storage=[]";

/// A loopback page with a `window.open` button and a
/// target=`_blank` link, both aimed at a route on the same server
/// that counts every request it gets: a popup that really opened
/// would show up in that count.
async fn start_popup_test_server(
) -> (
    String,
    std::sync::Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
    let page: &'static str = "<html><body>\
                 <button onclick=\"window.open('/popup')\">Popup</button>\
                 <a href=\"/blank\" target=\"_blank\">Blank link</a>\
                 </body></html>";
    let (hits_a, hits_b) = (hits.clone(), hits.clone());
    let router = axum::Router::new()
        .route("/", axum::routing::get(move || async move { axum::response::Html(page) }))
        .route("/popup", axum::routing::get(move || count_popup_hit(hits_a.clone())))
        .route("/blank", axum::routing::get(move || count_popup_hit(hits_b.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a test-local port");
    let port = listener.local_addr().expect("local addr").port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("test server error");
    });
    (format!("http://127.0.0.1:{port}/"), hits, task)
}

async fn count_popup_hit(hits: std::sync::Arc<Mutex<Vec<String>>>) -> &'static str {
    hits.lock().unwrap().push("popup page".to_string());
    "opened"
}

/// Waits for a `BrowsingUrlUpdate` carrying exactly `url`, skipping
/// any other events (and URL updates for pages passed on the way).
async fn expect_url_event(
    events: &mut tokio::sync::broadcast::Receiver<crate::events::ConversationEvent>,
    url: &str,
) {
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        let mut seen = Vec::new();
        loop {
            match events.recv().await {
                Ok(crate::events::ConversationEvent::BrowsingUrlUpdate { url: got }) => {
                    if got == url {
                        return Ok(());
                    }
                    seen.push(got);
                }
                Ok(_) => {}
                Err(e) => return Err(format!("event channel error: {e}; saw {seen:?}")),
            }
        }
    })
    .await;
    match found {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("{e}"),
        Err(_) => panic!("no BrowsingUrlUpdate for {url} within 5s"),
    }
}

async fn field_value(page: &Page, selector: &str) -> String {
    page.evaluate(format!("document.querySelector('{selector}').value"))
        .await
        .expect("evaluate should succeed")
        .into_value()
        .expect("value should be a string")
}

/// Waits for a frame produced *after* this call — the current frame
/// is marked seen first, so a stalled screencast can't pass on a
/// stale one.
async fn expect_fresh_frame(sub: &mut FrameSubscription) {
    sub.receiver.borrow_and_update();
    let frame = tokio::time::timeout(Duration::from_secs(5), sub.next_frame())
        .await
        .expect("expected a fresh frame within 5s — the screencast stalled")
        .expect("the session should still be open");
    assert!(!frame.data.is_empty());
}

/// Navigates to a page whose content changes continuously (a
/// `setInterval`-driven counter) — screencast only sends a new
/// frame when something visually changes, so a static page can't
/// distinguish "ack is working" from "nothing new to send
/// anyway."
async fn navigate_to_animated_page(conversation_id: i64) {
    let (animated_url, _server) = start_test_server(
        "<html><body><div id=\"counter\">0</div><script>
                    let n = 0;
                    setInterval(() => { document.getElementById('counter').innerText = String(++n); }, 100);
                </script></body></html>",
    )
    .await;
    navigate(conversation_id, &animated_url)
        .await
        .expect("navigate to the animated page should succeed");
}
