//! Turns on a Claude Code provider (SME-430), against the fake CLI
//! (`claude_cli::tests::fake`): the test binary run again as `claude`, so
//! no test runs the real CLI or reaches Anthropic.

use super::*;
use crate::claude_cli::tests::fake::{FakeCli, api_call, fixture_lines, is_interrupt, is_user, stream, text_block, text_reply, tool_reply};
use serde_json::json;

/// The usage rows of `conversation_id`'s calls: (input, cache read, cache
/// creation, output).
async fn usage_rows(pool: &PgPool, conversation_id: i64) -> Vec<(i64, i64, i64, i64)> {
    sqlx::query_as(
        "SELECT input_tokens, cache_read_input_tokens, cache_creation_input_tokens, output_tokens
         FROM model_call_usage WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
    .expect("the recorded calls")
}

async fn saved(pool: &PgPool, conversation_id: i64) -> Vec<(String, Vec<anthropic::ContentBlock>)> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("messages")
        .into_iter()
        .map(|m| {
            let blocks = m.blocks().expect("blocks");
            (m.role, blocks)
        })
        .collect()
}

/// The user messages process `n` was sent, as text.
fn user_texts(fake: &FakeCli, n: usize) -> Vec<String> {
    fake.stdin(n).iter().filter(|line| is_user(line)).map(|line| line["message"]["content"].as_str().unwrap_or_default().to_string()).collect()
}

/// The resume capture's reply (thinking, then "value-hang") and the lines
/// around it, as the fake's step after the prompt is read.
fn captured_thinking_reply() -> Value {
    let lines = fixture_lines("resume");
    json!({"emit": lines})
}

use serde_json::Value;

/// Part 1, the stream mapping: a reply's thinking (empty text, its
/// signature kept) and text are saved as one assistant message, its usage
/// recorded once, and the process ran with the plan's arguments and the
/// allowlist alone.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claude_code_turn_saves_thinking_and_text_and_its_usage(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("thinking", vec![vec![json!({"read": true}), captured_thinking_reply()]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn runs");

    let saved = saved(&pool, conversation.id).await;
    assert_eq!(saved.len(), 2, "{saved:?}");
    assert_eq!(
        saved[1],
        (
            "assistant".to_string(),
            vec![
                anthropic::ContentBlock::Thinking { thinking: String::new(), signature: "fixture-signature".to_string() },
                anthropic::ContentBlock::Text { text: "value-hang".to_string() },
            ]
        )
    );
    // The capture's own counts: message_start's input and cache, the
    // final message_delta's output.
    assert_eq!(usage_rows(&pool, conversation.id).await, vec![(4, 900, 160, 48)]);
    assert_eq!(user_texts(&fake, 0), vec!["hello".to_string()], "a new conversation's message goes as it is");
    let args = fake.args(0);
    for expected in ["-p", "--no-session-persistence", "--disable-slash-commands", "--strict-mcp-config", "--system-prompt-file"] {
        assert!(args.iter().any(|a| a == expected), "{expected} in {args:?}");
    }
    let model = args.iter().position(|a| a == "--model").and_then(|i| args.get(i + 1));
    assert_eq!(model.map(String::as_str), Some("claude-opus-5-5"));
    let env = fake.env_names(0);
    assert!(env.iter().any(|name| name == "DISABLE_COMPACT"), "{env:?}");
    assert!(env.iter().any(|name| name == "CLAUDE_CODE_MAX_OUTPUT_TOKENS"), "{env:?}");
    assert!(!env.iter().any(|name| name.starts_with("ANTHROPIC_") || name == "DATABASE_URL"), "nothing of smelt's: {env:?}");
}

/// Part 1, the bridge and the tool loop: the model's call reaches smelt
/// under smelt's name, smelt runs it, and its result answers the CLI's
/// parked MCP call; the second API call is the same process's, sent
/// nothing more on stdin.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tool_call_runs_in_smelt_and_its_result_answers_the_parked_call(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut steps = vec![json!({"read": true})];
    steps.extend(tool_reply(&[("toolu_a", "todoread", json!({})), ("toolu_b", "todoread", json!({}))]));
    steps.push(text_reply("Nothing to do."));
    let fake = FakeCli::new("loop", vec![steps]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn runs");

    let saved = saved(&pool, conversation.id).await;
    assert_eq!(saved.len(), 4, "{saved:?}");
    let names: Vec<&str> = saved[1].1.iter().filter_map(|b| match b {
        anthropic::ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
        _ => None,
    }).collect();
    assert_eq!(names, ["todoread", "todoread"], "smelt's own names, not mcp__smelt__");
    let results: Vec<(String, String)> = saved[2].1.iter().filter_map(|b| match b {
        anthropic::ContentBlock::ToolResult { tool_use_id, content, .. } => Some((tool_use_id.clone(), content.clone())),
        _ => None,
    }).collect();
    let answered: Vec<(String, String)> = fake.results(0).iter().map(|r| (r["id"].as_str().unwrap_or_default().to_string(), r["text"].as_str().unwrap_or_default().to_string())).collect();
    assert_eq!(answered, results, "each parked call got smelt's result, one at a time");
    assert!(matches!(&saved[3].1[..], [anthropic::ContentBlock::Text { text }] if text == "Nothing to do."));
    assert_eq!(fake.started(), 1, "one process for the turn");
    assert_eq!(user_texts(&fake, 0).len(), 1, "the results went through the bridge, not stdin");
    assert_eq!(usage_rows(&pool, conversation.id).await.len(), 2, "one row per API call");
}

/// Part 1, a new run per turn: the next turn's process gets smelt's
/// history as text before the new message.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_turn_starts_a_run_from_smelts_history(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("turns", vec![vec![json!({"read": true}), text_reply("Hi there.")], vec![json!({"read": true}), text_reply("Again.")]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    run_turn(&pool, conversation.id, hello()).await.expect("turn 2");

    assert_eq!(fake.started(), 2);
    let first = user_texts(&fake, 1);
    assert_eq!(first.len(), 1);
    assert!(first[0].starts_with(crate::claude_cli::render::REHYDRATED), "{}", first[0]);
    assert!(first[0].contains("### assistant\nHi there.") && first[0].ends_with("### The new message\nhello"), "{}", first[0]);
}

/// Part 1, ask_user: the turn ends with the question's call parked, so
/// the run is interrupted (the CLI records the call as stopped); the
/// answer's turn starts a new run, whose history carries the answer.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ask_user_interrupts_the_run_and_the_answer_reaches_the_next(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let ask = json!({"questions": [{"question": "Delete the build directory?", "header": "Delete", "options": [{"label": "Yes"}, {"label": "No"}]}]});
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_ask", crate::questions::ASK_USER, ask)]));
    let fake = FakeCli::new("ask", vec![first, vec![json!({"read": true}), text_reply("Deleting.")]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    fake.until("the run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;

    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers).await.expect("answered");
    fake.until("the answer's run is sent its message", |f| !user_texts(f, 1).is_empty()).await;
    let sent = &user_texts(&fake, 1)[0];
    assert!(sent.contains("[The result of tool call toolu_ask:]\nThe user answered:\n1. Delete: Yes"), "{sent}");
}

/// Part 1, Stop while the CLI streams: the run is interrupted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_mid_reply_interrupts_the_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let partial = api_call(10, vec![text_block("Thinking about it")], "end_turn", 1);
    let fake = FakeCli::new("stop", vec![vec![json!({"read": true}), json!({"emit": partial[..3]}), json!({"await_interrupt": true})]]);
    fake.pick_for(&pool, conversation.id).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    fake.until("the prompt is sent", |f| !user_texts(f, 0).is_empty()).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_turn_now(conversation.id);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await.expect("the turn ends").expect("join");
    assert!(ended.expect_err("stopped").to_string().contains(TURN_STOPPED));
    fake.until("the run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;
}

/// The S6 path: a reply cut off at its cap is the turn's, as on HTTP, and
/// the CLI's own continuation is interrupted at once, its usage recorded
/// as a call of its own.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_reply_interrupts_the_clis_own_continuation(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // The capture's first API call, to its message_stop, then the start
    // of the continuation it made on its own.
    let lines = fixture_lines("capped");
    let stop = lines.iter().position(|l| l["event"]["type"] == "message_stop").expect("a message_stop");
    let continuation = lines[stop..].iter().find(|l| l["event"]["type"] == "message_start").cloned().expect("the continuation");
    let fake = FakeCli::new("capped", vec![vec![
        json!({"read": true}),
        json!({"emit": lines[..=stop]}),
        json!({"emit": [continuation]}),
        json!({"await_interrupt": true}),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends on its cut-off reply");

    assert!(fake.stdin(0).iter().any(is_interrupt), "the continuation was interrupted");
    let saved = saved(&pool, conversation.id).await;
    assert!(saved.iter().any(|(role, blocks)| role == "assistant" && matches!(&blocks[..], [anthropic::ContentBlock::Thinking { .. }])), "{saved:?}");
    for _ in 0..100 {
        if usage_rows(&pool, conversation.id).await.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let rows = usage_rows(&pool, conversation.id).await;
    assert_eq!(rows.len(), 2, "the reply and the continuation: {rows:?}");
    assert!(rows.iter().any(|row| row.3 == 40), "the cut-off reply's output: {rows:?}");
    assert!(rows.iter().any(|row| row.3 == 0 && row.0 > 0), "the continuation, stopped before it wrote: {rows:?}");
}

/// Errors: a refused sign-in says how to sign in.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_sign_in_fails_the_turn_saying_how_to_sign_in(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("auth", vec![vec![json!({"read": true}), json!({"emit": fixture_lines("bad_token")}), json!({"exit": 1})]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("signed out");
    assert!(error.to_string().contains(crate::claude_cli::errors::NOT_SIGNED_IN), "{error}");
}

/// Errors: a CLI that says nothing is stopped, and the turn says so.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_silent_cli_times_out(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("silent", vec![vec![json!({"read": true}), json!({"emit": [stream(json!({"type": "ping"}))]}), json!({"hang": true})]]);
    fake.pick_for(&pool, conversation.id).await;
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(500, std::sync::atomic::Ordering::SeqCst);
    let ended = run_turn(&pool, conversation.id, hello()).await;
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);
    let error = ended.expect_err("silent");
    assert!(error.to_string().contains("said nothing for"), "{error}");
}

/// Usage: the context view says a Claude Code conversation's cost is
/// API-equivalent, paid by the subscription; another kind's isn't.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_context_view_says_the_subscription_pays(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let other = db::create_conversation(&pool).await.expect("another conversation");
    start_mock_upstream(&pool, vec![text_reply_body("hi")]).await;
    let fake = FakeCli::new("context", vec![]);
    fake.pick_for(&pool, conversation.id).await;

    let detail = crate::api::chat::context_detail(&pool, conversation.id).await.expect("the detail");
    assert!(detail.subscription, "a Claude Code conversation");
    let detail = crate::api::chat::context_detail(&pool, other.id).await.expect("the detail");
    assert!(!detail.subscription, "an HTTP provider's");
}
