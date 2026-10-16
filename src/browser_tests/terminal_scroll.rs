use super::*;

/// Watch the sandbox terminal body's scroll position every 8 ms: finer than
/// a frame, so a rebuilt body that sat at its top for even a frame shows up
/// here — the settle loops can only measure the body some milliseconds after
/// it first appears, and can't tell "sat at its top" from "rescued a beat
/// later". Installing is idempotent; `set_scroll_mark` clears the samples, so
/// the recorder holds one phase's worth at a time. It also keeps the last 30
/// `scroll` events on the body, capture-phase (scroll doesn't bubble): the
/// exact numbers the sticky logic sees, which is how a follow that stopped
/// partway gets explained.
pub(super) const TERMINAL_SCROLL_SAMPLER: &str = "(() => { if (!window.__sme108timer) { \
    window.__sme108timer = setInterval(() => { const b = document.querySelector('.task-terminal-body'); \
      if (!b) { return; } \
      (window.__sme108samples || []).push({t: Math.round(performance.now()), \
        top: Math.round(b.scrollTop), sh: Math.round(b.scrollHeight), ch: Math.round(b.clientHeight)}); \
      }, 8); \
    window.__sme108evts = []; \
    document.addEventListener('scroll', e => { const b = e.target; if (!b || !b.classList || \
      !b.classList.contains('task-terminal-body')) { return; } \
      window.__sme108evts.push({t: Math.round(performance.now()), top: Math.round(b.scrollTop), \
        sh: Math.round(b.scrollHeight), ch: Math.round(b.clientHeight), \
        d: Math.round(b.scrollHeight - b.scrollTop - b.clientHeight)}); \
      if (window.__sme108evts.length > 30) { window.__sme108evts.shift(); } }, true); } })()";

/// Start the recorder (if this is the phase that first needs it) and mark the
/// window at now, clearing what earlier phases recorded.
pub(super) async fn set_scroll_mark(page: &chromiumoxide::Page, name: &str) {
    page.evaluate(TERMINAL_SCROLL_SAMPLER).await.expect("install the scroll sampler");
    page.evaluate(format!("(() => {{ window.__sme108samples = []; window['{name}'] = performance.now(); }})()"))
        .await
        .expect("set the scroll mark");
}

/// The frame recorder's verdict for one switch: across every frame since the
/// mark in which the terminal body overflowed, it must never have sat at its
/// top (scrollTop 0) — the ticket's capture saw exactly that.
pub(super) async fn assert_never_painted_at_top(page: &chromiumoxide::Page, mark: &str) {
    let v: serde_json::Value = page
        .evaluate(
            format!(
                "(() => {{ const m = window['{mark}'] || 0; const s = (window.__sme108samples || \
                   []).filter(x => x.t >= m && x.sh > x.ch + 40); \
                   return {{ min: s.reduce((a, x) => Math.min(a, x.top), 1e9), n: s.length }}; }})()"
            ),
        )
        .await
        .expect("read the frame recorder")
        .into_value()
        .expect("an object");
    let min_top = v["min"].as_f64().unwrap_or(f64::NAN);
    let frames = v["n"].as_u64().unwrap_or(0);
    assert!(
        frames > 0,
        "the frame recorder saw no overflowing frame of the terminal body since {mark}"
    );
    assert!(
        min_top > 0.0,
        "the terminal body was painted at its top: the lowest scrollTop across {frames} \
         overflowing frames since {mark} was {min_top}px"
    );
}

/// How far the terminal body is scrolled from its bottom, in pixels; a huge
/// number when there is none, so a settle loop keeps waiting for the body.
pub(super) async fn terminal_distance_from_bottom(page: &chromiumoxide::Page) -> f64 {
    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         return b ? b.scrollHeight - b.scrollTop - b.clientHeight : 1e18; })()",
    )
    .await
    .expect("read the terminal's scroll position")
    .into_value()
    .expect("a number")
}

/// The terminal body's scrollTop/scrollHeight/clientHeight, rounded, joined —
/// for failure messages.
pub(super) async fn terminal_scroll_facts(page: &chromiumoxide::Page) -> String {    page.evaluate(
        "(() => { const b = document.querySelector('.task-terminal-body'); \
         return b ? [b.scrollTop, b.scrollHeight, b.clientHeight].map(Math.round).join('/') : 'no \
         body'; })()",
    )
    .await
    .expect("read the terminal's scroll facts")
    .into_value::<String>()
    .expect("a string")
}

/// The last up-to-30 `scroll` events the terminal body fired, with the exact
/// numbers the sticky logic saw at each: which event demoted `stuck` (a
/// distance over the 32 px slack) is in here.
pub(super) async fn terminal_scroll_events(page: &chromiumoxide::Page) -> String {
    page.evaluate(
        "(() => JSON.stringify(window.__sme108evts || []))()",
    )
    .await
    .expect("read the scroll event ring")
    .into_value::<String>()
    .expect("a string")
}

/// The terminal following its last line: the distance from the bottom settling
/// to within a pixel and holding — two reads 300 ms apart that agree, as the
/// transcript's settle does.
pub(super) async fn settle_terminal_bottom(page: &chromiumoxide::Page, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut last = f64::NAN;
    loop {
        let distance = terminal_distance_from_bottom(page).await;
        if distance <= 1.0 && distance == last {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the terminal never followed its last line: still {distance}px from the bottom \
             (scrollTop/scrollHeight/clientHeight: {}, scroll events: {})",
            terminal_scroll_facts(page).await,
            terminal_scroll_events(page).await
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The largest distance-from-bottom the terminal body shows over `secs` of
/// sampling at ~30 ms. A rebuilt body takes a beat to appear after a
/// switch-back resolves, so the window waits for it first, and a body that
/// vanishes mid-window counts as no sample rather than infinite drift.
pub(super) async fn terminal_max_distance(page: &chromiumoxide::Page, secs: u64) -> f64 {
    let present_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if terminal_distance_from_bottom(page).await < 1e17 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < present_deadline,
            "the terminal body never came back after the switch"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut max = f64::NAN;
    loop {
        let distance = terminal_distance_from_bottom(page).await;
        if distance < 1e17 {
            max = if max.is_nan() { distance } else { max.max(distance) };
        }
        if tokio::time::Instant::now() >= deadline {
            assert!(!max.is_nan(), "the terminal body vanished from the page");
            return max;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Wait for `terminal_id`'s running command to finish server-side rather than
/// sleeping a guess: the tier waits on server state, not timing.
pub(super) async fn wait_command_finished(pool: &sqlx::PgPool, terminal_id: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let running = db::terminal_command_is_running(pool, terminal_id)
            .await
            .expect("check the command's running state");
        if running.is_none() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the seeded command never finished running"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// SME-75: with the pointer over the transcript, a side panel appearing
/// doesn't move what's under it. In a wide window the panel sits beside the
/// chat and narrows it, which re-wraps long messages; in a narrow one it
/// sits above the chat and pushes the transcript's top edge down. The
/// transcript used to snap to its new bottom either way. After the pointer
/// leaves, it catches up to its bottom.
pub(super) async fn scenario_pointer_keeps_text_still(t: &Scenario<'_>) {
    // Where the pointer rests, as a fraction of the transcript's height: high
    // up in the wide window, so a lot of text below it re-wraps when the
    // panel narrows the chat; low in the narrow one, where the panel takes
    // the top of the window.
    for (width, height, aim) in [(1400, 900, 0.25), (800, 900, 0.75)] {
        let conversation = t.conversation().await;
        for i in 0..30 {
            db::create_message(
                t.pool,
                conversation.id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &[anthropic::ContentBlock::Text {
                    text: format!("message {i}: {}", "a long line that wraps across the width of the chat column ".repeat(6)),
                }],
            )
            .await
            .expect("seed a message");
        }
        let page = t.tab("about:blank").await;
        page.execute(
            chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(width, height, 1.0, false),
        )
        .await
        .expect("set the window size");
        page.goto(t.url(&format!("conversation/{}", conversation.id)))
            .await
            .expect("open the conversation");
        wait_for_live_client(&page, conversation.id).await;
        wait_for_transcript_at_bottom(&page).await;
        let centre: Vec<f64> = page
            .evaluate(format!("(() => {{ const r = document.querySelector('.messages').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height * {aim}]; }})()"))
            .await
            .expect("read the transcript's box")
            .into_value()
            .expect("two numbers");
        // Two moves, so the page sees the pointer move over the transcript.
        for dx in [0.0, 1.0] {
            page.move_mouse(chromiumoxide::layout::Point::new(centre[0] + dx, centre[1]))
                .await
                .expect("move the mouse over the transcript");
        }
        let watch = format!(
            "(() => {{ window.__sme75 = document.elementFromPoint({}, {}); return window.__sme75 ? window.__sme75.getBoundingClientRect().top : null; }})()",
            centre[0] + 1.0,
            centre[1]
        );
        let before: f64 = page
            .evaluate(watch.as_str())
            .await
            .expect("read what's under the pointer")
            .into_value()
            .expect("an element under the pointer");
        sandbox::create_pod(t.pool, conversation.id, Default::default()).await.expect("create_pod");
        wait_for_element(&page, ".sandbox-panel", Duration::from_secs(30)).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let after: f64 = page
            .evaluate("window.__sme75.getBoundingClientRect().top")
            .await
            .expect("read where it is now")
            .into_value()
            .expect("a number");
        assert!(
            (after - before).abs() <= 2.0,
            "at {width}px the text under the pointer moved when the sandbox panel appeared: top {before} -> {after}"
        );
        // Short of its bottom with the text held still means a snap is
        // pending. Then a scroll that isn't the user's (no wheel, touch,
        // key or press: the transcript keeping the text under the pointer
        // still) mustn't count as the user leaving the bottom (SME-75 code
        // review). Where holding the text still ended at the bottom (the
        // panel above, in a narrow window), nothing is pending.
        if transcript_distance_from_bottom(&page).await > 1.0 {
            page.evaluate("(() => { document.querySelector('.messages').scrollTop -= 100; })()")
                .await
                .expect("scroll the transcript");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0))
            .await
            .expect("move the mouse off the transcript");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let distance = transcript_distance_from_bottom(&page).await;
        assert!(
            distance <= 1.0,
            "at {width}px, once the pointer left, the transcript should catch up to its bottom, but it's {distance}px away"
        );

        // But a scroll the user makes with a pending snap (a scrollbar
        // drag, or keys after clicking the text: neither is a wheel, touch
        // or key event on the transcript) is theirs: a press on the
        // transcript gives up the pending snap, so leaving doesn't pull
        // them back (SME-75 code review 2). A new terminal card grows the
        // sandbox panel, a layout change that arms the snap.
        page.move_mouse(chromiumoxide::layout::Point::new(centre[0], centre[1]))
            .await
            .expect("move the mouse back over the transcript");
        sandbox::create_terminal(t.pool, conversation.id).await.expect("create_terminal");
        tokio::time::sleep(Duration::from_millis(800)).await;
        page.click(chromiumoxide::layout::Point::new(centre[0], centre[1]))
            .await
            .expect("click the transcript's text");
        page.evaluate("(() => { document.querySelector('.messages').scrollTop -= 100; })()")
            .await
            .expect("scroll the transcript up");
        tokio::time::sleep(Duration::from_millis(300)).await;
        page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0))
            .await
            .expect("move the mouse off the transcript");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let distance = transcript_distance_from_bottom(&page).await;
        assert!(
            distance >= 90.0,
            "at {width}px, a scroll after clicking the text should stay where the user put it, but the transcript is {distance}px from its bottom"
        );
    }
}
