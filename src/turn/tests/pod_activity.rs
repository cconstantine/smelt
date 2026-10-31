//! SME-78: which turns count as pod activity. A turn that brought the model
//! something new counts at its end; a wake for SME-79's idle notice alone
//! doesn't (the user's answer 2).

use super::*;

async fn fresh_conversation(pool: &PgPool) -> i64 {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    db::create_conversation(pool).await.expect("create conversation").id
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_users_turn_notes_activity_at_its_end_not_its_start(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    start_mock_upstream(&pool, vec![text_reply_body("Done.")]).await;
    let (reached, release) = test_hooks::pause_after_message_saved(conversation_id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation_id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn saves its message")
        .expect("hook");
    let mid_turn = crate::sandbox::activity_noted(conversation_id);
    let _ = release.send(());
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the turn ends")
        .expect("join");

    assert!(result.is_ok(), "{result:?}");
    assert!(!mid_turn, "noted while the turn was still running");
    assert!(crate::sandbox::activity_noted(conversation_id), "the turn's end is activity");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_for_an_idle_notice_alone_notes_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    let pod = db::create_sandbox_pod(&pool, conversation_id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    let command = format!("idle-{conversation_id}");
    db::create_terminal_command(&pool, conversation_id, terminal.id, &command, "npm run dev", 300)
        .await
        .expect("command");
    sqlx::query("UPDATE terminal_commands SET created_at = now() - interval '10 minutes' WHERE command_id = $1")
        .bind(&command)
        .execute(&pool)
        .await
        .expect("leave it untouched for ten minutes");
    start_mock_upstream(&pool, vec![text_reply_body("Still going; leaving it.")]).await;

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert!(
        saved.iter().any(|m| m.content.contains("is still running")),
        "the wake saved the idle notice: {saved:?}"
    );
    assert!(!crate::sandbox::activity_noted(conversation_id), "an idle notice's turn isn't activity");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_that_drains_a_finished_command_notes_activity(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    unnotified_finished_command(&pool, conversation_id, &format!("done-{conversation_id}")).await;
    start_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert!(!saved.is_empty(), "the wake drained the notice");
    assert!(crate::sandbox::activity_noted(conversation_id), "a finished command's notice is activity");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_that_drains_an_agent_message_notes_activity(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    sqlx::query(
        "INSERT INTO agent_messages (to_conversation_id, from_label, kind, body) VALUES ($1, 'child', 'report', 'All done.')",
    )
    .bind(conversation_id)
    .execute(&pool)
    .await
    .expect("a report waiting");
    start_mock_upstream(&pool, vec![text_reply_body("Thanks.")]).await;

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert!(saved.iter().any(|m| m.content.contains("All done.")), "the report was delivered: {saved:?}");
    assert!(crate::sandbox::activity_noted(conversation_id), "an agent's report is activity");
}
