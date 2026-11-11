use super::*;

/// SME-455: a live seat that owes its parent a report with nothing running
/// shows a `!` on its sidebar row — the tree stays open on it (the fold
/// used to hide exactly this tree), the title and aria-label say the age
/// of the owed moment, the parent's own row stays plain, and clearing the
/// column clears the mark. No pod, no provider: the mark reads a column,
/// so every conversation here is idle by construction.
pub(super) async fn scenario_report_owed_marker(t: &Scenario<'_>) {
    let pool = t.pool;
    let root = t.conversation().await;
    seed_user_message(pool, root.id, "a parent with a stuck seat").await;
    let seat = seed_agent(pool, root.id, "owes a report", None).await;
    sqlx::query("UPDATE agents SET report_owed_at = now() - interval '2 hours' WHERE conversation_id = $1")
        .bind(seat)
        .execute(pool)
        .await
        .expect("mark the seat owed");

    let home = t.tab(t.url("")).await;
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;

    // The owed seat keeps its tree open: the row is there with no click.
    let seat_row = format!(".conversation-item[data-conversation-id='{seat}']");
    let marker = format!("{seat_row} .conversation-report-owed");
    assert!(wait_for_count(&home, &seat_row, 1, Duration::from_secs(10)).await, "the owed seat keeps its tree open");

    // The mark, its title and aria-label (a pointer and a screen reader),
    // and the age of the original owed moment.
    assert!(wait_for_count(&home, &marker, 1, Duration::from_secs(10)).await, "the mark should show on the seat's row");
    let label: String = home
        .evaluate(format!(
            "(() => {{ const s = document.querySelector({marker:?}); \
             return s.getAttribute('aria-label') + '|' + s.title + '|' + s.textContent; }})()"
        ))
        .await
        .expect("read the mark")
        .into_value()
        .expect("text");
    assert_eq!(label, "owes its parent a report for 2h|owes its parent a report for 2h|!");

    // The parent's own row stays plain: the mark is per seat.
    let root_marker = format!(".conversation-item[data-conversation-id='{}'] .conversation-report-owed", root.id);
    assert!(wait_for_count(&home, &root_marker, 0, Duration::from_secs(2)).await, "the parent has no mark");

    // Clear the column; a reload loses the mark (the seat stays listed —
    // the tree may fold now that it's settled, so only the mark is read).
    sqlx::query("UPDATE agents SET report_owed_at = NULL WHERE conversation_id = $1")
        .bind(seat)
        .execute(pool)
        .await
        .expect("clear the owed mark");
    home.reload().await.expect("reload the home page");
    wait_for_element(&home, "[data-app-live]", Duration::from_secs(15)).await;
    assert!(wait_for_count(&home, &marker, 0, Duration::from_secs(10)).await, "the mark should clear with the column");
}
