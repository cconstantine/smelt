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

/// Review 3 M2: an API error before the reply begins (here the
/// subscription limit) leaves the session as it was: the call fails
/// without starting another process, and the next turn resumes it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_limit_before_the_reply_keeps_the_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let limit = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1_767_225_600}});
    let refused = json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 429, "result": "API Error: 429 rate limited"});
    let fake = FakeCli::new("limit-kept", vec![
        vec![json!({"read": true}), text_reply("one")],
        vec![json!({"read": true}), json!({"emit": [limit, refused]}), json!({"exit": 1})],
        vec![json!({"read": true}), text_reply("three")],
    ]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    let session = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("a session").session_id;

    run_turn(&pool, conversation.id, hello()).await.expect_err("limited");
    assert_eq!(fake.started(), 2, "no second process for the limited call");
    run_turn(&pool, conversation.id, hello()).await.expect("turn 3");

    let args = fake.args(2);
    let resumed = args.iter().position(|a| a == "--resume").and_then(|i| args.get(i + 1));
    assert_eq!(resumed, Some(&session), "turn 3 resumes the session: {args:?}");
}

/// Review 3 L16: L12's case on a loop's second reply. Once the first
/// reply's results are all in, the second reply's first call can reach
/// the bridge before smelt reads that reply's start; it's held, not
/// refused.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_second_replys_early_call_is_held(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let first = tool_reply(&[("toolu_a", "todoread", json!({}))]);
    let second = tool_reply(&[("toolu_b", "todoread", json!({}))]);
    let fake = FakeCli::new("early-second", vec![vec![
        json!({"read": true}),
        first[0].clone(),
        first[1].clone(),
        json!({"call_early": {"id": "toolu_b", "name": "mcp__smelt__todoread", "input": {}}}),
        json!({"sleep_ms": 300}),
        second[0].clone(),
        json!({"join": true}),
        text_reply("Done."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn runs");

    let results = fake.results(0);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results.iter().all(|r| r["is_error"] == false), "both calls got their results: {results:?}");
}

/// Review 3 L17: a run that resumes a session waits for that
/// conversation's last process to end, so two are never in one session:
/// here L3's restart for a larger cap, whose old run takes 1.5 s to go.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_resume_waits_for_the_last_process_to_end(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("one-at-once", vec![
        vec![json!({"read": true}), text_reply("one"), json!({"sleep_ms": 1_500})],
        vec![json!({"read": true}), text_reply("two")],
    ]);
    fake.pick_for(&pool, conversation.id).await;
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("the model");
    let mut cli = crate::claude_cli::run::TurnCli::default();
    super::review1::one_call(&pool, conversation.id, &model, &mut cli, vec![super::review1::said("user", "hi")], 1_000).await.expect("call 1");
    let next = vec![super::review1::said("user", "hi"), super::review1::said("assistant", "one"), super::review1::said("user", "more")];
    super::review1::one_call(&pool, conversation.id, &model, &mut cli, next, 1_200).await.expect("call 2");

    assert!(fake.args(1).iter().any(|a| a == "--resume"), "{:?}", fake.args(1));
    fake.until("the first process ends", |f| f.dir.join("exited.0").exists()).await;
    assert!(fake.time_of("started.1") >= fake.time_of("exited.0"), "the resume started after the last process ended");
}

/// Review 3 L18: a CLI that never reads its input holds no one else's
/// start: the start lock goes at its deadline from a timer, and the write
/// of its first message times out, failing only its own turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cli_that_wont_read_blocks_only_its_own_turn(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let (a, b) = (db::create_conversation(&pool).await.expect("a"), db::create_conversation(&pool).await.expect("b"));
    let deaf = FakeCli::new("deaf", vec![vec![json!({"hang": true})]]);
    let next = FakeCli::new("after-deaf", vec![vec![json!({"read": true}), text_reply("B")]]);
    deaf.pick_for(&pool, a.id).await;
    next.pick_for(&pool, b.id).await;
    use std::sync::atomic::Ordering::SeqCst;
    crate::claude_cli::run::LAUNCH_HOLD_FOR_TESTS.store(500, SeqCst);
    crate::claude_cli::run::WRITE_TIMEOUT_FOR_TESTS.store(3_000, SeqCst);
    let big = anthropic::AnthropicMessage { role: "user".to_string(), content: vec![anthropic::ContentBlock::Text { text: "x".repeat(400_000) }] };

    let stuck = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, a.id, big).await }
    });
    deaf.until("the deaf process starts", |f| f.started() == 1).await;
    let other = tokio::time::timeout(std::time::Duration::from_secs(2), run_turn(&pool, b.id, hello())).await;
    let stuck = tokio::time::timeout(std::time::Duration::from_secs(10), stuck).await;
    crate::claude_cli::run::LAUNCH_HOLD_FOR_TESTS.store(0, SeqCst);
    crate::claude_cli::run::WRITE_TIMEOUT_FOR_TESTS.store(0, SeqCst);

    other.expect("b's turn isn't held behind a's start").expect("b's turn runs");
    let error = stuck.expect("a's turn ends").expect("join").expect_err("a's write times out").to_string();
    assert!(error.contains("didn't take its input"), "{error}");
}

/// The conversation's session files, as the CLI would keep them.
pub(super) fn session_file(conversation_id: i64, session_id: &str) -> std::path::PathBuf {
    let env = crate::claude_cli::cli_env(|name| std::env::var_os(name));
    let config = std::path::PathBuf::from(env.into_iter().find(|(n, _)| n == "CLAUDE_CONFIG_DIR").expect("a config dir").1);
    let work = crate::claude_cli::files::work_dir(&config, conversation_id);
    crate::claude_cli::files::sessions_dir(&config, &work).join(format!("{session_id}.jsonl"))
}

/// Review 3 L20: a replaced session's file goes at once, and a deleted
/// conversation's sessions and working directory go with it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversations_cli_files_go_with_it(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("files", vec![vec![json!({"read": true}), text_reply("one")], vec![json!({"read": true}), text_reply("two")]]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    let old = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("a session").session_id;
    let old_file = session_file(conversation.id, &old);
    std::fs::create_dir_all(old_file.parent().expect("a dir")).expect("the sessions dir");
    std::fs::write(&old_file, "{}").expect("a session file");
    let first = db::list_messages(&pool, conversation.id).await.expect("messages")[0].id;
    sqlx::query("UPDATE messages SET content = $1 WHERE id = $2").bind(r#"[{"type":"text","text":"edited"}]"#).bind(first).execute(&pool).await.expect("edit");

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2, a new session");
    assert!(!old_file.exists(), "the replaced session's file is gone");
    let new = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("a session").session_id;
    let new_file = session_file(conversation.id, &new);
    std::fs::write(&new_file, "{}").expect("a session file");
    let work = new_file.parent().and_then(|p| p.parent()).expect("projects").parent().expect("the config dir").join("work").join(conversation.id.to_string());
    assert!(work.exists(), "the working dir is there");

    crate::agents::delete_subtree(&pool, conversation.id).await.expect("delete the conversation");
    assert!(!new_file.exists() && !new_file.parent().expect("dir").exists(), "its sessions go");
    assert!(!work.exists(), "its working dir goes");
}
