//! Turns on a Claude Code provider that smelt compacts (SME-431, SME-399's
//! B2): the summary through a one-shot CLI call, the session reset, and
//! the next call seeded from the summary.

use super::*;

/// How many compaction dividers `conversation_id` has.
async fn dividers(pool: &PgPool, conversation_id: i64) -> usize {
    saved(pool, conversation_id).await.iter().flat_map(|(_, blocks)| blocks).filter(|b| matches!(b, anthropic::ContentBlock::CompactionSummary { .. })).count()
}

/// The kinds of `conversation_id`'s recorded calls, in order.
async fn call_kinds(pool: &PgPool, conversation_id: i64) -> Vec<String> {
    for _ in 0..100 {
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM model_call_usage WHERE conversation_id = $1 ORDER BY id")
            .bind(conversation_id)
            .fetch_all(pool)
            .await
            .expect("the recorded calls");
        if kinds.iter().any(|k| k == "compaction") {
            return kinds;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Vec::new()
}

/// What the summary's one-shot process `n` was, as the plan has it: no
/// session, no tools, no bridge, one turn, smelt's compaction prompt, and
/// the live ids in its one message.
fn assert_summary_call(fake: &FakeCli, n: usize, live: &str) {
    let args = fake.args(n);
    for flag in ["--no-session-persistence", "--strict-mcp-config", "--max-turns"] {
        assert!(args.iter().any(|a| a == flag), "{flag} in {args:?}");
    }
    assert!(!args.iter().any(|a| a == "--mcp-config" || a == "--session-id" || a == "--resume"), "no bridge or session: {args:?}");
    // `args` leaves out empty arguments: the raw list has `--tools ""`.
    let raw = std::fs::read_to_string(fake.dir.join(format!("args.of.{n}"))).expect("the args");
    let raw: Vec<&str> = raw.split('\0').collect();
    let tools = raw.iter().position(|a| *a == "--tools").and_then(|i| raw.get(i + 1)).copied();
    assert_eq!(tools, Some(""), "built-in tools off: {raw:?}");
    assert_eq!(fake.prompt(n), crate::turn::compaction::COMPACTION_SYSTEM_PROMPT, "the compaction prompt");
    let sent = user_texts(fake, n);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains(live), "the live ids reach the summary: {sent:?}");
    assert!(fake.env_names(n).iter().any(|e| e == "DISABLE_COMPACT"), "Claude Code's own compaction stays off");
}

/// A pod and a terminal of `conversation_id`, as the summary must keep
/// them: the line `describe_live_state` writes for the terminal.
async fn live_ids(pool: &PgPool, conversation_id: i64) -> String {
    let pod = db::create_sandbox_pod(pool, conversation_id).await.expect("a pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("a terminal");
    format!("terminal_id {} in pod_id {}", terminal.id, pod.id)
}

/// From Parked: a tool loop whose reply filled the window past its
/// ceiling (65,536 less 16,384) compacts before the next call. The summary
/// is a one-shot process recorded as a `compaction` call; the parked run
/// is interrupted and ended, and the next call is a new session whose
/// first message holds the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_full_tool_loop_compacts_and_goes_on_in_a_new_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let tool = crate::claude_cli::tests::fake::tool_block("toolu_b2", "todoread", &json!({}));
    let fake = FakeCli::new("b2-parked", vec![
        vec![json!({"read": true}), json!({"emit": api_call(50_000, vec![tool], "tool_use", 5)}), json!({"call": [{"id": "toolu_b2", "name": "mcp__smelt__todoread", "input": {}}]})],
        vec![json!({"read": true}), text_reply("SUMMARY-B2")],
        vec![json!({"read": true}), text_reply("after the summary")],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 65_536).await;
    let live = live_ids(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn compacts and carries on");

    assert_eq!(fake.started(), 3, "the run, the summary, the new session");
    assert_summary_call(&fake, 1, &live);
    assert!(fake.stdin(0).iter().any(is_interrupt), "the parked run was interrupted");
    assert!(fake.args(2).iter().any(|a| a == "--session-id"), "a new session: {:?}", fake.args(2));
    let seed = user_texts(&fake, 2);
    assert!(seed[0].contains("SUMMARY-B2"), "the new session starts from the summary: {seed:?}");
    assert_eq!(dividers(&pool, conversation.id).await, 1);
    let kinds = call_kinds(&pool, conversation.id).await;
    assert_eq!(kinds.iter().filter(|k| *k == "compaction").count(), 1, "{kinds:?}");
    let session = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("the new session's row");
    assert!(fake.args(2).contains(&session.session_id), "the row is the new session's");
}

/// From Idle: a reply with nothing in it gets the empty-reply nudge (A8),
/// so the turn calls again on its idle run; that reply's 50,000 tokens are
/// past the ceiling, so it compacts first: the idle run's input is closed
/// and it ends, and the next call is a new session seeded from the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_full_idle_run_compacts_before_its_next_call(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let empty = {
        let mut lines = api_call(50_000, vec![], "end_turn", 3);
        lines.push(crate::claude_cli::tests::fake::result_ok());
        json!({"emit": lines})
    };
    let fake = FakeCli::new("b2-idle", vec![
        vec![json!({"read": true}), empty],
        vec![json!({"read": true}), text_reply("SUMMARY-IDLE")],
        vec![json!({"read": true}), text_reply("the rest")],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 65_536).await;
    let live = live_ids(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn compacts and carries on");

    assert_eq!(fake.started(), 3, "the run, the summary, the new session");
    assert_summary_call(&fake, 1, &live);
    fake.until("the idle run ends", |f| f.dir.join("exited.0").exists()).await;
    assert!(fake.args(2).iter().any(|a| a == "--session-id"), "a new session: {:?}", fake.args(2));
    assert!(user_texts(&fake, 2)[0].contains("SUMMARY-IDLE"), "{:?}", user_texts(&fake, 2));
    assert_eq!(dividers(&pool, conversation.id).await, 1);
}

/// A conversation too full for its window compacts by the proactive
/// check at its next turn: last turn's session (its row, and its file)
/// is replaced by a new one seeded from the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_full_conversation_compacts_at_its_next_turn(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let full = {
        let mut lines = api_call(31_000, vec![text_block("one")], "end_turn", 3);
        lines.push(crate::claude_cli::tests::fake::result_ok());
        json!({"emit": lines})
    };
    let fake = FakeCli::new("b2-next", vec![
        vec![json!({"read": true}), json!({"write_session": true}), full],
        vec![json!({"read": true}), text_reply("SUMMARY-NEXT")],
        vec![json!({"read": true}), text_reply("two")],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 32_768).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    let old = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("turn 1's session").session_id;
    assert!(super::review3::session_file(conversation.id, &old).exists(), "turn 1's session file");

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2 compacts and runs");

    assert_eq!(fake.started(), 3, "turn 1, the summary, turn 2's new session");
    assert!(fake.args(2).iter().any(|a| a == "--session-id"), "a new session: {:?}", fake.args(2));
    assert!(user_texts(&fake, 2)[0].contains("SUMMARY-NEXT"));
    assert!(!super::review3::session_file(conversation.id, &old).exists(), "the replaced session's file went");
    assert_eq!(dividers(&pool, conversation.id).await, 1);
}

/// The window the CLI reports (`result.modelUsage[model].contextWindow`)
/// is learned onto the model row, with its output cap.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_clis_reported_window_is_learned(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut lines = api_call(10, vec![text_block("hi")], "end_turn", 3);
    let mut result = crate::claude_cli::tests::fake::result_ok();
    result["modelUsage"] = json!({"claude-opus-5-5": {"contextWindow": 200_000, "maxOutputTokens": 64_000}});
    lines.push(result);
    let fake = FakeCli::new("b2-window", vec![vec![json!({"read": true}), json!({"emit": lines})]]);
    let provider = fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("a turn");

    let (window, cap): (Option<i32>, Option<i32>) = sqlx::query_as("SELECT reported_context_window, reported_max_output FROM provider_models WHERE provider_id = $1 AND model = 'claude-opus-5-5'")
        .bind(provider)
        .fetch_one(&pool)
        .await
        .expect("the model row");
    assert_eq!((window, cap), (Some(200_000), Some(64_000)));
}
