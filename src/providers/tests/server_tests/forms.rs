use super::*;

#[sqlx::test]
async fn test_a_provider_form_is_checked_and_the_secret_never_comes_back(pool: PgPool) {
    let mut app_events = crate::events::subscribe_app();
    assert_eq!(
        create_provider(&pool, input("  ", "http://h", "k")).await,
        Err("Give the provider a name.".to_string())
    );
    let bad_url = create_provider(&pool, input("x", "ftp://h", "k")).await.expect_err("scheme");
    assert!(bad_url.contains("http://"), "{bad_url}");
    assert!(create_provider(&pool, input("x", "localhost:11434", "k")).await.is_err(), "no scheme");
    assert!(create_provider(&pool, input("x", "http://h", "  ")).await.is_err(), "a new one needs a secret");

    let created = create_provider(&pool, input(" Home ", " http://ollama:11434 ", "secret-1234567890"))
        .await
        .expect("create");
    assert_eq!(
        created,
        ProviderSummary {
            id: created.id,
            name: "Home".to_string(),
            kind: ProviderKind::Ollama,
            base_url: "http://ollama:11434".to_string(),
            auth_kind: AuthKind::ApiKey,
            secret_hint: Some("\u{2026}7890".to_string()),
            prompt_caching: false,
            price_catalog_provider: None,
            keep_reasoning: true,
            server_caps: None,
        }
    );
    assert!(received(&mut app_events, &crate::events::AppEvent::ProvidersChanged));
    assert_eq!(
        create_provider(&pool, input("Home", "http://h", "k")).await,
        Err("There's already a provider named \u{201c}Home\u{201d}.".to_string())
    );

    let kept = update_provider(&pool, created.id, input("Home", "http://ollama:11434", ""))
        .await
        .expect("update");
    assert_eq!(kept.secret_hint, created.secret_hint, "a blank secret keeps the stored one");
    let stored = db::get_inference_provider(&pool, created.id).await.expect("get").expect("exists");
    assert_eq!(stored.secret, "secret-1234567890");
    let caching = update_provider(&pool, created.id, ProviderInput {
        prompt_caching: true,
        price_catalog_provider: Some(" deepseek ".to_string()),
        ..input("Home", "http://ollama:11434", "")
    })
    .await
    .expect("turn caching on");
    assert!(caching.prompt_caching, "the form's choice is saved and shown");
    assert_eq!(caching.price_catalog_provider.as_deref(), Some("deepseek"));
    let unpriced = update_provider(&pool, created.id, ProviderInput {
        price_catalog_provider: Some("  ".to_string()),
        ..input("Home", "http://ollama:11434", "")
    })
    .await
    .expect("no prices");
    assert_eq!(unpriced.price_catalog_provider, None, "a blank choice is none");
    assert_eq!(
        update_provider(&pool, created.id + 100, input("Z", "http://h", "")).await,
        Err("That provider no longer exists.".to_string())
    );
}

/// Effort is kept per model and sent only on an Anthropic provider:
/// another server may refuse the field (SME-106).
#[sqlx::test]
async fn test_a_models_effort_is_kept_and_sent_only_to_anthropic(pool: PgPool) {
    use crate::anthropic::Effort;
    let anthropic = db::create_inference_provider(&pool, "a", "anthropic", "http://127.0.0.1:1", "api_key", "k", false, None, true)
        .await
        .expect("anthropic provider");
    let ollama = provider(&pool, "o", "api_key").await;
    for p in [&anthropic, &ollama] {
        set_model_settings(&pool, p.id, "m", settings(None, None, Some(Effort::Low))).await.expect("set");
        let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
        assert_eq!(model_info(ProviderKind::Anthropic, "m".to_string(), None, Some(&row), None).effort_override, Some(Effort::Low));
    }

    let on_anthropic = db::create_conversation_with_id(&pool, 9_172_000_010).await.expect("conversation");
    db::set_conversation_model(&pool, on_anthropic.id, anthropic.id, "m").await.expect("pick");
    let turn = resolve_turn_model(&pool, on_anthropic.id).await.expect("resolves");
    assert_eq!(turn.effort, Some(Effort::Low));

    let on_ollama = db::create_conversation_with_id(&pool, 9_172_000_011).await.expect("conversation");
    db::set_conversation_model(&pool, on_ollama.id, ollama.id, "m").await.expect("pick");
    let turn = resolve_turn_model(&pool, on_ollama.id).await.expect("resolves");
    assert_eq!(turn.effort, None, "not sent to an Ollama server");

    set_model_settings(&pool, anthropic.id, "m", settings(None, None, None)).await.expect("clear");
    let turn = resolve_turn_model(&pool, on_anthropic.id).await.expect("resolves");
    assert_eq!(turn.effort, None, "unset sends none");
}
