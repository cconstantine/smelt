use super::*;

/// SME-236: a conversation's armed Delete names the agents that go with it,
/// and the longer label fits its row, on a phone and on a laptop.
pub(super) async fn scenario_delete_names_its_agents(t: &Scenario<'_>) {
    let root = t.conversation().await;
    seed_user_message(t.pool, root.id, "a parent with agents").await;
    // Its two children, made directly: the label needs only the rows. The
    // root's removal at the end of the run cascades to them.
    for brief in ["first agent", "second agent"] {
        let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ($1, $2) RETURNING id")
            .bind(brief)
            .bind(root.id)
            .fetch_one(t.pool)
            .await
            .expect("a child row");
        seed_user_message(t.pool, child, brief).await;
    }
    let row = format!(".conversation-item[data-conversation-id='{}']", root.id);
    let delete = format!("{row} .delete-conversation");
    for (width, height, mobile) in [(390, 844, true), (1400, 900, false)] {
        let page = t.tab("about:blank").await;
        page.execute(
            chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(width, height, 2.0, mobile),
        )
        .await
        .expect("set the window's size");
        page.goto(t.url("")).await.expect("open the home page");
        wait_for_element(&page, "[data-app-live]", Duration::from_secs(15)).await;
        if mobile {
            click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
        }
        wait_for_element(&page, &delete, Duration::from_secs(10)).await;
        click_when_present(&page, &delete, Duration::from_secs(5)).await;
        wait_for_element(&page, &format!("{delete}.confirm"), Duration::from_secs(5)).await;
        let measured: Vec<String> = page
            .evaluate(format!(
                "(() => {{ const row = document.querySelector(\"{row}\"); \
                 const label = row.querySelector('.two-step-confirm'); \
                 return [label.innerText, String(row.scrollWidth - row.clientWidth), \
                 String(document.documentElement.scrollWidth - document.documentElement.clientWidth)]; }})()"
            ))
            .await
            .expect("measure the row")
            .into_value()
            .expect("strings");
        assert_eq!(measured[0], "Delete + 2 agents?", "{width}px: {measured:?}");
        assert!(measured[1].parse::<f64>().unwrap_or(1.0) <= 0.0, "{width}px: the row overflows: {measured:?}");
        assert!(measured[2].parse::<f64>().unwrap_or(1.0) <= 0.0, "{width}px: the page scrolls sideways: {measured:?}");
        page.close().await.expect("close the tab");
    }
}

/// An agent row under `parent` in `state`, as `start_agent` leaves one,
/// with a persona at `persona_path` when given.
async fn seed_agent(pool: &sqlx::PgPool, parent: i64, title: &str, persona_path: Option<&str>) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ($1, $2) RETURNING id")
        .bind(title)
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child row");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, persona_name, persona_path, state, started_at)
         VALUES ($1, $2, 1, 'a brief', $3, $4, 'active', now())",
    )
    .bind(child)
    .bind(parent)
    .bind(persona_path.map(|_| "developer"))
    .bind(persona_path)
    .execute(pool)
    .await
    .expect("its agent row");
    seed_user_message(pool, child, title).await;
    child
}

/// An unanswered question in `conversation_id`, asked of `asked_of`.
async fn seed_question(pool: &sqlx::PgPool, conversation_id: i64, asked_of: &str) {
    sqlx::query(
        "INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of)
         VALUES ($1, 'toolu_tree', '[]', $2)",
    )
    .bind(conversation_id)
    .bind(asked_of)
    .execute(pool)
    .await
    .expect("a question");
}

/// The sidebar's rows under (and including) `root`, as
/// `id|depth|meta|count`: its persona and status line, and its count of
/// agents waiting under it.
async fn tree_rows(page: &chromiumoxide::Page, ids: &[i64]) -> Vec<String> {
    let ids = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    page.evaluate(format!(
        "(() => {{ const ids = new Set([{ids}].map(String)); \
         return [...document.querySelectorAll('.conversation-item')] \
           .filter(r => ids.has(r.dataset.conversationId)) \
           .map(r => [r.dataset.conversationId, r.dataset.depth, \
                      r.querySelector('.conversation-agent-meta')?.innerText ?? '', \
                      r.querySelector('.agents-waiting-count')?.innerText ?? ''].join('|')); }})()"
    ))
    .await
    .expect("read the tree")
    .into_value()
    .expect("strings")
}

/// Waits until the tree's rows for `ids` read `want`; panics with what
/// they read instead.
async fn wait_for_tree(page: &chromiumoxide::Page, ids: &[i64], want: &[String], what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = tree_rows(page, ids).await;
        if rows == want {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: the tree reads {rows:?}, not {want:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// SME-246: agents list under their parent with persona and status, fold
/// away under a caret, and a waiting one is found from its folded parent,
/// from the phone's folded list and from the parent's own banner, on a
/// fresh load and live; every row fits on a phone and on a laptop.
pub(super) async fn scenario_agent_tree(t: &Scenario<'_>) {
    let root = t.conversation().await;
    seed_user_message(t.pool, root.id, "a parent with agents").await;
    let long_dir = "a-checkout-with-a-rather-long-directory-name";
    let persona = format!("/workspace/{long_dir}/.agents/personas/developer.md");
    // Removed with the root at the end of the run: the delete cascades.
    let developer = seed_agent(t.pool, root.id, "developer agent", Some(&persona)).await;
    let asks_parent = seed_agent(t.pool, root.id, "asks its parent", None).await;
    let long_title = "a grandchild whose title is far too long for any sidebar row to show in full";
    let grandchild = seed_agent(t.pool, developer, long_title, None).await;
    seed_question(t.pool, asks_parent, "parent").await;
    seed_question(t.pool, grandchild, "user").await;
    let ids = [root.id, developer, asks_parent, grandchild];
    let row = |id: i64, depth: usize, meta: &str, count: &str| format!("{id}|{depth}|{meta}|{count}");
    let dev_meta = format!("idle \u{b7} {long_dir}:developer");
    let expanded = vec![
        row(root.id, 0, "", "?1"),
        row(developer, 1, &dev_meta, "?1"),
        row(grandchild, 2, "waiting for the user", ""),
        row(asks_parent, 1, "waiting for its parent", ""),
    ];
    for (width, height, mobile) in [(390, 844, true), (1400, 900, false)] {
        let page = t.tab("about:blank").await;
        page.execute(
            chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(width, height, 2.0, mobile),
        )
        .await
        .expect("set the window's size");
        page.goto(t.url(&format!("conversation/{}", root.id))).await.expect("open the parent");
        wait_for_live_client(&page, root.id).await;
        // The parent's banner links the agent waiting two levels down.
        let link = format!(".agents-waiting-banner .agents-waiting-link[href$='/conversation/{grandchild}']");
        wait_for_element(&page, &link, Duration::from_secs(10)).await;
        if mobile {
            // The folded list's button says something waits.
            wait_for_element(&page, ".sidebar-toggle .agents-waiting-count", Duration::from_secs(10)).await;
            click_when_present(&page, ".sidebar-toggle", Duration::from_secs(5)).await;
        }
        wait_for_tree(&page, &ids, &expanded, &format!("{width}px, loaded")).await;
        let measured: Vec<String> = page
            .evaluate(format!(
                "(() => {{ const ids = new Set([{}].map(String)); \
                 const rows = [...document.querySelectorAll('.conversation-item')].filter(r => ids.has(r.dataset.conversationId)); \
                 const over = rows.filter(r => r.scrollWidth > r.clientWidth).map(r => r.dataset.conversationId); \
                 return [over.join(','), String(document.documentElement.scrollWidth - document.documentElement.clientWidth)]; }})()",
                ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
            ))
            .await
            .expect("measure the rows")
            .into_value()
            .expect("strings");
        assert_eq!(measured[0], "", "{width}px: these rows overflow: {measured:?}");
        assert!(measured[1].parse::<f64>().unwrap_or(1.0) <= 0.0, "{width}px: the page scrolls sideways: {measured:?}");

        // Folding the parent hides its agents and keeps the count.
        let caret = format!(".conversation-item[data-conversation-id='{}'] .tree-caret", root.id);
        click_when_present(&page, &caret, Duration::from_secs(5)).await;
        wait_for_tree(&page, &ids, &[row(root.id, 0, "", "?1")], &format!("{width}px, folded")).await;
        let expanded_attr: String = page
            .evaluate(format!("document.querySelector({caret:?}).getAttribute('aria-expanded')"))
            .await
            .expect("read the caret")
            .into_value()
            .expect("a string");
        assert_eq!(expanded_attr, "false", "{width}px");
        click_when_present(&page, &caret, Duration::from_secs(5)).await;
        wait_for_tree(&page, &ids, &expanded, &format!("{width}px, unfolded")).await;
        page.close().await.expect("close the tab");
    }

    // Live: the developer agent asks the user, and an open tab's tree and
    // banner follow without a reload.
    let page = t.tab(t.url(&format!("conversation/{}", root.id))).await;
    wait_for_live_client(&page, root.id).await;
    wait_for_tree(&page, &ids, &expanded, "before the live change").await;
    seed_question(t.pool, developer, "user").await;
    crate::events::publish_app(crate::events::AppEvent::QuestionsChanged);
    let live = vec![
        row(root.id, 0, "", "?2"),
        row(developer, 1, &format!("waiting for the user \u{b7} {long_dir}:developer"), "?1"),
        row(grandchild, 2, "waiting for the user", ""),
        row(asks_parent, 1, "waiting for its parent", ""),
    ];
    wait_for_tree(&page, &ids, &live, "after the developer asked").await;
    assert!(
        wait_for_count(&page, ".agents-waiting-banner .agents-waiting-link", 2, Duration::from_secs(10)).await,
        "the banner didn't gain the developer agent"
    );
    // The banner's link opens the waiting agent, and its row shows, marked
    // open, though its grandparent was folded (review 1, finding 3).
    let caret = format!(".conversation-item[data-conversation-id='{}'] .tree-caret", root.id);
    click_when_present(&page, &caret, Duration::from_secs(5)).await;
    wait_for_tree(&page, &ids, &[row(root.id, 0, "", "?2")], "folded before the link").await;
    click_when_present(&page, &format!(".agents-waiting-link[href$='/conversation/{grandchild}']"), Duration::from_secs(5)).await;
    wait_for_live_client(&page, grandchild).await;
    wait_for_element(
        &page,
        &format!(".conversation-item.active[data-conversation-id='{grandchild}']"),
        Duration::from_secs(10),
    )
    .await;
}
