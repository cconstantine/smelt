use super::*;

/// SME-111: a reply cut off at its budget is followed by a notice; a tab
/// already open shows it live, as the transcript's notice line naming the
/// limit and what to change, not as the user talking.
pub(super) async fn scenario_cut_off_notice(t: &Scenario<'_>) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    // Live: the tab's event stream is open before the messages are saved.
    assert!(
        wait_until(|| async { crate::events::subscriber_count(conversation.id) > 0 }, Duration::from_secs(10)).await,
        "the tab never subscribed to its conversation"
    );
    for (role, text) in [("assistant", "Half a tho".to_string()), ("user", cut_off_notice(131_072, ReplyLimit::HalfWindow, None))] {
        let saved = db::create_message(t.pool, conversation.id, role, &[crate::anthropic::ContentBlock::Text { text }])
            .await
            .expect("save a message");
        crate::events::publish(
            conversation.id,
            crate::events::ConversationEvent::MessagesAppended { messages: vec![saved] },
        );
    }
    wait_for_element(&page, ".system-notice", Duration::from_secs(10)).await;
    let shown: String = page
        .evaluate("document.querySelector('.system-notice').innerText")
        .await
        .expect("read the notice")
        .into_value()
        .expect("a string");
    assert!(
        shown.contains("cut off at its limit of 131,072 tokens (half the context window)") && shown.contains("reasoning budget"),
        "{shown}"
    );
    let bubbles: usize = page
        .evaluate("[...document.querySelectorAll('.message')].filter(m => m.innerText.includes('Your last reply')).length")
        .await
        .expect("count raw notices")
        .into_value()
        .expect("a number");
    assert_eq!(bubbles, 0, "not shown as the user's own words");
}

/// SME-126: a real turn whose reply llama.cpp cut off in the middle of a
/// `write_file` call (the captured stream) shows its text and the notice
/// naming the call, live, and no row for the call.
pub(super) async fn scenario_cut_off_in_a_call(t: &Scenario<'_>) {
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                include_str!("../anthropic/fixtures/llama_cpp_messages_stream_cut_off.sse"),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the cut-off upstream");
    let address = format!("http://{}", listener.local_addr().expect("its address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let provider = db::create_inference_provider(t.pool, &unique_id("cut-off"), "anthropic", &address, "api_key", "key", false, None, true, None)
        .await
        .expect("save the cut-off provider");
    let conversation = t.conversation().await;
    db::set_conversation_model(t.pool, conversation.id, provider.id, "flash-next")
        .await
        .expect("put the conversation on it");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;

    let turn = crate::turn::run_turn(
        t.pool,
        conversation.id,
        anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: "Write me a poem.".to_string() }],
        },
    )
    .await;
    let mock = *MOCK_PROVIDER.get().expect("the mock provider");
    db::set_conversation_model(t.pool, conversation.id, mock, &t.mock.model)
        .await
        .expect("put the conversation back");
    db::delete_inference_provider(t.pool, provider.id).await.expect("delete the cut-off provider");
    turn.expect("a reply cut off in a call doesn't fail the turn");

    assert!(
        wait_for_text(&page, "I will write a short poem about rivers to a file.", Duration::from_secs(10)).await,
        "the reply's text isn't shown"
    );
    wait_for_element(&page, ".system-notice", Duration::from_secs(10)).await;
    let shown: String = page
        .evaluate("document.querySelector('.system-notice').innerText")
        .await
        .expect("read the notice")
        .into_value()
        .expect("a string");
    assert!(
        shown.contains("cut off at its limit of 16,384 tokens") && shown.contains("write_file") && shown.contains("which didn't run"),
        "{shown}"
    );
    let rows: usize = page
        .evaluate("document.querySelectorAll('.tool-row').length")
        .await
        .expect("count tool rows")
        .into_value()
        .expect("a number");
    assert_eq!(rows, 0, "the dropped call has no row");
}

/// A mock llama.cpp server answering `/v1/models` and `/props` with the
/// user's real server's (fixtures). Returns its address.
pub(super) async fn serve_mock_llama_cpp() -> String {
    let app = axum::Router::new()
        .route("/v1/models", axum::routing::get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")], include_str!("../anthropic/fixtures/llama_cpp_v1_models.json"))
        }))
        .route("/props", axum::routing::get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")], include_str!("../anthropic/fixtures/llama_cpp_props.json"))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the mock llama.cpp server");
    let address = format!("http://{}", listener.local_addr().expect("its address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    address
}

/// SME-111: the new-provider form suggests the llama.cpp kind for an
/// address that answers like one, and switches only when asked; a
/// llama.cpp provider's page shows what its server said and offers its
/// template's efforts and a reasoning budget.
pub(super) async fn scenario_llama_cpp_provider(t: &Scenario<'_>) {
    let address = serve_mock_llama_cpp().await;
    let form = t.tab(t.url("providers/new")).await;
    // Hydrated: the form asks for the price list from the browser.
    wait_for_resource(&form, "/api/price-sources").await;
    let url_field = wait_for_element(&form, "#provider-url", Duration::from_secs(10)).await;
    url_field.focus().await.expect("focus the address");
    url_field.type_str(&address).await.expect("type the address");
    let typed: String = form
        .evaluate("document.querySelector('#provider-url').value")
        .await
        .expect("read the address")
        .into_value()
        .expect("a string");
    assert_eq!(typed, address);
    // Leaving the field commits it, which asks the address.
    wait_for_element(&form, "#provider-name", Duration::from_secs(5))
        .await
        .focus()
        .await
        .expect("focus the name");
    if !wait_for_text(&form, "This looks like a llama.cpp server", Duration::from_secs(10)).await {
        let state: serde_json::Value = form
            .evaluate("({ url: document.querySelector('#provider-url').value, probes: performance.getEntriesByType('resource').filter(e => e.name.includes('probe')).map(e => e.name + ' ' + e.responseStatus), active: document.activeElement?.id })")
            .await
            .expect("read the form")
            .into_value()
            .expect("json");
        panic!("no llama.cpp suggestion: {state}");
    }
    let kind = |page: &chromiumoxide::Page| {
        let page = page.clone();
        async move {
            page.evaluate("document.querySelector('#provider-kind').value")
                .await
                .expect("read the kind")
                .into_value::<String>()
                .expect("a string")
        }
    };
    assert_eq!(kind(&form).await, "anthropic", "suggested, not switched");
    click_when_present(&form, ".provider-kind-suggestion button", Duration::from_secs(5)).await;
    wait_for_element(&form, "#provider-keep-reasoning", Duration::from_secs(5)).await;
    assert_eq!(kind(&form).await, "llama_cpp");

    let provider = db::create_inference_provider(t.pool, &unique_id("llama"), "llama_cpp", &address, "bearer", "key", false, None, true, None)
        .await
        .expect("create a llama.cpp provider");
    let page = t.tab(t.url(&format!("providers/{}", provider.id))).await;
    assert!(
        wait_for_text(&page, "flash-next \u{b7} 1 slot \u{b7} 262,144-token window", Duration::from_secs(15)).await,
        "the server's /props isn't shown"
    );
    let efforts: Vec<String> = page
        .evaluate("[...document.querySelector('select[aria-label=\"Effort for flash-next\"]').options].map(o => o.text)")
        .await
        .expect("read the efforts")
        .into_value()
        .expect("strings");
    assert_eq!(efforts, vec!["template's default", "low", "medium", "high"]);
    wait_for_element(&page, "input[aria-label=\"Reasoning budget for flash-next\"]", Duration::from_secs(5)).await;
    wait_for_element(&page, "input[aria-label=\"Max reply tokens for flash-next\"]", Duration::from_secs(5)).await;
    db::delete_inference_provider(t.pool, provider.id).await.expect("delete the llama.cpp provider");
}

/// A test server that holds every answer until the test calls `release`
/// (SME-118): a state that lasts only while a page or image is loading
/// then lasts until the test has seen it, however slow the runner. A
/// fixed delay instead gives the test a window it has to land in, which a
/// slow runner can miss.
///
/// Every request gets the same `200` with `body`. Dropping it stops
/// accepting connections, and a request still held closes unanswered. A
/// connection that hasn't sent its request yet (a preconnect) is left to
/// its client; one sent after a `release` is still answered.
pub(super) struct HeldPage {
    address: String,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    release: tokio::sync::watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
}

impl HeldPage {
    async fn start(content_type: &str, body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a held page");
        let address = format!("http://{}/", listener.local_addr().expect("the held page's address"));
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A watch, not a oneshot: Chrome may send more than one request (a
        // retry, a favicon), and each one is released.
        let (release, released) = tokio::sync::watch::channel(false);
        let response: std::sync::Arc<[u8]> = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
        .into();
        let counter = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut released = released.clone();
                let counter = counter.clone();
                let response = response.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 1024];
                    // Nothing read: a connection opened ahead of a request
                    // (Chrome preconnects) and closed unused.
                    if !matches!(tokio::io::AsyncReadExt::read(&mut socket, &mut request).await, Ok(n) if n > 0) {
                        return;
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // An error: the HeldPage went without releasing it.
                    if released.wait_for(|released| *released).await.is_err() {
                        return;
                    }
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &response).await;
                });
            }
        });
        Self { address, requests, release, server }
    }

    fn address(&self) -> &str {
        &self.address
    }

    /// How many requests have arrived, answered or held.
    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Answers every held request, and every later one at once.
    fn release(&self) {
        self.release.send_replace(true);
    }
}

impl Drop for HeldPage {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// SME-30: an image in a reply loads after the reply has rendered and
/// grows it. With the transcript stuck to its bottom, it stays at its
/// bottom; with the pointer over the transcript, the text under the
/// pointer doesn't move (SME-75's rule for anything that moves the bottom
/// that isn't new content).
pub(super) async fn scenario_markdown_late_image(t: &Scenario<'_>) {
    for pointer in [false, true] {
        // Held until the "before" state is recorded, so the image is still
        // loading then however slow the runner (SME-118).
        let image = HeldPage::start(
            "image/svg+xml",
            "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='400'><rect width='300' height='400' fill='#88c'/></svg>",
        )
        .await;
        let conversation = t.conversation().await;
        for i in 0..20 {
            db::create_message(
                t.pool,
                conversation.id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &[anthropic::ContentBlock::Text { text: format!("filler message {i}") }],
            )
            .await
            .expect("seed a message");
        }
        db::create_message(
            t.pool,
            conversation.id,
            "assistant",
            &[anthropic::ContentBlock::Text { text: format!("A picture:\n\n![late]({})\n\nThe end.", image.address()) }],
        )
        .await
        .expect("seed the reply with an image");
        let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
        wait_for_live_client(&page, conversation.id).await;
        wait_for_element(&page, ".markdown img.md-image", Duration::from_secs(10)).await;
        // Asked for and held, so "still loading" below is the held answer,
        // not an image that was never requested.
        assert!(
            wait_until(|| async { image.requests() >= 1 }, Duration::from_secs(10)).await,
            "the image was never requested"
        );
        let loaded: bool = page
            .evaluate("document.querySelector('.markdown img.md-image').complete")
            .await
            .expect("read the image")
            .into_value()
            .expect("a bool");
        assert!(!loaded, "the image should still be loading, or this checks nothing");
        wait_for_transcript_at_bottom(&page).await;
        let mut before = None;
        if pointer {
            let centre: Vec<f64> = page
                .evaluate("(() => { const r = document.querySelector('.messages').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height * 0.3]; })()")
                .await
                .expect("read the transcript's box")
                .into_value()
                .expect("two numbers");
            for dx in [0.0, 1.0] {
                page.move_mouse(chromiumoxide::layout::Point::new(centre[0] + dx, centre[1]))
                    .await
                    .expect("move the mouse over the transcript");
            }
            let top: f64 = page
                .evaluate(format!(
                    "(() => {{ window.__sme30 = document.elementFromPoint({}, {}); return window.__sme30.getBoundingClientRect().top; }})()",
                    centre[0] + 1.0,
                    centre[1]
                ))
                .await
                .expect("read what's under the pointer")
                .into_value()
                .expect("a number");
            before = Some(top);
        }
        image.release();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let complete: bool = page
                .evaluate("(() => { const i = document.querySelector('.markdown img.md-image'); return i.complete && i.naturalHeight > 0; })()")
                .await
                .expect("read the image")
                .into_value()
                .expect("a bool");
            if complete {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the late image never loaded");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        if let Some(before) = before {
            let after: f64 = page
                .evaluate("window.__sme30.getBoundingClientRect().top")
                .await
                .expect("read where it is now")
                .into_value()
                .expect("a number");
            assert!(
                (after - before).abs() <= 2.0,
                "the text under the pointer moved when the image loaded: top {before} -> {after}"
            );
            page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0))
                .await
                .expect("move the mouse off the transcript");
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        let distance = transcript_distance_from_bottom(&page).await;
        assert!(
            distance <= 1.0,
            "pointer {pointer}: the transcript should be at its bottom once the image has loaded, but it's {distance}px away"
        );
    }
}

/// Waits until `.messages` has settled at its bottom: the same distance,
/// within a pixel of it, on two reads 300 ms apart.
pub(super) async fn wait_for_transcript_at_bottom(page: &chromiumoxide::Page) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = f64::NAN;
    loop {
        let distance = transcript_distance_from_bottom(page).await;
        if distance <= 1.0 && distance == last {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the transcript never settled at its bottom: {distance}px away"
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// SME-57 code review: the address bar's navigation belongs to the chat
/// panel, not to the browsing panel. A session that closes while a
/// navigation is loading unmounts the panel; when the browser opens again
/// in that conversation, its address bar must still take an address.
pub(super) async fn scenario_address_bar_after_session_closes(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    crate::browsing::open_session(conversation.id, None)
        .await
        .expect("open a browsing session");
    // A page that answers only once released, so the navigation is still
    // loading when the test looks for it and when the session closes,
    // however slow the runner (SME-118; it used to answer on a fixed 3 s
    // timer).
    let slow = HeldPage::start("text/html", "<p>slow</p>").await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    let outcome: Result<(), String> = async {
        wait_for_live_client(&page, conversation.id).await;
        let input = wait_for_element(&page, ".browsing-address-input", Duration::from_secs(10)).await;
        input.focus().await.expect("focus the address bar");
        // The bar shows the session's URL (`about:blank`, there since the
        // client went live), and focusing it makes that the text being
        // edited, as a browser's does. Selected, so typing replaces it: before
        // SME-118 the scenario typed after it, its address was refused at
        // once ("unsupported scheme: about"), and the bar was disabled only
        // for that round trip, which a poll sometimes missed.
        page.evaluate("document.querySelector('.browsing-address-input').select()")
            .await
            .expect("select the address bar's text");
        input.type_str(slow.address()).await.expect("type the slow page's address");
        let typed: String = page
            .evaluate("document.querySelector('.browsing-address-input').value")
            .await
            .expect("read the address bar")
            .into_value()
            .expect("a string");
        if typed != slow.address() {
            return Err(format!("the address bar holds {typed:?}, not the address typed, {:?}", slow.address()));
        }
        input.press_key("Enter").await.expect("go");
        if !wait_until(|| async { slow.requests() >= 1 }, Duration::from_secs(10)).await {
            let error: String = page
                .evaluate("document.querySelector('.browsing-address-error')?.textContent ?? ''")
                .await
                .expect("read the address error")
                .into_value()
                .expect("a string");
            return Err(format!("the navigation never reached the slow page (address error: {error:?})"));
        }
        if !wait_for_count(&page, ".browsing-address-input[disabled]", 1, Duration::from_secs(5)).await {
            return Err("the address bar should show the navigation loading".to_string());
        }
        crate::browsing::close_session(conversation.id).await.expect("close the browsing session");
        if !wait_for_count(&page, ".browsing-panel", 0, Duration::from_secs(5)).await {
            return Err("the browsing panel should go when the session closes".to_string());
        }
        // Only now: the navigation was still loading when the session
        // closed, which is the case this scenario is for.
        slow.release();
        crate::browsing::open_session(conversation.id, None)
            .await
            .expect("open the browsing session again");
        if !wait_for_count(&page, ".browsing-address-input", 1, Duration::from_secs(10)).await {
            return Err("the browsing panel should come back when the session reopens".to_string());
        }
        // The navigation ends on `browsing`'s own 20 s NAV_TIMEOUT: the
        // page was released only after its context was disposed, so it
        // never answers, and chromiumoxide 0.7 drops a destroyed target
        // without resolving its `goto` (`Handler::on_target_destroyed`).
        // So this wait takes about 20 s, and the scenario's limit allows
        // for it. With the spawn dropped along with the panel, the bar
        // never comes back.
        if !wait_for_count(&page, ".browsing-address-input:not([disabled])", 1, Duration::from_secs(25)).await {
            return Err(
                "the address bar is still disabled after the session closed mid-navigation and reopened".to_string(),
            );
        }
        Ok(())
    }
    .await;
    // Closed before the checks, so one that fails as an `Err` doesn't leave
    // the session open. A panic inside the block (an `expect`, a helper's
    // own assert) or the scenario's time limit still skips this, leaving
    // the session open until the test process ends; no later scenario
    // uses this conversation.
    crate::browsing::close_session(conversation.id).await.expect("close the browsing session");
    if let Err(why) = outcome {
        panic!("{why}");
    }
}
