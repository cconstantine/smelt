use super::*;

#[test]
fn test_the_context_window_prefers_the_override_then_the_report_then_a_known_model() {
    assert_eq!(context_window("m", Some(&row(None, Some(32_000), Some(4096), None))), (32_000, true));
    assert_eq!(context_window("m", Some(&row(None, None, Some(4096), None))), (4096, true));
    assert_eq!(context_window("claude-opus-5", None), (200_000, true), "a known claude-* model");
    assert_eq!(context_window("llama3", None), (ASSUMED_CONTEXT_WINDOW, false));
    assert_eq!(
        context_window("llama3", Some(&row(None, Some(0), Some(-1), None))),
        (ASSUMED_CONTEXT_WINDOW, false),
        "a zero or negative size isn't one"
    );
}

/// SME-111: a reply's cap is the user's, else the provider's, else
/// 16,384 where a Claude model may sit behind the provider.
#[test]
fn test_the_output_cap_prefers_the_override_then_the_report_then_the_kinds() {
    // Sized, as the window is unless a case says otherwise.
    let capped = |max_output, reported_max_output| db::ProviderModelRow {
        max_output,
        reported_max_output,
        ..row(None, None, Some(262_144), None)
    };
    assert_eq!(output_cap(ProviderKind::Anthropic, "m", Some(&capped(Some(8192), Some(128_000)))), Some(8192));
    assert_eq!(output_cap(ProviderKind::Anthropic, "m", Some(&capped(None, Some(128_000)))), Some(128_000));
    assert_eq!(output_cap(ProviderKind::Anthropic, "m", None), Some(UNKNOWN_OUTPUT_CAP));
    assert_eq!(output_cap(ProviderKind::Other, "m", Some(&capped(Some(0), Some(-1)))), Some(UNKNOWN_OUTPUT_CAP), "not sizes");
    assert_eq!(output_cap(ProviderKind::LlamaCpp, "m", Some(&capped(None, None))), None, "the window bounds it");
    // SME-111 review 1: a window nothing sized (assumed 200,000)
    // doesn't bound a local server's real one, so it keeps 16,384.
    assert_eq!(output_cap(ProviderKind::LlamaCpp, "m", None), Some(UNKNOWN_OUTPUT_CAP));
    assert_eq!(output_cap(ProviderKind::Ollama, "m", Some(&row(None, None, None, None))), Some(UNKNOWN_OUTPUT_CAP));
    assert_eq!(output_cap(ProviderKind::Ollama, "m", Some(&capped(None, None))), None);
    assert_eq!(output_cap(ProviderKind::LlamaCpp, "m", Some(&capped(Some(32_768), None))), Some(32_768));
    // SME-111 review 1: Anthropic refuses more than the model's cap.
    assert_eq!(output_cap(ProviderKind::Anthropic, "m", Some(&capped(Some(200_000), Some(128_000)))), Some(128_000));
    assert_eq!(output_cap(ProviderKind::Other, "m", Some(&capped(Some(200_000), Some(128_000)))), Some(200_000), "a gateway's report may not be the model's");
    let info = model_info(ProviderKind::LlamaCpp, "m".to_string(), None, Some(&capped(Some(32_768), None)), None);
    assert_eq!((info.max_output_override, info.max_output), (Some(32_768), Some(32_768)));
}

#[test]
fn test_thinking_prefers_the_override_then_the_report_then_on() {
    assert!(thinking(None));
    assert!(!thinking(Some(&row(None, None, None, Some(false)))));
    assert!(thinking(Some(&row(Some(true), None, None, Some(false)))));
    assert!(!thinking(Some(&row(Some(false), None, None, None))));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_picker_states_from_no_providers_to_chosen(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    assert_eq!(
        conversation_model(&pool, conversation.id).await.expect("read"),
        Some(ConversationModel::NoProviders {})
    );
    assert_eq!(conversation_model(&pool, 999_999).await.expect("read"), None);

    let ollama = provider(&pool, "Home Ollama", "api_key").await;
    assert_eq!(
        conversation_model(&pool, conversation.id).await.expect("read"),
        Some(ConversationModel::NoDefault {})
    );

    db::set_default_model(&pool, ollama.id, "gemma4").await.expect("default");
    db::set_provider_model_reported(&pool, ollama.id, "gemma4", Some(4096), Some(true), Some(false), None, None)
        .await
        .expect("reported");
    let gemma = ModelChoice {
        provider_id: ollama.id,
        provider_name: "Home Ollama".to_string(),
        model: "gemma4".to_string(),
        context_window: 4096,
        context_window_known: true,
        tools: Some(false),
    };
    assert_eq!(
        conversation_model(&pool, conversation.id).await.expect("read"),
        Some(ConversationModel::Default { choice: gemma.clone() })
    );

    db::set_conversation_model(&pool, conversation.id, ollama.id, "llama3").await.expect("set");
    assert_eq!(
        conversation_model(&pool, conversation.id).await.expect("read"),
        Some(ConversationModel::Chosen {
            choice: ModelChoice {
                model: "llama3".to_string(),
                context_window: ASSUMED_CONTEXT_WINDOW,
                context_window_known: false,
                tools: None,
                ..gemma
            }
        })
    );
    assert_eq!(conversation_context_window(&pool, conversation.id).await, ASSUMED_CONTEXT_WINDOW);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_takes_the_default_and_keeps_it(pool: PgPool) {
    // Its own id: event buses are process-wide, keyed by id.
    let conversation = db::create_conversation_with_id(&pool, 9_172_000_001).await.expect("conversation");
    let bearer = provider(&pool, "gateway", "bearer").await;
    db::set_default_model(&pool, bearer.id, "m1").await.expect("default");
    db::set_provider_model_overrides(&pool, bearer.id, "m1", Some(false), Some(64_000), None, None, None)
        .await
        .expect("overrides");
    let mut events = crate::events::subscribe(conversation.id);

    let turn = resolve_turn_model(&pool, conversation.id).await.expect("resolves");

    assert_eq!(
        turn,
        TurnModel {
            endpoint: Endpoint {
                base_url: "http://ollama:11434".to_string(),
                auth: Auth::Bearer("the-secret".to_string()),
                slots_of: Some(bearer.id),
            },
            provider_id: bearer.id,
            model: "m1".to_string(),
            thinking: false,
            prompt_caching: false,
            price_catalog_provider: None,
            effort: None,
            kind: ProviderKind::Ollama,
            template: None,
            output_cap: None,
            reasoning_cap: None,
            output_cap_is_models_own: false,
            context_window: 64_000,
            thinking_stripped_through: None,
            adaptive_thinking: true,
        }
    );
    let kept = db::get_conversation_model(&pool, conversation.id).await.expect("read").expect("exists");
    assert_eq!((kept.provider_id, kept.model.as_deref()), (Some(bearer.id), Some("m1")));
    assert!(received(&mut events, &crate::events::ConversationEvent::ModelChanged {}), "tabs are told");

    // A later change of default doesn't move it.
    db::set_default_model(&pool, bearer.id, "m2").await.expect("default");
    assert_eq!(resolve_turn_model(&pool, conversation.id).await.expect("resolves").model, "m1");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_with_no_model_and_no_default_says_so(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    provider(&pool, "p", "api_key").await;
    assert_eq!(
        resolve_turn_model(&pool, conversation.id).await,
        Err(NO_MODEL_CONFIGURED.to_string())
    );
    assert_eq!(
        resolve_turn_model(&pool, 999_999).await,
        Err("conversation not found".to_string())
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_switched_conversations_turn_strips_thinking_up_to_the_switch(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let p = provider(&pool, "p", "api_key").await;
    let text = [crate::anthropic::ContentBlock::Text { text: "hi".to_string() }];
    db::set_conversation_model(&pool, conversation.id, p.id, "m1").await.expect("model");
    resolve_turn_model(&pool, conversation.id).await.expect("the first turn, on m1");
    // The user picks m2 while that turn is still running and saving
    // messages m1 signed.
    set_conversation_model(&pool, conversation.id, p.id, "m2").await.expect("switch");
    let during = db::create_message(&pool, conversation.id, "assistant", &text).await.expect("message");

    let turn = resolve_turn_model(&pool, conversation.id).await.expect("the next turn, on m2");
    assert_eq!(turn.thinking_stripped_through, Some(during.id), "includes what m1 wrote after the pick");
}

/// SME-72 review: a provider moved to another address is another
/// backend, whose signatures don't match.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_provider_moved_to_another_address_strips_old_thinking(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let p = provider(&pool, "p", "api_key").await;
    let text = [crate::anthropic::ContentBlock::Text { text: "hi".to_string() }];
    db::set_conversation_model(&pool, conversation.id, p.id, "m1").await.expect("model");
    resolve_turn_model(&pool, conversation.id).await.expect("first turn");
    let reply = db::create_message(&pool, conversation.id, "assistant", &text).await.expect("message");
    update_provider(&pool, p.id, input("p", "http://elsewhere:11434", "new-key")).await.expect("move it");

    let turn = resolve_turn_model(&pool, conversation.id).await.expect("next turn");
    assert_eq!(turn.thinking_stripped_through, Some(reply.id));
}

/// SME-72 review: the stored key only ever goes to the address it
/// was entered for.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_moving_a_provider_needs_its_key_again(pool: PgPool) {
    let created = create_provider(&pool, input("p", "http://one:11434", "secret-1234567890"))
        .await
        .expect("create");
    let refused = update_provider(&pool, created.id, input("p", "http://two:11434", ""))
        .await
        .expect_err("a new address without the key");
    assert!(refused.contains("key"), "{refused}");
    let stored = db::get_inference_provider(&pool, created.id).await.expect("get").expect("exists");
    assert_eq!(stored.base_url, "http://one:11434", "nothing changed");
    // The same address with a blank key still keeps it.
    update_provider(&pool, created.id, input("renamed", " http://one:11434 ", ""))
        .await
        .expect("a rename keeps the key");
}
