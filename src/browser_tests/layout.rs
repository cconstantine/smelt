use super::*;

/// Scenario 10: a conversation that doesn't exist says so, rather than
/// offering a chat box whose send fails with a raw database error.
pub(super) async fn scenario_missing_conversation(t: &Scenario<'_>) {
    let missing = t.tab(t.url("conversation/987654321")).await;
    assert!(
        wait_for_text(&missing, "doesn't exist", Duration::from_secs(10)).await,
        "a missing conversation should say it doesn't exist"
    );
    assert!(
        missing.find_element(CHAT_INPUT).await.is_err(),
        "a missing conversation shouldn't offer a message box"
    );
}

/// Scenario 11: a failed background notification belongs to its
/// conversation — switching away clears it.
pub(super) async fn scenario_notification_failure_stays(t: &Scenario<'_>) {
    let failing = t.conversation().await;
    let elsewhere = t.conversation().await;
    seed_user_message(t.pool, elsewhere.id, "hello from A").await;
    let watcher = t.tab(t.url(&format!("conversation/{}", failing.id))).await;
    wait_for_live_client(&watcher, failing.id).await;
    crate::events::publish(
        failing.id,
        crate::events::ConversationEvent::NotificationDeliveryFailed {
            detail: "scenario 11 failure".to_string(),
        },
    );
    assert!(
        wait_for_text(&watcher, "scenario 11 failure", Duration::from_secs(10)).await,
        "the watching tab should show B's notification failure"
    );
    click_conversation(&watcher, elsewhere.id).await;
    assert!(
        wait_for_text(&watcher, "hello from A", Duration::from_secs(10)).await,
        "switching to A should show A's messages"
    );
    assert!(
        wait_for_text_gone(&watcher, "scenario 11 failure", Duration::from_secs(2)).await,
        "B's notification failure followed the tab to A"
    );
}

/// Scenario 12: pods. A pod created in one conversation shows up as a dot
/// in another tab's sidebar, live; the pods page lists it; Stop there
/// removes the row, and the dot goes away, live.
pub(super) async fn scenario_pods(t: &Scenario<'_>) {
    let pool = t.pool;
    let with_pod = t.conversation().await;
    seed_user_message(pool, with_pod.id, "scenario 12").await;
    let watching = t.conversation().await;
    let sidebar_tab = t.tab(t.url(&format!("conversation/{}", watching.id))).await;
    wait_for_live_client(&sidebar_tab, watching.id).await;
    let dot = format!(".conversation-item[data-conversation-id=\"{}\"] .live-pod-dot", with_pod.id);
    assert!(
        wait_for_count(&sidebar_tab, &dot, 0, Duration::from_secs(5)).await,
        "no dot before the conversation has a pod"
    );
    let pod_id = sandbox::create_pod(pool, with_pod.id, Default::default()).await.expect("create_pod");
    assert!(
        wait_for_count(&sidebar_tab, &dot, 1, Duration::from_secs(10)).await,
        "the sidebar should mark a conversation whose pod started, without a reload"
    );

    let pods_page = t.tab(t.url("pods")).await;
    wait_for_resource(&pods_page, "/api/pods").await;
    let row = format!("tr[data-pod-id=\"{pod_id}\"]");
    assert!(
        wait_for_count(&pods_page, &row, 1, Duration::from_secs(10)).await,
        "the pods page should list the pod"
    );
    assert!(
        wait_for_text(&pods_page, "scenario 12", Duration::from_secs(5)).await,
        "the row should name its conversation"
    );
    let stop = format!("{row} .pod-stop");
    let neighbour = format!("{row} td:nth-last-child(2)");
    // In view and still first, so the click's own scrolling can't move
    // it between the two measurements.
    wait_for_stable(&pods_page, &stop, tokio::time::Instant::now() + Duration::from_secs(5)).await;
    let before = (element_box(&pods_page, &stop).await, element_box(&pods_page, &neighbour).await);
    click_when_present(&pods_page, &stop, Duration::from_secs(5)).await;
    let confirm = format!("{row} .pod-stop.confirm");
    wait_for_element(&pods_page, &confirm, Duration::from_secs(5)).await;
    let after = (element_box(&pods_page, &confirm).await, element_box(&pods_page, &neighbour).await);
    assert_eq!(
        before, after,
        "arming Stop must not move or resize the button or its neighbours (button, cell to its left)"
    );
    click_when_present(&pods_page, &confirm, Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&pods_page, &row, 0, Duration::from_secs(20)).await,
        "a stopped pod's row should go away"
    );
    assert!(
        wait_for_count(&sidebar_tab, &dot, 0, Duration::from_secs(10)).await,
        "the dot should go away when the pod stops, without a reload"
    );
}

/// Scenario 13: stopping a turn. The model (the slow mock) is mid-reply;
/// Stop shows in the sending tab and in a second tab watching the same
/// conversation; stopping ends the reply for good, says "Stopped.", and
/// leaves the chat usable.
pub(super) async fn scenario_stop_a_turn(t: &Scenario<'_>) {
    let to_stop = t.conversation().await;
    let url = t.url(&format!("conversation/{}", to_stop.id));
    let sender = t.tab(url.clone()).await;
    let observer = t.tab(url).await;
    wait_for_live_client(&sender, to_stop.id).await;
    wait_for_live_client(&observer, to_stop.id).await;
    send_from(&sender, "please stop me").await;
    assert!(
        wait_for_text(&sender, "zebra0", Duration::from_secs(10)).await,
        "the reply should start streaming"
    );
    assert!(
        wait_for_count(&observer, ".stop-turn", 1, Duration::from_secs(5)).await,
        "a tab that didn't send should also offer Stop while the turn runs"
    );
    // And both say the model is working, with how long it's been at it:
    // before, a slow model's turn looked like nothing at all was
    // happening (SME-41 D1).
    for tab in [&sender, &observer] {
        assert!(
            wait_for_count(tab, ".turn-working", 1, Duration::from_secs(5)).await,
            "a running turn should show that the model is working"
        );
    }
    // And the sidebar marks the conversation as busy, so it's visible
    // which conversations are working (SME-41 D9).
    let busy_mark = format!(".conversation-item[data-conversation-id='{}'] .conversation-busy", to_stop.id);
    assert!(
        wait_for_count(&observer, &busy_mark, 1, Duration::from_secs(5)).await,
        "the sidebar should mark a conversation whose turn is running"
    );
    click_when_present(&sender, ".stop-turn", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&sender, "Stopped.", Duration::from_secs(5)).await,
        "stopping should say so"
    );
    assert!(
        wait_for_count(&sender, ".stop-turn", 0, Duration::from_secs(5)).await
            && wait_for_count(&observer, ".stop-turn", 0, Duration::from_secs(5)).await,
        "Stop should go away in every tab once the turn has ended"
    );
    assert!(
        wait_for_count(&sender, ".turn-working", 0, Duration::from_secs(5)).await
            && wait_for_count(&observer, ".turn-working", 0, Duration::from_secs(5)).await,
        "the working line should go away once the turn has ended"
    );
    assert!(
        wait_for_count(&observer, &busy_mark, 0, Duration::from_secs(5)).await,
        "the sidebar's busy mark should go away once the turn has ended"
    );
    // Once the mock's reply is no longer open upstream (cut off, or, if the
    // stop didn't take, finished), nothing more of it can arrive.
    assert!(
        t.mock.wait_until_idle(Duration::from_secs(10)).await,
        "the stopped reply's response was never closed"
    );
    assert!(
        !wait_for_text(&sender, "zebra24", Duration::from_millis(500)).await,
        "a stopped reply must not keep arriving"
    );
    let input = wait_for_element(&sender, CHAT_INPUT, Duration::from_secs(5)).await;
    let disabled: bool = input
        .call_js_fn("function() { return this.disabled; }", false)
        .await
        .expect("read disabled")
        .result
        .value
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    assert!(!disabled, "the message box should be usable after a stop");
}

/// Scenario 14: replies travel on the conversation stream, so every tab
/// sees one live, a reload mid-reply keeps the text so far, and the sender
/// sees its own message once. And five tabs, a streaming reply and a Stop
/// from another tab fit under the browser's 6-connections-per-host limit:
/// a tab holds one connection even while a reply streams, which leaves the
/// sixth for ordinary requests (a tab's own loading needs one too). When
/// the reply had its own stream, it took the sixth, and Stop queued.
pub(super) async fn scenario_reply_on_conversation_stream(t: &Scenario<'_>) {
    let shared = t.conversation().await;
    let url = t.url(&format!("conversation/{}", shared.id));
    let mut tabs = Vec::new();
    for _ in 0..5 {
        let tab = t.tab(url.clone()).await;
        wait_for_live_client(&tab, shared.id).await;
        tabs.push(tab);
    }
    send_from(&tabs[0], "stream to everyone").await;
    assert!(
        wait_for_text(&tabs[4], "zebra2", Duration::from_secs(10)).await,
        "a tab that didn't send should see the reply stream in"
    );
    assert!(
        !wait_for_text(&tabs[4], "zebra24", Duration::from_millis(100)).await,
        "...while it's still streaming"
    );
    tabs[3].reload().await.expect("reload a tab mid-reply");
    wait_for_live_client(&tabs[3], shared.id).await;
    assert!(
        wait_for_text(&tabs[3], "zebra", Duration::from_secs(3)).await
            && !wait_for_text(&tabs[3], "zebra24", Duration::from_millis(100)).await,
        "a tab reloaded mid-reply should show the reply so far"
    );
    let sent_count: usize = tabs[0]
        .evaluate("document.querySelector('.messages').innerText.split('stream to everyone').length - 1")
        .await
        .expect("count the sent message")
        .into_value()
        .expect("a number");
    assert_eq!(sent_count, 1, "the sender should see its own message once in the conversation");
    click_when_present(&tabs[4], ".stop-turn", Duration::from_secs(5)).await;
    for (i, tab) in tabs.iter().enumerate() {
        assert!(
            wait_for_count(tab, ".stop-turn", 0, Duration::from_secs(5)).await,
            "tab {i}: Stop should go away once the turn has stopped"
        );
    }
}

/// Scenario 15: with a browsing session open, the chat stays usable at a
/// laptop width (this browser is 1400x900). The live frame used to be a
/// fixed 1280px that couldn't shrink, which squeezed the messages and
/// input to 48px and made the page scroll sideways (SME-40 F2).
pub(super) async fn scenario_busy_session_layout(t: &Scenario<'_>) {
    let pool = t.pool;
    let browsing = t.conversation().await;
    crate::browsing::open_session(browsing.id, Some(crate::egress_proxy::sandbox_dial(pool.clone(), browsing.id)))
        .await
        .expect("open a browsing session");
    // With a todo list and a sandbox terminal open too, as in a real
    // session: side by side, the three panels shared ~450px, so the
    // browser was tiny and the terminal pushed out of sight (SME-41 D16).
    db::set_conversation_todos(pool, browsing.id, &[anthropic::tools::TodoItem {
        content: "check the page".to_string(),
        status: anthropic::tools::TodoStatus::InProgress,
    }])
    .await
    .expect("seed a todo list");
    sandbox::create_pod(pool, browsing.id, Default::default()).await.expect("create_pod");
    sandbox::create_terminal(pool, browsing.id).await.expect("create_terminal");
    let page = t.tab(t.url(&format!("conversation/{}", browsing.id))).await;
    wait_for_live_client(&page, browsing.id).await;
    wait_for_element(&page, ".browsing-panel-frame-wrap", Duration::from_secs(10)).await;
    let layout: Vec<f64> = page
        .evaluate(
            "(() => { const w = s => document.querySelector(s).getBoundingClientRect().width; \
             return [w('.messages'), w('.composer input'), document.documentElement.scrollWidth - document.documentElement.clientWidth, \
             w('.browsing-panel-frame-wrap'), w('.browsing-panel'), w('.browsing-address-bar')]; })()",
        )
        .await
        .expect("measure the layout")
        .into_value()
        .expect("numbers");
    wait_for_element(&page, ".sandbox-panel .task-terminal", Duration::from_secs(10)).await;
    let panels: Vec<f64> = page
        .evaluate(
            "(() => { const b = s => document.querySelector(s).getBoundingClientRect(); \
             const frame = b('.browsing-panel-frame-wrap'), sandbox = b('.sandbox-panel'), todo = b('.todo-panel'); \
             return [frame.width, sandbox.width, Math.min(sandbox.bottom, innerHeight) - Math.max(sandbox.top, 0), todo.width]; })()",
        )
        .await
        .expect("measure the side panels")
        .into_value()
        .expect("numbers");
    // Closed before the checks, so a failing one doesn't leave the session open.
    crate::browsing::close_session(browsing.id).await.expect("close the browsing session");
    assert!(layout[0] >= 300.0, "the messages are too narrow to use: {layout:?}");
    assert!(layout[1] >= 150.0, "the message box is too narrow to use: {layout:?}");
    assert!(layout[2] <= 0.0, "the page scrolls sideways: {layout:?}");
    assert!(layout[3] <= layout[4], "the frame overflows its panel: {layout:?}");
    assert!(layout[5] <= layout[4], "the address bar overflows its panel: {layout:?}");
    assert!(panels[0] >= 400.0, "the live browser is too small to use: {panels:?}");
    assert!(panels[1] >= 400.0, "the sandbox panel is too narrow to read: {panels:?}");
    assert!(panels[2] >= 240.0, "the sandbox panel should be visible without scrolling: {panels:?}");
}

/// Scenario 16: every settings page is reachable from the sidebar. Sandbox
/// volumes had no link anywhere; the only way in was typing the URL
/// (SME-40 F7).
pub(super) async fn scenario_sidebar(t: &Scenario<'_>) {
    // A row to measure, even when this scenario runs alone.
    let conversation = t.conversation().await;
    seed_user_message(t.pool, conversation.id, "a sidebar row").await;
    let page = t.tab(t.url("")).await;
    // And the sidebar's two-step Delete keeps its size when armed: the
    // armed label was bold, so it came out wider than the width the
    // button had reserved for it (SME-40 F9). Arming is client-side
    // only; closing this tab disarms it.
    let delete = ".conversation-item .delete-conversation";
    wait_for_stable(&page, delete, tokio::time::Instant::now() + Duration::from_secs(10)).await;
    let before = element_box(&page, delete).await;
    click_when_present(&page, delete, Duration::from_secs(5)).await;
    wait_for_element(&page, ".conversation-item .delete-conversation.confirm", Duration::from_secs(5)).await;
    let after = element_box(&page, ".conversation-item .delete-conversation.confirm").await;
    assert_eq!(before.2, after.2, "arming the sidebar's Delete changed its width");
    // A title uses its row, and each row says how long ago the
    // conversation was active. Titles were cut at about 15 characters
    // with most of the row empty (SME-41 D8).
    let row: Vec<String> = page
        .evaluate(
            "(() => { const item = document.querySelector('.conversation-item:not(:hover)') || document.querySelector('.conversation-item'); \
             const w = s => item.querySelector(s).getBoundingClientRect().width; \
             const age = item.querySelector('.conversation-age'); \
             return [String(w('.conversation-title') / item.getBoundingClientRect().width), age ? age.innerText : '']; })()",
        )
        .await
        .expect("measure a sidebar row")
        .into_value()
        .expect("strings");
    assert!(row[0].parse::<f64>().unwrap_or(0.0) > 0.6, "the title should use most of its row: {row:?}");
    assert!(
        row[1] == "now" || row[1].trim_end_matches(['m', 'h', 'd', 'w']).parse::<u32>().is_ok(),
        "each row should say how long ago it was active: {row:?}"
    );
    page.close().await.expect("close the tab");
    let page = t.tab(t.url("")).await;
    // Named the way a user thinks of them: "Sandboxes", not "Pods"
    // (SME-41 D11).
    let links: String = page
        .evaluate("Array.from(document.querySelectorAll('.sidebar-body a')).map(a => a.innerText).join('|')")
        .await
        .expect("read the sidebar links")
        .into_value()
        .expect("text");
    assert!(links.contains("Sandboxes") && !links.contains("Pods"), "sidebar links: {links}");
    click_when_present(&page, ".sidebar a[href='/sandbox-volumes']", Duration::from_secs(10)).await;
    assert!(
        wait_for_text(&page, "Sandbox volumes", Duration::from_secs(10)).await,
        "the sidebar's volumes link should open the volumes page"
    );
}

/// Scenario 17: a phone-width window. The sidebar kept its 272px, the chat
/// got about 100px and the page scrolled sideways (SME-40 F8). With
/// `mobile` on, a page without a viewport meta tag lays out at 980px, so
/// this also checks the tag is there.
pub(super) async fn scenario_phone_width(t: &Scenario<'_>) {
    let phone_conversation = t.conversation().await;
    let page = t.tab("about:blank").await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone");
    page.goto(t.url(&format!("conversation/{}", phone_conversation.id)))
        .await
        .expect("open the conversation");
    wait_for_live_client(&page, phone_conversation.id).await;
    let layout: Vec<f64> = page
        .evaluate(
            "(() => { const w = s => document.querySelector(s).getBoundingClientRect().width; \
             return [innerWidth, w('.messages'), w('.composer input'), \
             document.documentElement.scrollWidth - document.documentElement.clientWidth]; })()",
        )
        .await
        .expect("measure the layout")
        .into_value()
        .expect("numbers");
    assert_eq!(layout[0], 390.0, "the page isn't laid out at the phone's width: {layout:?}");
    assert!(layout[1] >= 300.0, "the messages are too narrow on a phone: {layout:?}");
    assert!(layout[2] >= 200.0, "the message box is too narrow on a phone: {layout:?}");
    assert!(layout[3] <= 0.0, "the page scrolls sideways on a phone: {layout:?}");
    // The conversation comes first: the list is folded behind a
    // button, and the chat sits above the side panels. The list and
    // the sandbox panel used to push the transcript to the bottom of
    // the screen (SME-41 D4).
    let order: Vec<f64> = page
        .evaluate(
            "(() => { const top = s => { const e = document.querySelector(s); return e ? e.getBoundingClientRect().top : -1; }; \
             const list = document.querySelector('.conversation-list'); \
             return [list && list.offsetParent !== null ? 1 : 0, top('.chat-main'), top('.side-panels-row')]; })()",
        )
        .await
        .expect("measure the order")
        .into_value()
        .expect("numbers");
    assert_eq!(order[0], 0.0, "the conversation list should be folded away on a phone: {order:?}");
    assert!(order[2] < 0.0 || order[1] < order[2], "the chat should come before the side panels: {order:?}");
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    let mut list_shown = false;
    for _ in 0..25 {
        list_shown = page
            .evaluate("document.querySelector('.conversation-list').offsetParent !== null")
            .await
            .expect("check the list")
            .into_value()
            .expect("a bool");
        if list_shown {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(list_shown, "the button should open the conversation list");
}
