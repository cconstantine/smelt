use super::*;

/// Scenario 26: conversations share no browser data.
pub(super) async fn conversations_share_no_data(base: &str, conversation_id: i64) {
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
}

/// SME-42: a session with a sandbox route, and a page from another site in a routed session.
pub(super) async fn sandbox_routes() {
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
