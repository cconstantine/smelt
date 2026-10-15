use super::*;

/// An Ollama-shaped mock: `/api/tags` lists two models, `/api/show`
/// knows one of them and fails for the other.
async fn mock_ollama() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new()
        .route(
            "/api/tags",
            axum::routing::get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "application/json")],
                 r#"{"models":[{"name":"gemma4"},{"name":"broken"}]}"#)
            }),
        )
        .route(
            "/api/show",
            axum::routing::post(|body: String| async move {
                if body.contains("gemma4") {
                    (axum::http::StatusCode::OK,
                     r#"{"capabilities":["completion","tools"],"parameters":"num_ctx 8192"}"#.to_string())
                } else {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "model is corrupt".to_string())
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

#[sqlx::test]
async fn test_an_ollama_providers_models_come_with_their_details(pool: PgPool) {
    let base = mock_ollama().await;
    let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k", false, None, true)
        .await
        .expect("create");
    set_model_settings(&pool, p.id, "typed-by-hand", settings(None, Some(16_384), None)).await.expect("set");

    let listing = provider_models(&pool, p.id, true).await.expect("models");

    assert_eq!(listing.listing_error, None);
    let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gemma4", "broken", "typed-by-hand"]);
    let gemma = &listing.models[0];
    assert_eq!(
        (gemma.context_window, gemma.context_window_known, gemma.thinking, gemma.reported_tools),
        (8192, true, false, Some(true)),
        "num_ctx, and no thinking capability means thinking off"
    );
    let broken = &listing.models[1];
    assert!(broken.details_error.as_deref().is_some_and(|e| e.contains("500")), "{broken:?}");
    assert!(!broken.context_window_known);
    assert_eq!(listing.models[2].context_window, 16_384);
    // Stored, so a turn uses it without asking again.
    let stored = db::get_provider_model(&pool, p.id, "gemma4").await.expect("get").expect("stored");
    assert_eq!(stored.reported_context_window, Some(8192));
}

/// SME-72 review: the picker's list doesn't ask an Ollama server
/// about every model each time it opens; it uses what's stored.
#[sqlx::test]
async fn test_a_quick_listing_skips_the_per_model_questions(pool: PgPool) {
    let base = mock_ollama().await;
    let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k", false, None, true)
        .await
        .expect("create");
    let listing = provider_models(&pool, p.id, false).await.expect("models");
    let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gemma4", "broken"]);
    assert!(listing.models.iter().all(|m| m.details_error.is_none()), "nobody asked");
    assert_eq!(db::get_provider_model(&pool, p.id, "gemma4").await.expect("get"), None, "nothing stored");
}

/// SME-72 review: adding a model by id keeps any settings it has.
#[sqlx::test]
async fn test_adding_a_model_by_id_keeps_its_settings(pool: PgPool) {
    let p = provider(&pool, "p", "api_key").await;
    set_model_settings(&pool, p.id, "llama3", settings(Some(false), Some(32_768), None)).await.expect("set");
    add_model(&pool, p.id, " llama3 ").await.expect("add");
    let row = db::get_provider_model(&pool, p.id, "llama3").await.expect("get").expect("exists");
    assert_eq!((row.thinking, row.context_window), (Some(false), Some(32_768)));
    add_model(&pool, p.id, "fresh").await.expect("add");
    assert!(db::get_provider_model(&pool, p.id, "fresh").await.expect("get").is_some());
    assert!(add_model(&pool, p.id, "  ").await.is_err());
    assert_eq!(
        add_model(&pool, p.id + 1, "m").await,
        Err("That provider no longer exists.".to_string())
    );
}

/// SME-72 review 2: a model the provider stopped listing goes away,
/// unless it was added by hand or has settings of the user's.
#[sqlx::test]
async fn test_a_model_the_provider_dropped_isnt_shown(pool: PgPool) {
    let base = mock_ollama().await;
    let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k", false, None, true)
        .await
        .expect("create");
    db::set_provider_model_reported(&pool, p.id, "removed", Some(4096), None, Some(true), None, None)
        .await
        .expect("seen once");
    add_model(&pool, p.id, "by-hand").await.expect("add");
    set_model_settings(&pool, p.id, "tuned", settings(None, Some(16_384), None)).await.expect("set");

    let listing = provider_models(&pool, p.id, false).await.expect("models");
    let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gemma4", "broken", "by-hand", "tuned"]);
}

/// SME-72 review 2: `/api/ps` is one server-wide list, fetched once
/// per page, not once per model.
#[sqlx::test]
async fn test_ollamas_loaded_models_are_fetched_once(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let ps_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = ps_calls.clone();
    let app = axum::Router::new()
        .route("/api/tags", axum::routing::get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")],
             r#"{"models":[{"name":"a"},{"name":"b"},{"name":"c"}]}"#)
        }))
        .route("/api/show", axum::routing::post(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")],
             r#"{"capabilities":["completion","tools"]}"#)
        }))
        .route("/api/ps", axum::routing::get(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ([(axum::http::header::CONTENT_TYPE, "application/json")],
                 r#"{"models":[{"name":"b","model":"b","context_length":8192}]}"#)
            }
        }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let p = db::create_inference_provider(&pool, "o", "ollama", &format!("http://{addr}"), "api_key", "k", false, None, true)
        .await
        .expect("create");

    let listing = provider_models(&pool, p.id, true).await.expect("models");

    assert_eq!(ps_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let b = listing.models.iter().find(|m| m.id == "b").expect("b");
    assert_eq!((b.context_window, b.context_window_known), (8192, true));
}

/// SME-111: Anthropic's listing's `max_tokens` is kept as the
/// model's reported cap, and a turn on it is capped by it.
#[sqlx::test]
async fn test_an_anthropic_models_reported_cap_is_kept_and_used(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route("/v1/models", axum::routing::get(|| async {
        ([(axum::http::header::CONTENT_TYPE, "application/json")],
         r#"{"data":[{"id":"claude-x","max_input_tokens":1000000,"max_tokens":64000}],"has_more":false}"#)
    }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let p = db::create_inference_provider(&pool, "a", "anthropic", &format!("http://{addr}"), "api_key", "k", false, None, true)
        .await
        .expect("create");
    let listing = provider_models(&pool, p.id, true).await.expect("models");
    assert_eq!((listing.models[0].max_output, listing.models[0].max_output_override), (Some(64_000), None));
    let conversation = db::create_conversation_with_id(&pool, 9_172_000_021).await.expect("conversation");
    db::set_conversation_model(&pool, conversation.id, p.id, "claude-x").await.expect("pick");
    assert_eq!(resolve_turn_model(&pool, conversation.id).await.expect("resolves").output_cap, Some(64_000));
}

#[sqlx::test]
async fn test_an_unreachable_providers_models_still_list_what_is_stored(pool: PgPool) {
    let p = db::create_inference_provider(&pool, "gone", "anthropic", "http://127.0.0.1:1", "api_key", "k", false, None, true)
        .await
        .expect("create");
    set_model_settings(&pool, p.id, "claude-opus-5", settings(Some(true), None, None)).await.expect("set");
    let listing = provider_models(&pool, p.id, true).await.expect("models");
    assert!(listing.listing_error.is_some());
    assert_eq!(listing.models.len(), 1);
    assert_eq!(listing.models[0].id, "claude-opus-5");
}

#[sqlx::test]
async fn test_the_default_is_checked_and_announced(pool: PgPool) {
    let p = provider(&pool, "p", "api_key").await;
    let mut app_events = crate::events::subscribe_app();
    assert!(set_default_model(&pool, p.id, "").await.is_err());
    assert_eq!(
        set_default_model(&pool, p.id + 1, "m").await,
        Err("That provider no longer exists.".to_string())
    );
    set_default_model(&pool, p.id, "m").await.expect("set");
    assert!(received(&mut app_events, &crate::events::AppEvent::ProvidersChanged));
    assert_eq!(default_model(&pool).await.expect("read").map(|c| c.model), Some("m".to_string()));
    delete_provider(&pool, p.id).await.expect("delete");
    assert_eq!(default_model(&pool).await.expect("read"), None);
}

#[sqlx::test]
async fn test_setting_a_model_checks_it_and_tells_the_tabs(pool: PgPool) {
    let conversation = db::create_conversation_with_id(&pool, 9_172_000_002).await.expect("conversation");
    let p = provider(&pool, "p", "api_key").await;
    let mut events = crate::events::subscribe(conversation.id);
    assert_eq!(
        set_conversation_model(&pool, conversation.id, p.id, "  ").await,
        Err("Choose a model.".to_string())
    );
    assert_eq!(
        set_conversation_model(&pool, conversation.id, p.id + 1, "m").await,
        Err("That provider no longer exists.".to_string())
    );
    set_conversation_model(&pool, conversation.id, p.id, " m1 ").await.expect("set");
    let set = db::get_conversation_model(&pool, conversation.id).await.expect("read").expect("exists");
    assert_eq!(set.model.as_deref(), Some("m1"), "trimmed");
    assert!(received(&mut events, &crate::events::ConversationEvent::ModelChanged {}), "tabs are told");
}
