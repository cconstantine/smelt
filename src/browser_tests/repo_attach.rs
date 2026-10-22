//! SME-263: a "Work on a repo" error the tab's request read is kept for its
//! conversation, through a switch and once the first message hides the
//! form. Each scenario forces a late failure
//! with no server hook, the way
//! `api::git::tests::test_work_on_a_repo_that_fails_after_trusting_still_tells_the_models`
//! does: an uncommitted repo row from another remote holds the
//! conversation's directory, the attach trusts its remote and then waits on
//! that row, and committing the row makes it fail with "already used by".

use super::*;

/// The attach's directory, held by another remote's uncommitted repo row in
/// `conversation`. Dropped without a commit (a failed scenario), it rolls
/// back.
async fn hold_the_directory<'a>(
    pool: &'a sqlx::PgPool,
    conversation: i64,
    dir: &str,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut holding = pool.begin().await.expect("begin the holding transaction");
    sqlx::query(
        "INSERT INTO conversation_repos (conversation_id, url, remote_key, dir, status)
         VALUES ($1, $2, $3, $4, 'cloning')",
    )
    .bind(conversation)
    .bind(format!("https://github.com/x/{dir}"))
    .bind(format!("github.com/x/{dir}"))
    .bind(dir)
    .execute(&mut *holding)
    .await
    .expect("hold the directory, uncommitted");
    holding
}

/// Types `https://github.com/o/{dir}` into the open conversation's "Work on
/// a repo", clicks Clone, and waits until the attach waits on the held
/// directory.
async fn clone_into_the_held_directory(page: &chromiumoxide::Page, pool: &sqlx::PgPool, dir: &str) {
    let url = format!("https://github.com/o/{dir}");
    let field = wait_for_element(page, "#repo-attach-url", Duration::from_secs(10)).await;
    field.focus().await.expect("focus the repo URL");
    field.type_str(&url).await.expect("type the repo URL");
    let typed: String = page
        .evaluate("document.querySelector('#repo-attach-url').value")
        .await
        .expect("read the repo URL back")
        .into_value()
        .expect("text");
    assert_eq!(typed, url, "the repo URL field should hold what was typed");
    click_when_present(page, ".repo-attach button[type=submit]", Duration::from_secs(5)).await;
    assert!(
        wait_until(|| async { crate::git::test_support::lock_waiters(pool).await >= 1 }, Duration::from_secs(15)).await,
        "the attach never waited on the held directory"
    );
}

/// Commits the held row and waits until nothing waits on a lock any more:
/// the attach has failed with "already used by".
async fn release(holding: sqlx::Transaction<'_, sqlx::Postgres>, pool: &sqlx::PgPool) {
    holding.commit().await.expect("commit the other remote's repo");
    assert!(
        wait_until(|| async { crate::git::test_support::lock_waiters(pool).await == 0 }, Duration::from_secs(15)).await,
        "the attach still waits after the hold was committed"
    );
}

/// The Clone button's label and whether it's disabled.
async fn clone_button(page: &chromiumoxide::Page) -> (String, bool) {
    page.evaluate(
        "(() => { const b = document.querySelector('.repo-attach button[type=submit]'); \
           return b ? [b.innerText, b.disabled] : ['', false]; })()",
    )
    .await
    .expect("read the Clone button")
    .into_value()
    .expect("a label and a bool")
}

/// Clicks conversation `id`'s sidebar row and waits for the page to be live
/// on it.
async fn switch_to(page: &chromiumoxide::Page, id: i64) {
    click_conversation(page, id).await;
    wait_for_live_client(page, id).await;
}

/// SME-263 (Q1): Clone in A, switch to B while the attach waits, and back.
/// A still shows the clone in flight; once it fails while B is open, back
/// in A the error shows in the form. Before the fix the answer was dropped
/// on `selected() != Some(id)` and the switch's reset emptied the form.
pub(super) async fn scenario_attach_error_outlives_a_switch(t: &Scenario<'_>) {
    let a = t.conversation().await;
    let b = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", a.id))).await;
    wait_for_live_client(&page, a.id).await;
    let dir = unique_id("sme263");
    let holding = hold_the_directory(t.pool, a.id, &dir).await;
    clone_into_the_held_directory(&page, t.pool, &dir).await;

    switch_to(&page, b.id).await;
    switch_to(&page, a.id).await;
    wait_for_element(&page, ".repo-attach button[type=submit]", Duration::from_secs(10)).await;
    let (label, disabled) = clone_button(&page).await;
    assert!(
        label.starts_with("Cloning") && disabled,
        "back in A while its clone waits, Clone should read \"Cloning…\" and be disabled: {label:?}, disabled {disabled}"
    );

    switch_to(&page, b.id).await;
    release(holding, t.pool).await;
    switch_to(&page, a.id).await;
    assert!(
        wait_for_text(&page, "already used by", Duration::from_secs(10)).await,
        "back in A, the attach's error should show"
    );
    let in_the_form: bool = page
        .evaluate("(document.querySelector('.repo-attach-error')?.innerText ?? '').includes('already used by')")
        .await
        .expect("read the form's error")
        .into_value()
        .expect("a bool");
    assert!(in_the_form, "the error should show in A's \"Work on a repo\" form");
    let (label, disabled) = clone_button(&page).await;
    assert!(label == "Clone" && !disabled, "once it failed, Clone is offered again: {label:?}, disabled {disabled}");
}

/// Whether a `p.error` line in the transcript says the attach failed.
const ATTACH_LINE: &str = "Array.from(document.querySelectorAll('.messages p.error')).some(p => p.innerText.includes('\"Work on a repo\" failed.') && p.innerText.includes('already used by'))";

/// SME-263 (Q2): Clone in C, then send the first message while the attach
/// waits: the start screen and its form go. The attach's error, arriving
/// afterwards, shows as a line under the transcript; the next send clears
/// it. Before the fix it was written to the form's state with nothing to
/// render it.
pub(super) async fn scenario_attach_error_after_the_first_message(t: &Scenario<'_>) {
    let c = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", c.id))).await;
    wait_for_live_client(&page, c.id).await;
    let dir = unique_id("sme263-first");
    let holding = hold_the_directory(t.pool, c.id, &dir).await;
    clone_into_the_held_directory(&page, t.pool, &dir).await;

    send_from(&page, "first message while cloning").await;
    assert!(
        wait_for_count(&page, ".conversation-empty", 0, Duration::from_secs(10)).await,
        "the first message should take the start screen down"
    );
    release(holding, t.pool).await;
    assert!(
        wait_until(
            || async { page.evaluate(ATTACH_LINE).await.ok().and_then(|v| v.into_value::<bool>().ok()).unwrap_or(false) },
            Duration::from_secs(10),
        )
        .await,
        "the attach's error should show as a line under the transcript"
    );

    assert!(t.mock.wait_until_idle(Duration::from_secs(20)).await, "the first reply never finished upstream");
    assert!(wait_for_text(&page, "zebra24", Duration::from_secs(10)).await, "the first reply should show");
    send_from(&page, "second message").await;
    assert!(
        wait_until(
            || async { page.evaluate(ATTACH_LINE).await.ok().and_then(|v| v.into_value::<bool>().ok()) == Some(false) },
            Duration::from_secs(10),
        )
        .await,
        "the next send should clear the attach's line"
    );
}
