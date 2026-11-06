//! Turns on a Claude Code provider: the cases PR #285's round-1 review
//! (SME-430) added.

use super::*;

/// Review 3 M1: a Stop while the CLI streams the second reply of a tool
/// loop (its results already delivered) leaves no session to resume
/// past: the next turn starts a new one from smelt's history.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_mid_loop_leaves_no_session_behind(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let partial = api_call(10, vec![text_block("Working on")], "end_turn", 1);
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_m1", "todoread", json!({}))]));
    first.extend([json!({"stamp": "second_reply"}), json!({"emit": partial[..3]}), json!({"await_interrupt": true})]);
    let fake = FakeCli::new("stop-loop", vec![first, vec![json!({"read": true}), text_reply("again")]]);
    fake.pick_for(&pool, conversation.id).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    fake.until("the second reply streams", |f| f.dir.join("second_reply.stamp").exists()).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_turn_now(conversation.id);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await.expect("the turn ends");
    run_turn(&pool, conversation.id, hello()).await.expect("the next turn");

    assert!(user_texts(&fake, 1)[0].starts_with(crate::claude_cli::render::REHYDRATED), "{:?}", user_texts(&fake, 1));
}

/// The same for a Stop during a turn's first reply, after an earlier
/// turn left a session.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_in_a_turns_first_reply_leaves_no_session_behind(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let partial = api_call(10, vec![text_block("Starting")], "end_turn", 1);
    let fake = FakeCli::new("stop-first", vec![
        vec![json!({"read": true}), text_reply("one")],
        vec![json!({"read": true}), json!({"emit": partial[..3]}), json!({"await_interrupt": true})],
        vec![json!({"read": true}), text_reply("three")],
    ]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    fake.until("turn 2 is sent its message", |f| !user_texts(f, 1).is_empty()).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop_turn_now(conversation.id);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await.expect("the turn ends");
    run_turn(&pool, conversation.id, hello()).await.expect("turn 3");

    assert!(user_texts(&fake, 2)[0].starts_with(crate::claude_cli::render::REHYDRATED), "{:?}", user_texts(&fake, 2));
}
