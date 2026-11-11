use super::*;

/// SME-480 T11: archived trees leave the default sidebar and come back
/// with "Show archived"; one owing the user an answer stays listed (D1),
/// as does the open one (D7); the header archives and unarchives.
/// No pod, no provider: everything here is rows and the API.
pub(super) async fn scenario_archived_conversations(t: &Scenario<'_>) {
    let pool = t.pool;
    let a = t.conversation().await;
    seed_user_message(pool, a.id, "a finished driver").await;
    let agent = seed_agent(pool, a.id, "its finished seat", None).await;
    let b = t.conversation().await;
    seed_user_message(pool, b.id, "a driver still in use").await;
    crate::api::archive::archive_in(pool, a.id, true).await.expect("archive A");

    let row = |id: i64| format!(".conversation-item[data-conversation-id='{id}']");
    let (a_row, agent_row, b_row) = (row(a.id), row(agent), row(b.id));
    let tag = |id: i64| format!("{} .conversation-archived", row(id));

    // 1. Home hides A's whole tree; B is listed; the toggle counts A.
    let home = t.tab(t.url("")).await;
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    assert!(wait_for_count(&home, &b_row, 1, Duration::from_secs(10)).await, "B is listed");
    assert!(wait_for_count(&home, &a_row, 0, Duration::from_secs(2)).await, "A is hidden");
    assert!(wait_for_count(&home, &agent_row, 0, Duration::from_secs(2)).await, "and so is its agent");
    assert_eq!(text_of(&home, ".show-archived").await, "Show archived (1)");

    // 2. The toggle shows A, tagged, and hides it again.
    click_when_present(&home, ".show-archived", Duration::from_secs(5)).await;
    assert!(wait_for_count(&home, &tag(a.id), 1, Duration::from_secs(10)).await, "A shows, tagged");
    assert!(wait_for_count(&home, &tag(agent), 0, Duration::from_secs(2)).await, "only the root carries the tag");
    assert_eq!(attr_of(&home, ".show-archived", "aria-pressed").await, "true");
    assert_eq!(text_of(&home, ".show-archived").await, "Hide archived");
    click_when_present(&home, ".show-archived", Duration::from_secs(5)).await;
    assert!(wait_for_count(&home, &a_row, 0, Duration::from_secs(10)).await, "hidden again");

    // 3. D1: the agent waits for the user, so A's tree is listed, tagged,
    // with its marks, and the phone's folded list counts it.
    sqlx::query(
        "INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of)
         VALUES ($1, 'toolu_archived', '[]', 'user')",
    )
    .bind(agent)
    .execute(pool)
    .await
    .expect("a question for the user");
    home.reload().await.expect("reload");
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    assert!(wait_for_count(&home, &tag(a.id), 1, Duration::from_secs(10)).await, "an owed answer is never hidden");
    assert!(
        wait_for_count(&home, &format!("{agent_row} .conversation-waiting"), 1, Duration::from_secs(10)).await,
        "the agent's row shows its ?"
    );
    let phone = t.tab("about:blank").await;
    phone
        .execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true))
        .await
        .expect("emulate a phone");
    phone.goto(t.url("")).await.expect("open Home on the phone");
    wait_for_element(&phone, "[data-app-live]", Duration::from_secs(15)).await;
    // Alone in its run (see `scenarios`), so this lane's question is the
    // only one waiting.
    assert!(
        wait_for_text(&phone, ".sidebar-toggle .agents-waiting-count", "?1", Duration::from_secs(10)).await,
        "the phone's folded list counts the archived tree's question"
    );
    sqlx::query("DELETE FROM pending_questions WHERE conversation_id = $1")
        .bind(agent)
        .execute(pool)
        .await
        .expect("answer it away");
    home.reload().await.expect("reload");
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    assert!(wait_for_count(&home, &a_row, 0, Duration::from_secs(10)).await, "nothing owed: hidden again");

    // 4. The header archives B: its row stays while it's open (D7), tagged,
    // and is gone from Home; opened by URL, Unarchive lists it again.
    let open_b = t.tab(t.url(&format!("conversation/{}", b.id))).await;
    wait_for_live_client(&open_b, b.id).await;
    click_when_present(&open_b, ".conversation-archive", Duration::from_secs(10)).await;
    assert!(wait_for_count(&open_b, &tag(b.id), 1, Duration::from_secs(10)).await, "the open row stays, tagged");
    assert!(wait_for_text(&open_b, ".conversation-archive", "Unarchive", Duration::from_secs(10)).await);
    home.reload().await.expect("reload");
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    assert!(wait_for_count(&home, &b_row, 0, Duration::from_secs(10)).await, "B is gone from Home");
    open_b.reload().await.expect("reload B");
    wait_for_live_client(&open_b, b.id).await;
    click_when_present(&open_b, ".conversation-archive", Duration::from_secs(10)).await;
    assert!(wait_for_text(&open_b, ".conversation-archive", "Archive", Duration::from_secs(10)).await);
    home.reload().await.expect("reload");
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    assert!(wait_for_count(&home, &b_row, 1, Duration::from_secs(10)).await, "B is listed again");
    assert!(wait_for_count(&home, &tag(b.id), 0, Duration::from_secs(2)).await, "untagged");
}

async fn text_of(page: &chromiumoxide::Page, selector: &str) -> String {
    page.evaluate(format!("document.querySelector({selector:?})?.innerText ?? ''"))
        .await
        .expect("read the text")
        .into_value()
        .expect("a string")
}

async fn attr_of(page: &chromiumoxide::Page, selector: &str, name: &str) -> String {
    page.evaluate(format!("document.querySelector({selector:?})?.getAttribute({name:?}) ?? ''"))
        .await
        .expect("read the attribute")
        .into_value()
        .expect("a string")
}

/// Waits up to `timeout` for `selector`'s text to be exactly `want`.
async fn wait_for_text(page: &chromiumoxide::Page, selector: &str, want: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if text_of(page, selector).await.trim() == want {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
