use super::*;

/// SME-441: a conversation idle (its turn ended) but with a terminal
/// command still running is marked in every open window's sidebar — a
/// pulsing amber dot beside the green pod dot, its hover title the count —
/// on both event relays, for a child agent too, and it clears itself when
/// the command finishes or the pod stops. Every conversation stays
/// turn-idle throughout: no provider is called, so "idle" is real by
/// construction.
pub(super) async fn scenario_live_command_marker(t: &Scenario<'_>) {
    let pool = t.pool;
    let idle = t.conversation().await;
    seed_user_message(pool, idle.id, "idle, command running").await;
    let watching = t.conversation().await;
    seed_user_message(pool, watching.id, "the watching conversation").await;
    // Two tabs, one per relay: the home tab rides the app-wide stream, the
    // conversation tab the selected conversation's relay (SME-269).
    let home = t.tab(t.url("")).await;
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    let conv = t.tab(t.url(&format!("conversation/{}", watching.id))).await;
    wait_for_live_client(&conv, watching.id).await;

    let marker = format!(".conversation-item[data-conversation-id=\"{}\"] .live-command-dot", idle.id);
    let dot = format!(".conversation-item[data-conversation-id=\"{}\"] .live-pod-dot", idle.id);
    let pod = sandbox::create_pod(pool, idle.id, Default::default()).await.expect("create_pod");
    let terminal = sandbox::create_terminal(pool, idle.id).await.expect("create_terminal");

    // b. A real tool dispatch records the command; the marker appears on
    //    both tabs without a reload — fails without the `files.rs` create
    //    publish, the relay branch, the apply_event/EventEffect arms or the
    //    app-stream arm.
    let command_id = unique_id("cmd-live");
    anthropic::tools::execute(
        pool,
        idle.id,
        &command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal, "command": "sleep 8 && echo cmd_done"}),
    )
    .await
    .expect("run_terminal_command");
    for (tab, what) in [(&home, "home tab"), (&conv, "conversation tab")] {
        assert!(wait_for_count(tab, &marker, 1, Duration::from_secs(10)).await, "{what}: the marker should show while the command runs");
        // The hover title, to a pointer and a screen reader, count only.
        let label: String = tab
            .evaluate(format!("(() => {{ const d = document.querySelector({marker:?}); return d.getAttribute('aria-label') + '|' + d.title; }})()"))
            .await
            .expect("read the marker")
            .into_value()
            .expect("text");
        assert_eq!(label, "1 command running|1 command running", "{what}");
        // It composes with the green pod dot, and reads distinctly from it.
        assert!(wait_for_count(tab, &dot, 1, Duration::from_secs(5)).await, "{what}: the pod's green dot should stand too");
        let colors: Vec<String> = tab
            .evaluate(format!(
                "(() => {{ const row = document.querySelector(\".conversation-item[data-conversation-id='{}']\"); \
                 return [getComputedStyle(row.querySelector('.live-command-dot')).backgroundColor, \
                         getComputedStyle(row.querySelector('.live-pod-dot')).backgroundColor]; }})()",
                idle.id
            ))
            .await
            .expect("read the colours")
            .into_value()
            .expect("strings");
        assert_ne!(colors[0], colors[1], "{what}: the marker is the pod dot's colour: {colors:?}");
        // And it has a colour at all: with the `.live-command-dot` rule (or
        // its background) gone the computed bg is the transparent initial
        // value, which ≠ green would happily pass — Done-when's *visible*
        // dot demands more than difference.
        assert_ne!(colors[0], "rgba(0, 0, 0, 0)", "{what}: the marker is invisible — computed background is transparent: {colors:?}");
    }

    // c. The command finishes by itself; the marker goes on both tabs —
    //    fails without the `agent.rs` Exit publish.
    for (tab, what) in [(&home, "home tab"), (&conv, "conversation tab")] {
        assert!(wait_for_count(tab, &marker, 0, Duration::from_secs(30)).await, "{what}: the marker should clear when the command finishes");
    }

    // d. Pod stop while a command runs: both marks go, live. Two carriers,
    //    either one on its own enough here: `live_commands` refetches on
    //    `pods_changed()`, and `close_pod_terminals` publishes
    //    CommandsChanged once a `lost` mark landed — stub that publish and
    //    this scenario still passes, PodsChanged alone carries this path
    //    (f175981b's red-first); `quiet_stop.rs`'s user-stop test holds it.
    let long_id = unique_id("cmd-long");
    anthropic::tools::execute(
        pool,
        idle.id,
        &long_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal, "command": "sleep 120"}),
    )
    .await
    .expect("run_terminal_command (long)");
    assert!(wait_for_count(&home, &marker, 1, Duration::from_secs(10)).await, "the marker should show again");
    sandbox::stop_pod_for_user(pool, pod).await.expect("stop_pod_for_user");
    for (tab, what) in [(&home, "home tab"), (&conv, "conversation tab")] {
        assert!(wait_for_count(tab, &marker, 0, Duration::from_secs(10)).await, "{what}: the marker should clear with the pod");
        assert!(wait_for_count(tab, &dot, 0, Duration::from_secs(10)).await, "{what}: the green dot should clear with the pod");
    }

    // e. A child agent's row marks the same way: its commands ride its own
    //    conversation and pod. The seeded tree folds once settled (an
    //    active agent without a running turn is not busy), so it is
    //    unfolded with a real click.
    let root = t.conversation().await;
    seed_user_message(pool, root.id, "a parent with an agent").await;
    let agent1 = seed_agent(pool, root.id, "an agent on a command", None).await;
    // The seeded rows move no events of their own; say so, as the tree
    // test does, so both tabs' trees gain the child.
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    let caret = format!(".conversation-item[data-conversation-id='{}'] .tree-caret", root.id);
    let agent_row = format!(".conversation-item[data-conversation-id='{agent1}']");
    let root_marker = format!(".conversation-item[data-conversation-id='{}'] .live-command-dot", root.id);
    let agent_marker = format!("{agent_row} .live-command-dot");
    // The settled tree folds by itself (an active agent without a running
    // turn is not busy), so each tab unfolds it with a real click if the
    // child's row isn't there.
    for tab in [&home, &conv] {
        wait_for_element(tab, &caret, Duration::from_secs(10)).await;
        if !wait_for_count(tab, &agent_row, 1, Duration::from_secs(2)).await {
            click_when_present(tab, &caret, Duration::from_secs(5)).await;
        }
        wait_for_element(tab, &agent_row, Duration::from_secs(10)).await;
    }
    let agent_pod = sandbox::create_pod(pool, agent1, Default::default()).await.expect("agent create_pod");
    let agent_terminal = sandbox::create_terminal(pool, agent1).await.expect("agent terminal");
    let agent_command = unique_id("cmd-agent");
    anthropic::tools::execute(
        pool,
        agent1,
        &agent_command,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": agent_terminal, "command": "sleep 120"}),
    )
    .await
    .expect("run_terminal_command (agent)");
    for (tab, what) in [(&home, "home tab"), (&conv, "conversation tab")] {
        assert!(wait_for_count(tab, &agent_marker, 1, Duration::from_secs(10)).await, "{what}: the agent's row should mark");
        // The parent's own row stays plain: the mark is per conversation.
        assert!(wait_for_count(tab, &root_marker, 0, Duration::from_secs(5)).await, "{what}: the parent's row must not mark");
    }

    // f. Layout guard (SME-236's width): at 390px, with the marker showing,
    //    neither the row nor the document overflow sideways.
    let phone = t.tab("about:blank").await;
    phone
        .execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true))
        .await
        .expect("set the window's size");
    phone.goto(t.url("")).await.expect("open the home page");
    wait_for_element(&phone, "[data-app-live]", Duration::from_secs(15)).await;
    click_when_present(&phone, ".sidebar-toggle", Duration::from_secs(5)).await;
    if !wait_for_count(&phone, &agent_row, 1, Duration::from_secs(2)).await {
        click_when_present(&phone, &caret, Duration::from_secs(5)).await;
    }
    wait_for_element(&phone, &agent_marker, Duration::from_secs(10)).await;
    let measured: Vec<String> = phone
        .evaluate(format!(
            "(() => {{ const row = document.querySelector({agent_row:?}); \
             return [String(row.scrollWidth - row.clientWidth), \
                     String(document.documentElement.scrollWidth - document.documentElement.clientWidth)]; }})()"
        ))
        .await
        .expect("measure the row")
        .into_value()
        .expect("strings");
    assert!(measured[0].parse::<f64>().unwrap_or(1.0) <= 0.0, "the marking row overflows at 390px: {measured:?}");
    assert!(measured[1].parse::<f64>().unwrap_or(1.0) <= 0.0, "the page scrolls sideways at 390px: {measured:?}");
    phone.close().await.expect("close the tab");

    // g. The agent's pod stop clears its marker too — plan 5.e's promise,
    //    asserted rather than left as cleanup. The same two carriers as
    //    step d: the `pods_changed()` refetch this scenario drives, and
    //    the `close_pod_terminals` publish (`quiet_stop.rs`'s user-stop
    //    test), reaching the agent's own conversation on both relays.
    sandbox::stop_pod_for_user(pool, agent_pod).await.expect("stop the agent's pod");
    for (tab, what) in [(&home, "home tab"), (&conv, "conversation tab")] {
        assert!(wait_for_count(tab, &agent_marker, 0, Duration::from_secs(10)).await, "{what}: the agent's marker should clear with its pod too");
    }
}
