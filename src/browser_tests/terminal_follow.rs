use super::*;

/// Scenario 28 (SME-108): switching away from a conversation and back leaves
/// its sandbox terminal following its last line, not scrolled to the top. The
/// terminal body is destroyed on the switch and rebuilt on the way back,
/// filled in the same write that creates it, so the follow-bottom must land on
/// that first mount — before any frame could show the body at its top. The
/// ticket's capture came from a busy session mid-turn, so the switch is tried
/// at every timing it can land in: once the output had finished, the back-click
/// racing the rebuild, mid-burst, and rapid repeated switches. A frame-rate
/// recorder across each switch says whether a rebuilt body was ever *painted*
/// at its top, which a settled read can miss even though the ticket's own
/// capture would have caught it.
pub(super) async fn scenario_terminal_follow_after_switch(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let other = t.conversation().await;
    // A transcript with real height, as a busy session's has: the side-panel
    // row's bounded height then resolves through the flex chain late in the
    // same batch that creates the terminal body.
    for i in 0..40 {
        db::create_message(
            t.pool,
            conversation.id,
            if i % 2 == 0 { "user" } else { "assistant" },
            &[anthropic::ContentBlock::Text { text: format!("switch-filler message {i}") }],
        )
        .await
        .expect("seed a message");
    }
    sandbox::create_pod(t.pool, conversation.id, Default::default())
        .await
        .expect("create_pod");
    let terminal =
        sandbox::create_terminal(t.pool, conversation.id).await.expect("create_terminal");

    let page = t.tab(t.url("")).await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("set the window size");
    let row = |id: i64| format!(".conversation-item[data-conversation-id=\"{id}\"]");
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    wait_for_live_client(&page, conversation.id).await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal}"), Duration::from_secs(15)).await,
        "the terminal card should render"
    );

    scenario_terminal_follow_after_switch_unhurried(t, &conversation, &other, &page, row, terminal).await;
    scenario_terminal_follow_after_switch_racing_the_rebuild(&conversation, &other, &page, row).await;
    scenario_terminal_follow_after_switch_mid_burst(t, &conversation, &other, &page, row, terminal).await;
    scenario_terminal_follow_after_switch_rapid_switches(t, &conversation, &other, &page, row, terminal).await;
    scenario_terminal_follow_after_switch_phone_width(t, &conversation, &other, &page, row, terminal).await;
    scenario_terminal_follow_after_switch_scrolled_up(t, &conversation, &page, terminal).await;
    scenario_terminal_follow_after_switch_clamped_by_a_resize(t, &conversation, &page, terminal).await;
}


/// Phase A: the ticket's sequence, unhurried: a burst past the body's height, followed, then a switch away and back.
async fn scenario_terminal_follow_after_switch_unhurried(t: &Scenario<'_>, conversation: &crate::models::Conversation, other: &crate::models::Conversation, page: &chromiumoxide::Page, row: impl Fn(i64) -> String, terminal: i64) {
    // Phase A — the ticket's sequence, unhurried: a burst well past the body's
    // height, followed live, finished before the switch.
    let command_id = unique_id("cmd-follow");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 90); do echo \"follow-seed-$i\"; done"}),
    )
    .await
    .expect("run_terminal_command");
    assert!(
        wait_for_text(&page, "follow-seed-90", Duration::from_secs(30)).await,
        "the command's last line should stream in"
    );
    wait_command_finished(t.pool, terminal).await;
    // Following live through the burst, and settled before the switch: the
    // starting state the ticket's capture had ("shows 31–40 at the bottom").
    settle_terminal_bottom(&page, 8).await;
    let scrollable: bool = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             return b && b.scrollHeight > b.clientHeight + 40; })()",
        )
        .await
        .expect("measure the terminal body")
        .into_value()
        .expect("a bool");
    assert!(
        scrollable,
        "the seeded output doesn't overflow the terminal body, so the follow checks measure nothing"
    );

    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    wait_for_live_client(&page, other.id).await;
    assert!(
        wait_for_count(&page, ".task-terminal-body", 0, Duration::from_secs(10)).await,
        "switching away should take the terminal body with it"
    );
    set_scroll_mark(&page, "sme108A").await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    wait_for_live_client(&page, conversation.id).await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal}"), Duration::from_secs(15)).await,
        "the terminal should be back with the conversation"
    );
    settle_terminal_bottom(&page, 8).await;
    assert_never_painted_at_top(&page, "sme108A").await;
}

/// Phase B: the switch back racing the rebuild itself.
async fn scenario_terminal_follow_after_switch_racing_the_rebuild(conversation: &crate::models::Conversation, other: &crate::models::Conversation, page: &chromiumoxide::Page, row: impl Fn(i64) -> String) {
    // Phase B — the switch-back racing the rebuild itself: away and back with
    // nothing waited between the clicks, so the back-click lands while the
    // away-switch is still resolving.
    set_scroll_mark(&page, "sme108B").await;
    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    settle_terminal_bottom(&page, 8).await;
    assert_never_painted_at_top(&page, "sme108B").await;
}

/// Phase C: the ticket's actual conditions: the switch lands while a burst and a streaming reply arrive.
async fn scenario_terminal_follow_after_switch_mid_burst(t: &Scenario<'_>, conversation: &crate::models::Conversation, other: &crate::models::Conversation, page: &chromiumoxide::Page, row: impl Fn(i64) -> String, terminal: i64) {
    // Phase C — the ticket's actual conditions: the switch lands while a
    // second command's output is still streaming. Lines that arrive while the
    // page is on the other conversation never reach the old body; the rebuilt
    // body comes back holding the snapshot's share and must keep following the
    // lines that arrive after.
    let busy_id = unique_id("cmd-busy");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &busy_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 120); do echo \"busy-$i\"; sleep 0.05; done"}),
    )
    .await
    .expect("run_terminal_command for the mid-stream switch");
    assert!(
        wait_for_text(&page, "busy-010", Duration::from_secs(20)).await,
        "the second command's output should be streaming"
    );
    // A streaming reply alongside the terminal's output, as the busy turn had:
    // transcript churn racing the rebuild too. The mock replies slowly (a word
    // every 200 ms), so it's still streaming through the switch below.
    send_from(&page, "churn the transcript while the terminal fills").await;
    assert!(
        wait_for_text(&page, "zebra0", Duration::from_secs(10)).await,
        "the reply should start streaming"
    );
    set_scroll_mark(&page, "sme108C").await;
    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    // Through 3 s of streaming the follow keeps it within a few lines of the
    // bottom; a body that stopped following drifts a whole body-height away as
    // lines pile up below the fold.
    let worst = terminal_max_distance(&page, 3).await;
    assert!(
        worst <= 60.0,
        "streaming after the switch-back left the terminal {worst}px from its bottom — it stopped \
         following its last line (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    assert_never_painted_at_top(&page, "sme108C").await;
    assert!(
        t.mock.wait_until_idle(Duration::from_secs(30)).await,
        "the streaming reply never finished upstream"
    );
    assert!(
        wait_for_text(&page, "busy-120", Duration::from_secs(30)).await,
        "the streaming command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
}

/// Phase E: rapid repeated switches mid-stream.
async fn scenario_terminal_follow_after_switch_rapid_switches(t: &Scenario<'_>, conversation: &crate::models::Conversation, other: &crate::models::Conversation, page: &chromiumoxide::Page, row: impl Fn(i64) -> String, terminal: i64) {
    // Phase E — rapid repeated switches mid-stream: four clicks, away and back
    // twice, nothing waited for.
    let rapid_id = unique_id("cmd-rapid");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &rapid_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 400); do echo \"rapid-$i\"; sleep 0.05; done"}),
    )
    .await
    .expect("run_terminal_command for the rapid switches");
    assert!(
        wait_for_text(&page, "rapid-005", Duration::from_secs(20)).await,
        "the rapid command's output should be streaming"
    );
    set_scroll_mark(&page, "sme108E").await;
    for _ in 0..2 {
        click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
        click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    }
    let worst = terminal_max_distance(&page, 3).await;
    assert!(
        worst <= 120.0,
        "rapid switching mid-stream left the terminal {worst}px from its bottom \
         (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    assert_never_painted_at_top(&page, "sme108E").await;
    assert!(
        wait_for_text(&page, "rapid-400", Duration::from_secs(40)).await,
        "the rapid command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
}

/// Phase F (F1 to F3): a phone-width window, a resize mid-burst, a reload's fresh mount, and the drawer's switch back.
async fn scenario_terminal_follow_after_switch_phone_width(t: &Scenario<'_>, conversation: &crate::models::Conversation, other: &crate::models::Conversation, page: &chromiumoxide::Page, row: impl Fn(i64) -> String, terminal: i64) {
    // Phase F — a phone-width window, the layout the ticket's capture most
    // resembles: the side panels stack in a column that itself scrolls, the
    // terminal body is short, and the conversation list is a drawer, so the
    // switch-back is a toggle + row click rather than a row click alone.
    //
    // F1, recorded not asserted: shrink the window while the body holds the
    // (finished) rapid output and was following it, then send a short burst.
    // A resize is not a user scrolling up, so the body should keep following;
    // the sticky logic reads the resize's own clamp as a scroll, though,
    // which is exactly what would stop a follow — so the follow is asserted
    // here once the burst finishes and the body has had lines to catch up on.
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resize_id = unique_id("cmd-resize");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &resize_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 20); do echo \\\"resize-$i\\\"; sleep 0.05; done"}),
    )
    .await
    .expect("run_terminal_command after the resize");
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // F1b: the resize landing mid-burst, so the clamp arrives while the
    // follow is running — the race the sticky logic can't see its way out of.
    let burst_id = unique_id("cmd-resize-mid");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &burst_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 300); do echo \\\"mid-$i\\\"; sleep 0.02; done"}),
    )
    .await
    .expect("start the mid-burst command");
    assert!(
        wait_for_text(&page, "mid-010", Duration::from_secs(20)).await,
        "the mid-burst command's output should be streaming"
    );
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("widen the window mid-burst");
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;

    // F2: a fresh mount from a reload — the ticket's open question: open a
    // conversation whose terminal has more output than fits, and the body
    // must show its last line, not its first.
    page.reload().await.expect("reload the page");
    wait_for_live_client(&page, conversation.id).await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal}"), Duration::from_secs(15)).await,
        "the terminal card should render after the reload"
    );
    settle_terminal_bottom(&page, 8).await;

    // F3: streaming at phone width, then the drawer switch-back. (F1b widened
    // the window again; narrow it here so the drawer — and only the drawer —
    // carries the switch.)
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone again");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let phone_id = unique_id("cmd-phone");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &phone_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 200); do echo \\\"phone-$i\\\"; sleep 0.04; done"}),
    )
    .await
    .expect("run_terminal_command for the phone-width switch");
    assert!(
        wait_for_text(&page, "phone-005", Duration::from_secs(20)).await,
        "the phone-width command's output should be streaming"
    );
    settle_terminal_bottom(&page, 8).await;
    set_scroll_mark(&page, "sme108F").await;
    // The drawer holds the list: open it, pick the other conversation (which
    // closes it again), then come back the same way mid-stream.
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    click_when_present(&page, &row(other.id), Duration::from_secs(10)).await;
    click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
    click_when_present(&page, &row(conversation.id), Duration::from_secs(10)).await;
    let worst = terminal_max_distance(&page, 3).await;
    assert!(
        worst <= 60.0,
        "streaming after the phone-width switch-back left the terminal {worst}px from its bottom \
         (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    assert_never_painted_at_top(&page, "sme108F").await;
    assert!(
        wait_for_text(&page, "phone-200", Duration::from_secs(40)).await,
        "the phone-width command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
}

/// F4: the other half of the sticky rule: a user scrolled up stays put, and scrolling back down follows again.
async fn scenario_terminal_follow_after_switch_scrolled_up(t: &Scenario<'_>, conversation: &crate::models::Conversation, page: &chromiumoxide::Page, terminal: i64) {
    // F4 — the other half of the sticky rule the fix must keep: scrolling up
    // mid-stream is a user gesture, and the follow must stop and stay put
    // (SME-83's semantics, now distinguished from a resize by *how* the
    // position moved rather than by merely being off the bottom).
    //
    // The gesture happens while nothing is arriving, and the streaming starts
    // after it. A `scrollTop` write and a follow-scroll inside the same frame
    // coalesce into ONE scroll event describing only the final position, so the
    // app can miss the scroll-up entirely — which is what this phase did, now
    // and then, at 50 ms a line. Quiet first keeps the check watching the follow
    // instead of racing the event loop.
    let fill_id = unique_id("cmd-updown-fill");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &fill_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 200); do echo \\\"updown-$i\\\"; done"}),
    )
    .await
    .expect("run_terminal_command to fill the terminal for the scroll-up check");
    assert!(
        wait_for_text(&page, "updown-200", Duration::from_secs(30)).await,
        "the fill command's output should arrive"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
    let up = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             b.scrollTop = b.scrollHeight - b.clientHeight - 400; \
             return Math.round(b.scrollHeight - b.scrollTop - b.clientHeight); })()",
        )
        .await
        .expect("scroll the terminal up 400px")
        .into_value::<f64>()
        .expect("a number");
    assert!((300.0..=500.0).contains(&up), "the scroll-up should have landed ~400px up, got {up}");
    // A beat for the app to take the scroll event in before anything else can
    // move the view.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stream_id = unique_id("cmd-updown-stream");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &stream_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 100); do echo \\\"stream-$i\\\"; sleep 0.1; done"}),
    )
    .await
    .expect("run_terminal_command to stream while the user reads");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let still = terminal_distance_from_bottom(&page).await;
    assert!(
        (300.0..=1e5).contains(&still),
        "output arrived while the user had scrolled up and yanked the view to the bottom: \
         {still}px from the bottom (was ~400px, scrollTop/scrollHeight/clientHeight: {}, scroll \
         events: {})",
        terminal_scroll_facts(&page).await,
        terminal_scroll_events(&page).await
    );
    // Back to the bottom is following again: scroll down, and the follow
    // resumes on the next line rather than needing the command to finish.
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         b.scrollTop = b.scrollHeight; })()",
    )
    .await
    .expect("scroll back to the bottom");
    tokio::time::sleep(Duration::from_millis(500)).await;
    settle_terminal_bottom(&page, 8).await;
    assert!(
        wait_for_text(&page, "stream-100", Duration::from_secs(60)).await,
        "the streaming command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
}

/// Phase G: a resize that clamps a reader who scrolled up doesn't turn the follow back on.
async fn scenario_terminal_follow_after_switch_clamped_by_a_resize(t: &Scenario<'_>, conversation: &crate::models::Conversation, page: &chromiumoxide::Page, terminal: i64) {
    // Phase G — the other direction on the same rule, which the first fix left
    // as it found it: growing the window *shrinks* the scroll range, so a reader
    // close enough to the bottom gets clamped down onto it, landing inside the
    // slack. Position alone then reads as "at the bottom", and the follow comes
    // back to life for someone who asked, a moment ago, not to be followed. A
    // clamp is geometry; only the user moving back down asks again. The gesture
    // happens while output is quiet, for the reason F4 gives, and the output
    // then comes one line every 200 ms so the reader's drift stays well under
    // the ~580 px the body's client height gains — or the resize wouldn't reach
    // them and the phase would prove nothing.
    let up = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             b.scrollTop = b.scrollHeight - b.clientHeight - 100; \
             return Math.round(b.scrollHeight - b.scrollTop - b.clientHeight); })()",
        )
        .await
        .expect("scroll the terminal up 100px to read")
        .into_value::<f64>()
        .expect("a number");
    assert!((60.0..=200.0).contains(&up), "the scroll-up should have landed ~100px up, got {up}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let restick_id = unique_id("cmd-restick");
    anthropic::tools::execute(
        t.pool,
        conversation.id,
        &restick_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal,
            "command": "for i in $(seq -w 1 100); do echo \\\"restick-$i\\\"; sleep 0.2; done"}),
    )
    .await
    .expect("run_terminal_command for the re-stick check");
    // That scroll-up really did turn the follow off: lines arrive and the view
    // drifts away from the bottom instead of being held on it. Without this the
    // check below couldn't tell "woken by the clamp" from "never switched off
    // at all".
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let reading = terminal_distance_from_bottom(&page).await;
    assert!(
        reading > 60.0,
        "output yanked the reader along before any resize: {reading}px from the bottom, it should \
         have kept drifting away (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    // Tag the body element first: a rebuilt body arrives with a brand-new
    // `StickyBottom`, which starts out following, whatever the one it replaced
    // had decided — so whether the resize replaced it decides what the check
    // below is actually measuring.
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         b.__sme108tag = 'before-grow'; })()",
    )
    .await
    .expect("tag the terminal body before the resize");
    // Grow the window by far more than the ~100 px they have drifted: the new
    // scroll range no longer reaches where they were, so the browser clamps them
    // down onto its bottom edge — once. A beat later they should already be back
    // above it, by about as many lines as arrived in that beat: the follow is
    // off, nothing scrolls them, and the output piles up below. That is *not*
    // what the browser does by default — Chrome's scroll anchoring keeps a
    // scroller that sits at its end pinned onto every new bottom, which looks
    // exactly like a follow and which no scroll logic can undo; the body's
    // `overflow-anchor: none` (assets/css/03-panels.css) is what stops it, and the growth
    // checked below is what it stops.
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("grow the window while they read");
    // The clamp lands within a frame or two of the grow on a healthy page, but
    // betting one fixed sleep on it makes this premise itself the flake — poll
    // for it, and only fail as a premise once the reader has had 3s to clamp.
    let clamp_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut after_grow = terminal_distance_from_bottom(&page).await;
    while after_grow > 40.0 && tokio::time::Instant::now() < clamp_deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        after_grow = terminal_distance_from_bottom(&page).await;
    }
    assert!(
        after_grow <= 40.0,
        "the window grow didn't reach the reader to clamp them onto the new bottom (they were \
         still {after_grow}px above it after 3s), so this phase measures nothing \
         (scrollTop/scrollHeight/clientHeight: {})",
        terminal_scroll_facts(&page).await
    );
    let same_element: bool = page
        .evaluate(
            "(() => { const b = document.querySelector('.task-terminal-body'); \
             return !!b && b.__sme108tag === 'before-grow'; })()",
        )
        .await
        .expect("ask whether the resize kept the same terminal body element")
        .into_value()
        .expect("a bool");
    // Lines keep arriving: left alone, the view stays where the clamp put it and
    // the gap only grows. A follow woken by the clamp — or a browser pinning the
    // clamped scroller onto each new bottom — keeps it at 0 instead. Confirm the
    // command is still running first, or the growth below would only prove the
    // output had stopped.
    let still_streaming = db::terminal_command_is_running(t.pool, terminal)
        .await
        .expect("the re-stick command's running state")
        .is_some();
    assert!(
        still_streaming,
        "the re-stick command had already finished by the time of the resize, so nothing was \
         arriving to follow"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let drifted = terminal_distance_from_bottom(&page).await;
    assert!(
        drifted > after_grow + 60.0,
        "a window grow left the reader being followed after all: {drifted}px from the bottom \
         after 2.5s of streaming, when it was {after_grow}px at the start of that window — the \
         gap should have kept growing by a line every 200 ms (same element through the resize: \
         {same_element}; scrollTop/scrollHeight/clientHeight: {}, scroll events: {})",
        terminal_scroll_facts(&page).await,
        terminal_scroll_events(&page).await
    );
    // And the user asking to be back at the bottom does bring the follow back.
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         b.scrollTop = b.scrollHeight; })()",
    )
    .await
    .expect("scroll back to the bottom");
    settle_terminal_bottom(&page, 8).await;
    assert!(
        wait_for_text(&page, "restick-100", Duration::from_secs(60)).await,
        "the re-stick command should finish"
    );
    wait_command_finished(t.pool, terminal).await;
    settle_terminal_bottom(&page, 8).await;
}
