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
