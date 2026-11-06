//! Turns on a Claude Code provider: the cases PR #283's round-1 review
//! (SME-430) added.

use super::*;

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
