//! Turns on a Claude Code provider: the cases PR #291's round-1 review
//! (SME-431) added.

use super::*;

/// The `xmin` of this provider's model row: it changes on any write.
async fn row_version(pool: &PgPool, provider_id: i64) -> String {
    sqlx::query_scalar("SELECT xmin::text FROM provider_models WHERE provider_id = $1 AND model = 'claude-opus-5-5'")
        .bind(provider_id)
        .fetch_one(pool)
        .await
        .expect("the model row")
}

/// A reply whose `result` reports `window` for the model.
fn reply_reporting(window: i64) -> Value {
    let mut lines = api_call(10, vec![text_block("hi")], "end_turn", 3);
    let mut result = crate::claude_cli::tests::fake::result_ok();
    result["modelUsage"] = json!({"claude-opus-5-5": {"contextWindow": window, "maxOutputTokens": 64_000}});
    lines.push(result);
    json!({"emit": lines})
}

/// Whether `rx` has had `ProvidersChanged` since it was last drained.
fn providers_changed(rx: &mut tokio::sync::broadcast::Receiver<crate::events::AppEvent>) -> bool {
    let mut seen = false;
    while let Ok(event) = rx.try_recv() {
        seen |= event == crate::events::AppEvent::ProvidersChanged;
    }
    seen
}

/// L3: a deleted conversation's CLI files stay gone though its last run
/// was still ending: that process writes its session 1.5 s after its turn,
/// and the delete waits for it to end before removing the files.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deleted_conversations_files_dont_come_back(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("r1-l3", vec![vec![json!({"read": true}), text_reply("one"), json!({"sleep_ms": 1_500}), json!({"write_session": true})]]);
    fake.pick_for(&pool, conversation.id).await;
    run_turn(&pool, conversation.id, hello()).await.expect("a turn");
    let session = db::get_claude_cli_session(&pool, conversation.id).await.expect("read").expect("a session").session_id;
    let file = super::review3::session_file(conversation.id, &session);

    crate::agents::delete_subtree(&pool, conversation.id).await.expect("delete the conversation");
    fake.until("the run's process ends", |f| f.dir.join("exited.0").exists()).await;

    assert!(!file.exists(), "the session file the ending run wrote is gone too");
}

/// A thinking block as the CLI streams one: no text, a signature.
fn thinking_block() -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "signature_delta", "signature": "c2ln"}}),
    ]
}

/// A tool loop whose reply filled the window (65,536) past its ceiling,
/// then `summary`'s steps as the summary's one-shot process.
fn full_loop_then(summary: Vec<Value>) -> Vec<Vec<Value>> {
    let tool = crate::claude_cli::tests::fake::tool_block("toolu_r1", "todoread", &json!({}));
    vec![
        vec![json!({"read": true}), json!({"emit": api_call(50_000, vec![tool], "tool_use", 5)}), json!({"call": [{"id": "toolu_r1", "name": "mcp__smelt__todoread", "input": {}}]})],
        summary,
        vec![json!({"read": true}), text_reply("unreached")],
    ]
}

/// M1: a summary that comes back with nothing in it, its thinking having
/// spent the cap, isn't saved over the history: the compaction fails, the
/// history stays, and the summary call had room for thinking (`--effort
/// low`, a cap of `SUMMARY_MAX_TOKENS` plus `once::THINKING_ROOM`).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_summary_fails_the_compaction_and_keeps_the_history(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut empty = api_call(30_000, vec![thinking_block()], "max_tokens", 6_144);
    empty.push(crate::claude_cli::tests::fake::result_ok());
    let fake = FakeCli::new("r1-m1", full_loop_then(vec![json!({"read": true}), json!({"emit": empty})]));
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 65_536).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the compaction fails").to_string();

    assert!(error.contains("summary"), "{error}");
    assert_eq!(fake.started(), 2, "the run and the summary, no new session");
    let saved = saved(&pool, conversation.id).await;
    assert!(!saved.iter().flat_map(|(_, b)| b).any(|b| matches!(b, anthropic::ContentBlock::CompactionSummary { .. })), "nothing replaced the history: {saved:?}");
    let args = fake.args(1);
    let effort = args.iter().position(|a| a == "--effort").and_then(|i| args.get(i + 1)).map(String::as_str);
    assert_eq!(effort, Some("low"), "{args:?}");
    assert_eq!(fake.cap(1), "6144", "the summary's cap, with room to think");
}

/// M1, the other half: a summary cut off at its cap, half written, isn't
/// saved either.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_summary_fails_the_compaction(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut cut = api_call(30_000, vec![thinking_block(), text_block("The user asked for")], "max_tokens", 6_144);
    cut.push(crate::claude_cli::tests::fake::result_ok());
    let fake = FakeCli::new("r1-m1-cut", full_loop_then(vec![json!({"read": true}), json!({"emit": cut})]));
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 65_536).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the compaction fails").to_string();

    assert!(error.contains("summary"), "{error}");
    let saved = saved(&pool, conversation.id).await;
    assert!(!saved.iter().flat_map(|(_, b)| b).any(|b| matches!(b, anthropic::ContentBlock::CompactionSummary { .. })), "{saved:?}");
}

/// L1: a one-shot call cut off at its cap is stopped at once: the CLI's
/// own continuation (here, a call that would stamp after 2 s) never runs,
/// and the call returns without waiting out `WIND_DOWN`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_one_shot_is_stopped_at_once(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("r1-l1", vec![vec![
        json!({"read": true}),
        json!({"emit": api_call(100, vec![text_block("Half a summ")], "max_tokens", 6_144)}),
        json!({"sleep_ms": 2_000}),
        json!({"stamp": "continued"}),
        json!({"emit": api_call(200, vec![text_block("ary.")], "end_turn", 3)}),
        json!({"emit": [crate::claude_cli::tests::fake::result_ok()]}),
    ]]);
    fake.pick_for(&pool, conversation.id).await;
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("the model");
    let mut request = super::review1::cli_request(vec![super::review1::said("user", "summarize")], 2_048);
    request.tools.clear();
    let meter = anthropic::stream::UsageMeter::default();
    let mut relay = |_: &str| {};

    let started = std::time::Instant::now();
    let turn = crate::claude_cli::once::once(conversation.id, &model, &request, &mut relay, &meter).await.expect("the cut-off reply");
    let took = started.elapsed();

    assert_eq!(turn.stop_reason, "max_tokens");
    assert!(took < std::time::Duration::from_millis(1_500), "returned at once: {took:?}");
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    assert!(!fake.dir.join("continued.stamp").exists(), "its continuation never ran");
}

/// L5: a window the CLI reports tells the app (the providers page and
/// pickers refresh), and the same window again writes nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reported_window_tells_the_app_and_isnt_rewritten(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("r1-l5", vec![
        vec![json!({"read": true}), reply_reporting(200_000)],
        vec![json!({"read": true}), reply_reporting(200_000)],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    let mut app = crate::events::subscribe_app();

    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    assert!(providers_changed(&mut app), "the new window tells the app");
    let version = row_version(&pool, provider).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2");
    assert_eq!(fake.started(), 2, "a run each turn, each reporting the window");
    assert_eq!(row_version(&pool, provider).await, version, "the same window writes nothing");
}
