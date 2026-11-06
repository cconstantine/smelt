//! A Claude Code conversation's CLI session across turns (SME-430, PR 2):
//! each turn resumes it and sends only what's new, and only a history
//! that went another way, a session that can't resume, or a failed call
//! starts a new one from smelt's history.

use super::*;

/// The value after `flag` in process `n`'s arguments.
fn arg_after(fake: &FakeCli, n: usize, flag: &str) -> Option<String> {
    let args = fake.args(n);
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_turns_resume_their_session_and_send_only_whats_new(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_s1", "todoread", json!({}))]));
    first.push(text_reply("one"));
    let fake = FakeCli::new("resume", vec![first, vec![json!({"read": true}), text_reply("two")], vec![json!({"read": true}), text_reply("three")]]);
    fake.pick_for(&pool, conversation.id).await;

    for _ in 0..3 {
        run_turn(&pool, conversation.id, hello()).await.expect("a turn");
    }

    let session = arg_after(&fake, 0, "--session-id").expect("the first turn starts a session");
    assert!(!fake.args(0).iter().any(|a| a == "--no-session-persistence"), "it's kept on disk");
    for n in [1, 2] {
        assert_eq!(arg_after(&fake, n, "--resume").as_deref(), Some(session.as_str()), "turn {n} resumes it");
        assert_eq!(user_texts(&fake, n), ["hello"], "turn {n} sends only what's new");
    }
    let row = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("a session row");
    assert_eq!(row.session_id, session);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_history_that_went_another_way_starts_a_new_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("diverged", vec![vec![json!({"read": true}), text_reply("one")], vec![json!({"read": true}), text_reply("two")]]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    let first = db::list_messages(&pool, conversation.id).await.expect("messages")[0].id;
    sqlx::query("UPDATE messages SET content = $1 WHERE id = $2")
        .bind(r#"[{"type":"text","text":"edited"}]"#)
        .bind(first)
        .execute(&pool)
        .await
        .expect("edit the history");

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2");

    let (old, new) = (arg_after(&fake, 0, "--session-id"), arg_after(&fake, 1, "--session-id"));
    assert!(new.is_some() && new != old, "a new session: {old:?} {new:?}");
    assert_eq!(arg_after(&fake, 1, "--resume"), None);
    assert!(user_texts(&fake, 1)[0].starts_with(crate::claude_cli::render::REHYDRATED), "{:?}", user_texts(&fake, 1));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_session_that_wont_resume_is_replaced(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let gone = json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["No conversation found with session ID: x"]});
    let fake = FakeCli::new("gone", vec![
        vec![json!({"read": true}), text_reply("one")],
        vec![json!({"read": true}), json!({"emit": [gone]}), json!({"exit": 1})],
        vec![json!({"read": true}), text_reply("fresh")],
    ]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2 carries on in a new session");

    assert!(arg_after(&fake, 1, "--resume").is_some(), "it tried to resume");
    assert!(arg_after(&fake, 2, "--session-id").is_some(), "then started a new session");
    assert!(user_texts(&fake, 2)[0].starts_with(crate::claude_cli::render::REHYDRATED));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_call_forgets_the_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let refused = json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 500, "result": "boom"});
    let fake = FakeCli::new("forget", vec![
        vec![json!({"read": true}), text_reply("one")],
        vec![json!({"read": true}), json!({"emit": [stream(json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}})), refused]}), json!({"exit": 1})],
    ]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    assert!(db::get_claude_cli_session(&pool, conversation.id).await.expect("read").is_some());

    run_turn(&pool, conversation.id, hello()).await.expect_err("it fails");

    assert_eq!(db::get_claude_cli_session(&pool, conversation.id).await.expect("read"), None, "the next turn starts over");
}

/// SME-399's L11: CLI processes share one config dir, and each writes its
/// `.claude.json` as it starts, which concurrent starts can clobber. A
/// process starts only once the last one to start has said its first line.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_cli_processes_start_one_at_a_time(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let (a, b) = (db::create_conversation(&pool).await.expect("a"), db::create_conversation(&pool).await.expect("b"));
    let slow = FakeCli::new("start-a", vec![vec![json!({"read": true}), json!({"sleep_ms": 800}), json!({"stamp": "first_line"}), text_reply("A")]]);
    let next = FakeCli::new("start-b", vec![vec![json!({"read": true}), text_reply("B")]]);
    slow.pick_for(&pool, a.id).await;
    next.pick_for(&pool, b.id).await;

    let first = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a.id, hello()).await }
    });
    slow.until("the first process starts", |f| f.started() == 1).await;
    run_turn(&pool, b.id, hello()).await.expect("turn b");
    first.await.expect("join").expect("turn a");

    assert!(next.time_of("started.0") >= slow.time_of("first_line.stamp"), "b started after a's first line");
}
