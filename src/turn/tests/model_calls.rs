use super::*;

/// Every completed model call is recorded with its own usage: the
/// compaction's summary call, then each call of the turn's tool loop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_model_call_of_a_turn_is_recorded_with_its_usage(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let tool_use_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_recording_mock_upstream(&pool, vec![
        with_usage(&text_reply_body("Summary: nothing live."), 5_000, 0, 300),
        with_usage(&tool_use_body, 1_000, 2_000, 40),
        with_usage(&text_reply_body("Done"), 100, 3_000, 7),
    ])
    .await;
    let provider_id: i64 = sqlx::query_scalar("SELECT default_provider_id FROM inference_settings")
        .fetch_one(&pool)
        .await
        .expect("the mock provider");

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let rows: Vec<(Option<i64>, String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT provider_id, model, kind, input_tokens, cache_read_input_tokens, output_tokens
         FROM model_call_usage WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conversation.id)
    .fetch_all(&pool)
    .await
    .expect("the recorded calls");
    let model = crate::providers::test_support::MOCK_MODEL.to_string();
    assert_eq!(
        rows,
        vec![
            (Some(provider_id), model.clone(), "compaction".to_string(), 5_000, 0, 300),
            (Some(provider_id), model.clone(), "turn".to_string(), 1_000, 2_000, 40),
            (Some(provider_id), model.clone(), "turn".to_string(), 100, 3_000, 7),
        ]
    );
    assert_eq!(
        db::get_conversation_usage(&pool, conversation.id).await.expect("usage").map(|u| u.input_tokens),
        Some(100),
        "the last turn call is the conversation's last known usage"
    );
    let spend = context_detail(&pool, conversation.id).await.expect("context detail").spend;
    assert_eq!(
        (spend.calls, spend.input_tokens, spend.cache_read_input_tokens, spend.output_tokens),
        (3, 6_100, 5_000, 347),
        "the detail view sums every call, compaction included"
    );
}

/// Each call is priced from the catalog when it's recorded, as the
/// provider's catalog entry; a provider without one records no cost.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_recorded_call_is_priced_from_the_catalog(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    start_recording_mock_upstream(&pool, vec![with_usage(&text_reply_body("Hi!"), 10_000, 100_000, 1_000)]).await;
    let mut providers = crate::pricing::Providers::new();
    providers.insert(
        "priced".to_string(),
        crate::pricing::CatalogProvider {
            name: "Priced".to_string(),
            api: None,
            models: [(
                crate::providers::test_support::MOCK_MODEL.to_string(),
                crate::pricing::ModelPrices {
                    base: crate::pricing::Prices { input: 4.0, output: 20.0, cache_read: Some(0.2), cache_write: Some(5.0) },
                    tiers: vec![],
                },
            )]
            .into(),
        },
    );
    crate::pricing::CATALOG.set_for_test(providers);

    run_turn(&pool, conversation.id, hello()).await.expect("unpriced turn");
    sqlx::query("UPDATE inference_providers SET price_catalog_provider = 'priced'")
        .execute(&pool)
        .await
        .expect("price the mock provider");
    run_turn(&pool, conversation.id, hello()).await.expect("priced turn");

    let costs: Vec<Option<f64>> =
        sqlx::query_scalar("SELECT cost_usd FROM model_call_usage WHERE conversation_id = $1 ORDER BY id")
            .bind(conversation.id)
            .fetch_all(&pool)
            .await
            .expect("costs");
    // 10k uncached at $4, 100k read at $0.20, 1k out at $20 per million.
    assert_eq!(costs.len(), 2);
    assert_eq!(costs[0], None, "no catalog entry, no cost");
    assert!((costs[1].expect("priced") - 0.08).abs() < 1e-9, "{costs:?}");
}

/// A model's effort goes out as `output_config.effort` on its turns, and
/// nothing goes out while it's unset.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_models_effort_is_sent_on_its_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn without effort");
    sqlx::query("INSERT INTO provider_models (provider_id, model, effort) SELECT id, $1, 'low' FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("set the mock model's effort");
    run_turn(&pool, conversation.id, hello()).await.expect("turn with effort");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("output_config").is_none(), "unset sends none: {}", requests[0]);
    assert_eq!(requests[1]["output_config"], serde_json::json!({"effort": "low"}));
}

/// SME-111: a llama.cpp provider's turn tells the chat template its
/// settings in `chat_template_kwargs`, never as `output_config`, and its
/// compaction turns the template's reasoning off.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_llama_cpp_turn_sends_template_settings_and_compaction_turns_thinking_off(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier message".to_string() }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage { input_tokens: 190_000, ..Default::default() },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi"),
    ])
    .await;
    sqlx::query(
        "UPDATE inference_providers SET kind = 'llama_cpp', keep_reasoning = false,
         server_caps = '{\"template_caps\": {\"supports_reasoning_effort\": true, \"supports_preserve_reasoning\": true}}'",
    )
    .execute(&pool)
    .await
    .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, effort) SELECT id, $1, 'high' FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("set the mock model's effort");

    run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert_eq!(
        requests[0]["chat_template_kwargs"],
        serde_json::json!({"enable_thinking": false, "preserve_thinking": false}),
        "the summary isn't spent on reasoning: {}",
        requests[0]
    );
    assert_eq!(
        requests[1]["chat_template_kwargs"],
        serde_json::json!({"reasoning_effort": "high", "preserve_thinking": false})
    );
    assert!(requests[1].get("output_config").is_none(), "{}", requests[1]);
}

/// SME-111: a turn asks for a reply as long as the conversation has room
/// for: half a llama.cpp model's window at the start, and after a
/// compaction the new, smaller history's room, not the old one's.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turns_reply_budget_grows_with_the_room_left(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier message".to_string() }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage { input_tokens: 250_000, ..Default::default() },
    )
    .await
    .expect("seed usage near the window");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        with_usage(&text_reply_body("Hi"), 40_000, 0, 10),
        text_reply_body("Again"),
    ])
    .await;
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window) SELECT id, $1, 262144 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("size the mock model");

    run_turn(&pool, conversation.id, hello()).await.expect("first turn");
    run_turn(&pool, conversation.id, hello()).await.expect("second turn");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 3, "a compaction, then two turns");
    assert_eq!(requests[0]["max_tokens"], 2048, "the summary's own");
    assert_eq!(requests[1]["max_tokens"], 131_072, "half the window, after compacting: {}", requests[1]["max_tokens"]);
    // 262,144 - (40,010 measured + "hello") - 4,096 of headroom.
    let second = requests[2]["max_tokens"].as_u64().expect("a number");
    assert!((131_072 - 1..=131_072).contains(&second), "still half the window: {second}");
}

/// SME-111: the user's "Max reply tokens" caps a turn's reply budget.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_models_max_reply_tokens_caps_its_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window, max_output) SELECT id, $1, 262144, 8192 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("cap the mock model");
    run_turn(&pool, conversation.id, hello()).await.expect("turn");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 8192);
}

/// SME-111: a llama.cpp model's turn caps its thinking at three quarters
/// of the reply budget, or the model's reasoning budget; an Anthropic
/// one's thinking stays adaptive.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_llama_cpp_turn_sends_a_thinking_budget(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("on Anthropic");
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window) SELECT id, $1, 262144 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("size the mock model");
    run_turn(&pool, conversation.id, hello()).await.expect("automatic budget");
    sqlx::query("UPDATE provider_models SET reasoning_budget = 20000")
        .execute(&pool)
        .await
        .expect("cap its reasoning");
    run_turn(&pool, conversation.id, hello()).await.expect("capped budget");
    sqlx::query("UPDATE provider_models SET thinking = false")
        .execute(&pool)
        .await
        .expect("thinking off");
    run_turn(&pool, conversation.id, hello()).await.expect("thinking off");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["thinking"], serde_json::json!({"type": "adaptive"}));
    let budget = requests[1]["max_tokens"].as_u64().expect("a number");
    assert_eq!(
        requests[1]["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": budget * 3 / 4}),
        "three quarters of {budget}"
    );
    assert_eq!(requests[2]["thinking"], serde_json::json!({"type": "enabled", "budget_tokens": 20000}));
    assert!(requests[3].get("thinking").is_none(), "{}", requests[3]);
    assert_eq!(requests[3]["chat_template_kwargs"], serde_json::json!({"enable_thinking": false}));
}

/// The context detail's group data (SME-244): an agent's shows its own
/// spend plus the whole group's, labelled with the group root's title;
/// the root's shows the group against its own, unlabelled. No turn run,
/// so no mock or guard: the pool-only shape is proven by
/// `prompt.rs`'s `test_context_detail_shows_the_system_prompt_a_turn_sends`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_context_detail_shows_the_groups_spend_and_a_childs_group(pool: PgPool) {
    let root = db::create_conversation(&pool).await.expect("root");
    sqlx::query("UPDATE conversations SET title = 'Build the thing' WHERE id = $1")
        .bind(root.id)
        .execute(&pool)
        .await
        .expect("title the root");
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ($1, $2) RETURNING id")
        .bind("agent of the root")
        .bind(root.id)
        .fetch_one(&pool)
        .await
        .expect("a child row");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'a brief', 'active')")
        .bind(child)
        .bind(root.id)
        .execute(&pool)
        .await
        .expect("its agent row");
    let provider = db::create_inference_provider(
        &pool,
        "p",
        "anthropic",
        "https://api.anthropic.com",
        "api_key",
        "sk-ant-0123456789",
        false,
        None,
        true,
        None,
    )
    .await
    .expect("provider");
    let call = |conversation_id| db::ModelCall {
        conversation_id,
        provider_id: provider.id,
        model: "m",
        kind: db::ModelCallKind::Turn,
        usage: crate::anthropic::TokenUsage {
            input_tokens: 10,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
        cost_usd: Some(1.0),
        finished: true,
    };
    db::record_model_call(&pool, &call(root.id)).await.expect("the root's call");
    db::record_model_call(&pool, &call(child)).await.expect("the child's call");

    let detail = context_detail(&pool, child).await.expect("the child's context detail");
    assert_eq!(detail.spend.calls, 1, "the agent's own spend is its own");
    assert_eq!(detail.group_spend.calls, 2, "and its group runs from the root down");
    assert_eq!(detail.group_root_title.as_deref(), Some("Build the thing"), "labelled with the group's root");

    let root_detail = context_detail(&pool, root.id).await.expect("the root's context detail");
    assert_eq!(root_detail.spend.calls, 1, "the root's own spend stays its own");
    assert_eq!(root_detail.group_spend.calls, 2, "its group holds its call and its agent's");
    assert_eq!(root_detail.group_root_title, None, "the root is not in another conversation's group");
}
