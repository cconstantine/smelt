//! Turns on a Claude Code provider: the cases PR #283's round-2 review
//! (SME-430) added.

use super::review1::{one_call, said};
use crate::claude_cli::tests::fake::fixture_lines;
use super::*;

/// Review 2 M3: near the window each call's cap shrinks with the room
/// left; a run launched with a larger cap is kept (smelt's own room check
/// governs the reply), so the turn isn't re-sent uncached at every call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_shrinking_cap_keeps_the_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("shrinking", vec![vec![json!({"read": true}), text_reply("one"), json!({"read": true}), text_reply("two")], vec![json!({"read": true}), text_reply("unwanted")]]);
    fake.pick_for(&pool, conversation.id).await;
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("the model");
    let mut cli = crate::claude_cli::run::TurnCli::default();
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(3_000, std::sync::atomic::Ordering::SeqCst);
    let first = one_call(conversation.id, &model, &mut cli, vec![said("user", &"long ".repeat(1_000))], 1_000).await;
    let grown = vec![said("user", &"long ".repeat(1_000)), said("assistant", "one"), said("user", &"longer ".repeat(1_000))];
    let second = one_call(conversation.id, &model, &mut cli, grown, 800).await;
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);
    first.expect("the first call");
    second.expect("the second call");
    assert_eq!(fake.started(), 1, "one process for both calls");
}

/// Review 2 L12: the reply's first call can reach the bridge before
/// smelt has read the reply's block from stdout; it's held until the
/// reply's `message_stop`, not refused as a stray.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_before_its_block_is_read_is_the_replys(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let reply = tool_reply(&[("toolu_early", "todoread", json!({}))]);
    let fake = FakeCli::new("early", vec![vec![
        json!({"read": true}),
        json!({"call_early": {"id": "toolu_early", "name": "mcp__smelt__todoread", "input": {}}}),
        json!({"sleep_ms": 300}),
        reply[0].clone(),
        json!({"join": true}),
        text_reply("Done."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn runs");

    let results = fake.results(0);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["is_error"], false, "the reply's own call got its result: {results:?}");
}

/// Review 2 L13: the request-built refusal is reachable. A conversation
/// whose compactions left it tight (on an API provider, at the same
/// window) and that then moves to Claude Code passes the proactive check
/// with nothing measured since, and is caught only once its request is
/// built: it fails plainly there too, starting no CLI.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tight_conversation_fails_plainly_when_its_request_is_built(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("tight", vec![vec![json!({"read": true}), text_reply("unreached")]]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    let window = 40_000;
    super::review1::set_cli_window(&pool, provider, window).await;
    // Two compactions, each measured near the window: tight.
    let near = anthropic::TokenUsage { input_tokens: 25_000, ..Default::default() };
    crate::turn::state::with_compaction_progress(conversation.id, |p| {
        for _ in 0..2 {
            p.compacted(window as u32);
            p.measured(&near, window as u32);
        }
        assert!(p.room_only_at(window as u32), "the progress is tight");
    });
    let block = [anthropic::ContentBlock::Text { text: "word ".repeat(30_000) }];
    db::create_message(&pool, conversation.id, "user", &block).await.expect("history");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("it doesn't fit").to_string();
    assert!(error.contains(crate::claude_cli::CANT_COMPACT_YET), "{error}");
    assert_eq!(fake.started(), 0, "no CLI ran");
}

/// Review 2 L15, against the real CLI (capture `pending`): with smelt's
/// MCP server not answering, the CLI waits about 30 s, then starts with
/// the server `pending` and none of its tools, and would run the turn
/// without them. That fails the call. An init that says `pending` but
/// lists smelt's tools (the server came up as it started) runs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pending_bridge_fails_only_without_its_tools(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let captured = fixture_lines("pending").into_iter().find(|l| l["subtype"] == "init").expect("the capture's init");
    assert_eq!(captured["mcp_servers"][0]["status"], "pending", "the real CLI reports pending");
    let with_tools = json!({"type": "system", "subtype": "init", "tools": ["mcp__smelt__todoread"], "mcp_servers": [{"name": "smelt", "status": "pending"}]});
    let fake = FakeCli::new("pending", vec![
        vec![json!({"read": true}), json!({"emit": [captured]}), text_reply("No tools.")],
        vec![json!({"read": true}), json!({"emit": [with_tools]}), text_reply("With tools.")],
    ]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("no tools").to_string();
    assert!(error.contains("couldn't connect to smelt's tools"), "{error}");
    run_turn(&pool, conversation.id, hello()).await.expect("smelt's tools are listed");
}
