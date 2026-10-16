use super::*;

fn llama_turn_model(template: Option<TemplateSettings>) -> TurnModel {
    TurnModel { adaptive_thinking: true,
        endpoint: Endpoint { base_url: "http://llama".to_string(), auth: Auth::Bearer("k".to_string()), slots_of: None },
        provider_id: 1,
        model: "flash-next".to_string(),
        thinking: true,
        prompt_caching: false,
        price_catalog_provider: None,
        effort: None,
        kind: ProviderKind::LlamaCpp,
        template,
        output_cap: None,
        reasoning_cap: None,
        output_cap_is_models_own: false,
        context_window: 262_144,
        thinking_stripped_through: None,
    }
}

/// SME-111: thinking may take three quarters of the reply budget,
/// leaving 4,096 for the answer, and no more than the user's cap.
#[test]
fn test_the_reasoning_budget_leaves_room_to_answer() {
    assert_eq!(reasoning_budget(131_072, None), 98_304, "three quarters");
    assert_eq!(reasoning_budget(131_072, Some(20_000)), 20_000, "the user's cap");
    assert_eq!(reasoning_budget(131_072, Some(200_000)), 98_304, "a cap above it changes nothing");
    assert_eq!(reasoning_budget(12_000, None), 7_904, "4,096 left to answer");
    assert_eq!(reasoning_budget(2_048, None), 0, "too small to think in");
}

/// SME-111: only llama.cpp is sent a budget; with thinking off,
/// nothing.
#[test]
fn test_only_llama_cpp_is_sent_a_thinking_budget() {
    use crate::anthropic::ThinkingConfig;
    let llama = TurnModel { reasoning_cap: Some(50_000), ..llama_turn_model(None) };
    assert_eq!(llama.thinking_config(131_072), Some(ThinkingConfig::Enabled { budget_tokens: 50_000 }));
    assert_eq!(TurnModel { thinking: false, ..llama.clone() }.thinking_config(131_072), None);
    assert_eq!(llama.thinking_config(4_096), None, "no room to think in (review 1)");
    for kind in [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::Other] {
        assert_eq!(
            TurnModel { kind, ..llama.clone() }.thinking_config(131_072),
            Some(ThinkingConfig::Adaptive),
            "{kind:?}"
        );
    }
}

/// B9 (SME-122): a model whose listing said it can't take adaptive
/// thinking (Haiku 4.5) is sent a budget, at least the API's 1,024
/// and leaving room to answer; with too little room, no thinking.
#[test]
fn test_a_model_without_adaptive_thinking_is_sent_a_budget() {
    use crate::anthropic::ThinkingConfig;
    for kind in [ProviderKind::Anthropic, ProviderKind::Other] {
        let haiku = TurnModel { kind, adaptive_thinking: false, ..llama_turn_model(None) };
        assert_eq!(
            haiku.thinking_config(64_000),
            Some(ThinkingConfig::Enabled { budget_tokens: 48_000 }),
            "{kind:?}"
        );
        assert_eq!(haiku.thinking_config(5_000), None, "{kind:?}: under 1,024 to think in");
        assert_eq!(TurnModel { thinking: false, ..haiku.clone() }.thinking_config(64_000), None);
    }
}

/// B9 (SME-122): what the listing says about adaptive thinking is
/// kept with the model, and a turn on it is sent a budget.
#[sqlx::test]
async fn test_a_turn_on_a_model_without_adaptive_thinking_knows_it(pool: PgPool) {
    let conversation = db::create_conversation_with_id(&pool, 9_172_000_009).await.expect("conversation");
    let anthropic = provider(&pool, "anthropic", "api_key").await;
    db::set_default_model(&pool, anthropic.id, "claude-haiku-4-5").await.expect("default");
    let details = crate::anthropic::models::ModelDetails {
        thinking: Some(true),
        adaptive_thinking: Some(false),
        ..Default::default()
    };
    super::super::super::server::store_details(&pool, anthropic.id, "claude-haiku-4-5", &details).await.expect("stored");

    let turn = resolve_turn_model(&pool, conversation.id).await.expect("resolves");

    assert!(turn.thinking && !turn.adaptive_thinking, "{turn:?}");
}

/// SME-111: what a llama.cpp request tells its chat template, with
/// thinking on and off; nothing for any other kind.
#[test]
fn test_a_llama_cpp_turn_sends_its_template_settings() {
    use crate::anthropic::{ChatTemplateKwargs, Effort};
    let set = TemplateSettings { effort: Some(Effort::Low), preserve_thinking: Some(false) };
    let turn = llama_turn_model(Some(set.clone()));
    assert_eq!(
        turn.chat_template_kwargs(true),
        Some(ChatTemplateKwargs { enable_thinking: None, reasoning_effort: Some(Effort::Low), preserve_thinking: Some(false) })
    );
    assert_eq!(
        turn.chat_template_kwargs(false),
        Some(ChatTemplateKwargs { enable_thinking: Some(false), reasoning_effort: None, preserve_thinking: Some(false) }),
        "thinking off says so, and effort means nothing without it"
    );
    assert_eq!(llama_turn_model(Some(TemplateSettings::default())).chat_template_kwargs(true), None, "nothing to send");
    assert_eq!(
        llama_turn_model(None).chat_template_kwargs(false),
        Some(ChatTemplateKwargs { enable_thinking: Some(false), ..Default::default() })
    );
    for kind in [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::Other] {
        let other = TurnModel { kind, ..llama_turn_model(Some(set.clone())) };
        assert_eq!(other.chat_template_kwargs(false), None, "{kind:?} isn't sent them");
    }
}

fn llama_provider(caps: Option<serde_json::Value>, keep_reasoning: bool) -> db::InferenceProvider {
    db::InferenceProvider {
        id: 1,
        name: "llama".to_string(),
        kind: "llama_cpp".to_string(),
        base_url: "http://llama".to_string(),
        auth_kind: "bearer".to_string(),
        secret: "k".to_string(),
        prompt_caching: false,
        price_catalog_provider: None,
        keep_reasoning,
        server_caps: caps,
        max_concurrent_requests: None,
        created_at: chrono::NaiveDateTime::default(),
        updated_at: chrono::NaiveDateTime::default(),
    }
}

/// SME-111: a setting goes only to a template whose caps say it
/// reads it; one with no caps read yet is sent none.
#[test]
fn test_template_settings_follow_the_templates_caps() {
    use crate::anthropic::Effort;
    let row = db::ProviderModelRow { effort: Some("medium".to_string()), ..row(None, None, None, None) };
    let both = serde_json::json!({"template_caps": {"supports_reasoning_effort": true, "supports_preserve_reasoning": true}});
    assert_eq!(
        template_settings(&llama_provider(Some(both.clone()), false), Some(&row)),
        TemplateSettings { effort: Some(Effort::Medium), preserve_thinking: Some(false) }
    );
    assert_eq!(
        template_settings(&llama_provider(Some(both), true), None),
        TemplateSettings { effort: None, preserve_thinking: Some(true) }
    );
    // SME-111 review 1: a level the row doesn't offer for llama.cpp
    // (set while the provider was another kind) isn't sent: the
    // user's template refuses `max`, failing every turn.
    let effort_caps = serde_json::json!({"template_caps": {"supports_reasoning_effort": true}});
    for (stored, sent) in [("max", None), ("xhigh", None), ("high", Some(Effort::High))] {
        let stale = db::ProviderModelRow { effort: Some(stored.to_string()), ..self::row(None, None, None, None) };
        assert_eq!(template_settings(&llama_provider(Some(effort_caps.clone()), false), Some(&stale)).effort, sent, "{stored}");
    }
    let neither = serde_json::json!({"template_caps": {"supports_reasoning_effort": false}});
    assert_eq!(template_settings(&llama_provider(Some(neither), false), Some(&row)), TemplateSettings::default());
    assert_eq!(template_settings(&llama_provider(None, false), Some(&row)), TemplateSettings::default(), "never read");
    assert_eq!(
        template_settings(&llama_provider(Some(serde_json::json!("garbage")), false), Some(&row)),
        TemplateSettings::default(),
        "unreadable"
    );
}

/// A llama.cpp-shaped mock: `/v1/models` lists one model, and
/// `/props` answers with the real server's fixture until
/// `props_up` is cleared, then fails.
async fn mock_llama_cpp(props_up: std::sync::Arc<std::sync::atomic::AtomicBool>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new()
        .route(
            "/v1/models",
            axum::routing::get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "application/json")],
                 include_str!("../../../anthropic/fixtures/llama_cpp_v1_models.json"))
            }),
        )
        .route(
            "/props",
            axum::routing::get(move || {
                let up = props_up.load(std::sync::atomic::Ordering::SeqCst);
                async move {
                    if up {
                        (axum::http::StatusCode::OK, include_str!("../../../anthropic/fixtures/llama_cpp_props.json").to_string())
                    } else {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "loading model".to_string())
                    }
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

/// SME-111: a llama.cpp provider's page reads `/props` with its
/// models, keeps it, and a turn sends what its template supports.
#[sqlx::test]
async fn test_a_llama_cpp_providers_props_are_kept_and_used_by_its_turns(pool: PgPool) {
    use crate::anthropic::Effort;
    let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let base = mock_llama_cpp(up.clone()).await;
    let p = db::create_inference_provider(&pool, "llama", "llama_cpp", &base, "bearer", "k", false, None, false, None)
        .await
        .expect("create");
    set_model_settings(&pool, p.id, "flash-next", settings(None, None, Some(Effort::Low))).await.expect("effort");

    let listing = provider_models(&pool, p.id, true).await.expect("models");
    assert_eq!(listing.kind, ProviderKind::LlamaCpp);
    assert_eq!(listing.server_error, None);
    let caps = listing.server_caps.clone().expect("read /props");
    assert_eq!((caps.n_ctx, caps.total_slots), (Some(262_144), Some(1)));
    let model = &listing.models[0];
    assert_eq!((model.id.as_str(), model.context_window, model.reported_tools), ("flash-next", 262_144, Some(true)));
    let summary: ProviderSummary = db::get_inference_provider(&pool, p.id).await.expect("get").expect("exists").into();
    assert_eq!(summary.server_caps, Some(caps.clone()), "kept on the provider");

    let conversation = db::create_conversation_with_id(&pool, 9_172_000_020).await.expect("conversation");
    db::set_conversation_model(&pool, conversation.id, p.id, "flash-next").await.expect("pick");
    let turn = resolve_turn_model(&pool, conversation.id).await.expect("resolves");
    assert_eq!(turn.kind, ProviderKind::LlamaCpp);
    assert_eq!(turn.effort, None, "not as output_config");
    assert_eq!(turn.template, Some(TemplateSettings { effort: Some(Effort::Low), preserve_thinking: Some(false) }));

    // The server is down: the page says why, and keeps what it read.
    up.store(false, std::sync::atomic::Ordering::SeqCst);
    let listing = provider_models(&pool, p.id, true).await.expect("models");
    assert!(listing.server_error.as_deref().is_some_and(|e| e.contains("503")), "{listing:?}");
    assert_eq!(listing.server_caps, Some(caps));
}

/// SME-162 review 1: an automatic provider's page reading `/props`
/// sets its pool from the count at once, so a turn already running
/// takes it from its next model call.
#[sqlx::test]
async fn test_reading_props_sets_an_automatic_providers_pool_at_once(pool: PgPool) {
    let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let base = mock_llama_cpp(up).await;
    // The pools are process-wide by provider id: an id of its own.
    sqlx::query("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH 9162000301")
        .execute(&pool)
        .await
        .expect("renumber");
    let p = db::create_inference_provider(&pool, "llama", "llama_cpp", &base, "bearer", "k", false, None, false, Some(0))
        .await
        .expect("create");
    let running = endpoint(&p);
    assert!(crate::providers::limiter::current(p.id).is_none(), "nothing read yet: no limit");

    provider_models(&pool, p.id, true).await.expect("models");

    let slots = crate::providers::limiter::current(running.slots_of.expect("a model endpoint"));
    assert_eq!(slots.map(|s| s.available_permits()), Some(1), "the fixture's one slot");

    // A turn that read the row before the page read `/props` builds its
    // endpoint from it now: the pool the read set stays.
    endpoint(&p);
    let slots = crate::providers::limiter::current(p.id);
    assert_eq!(slots.map(|s| s.available_permits()), Some(1), "an older row doesn't clear it");
}

/// A llama.cpp-shaped mock whose `/v1/models` waits until `release`
/// has a permit, so a test can act while a listing is in flight;
/// `arrived` says a listing has reached it.
async fn gated_llama_cpp() -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<tokio::sync::Semaphore>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let arrived = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let (seen, gate) = (arrived.clone(), release.clone());
    let app = axum::Router::new()
        .route(
            "/v1/models",
            axum::routing::get(move || {
                let (seen, gate) = (seen.clone(), gate.clone());
                async move {
                    seen.store(true, std::sync::atomic::Ordering::SeqCst);
                    gate.acquire().await.expect("the gate stays open").forget();
                    ([(axum::http::header::CONTENT_TYPE, "application/json")],
                     include_str!("../../../anthropic/fixtures/llama_cpp_v1_models.json"))
                }
            }),
        )
        .route(
            "/props",
            axum::routing::get(|| async { include_str!("../../../anthropic/fixtures/llama_cpp_props.json") }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), arrived, release)
}

/// SME-162 review 2: a limit saved while the page's listing is in
/// flight isn't undone when that listing's `/props` read lands: the pool
/// comes from the row the read wrote, not the row the listing started
/// with. And a read for an address the provider no longer has keeps
/// nothing.
#[sqlx::test]
async fn test_a_props_read_doesnt_undo_a_save_made_meanwhile(pool: PgPool) {
    let (base, arrived, release) = gated_llama_cpp().await;
    sqlx::query("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH 9162000302")
        .execute(&pool)
        .await
        .expect("renumber");
    let p = db::create_inference_provider(&pool, "llama", "llama_cpp", &base, "bearer", "k", false, None, true, Some(0))
        .await
        .expect("create");
    let llama = |max, url: &str| ProviderInput {
        kind: ProviderKind::LlamaCpp,
        max_concurrent_requests: max,
        ..input("llama", url, "")
    };

    let listing = tokio::spawn({
        let pool = pool.clone();
        async move { provider_models(&pool, p.id, true).await }
    });
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !arrived.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the listing reached the server");
    update_provider(&pool, p.id, llama(Some(4), &base)).await.expect("save 4");
    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), listing)
        .await
        .expect("the listing ends")
        .expect("the task ran")
        .expect("models");
    let slots = crate::providers::limiter::current(p.id).map(|s| s.available_permits());
    assert_eq!(slots, Some(4), "the saved 4, not the server's 1 slot");

    // The address changes while a listing of the old one is in flight.
    arrived.store(false, std::sync::atomic::Ordering::SeqCst);
    let listing = tokio::spawn({
        let pool = pool.clone();
        async move { provider_models(&pool, p.id, true).await }
    });
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !arrived.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the listing reached the server");
    update_provider(&pool, p.id, ProviderInput { secret: "k".to_string(), ..llama(Some(0), "http://127.0.0.1:1") })
        .await
        .expect("a new address");
    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), listing)
        .await
        .expect("the listing ends")
        .expect("the task ran")
        .expect("models");
    let stored = db::get_inference_provider(&pool, p.id).await.expect("get").expect("exists");
    assert_eq!(stored.server_caps, None, "the old server's /props isn't kept for the new address");
    assert!(crate::providers::limiter::current(p.id).is_none(), "automatic, nothing read: no limit");
}

/// SME-111 review 2: the picker's quick listing (no `/props`)
/// doesn't replace the slot window and tool support `/props` gave
/// with `/v1/models`' whole-server `meta.n_ctx` and no tool flag.
#[sqlx::test]
async fn test_a_quick_llama_cpp_listing_keeps_what_props_said(pool: PgPool) {
    let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let base = mock_llama_cpp(up).await;
    let p = db::create_inference_provider(&pool, "llama", "llama_cpp", &base, "bearer", "k", false, None, true, None)
        .await
        .expect("create");
    // A four-slot server: each slot has a quarter of the 262,144.
    let caps = serde_json::json!({
        "n_ctx": 65536, "total_slots": 4, "model_alias": "flash-next",
        "template_caps": {"supports_tool_calls": false}
    });
    db::set_inference_provider_server_caps(&pool, p.id, &p.kind, &p.base_url, &caps)
        .await
        .expect("caps")
        .expect("written");

    let listing = provider_models(&pool, p.id, false).await.expect("models");

    let model = &listing.models[0];
    assert_eq!((model.context_window, model.reported_tools), (65_536, Some(false)), "{model:?}");
    let stored = db::get_provider_model(&pool, p.id, "flash-next").await.expect("get").expect("stored");
    assert_eq!((stored.reported_context_window, stored.reported_tools), (Some(65_536), Some(false)));
}

#[sqlx::test]
async fn test_model_settings_are_checked(pool: PgPool) {
    let p = provider(&pool, "p", "api_key").await;
    assert!(set_model_settings(&pool, p.id, " ", settings(None, None, None)).await.is_err());
    assert!(set_model_settings(&pool, p.id, "m", settings(None, Some(10), None)).await.is_err(), "too small");
    assert!(set_model_settings(&pool, p.id, "m", settings(None, Some(u32::MAX), None)).await.is_err(), "too big");
    assert_eq!(
        set_model_settings(&pool, p.id + 1, "m", settings(Some(true), None, None)).await,
        Err("That provider no longer exists.".to_string())
    );
    set_model_settings(&pool, p.id, "m", settings(Some(false), Some(32_768), None)).await.expect("set");
    let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
    assert_eq!((row.thinking, row.context_window), (Some(false), Some(32_768)));
    let reply = |max_output| ModelSettings { max_output: Some(max_output), ..Default::default() };
    assert!(set_model_settings(&pool, p.id, "m", reply(100)).await.is_err(), "too small");
    assert!(set_model_settings(&pool, p.id, "m", reply(u32::MAX)).await.is_err(), "too big");
    set_model_settings(&pool, p.id, "m", reply(65_536)).await.expect("set");
    let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
    assert_eq!((row.max_output, row.context_window), (Some(65_536), None), "every setting saved together");
    let thinking = |reasoning_budget| ModelSettings { reasoning_budget: Some(reasoning_budget), ..Default::default() };
    assert!(set_model_settings(&pool, p.id, "m", thinking(0)).await.is_err(), "zero");
    set_model_settings(&pool, p.id, "m", thinking(20_000)).await.expect("set");
    let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
    assert_eq!(row.reasoning_budget, Some(20_000));
    assert_eq!(model_info(ProviderKind::LlamaCpp, "m".to_string(), None, Some(&row), None).reasoning_budget_override, Some(20_000));
}
