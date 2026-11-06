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

/// A fixture's lines without its `init`: one captured with no tools names
/// no smelt server, which a turn with tools refuses (review 1 L7).
fn without_init(lines: Vec<Value>) -> Vec<Value> {
    lines.into_iter().filter(|l| !(l["type"] == "system" && l["subtype"] == "init")).collect()
}

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
    // Review 1 M2: argv is readable by every user on the host; the
    // bridge's token goes in a file only the server's user can read.
    let args = fake.args(0);
    assert!(!args.iter().any(|a| a.contains("Bearer") || a.contains("Authorization")), "no token on the command line: {args:?}");
    let config = args.iter().position(|a| a == "--mcp-config").and_then(|i| args.get(i + 1)).expect("an --mcp-config");
    assert!(config.ends_with(".json") && !config.contains('{'), "a file's path: {config}");
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
    assert!(sent.contains("(smelt ran `ask_user` (call toolu_ask); it returned:)\nThe user answered:\n1. Delete: Yes"), "{sent}");
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
    // Captured with no tools, so its init names no smelt server; smelt's
    // turns always have tools, and an init without the bridge fails (L7).
    let lines = without_init(fixture_lines("capped"));
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
    let fake = FakeCli::new("auth", vec![vec![json!({"read": true}), json!({"emit": without_init(fixture_lines("bad_token"))}), json!({"exit": 1})]]);
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

/// The prompts dir's files for `conversation_id`'s runs.
fn run_files(conversation_id: i64) -> Vec<std::path::PathBuf> {
    let env = crate::claude_cli::cli_env(|name| std::env::var_os(name));
    let Some((_, dir)) = env.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR") else {
        return Vec::new();
    };
    let prefix = format!("{conversation_id}-");
    std::fs::read_dir(std::path::Path::new(dir).join("prompts"))
        .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix))).collect())
        .unwrap_or_default()
}

/// Review 1 L6: a turn stopped while its first message is still being
/// written (a CLI that doesn't read, and a message bigger than the pipe)
/// leaves no prompt or MCP config file behind.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_run_dropped_before_it_starts_leaves_no_files(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("unread", vec![vec![json!({"hang": true})]]);
    fake.pick_for(&pool, conversation.id).await;
    let big = anthropic::AnthropicMessage { role: "user".to_string(), content: vec![anthropic::ContentBlock::Text { text: "x".repeat(400_000) }] };

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, big).await }
    });
    fake.until("the process starts", |f| f.started() == 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!run_files(conversation.id).is_empty(), "the run's files exist while it starts");
    stop_turn_now(conversation.id);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await.expect("the turn ends");
    for _ in 0..50 {
        if run_files(conversation.id).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(run_files(conversation.id), Vec::<std::path::PathBuf>::new());
}

/// Review 1 M1: a CLI that dies while smelt runs the reply's tool doesn't
/// fail the turn: the next call starts a new run, whose history carries
/// the tool's result as text.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cli_that_dies_mid_tool_is_replaced_by_a_new_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let calls = tool_reply(&[("toolu_gone", "todoread", json!({}))]);
    let fake = FakeCli::new("dies", vec![
        vec![json!({"read": true}), calls[0].clone(), json!({"exit": 137})],
        vec![json!({"read": true}), text_reply("Carried on.")],
    ]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn carries on");

    assert_eq!(fake.started(), 2, "a second process");
    let sent = &user_texts(&fake, 1)[0];
    assert!(sent.contains("toolu_gone"), "the result goes to the new run as text: {sent}");
    let saved = saved(&pool, conversation.id).await;
    assert!(matches!(&saved.last().expect("a reply").1[..], [anthropic::ContentBlock::Text { text }] if text == "Carried on."), "{saved:?}");
}

/// Review 1 L1: a `tools/call` that isn't one of the reply's calls, by id
/// or (with none) by name and input, is refused at once rather than left
/// waiting until the run ends; the reply's own call is answered as usual.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_that_isnt_the_replys_is_refused_at_once(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let reply = tool_reply(&[("toolu_real", "todoread", json!({}))]);
    let fake = FakeCli::new("stray", vec![vec![
        json!({"read": true}),
        reply[0].clone(),
        json!({"call": [
            {"id": "toolu_stray", "name": "mcp__smelt__todoread", "input": {}},
            {"id": null, "name": "mcp__smelt__todoread", "input": {"unasked": true}},
            {"id": "toolu_real", "name": "mcp__smelt__todoread", "input": {}},
        ]}),
        text_reply("Done."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(3_000, std::sync::atomic::Ordering::SeqCst);
    let ended = run_turn(&pool, conversation.id, hello()).await;
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);
    ended.expect("the turn runs");

    let results = fake.results(0);
    let refused: Vec<(bool, &str)> = results.iter().map(|r| (r["is_error"] == true, r["text"].as_str().unwrap_or_default())).collect();
    assert_eq!(refused.len(), 3, "{results:?}");
    assert!(refused[0].0 && refused[0].1 == crate::claude_cli::run::NOT_THE_REPLYS, "{results:?}");
    assert!(refused[1].0 && refused[1].1 == crate::claude_cli::run::NOT_THE_REPLYS, "{results:?}");
    assert!(!refused[2].0, "the reply's own call gets its result: {results:?}");
}

/// Review 1 L2: the CLI answering one of the reply's calls itself (a
/// local refusal smelt never heard of) means its context and smelt's went
/// different ways: the call fails, saying so, rather than reading the
/// CLI's next reply as the answer to smelt's results.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_the_cli_answered_itself_fails_the_call(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let reply = tool_reply(&[("toolu_self", "todoread", json!({}))]);
    let own = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_self", "is_error": true, "content": "refused locally"}]}});
    let fake = FakeCli::new("itself", vec![vec![json!({"read": true}), reply[0].clone(), json!({"emit": [own]}), text_reply("Unrelated.")]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("diverged");
    assert!(error.to_string().contains("answered tool call toolu_self itself"), "{error}");
    let saved = saved(&pool, conversation.id).await;
    assert!(!saved.iter().any(|(_, blocks)| blocks.iter().any(|b| matches!(b, anthropic::ContentBlock::Text { text } if text == "Unrelated."))), "{saved:?}");
}

/// A request on `conversation_id`'s Claude Code model, with `messages`
/// and a reply cap of `max_tokens`, as the turn builds one.
fn cli_request(messages: Vec<anthropic::AnthropicMessage>, max_tokens: u32) -> anthropic::CreateMessageRequest {
    anthropic::CreateMessageRequest {
        model: "claude-opus-5-5".to_string(),
        max_tokens,
        system: Some("You are smelt.".to_string()),
        messages,
        stream: true,
        // One of smelt's tools, so the run has the bridge.
        tools: vec![anthropic::ToolDefinition { name: "todoread".to_string(), description: "Reads the todo list.".to_string(), input_schema: json!({"type": "object"}) }],
        thinking: None,
        output_config: None,
        chat_template_kwargs: None,
        prompt_caching: false,
    }
}

fn said(role: &str, text: &str) -> anthropic::AnthropicMessage {
    anthropic::AnthropicMessage { role: role.to_string(), content: vec![anthropic::ContentBlock::Text { text: text.to_string() }] }
}

/// One model call through `cli`.
async fn one_call(
    conversation_id: i64,
    model: &crate::providers::TurnModel,
    cli: &mut crate::claude_cli::run::TurnCli,
    messages: Vec<anthropic::AnthropicMessage>,
    cap: u32,
) -> Result<Vec<anthropic::ContentBlock>, String> {
    let request = cli_request(messages, cap);
    let (meter, extra) = (anthropic::stream::UsageMeter::default(), anthropic::stream::UsageMeter::default());
    let mut relay = |_: &str| {};
    crate::claude_cli::run::call(conversation_id, model, &request, &mut relay, &meter, &extra, cli).await.map(|turn| turn.content)
}

/// Review 1 L3, and the history going another way mid-turn: a run is
/// reused only for the cap it was started with (the CLI takes it at
/// launch) and a history that carries on from its last call; either
/// change starts a new run, given the history as text.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_new_cap_or_another_history_starts_a_new_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("relaunch", vec![
        vec![json!({"read": true}), text_reply("one"), json!({"read": true}), text_reply("two")],
        vec![json!({"read": true}), text_reply("three")],
        vec![json!({"read": true}), text_reply("four")],
    ]);
    fake.pick_for(&pool, conversation.id).await;
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("the model");
    let mut cli = crate::claude_cli::run::TurnCli::default();
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(3_000, std::sync::atomic::Ordering::SeqCst);
    let first = one_call(conversation.id, &model, &mut cli, vec![said("user", "hi")], 1_000).await;
    let same = one_call(conversation.id, &model, &mut cli, vec![said("user", "hi"), said("assistant", "one"), said("user", "again")], 1_000).await;
    let new_cap = one_call(conversation.id, &model, &mut cli, vec![said("user", "hi"), said("assistant", "one"), said("user", "again"), said("assistant", "two"), said("user", "more")], 900).await;
    let other = one_call(conversation.id, &model, &mut cli, vec![said("user", "edited"), said("assistant", "x"), said("user", "and")], 900).await;
    crate::claude_cli::run::SILENCE_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);
    let text = |r: Result<Vec<anthropic::ContentBlock>, String>| match r.expect("a reply").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => format!("{other:?}"),
    };
    assert_eq!([text(first), text(same), text(new_cap), text(other)], ["one", "two", "three", "four"]);
    assert_eq!(fake.started(), 3, "the same run, then a new cap's, then another history's");
    assert_eq!(user_texts(&fake, 0), ["hi", "again"], "the same run is sent only what's new");
    assert!(user_texts(&fake, 1)[0].ends_with("### The new message\nmore"), "{:?}", user_texts(&fake, 1));
    assert!(user_texts(&fake, 2)[0].contains("edited"), "{:?}", user_texts(&fake, 2));
}

/// Review 1 L4: a CLI that exits before it finishes fails the turn, and
/// the error, which the conversation keeps, quotes its stderr only as a
/// short line with paths, emails and tokens taken out.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_exit_before_the_result_quotes_a_scrubbed_line(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let said = "Error: couldn't open /home/dev/.claude/secret.json for me@example.com with sk-ant-oat01-ABCDEFGHIJ";
    let partial = api_call(10, vec![text_block("Starting")], "end_turn", 1);
    let fake = FakeCli::new("stderr", vec![vec![json!({"read": true}), json!({"emit": partial[..3]}), json!({"stderr": said}), json!({"exit": 3})]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("it exited").to_string();
    assert!(error.contains("Claude Code exited (exit status: 3) before it finished"), "{error}");
    assert!(error.contains("couldn't open"), "the line is quoted: {error}");
    for leak in ["/home/dev", "me@example.com", "sk-ant-"] {
        assert!(!error.contains(leak), "{leak} in {error}");
    }
}

/// Review 1 L7: a CLI that couldn't connect to smelt's tools fails the
/// call, rather than running the turn with no tools.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cli_without_smelts_tools_fails_the_call(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let init = json!({"type": "system", "subtype": "init", "tools": [], "mcp_servers": [{"name": "smelt", "status": "failed", "source": "dynamic"}]});
    let fake = FakeCli::new("unbridged", vec![vec![json!({"read": true}), json!({"emit": [init]}), text_reply("No tools.")]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("no tools").to_string();
    assert!(error.contains("couldn't connect to smelt's tools"), "{error}");
}

// --- Review 1's missing tests ---

/// Gives this provider's Claude Code model a window of `window`.
async fn set_cli_window(pool: &PgPool, provider_id: i64, window: i32) {
    sqlx::query("INSERT INTO provider_models (provider_id, model, context_window) VALUES ($1, 'claude-opus-5-5', $2)")
        .bind(provider_id)
        .bind(window)
        .execute(pool)
        .await
        .expect("seed the window");
}

/// A conversation too full for its window ends its turn plainly on this
/// kind, by the proactive check, and calls nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_full_conversation_fails_plainly_by_the_proactive_check(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("full", vec![vec![json!({"read": true}), text_reply("unreached")]]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    set_cli_window(&pool, provider, 32_768).await;
    let usage = anthropic::TokenUsage { input_tokens: 31_000, ..Default::default() };
    db::upsert_conversation_usage(&pool, conversation.id, &usage).await.expect("the last usage");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("it doesn't fit").to_string();
    assert!(error.contains(crate::claude_cli::CANT_COMPACT_YET), "{error}");
    assert_eq!(fake.started(), 0, "no CLI ran");
}

/// Stop while smelt runs the reply's tool (the run Parked): the run is
/// interrupted, so the CLI records the call as stopped, and the call is
/// never answered with a result.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_mid_tool_interrupts_the_parked_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut steps = vec![json!({"read": true})];
    steps.extend(tool_reply(&[("toolu_mid", "todoread", json!({}))]));
    let fake = FakeCli::new("stop-parked", vec![steps]);
    fake.pick_for(&pool, conversation.id).await;
    let (reached, release) = crate::turn::test_hooks::pause_after_reply_saved(conversation.id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached).await.expect("the reply is saved").expect("the hook");
    stop_turn_now(conversation.id);
    drop(release);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await.expect("the turn ends").expect("join");
    assert!(ended.expect_err("stopped").to_string().contains(TURN_STOPPED));
    fake.until("the parked run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;
    assert_eq!(fake.results(0), Vec::<Value>::new(), "the call got no result");
}

/// A call of the reply that the next request has no result for is
/// answered NOT_RUN, the request's other blocks riding on it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_with_no_result_is_answered_not_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut steps = vec![json!({"read": true})];
    steps.extend(tool_reply(&[("toolu_none", "todoread", json!({}))]));
    steps.push(text_reply("ok"));
    let fake = FakeCli::new("not-run", vec![steps]);
    fake.pick_for(&pool, conversation.id).await;
    let model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("the model");
    let mut cli = crate::claude_cli::run::TurnCli::default();
    let first = vec![said("user", "hi")];
    let reply = one_call(conversation.id, &model, &mut cli, first.clone(), 1_000).await.expect("the call");
    let mut next = first;
    next.push(anthropic::AnthropicMessage { role: "assistant".to_string(), content: reply });
    next.push(said("user", "a notice"));
    one_call(conversation.id, &model, &mut cli, next, 1_000).await.expect("the next call");

    let results = fake.results(0);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["is_error"], true);
    let text = results[0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with(crate::claude_cli::run::NOT_RUN) && text.ends_with("a notice"), "{text}");
}

/// SME-343: a Claude Code turn holds its provider's one request slot for
/// the whole turn, its tool pass included.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claude_code_turn_holds_its_slot_across_its_tools(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut steps = vec![json!({"read": true})];
    steps.extend(tool_reply(&[("toolu_slot", "todoread", json!({}))]));
    steps.push(text_reply("Done."));
    let fake = FakeCli::new("slot", vec![steps]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    sqlx::query("UPDATE inference_providers SET max_concurrent_requests = 1 WHERE id = $1").bind(provider).execute(&pool).await.expect("one slot");
    let (reached, release) = crate::turn::test_hooks::pause_after_reply_saved(conversation.id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached).await.expect("the reply is saved").expect("the hook");
    let pool_now = crate::providers::limiter::current(provider).expect("the provider's pool");
    assert_eq!(pool_now.available_permits(), 0, "the turn holds the one slot between its calls");
    let _ = release.send(());
    tokio::time::timeout(std::time::Duration::from_secs(10), turn).await.expect("the turn ends").expect("join").expect("the turn runs");
    assert_eq!(crate::providers::limiter::current(provider).expect("the pool").available_permits(), 1, "and gives it up at its end");
}

/// The subscription's limit reaches the turn's error, saying which limit
/// and when it resets. Synthesized: no capture of a real limit exists.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reached_limit_fails_the_turn_saying_when_it_resets(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let limit = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "seven_day", "resetsAt": 1_767_225_600}});
    let refused = json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 429, "result": "API Error: 429 rate limited"});
    let fake = FakeCli::new("limit", vec![vec![json!({"read": true}), json!({"emit": [limit, refused]}), json!({"exit": 1})]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("limited").to_string();
    assert!(error.contains("usage limit (seven-day) is reached. It resets at 2026-01-01 00:00 UTC."), "{error}");
}
