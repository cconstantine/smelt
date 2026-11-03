//! SME-73: saving the window an overflow refusal named, and telling the
//! app about it so the providers page and pickers refresh.

use super::*;

fn turn_model(provider_id: i64, base_url: &'static str) -> TurnModel {
    TurnModel {
        adaptive_thinking: true,
        endpoint: Endpoint { base_url: base_url.to_string(), auth: Auth::ApiKey("k".to_string()), slots_of: None },
        provider_id,
        model: "flash-next".to_string(),
        thinking: true,
        prompt_caching: false,
        price_catalog_provider: None,
        effort: None,
        kind: ProviderKind::LlamaCpp,
        template: None,
        output_cap: None,
        reasoning_cap: None,
        output_cap_is_models_own: false,
        context_window: 262_144,
        thinking_stripped_through: None,
    }
}

/// A learned window is saved for the model and `ProvidersChanged` goes
/// out; the same or a larger one next changes nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_learning_a_window_saves_it_and_tells_the_app(pool: PgPool) {
    let ollama = provider(&pool, "home", "api_key").await;
    let mut app = crate::events::subscribe_app();
    let model = turn_model(ollama.id, "http://ollama:11434");

    assert!(learn_context_window(&pool, &model, 32_768).await.expect("learn"), "a null window is set");
    assert!(received(&mut app, &crate::events::AppEvent::ProvidersChanged), "the page and pickers refresh");
    let row = db::get_provider_model(&pool, ollama.id, "flash-next").await.expect("get").expect("exists");
    assert_eq!(row.context_window, Some(32_768));

    assert!(!learn_context_window(&pool, &model, 65_536).await.expect("learn again"), "it only ever lowers: nothing to save");
    assert!(!learn_context_window(&pool, &model, 32_768).await.expect("learn again"), "the same window: nothing changed");
    let row = db::get_provider_model(&pool, ollama.id, "flash-next").await.expect("get").expect("exists");
    assert_eq!(row.context_window, Some(32_768));
}

/// A turn on a provider that moved mid-flight teaches the old server's
/// window to nobody: the write is dropped, and a dropped write that
/// saved nothing still reads right — the model keeps what it had.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_window_learned_after_the_provider_moved_changes_nothing(pool: PgPool) {
    let ollama = provider(&pool, "home", "api_key").await;
    let model = turn_model(ollama.id, "http://was-here-before");

    assert!(!learn_context_window(&pool, &model, 32_768).await.expect("stale learn"), "the provider isn't there anymore");
    assert_eq!(db::get_provider_model(&pool, ollama.id, "flash-next").await.expect("get"), None);
}
