use super::*;

/// Mechanical CRUD, mirroring the MCP server table's: a characterization
/// round trip rather than test-first.
#[sqlx::test]
async fn test_inference_provider_round_trip_keeps_the_secret_unless_replaced(pool: PgPool) {
    let created = test_provider(&pool, "anthropic").await;
    assert_eq!(list_inference_providers(&pool).await.expect("list"), vec![created.clone()]);

    let renamed = update_inference_provider(&pool, created.id, "work", "other", "https://gw.example", "bearer", None, false, None, true, None)
        .await
        .expect("update")
        .expect("exists");
    assert_eq!(
        (renamed.name.as_str(), renamed.kind.as_str(), renamed.base_url.as_str(), renamed.auth_kind.as_str()),
        ("work", "other", "https://gw.example", "bearer")
    );
    assert_eq!(renamed.secret, "sk-ant-0123456789", "no new secret keeps the stored one");

    let rekeyed = update_inference_provider(&pool, created.id, "work", "other", "https://gw.example", "bearer", Some("new-secret"), true, Some("anthropic"), true, None)
        .await
        .expect("update")
        .expect("exists");
    assert_eq!(rekeyed.secret, "new-secret");
    assert!(!renamed.prompt_caching && rekeyed.prompt_caching, "set outright by each update");
    assert_eq!(
        (renamed.price_catalog_provider.as_deref(), rekeyed.price_catalog_provider.as_deref()),
        (None, Some("anthropic"))
    );

    assert_eq!(
        update_inference_provider(&pool, 999_999, "x", "other", "u", "bearer", None, false, None, true, None).await.expect("update"),
        None
    );
}

/// SME-111 review 2: what a llama.cpp server said about itself goes
/// when the provider points at another address or becomes another
/// kind, so a turn never sends settings for the old server's template.
#[sqlx::test]
async fn test_a_providers_server_caps_go_when_its_address_or_kind_changes(pool: PgPool) {
    let caps = serde_json::json!({"template_caps": {"supports_reasoning_effort": true}});
    let p = create_inference_provider(&pool, "llama", "llama_cpp", "http://one", "bearer", "k", false, None, true, None)
        .await
        .expect("create");
    let caps_after = |name: &'static str, kind: &'static str, url: &'static str| {
        let pool = pool.clone();
        let caps = caps.clone();
        async move {
            let now = get_inference_provider(&pool, p.id).await.expect("get").expect("exists");
            set_inference_provider_server_caps(&pool, p.id, &now.kind, &now.base_url, &caps)
                .await
                .expect("caps")
                .expect("written for the server it points at");
            update_inference_provider(&pool, p.id, name, kind, url, "bearer", None, false, None, false, None)
                .await
                .expect("update")
                .expect("exists")
                .server_caps
        }
    };
    assert_eq!(caps_after("renamed", "llama_cpp", "http://one").await, Some(caps.clone()), "same server");
    assert_eq!(caps_after("renamed", "llama_cpp", "http://two").await, None, "another address");
    assert_eq!(caps_after("renamed", "other", "http://two").await, None, "another kind");
}

/// SME-162 review 2: a server's `/props` is kept only while the provider
/// still points at that server: one read before an address change is
/// dropped, not stored for the new address.
#[sqlx::test]
async fn test_server_caps_are_kept_only_for_the_server_they_came_from(pool: PgPool) {
    let caps = serde_json::json!({"total_slots": 4});
    let p = create_inference_provider(&pool, "llama", "llama_cpp", "http://one", "bearer", "k", false, None, true, Some(0))
        .await
        .expect("create");
    update_inference_provider(&pool, p.id, "llama", "llama_cpp", "http://two", "bearer", Some("k"), false, None, true, Some(0))
        .await
        .expect("update")
        .expect("exists");
    let written = set_inference_provider_server_caps(&pool, p.id, "llama_cpp", "http://one", &caps).await.expect("caps");
    assert_eq!(written, None, "read from the old address");
    let stored = get_inference_provider(&pool, p.id).await.expect("get").expect("exists");
    assert_eq!(stored.server_caps, None);
    let written = set_inference_provider_server_caps(&pool, p.id, "llama_cpp", "http://two", &caps)
        .await
        .expect("caps")
        .expect("written");
    assert_eq!(written.server_caps, Some(caps));
    assert!(written.updated_at > stored.updated_at, "a new version");
}

#[sqlx::test]
async fn test_a_providers_debug_hides_its_secret(pool: PgPool) {
    let provider = test_provider(&pool, "p").await;
    let shown = format!("{provider:?}");
    assert!(!shown.contains("sk-ant-0123456789"), "{shown}");
    assert!(shown.contains("anthropic"), "{shown}");
}

#[sqlx::test]
async fn test_deleting_a_provider_clears_the_default_and_conversations_using_it(pool: PgPool) {
    let doomed = test_provider(&pool, "doomed").await;
    let kept = test_provider(&pool, "kept").await;
    set_default_model(&pool, doomed.id, "m1").await.expect("default");
    let on_doomed = create_conversation(&pool).await.expect("conversation");
    let on_kept = create_conversation(&pool).await.expect("conversation");
    set_conversation_model(&pool, on_doomed.id, doomed.id, "m1").await.expect("set");
    set_conversation_model(&pool, on_kept.id, kept.id, "m2").await.expect("set");

    assert!(delete_inference_provider(&pool, doomed.id).await.expect("delete"));

    assert_eq!(get_inference_provider(&pool, doomed.id).await.expect("get"), None);
    assert_eq!(get_default_model(&pool).await.expect("default"), None);
    let cleared = get_conversation_model(&pool, on_doomed.id).await.expect("get").expect("exists");
    assert_eq!((cleared.provider_id, cleared.model), (None, None));
    let untouched = get_conversation_model(&pool, on_kept.id).await.expect("get").expect("exists");
    assert_eq!((untouched.provider_id, untouched.model.as_deref()), (Some(kept.id), Some("m2")));
    assert!(!delete_inference_provider(&pool, doomed.id).await.expect("delete again"));
}

#[sqlx::test]
async fn test_deleting_a_provider_keeps_another_providers_default(pool: PgPool) {
    let doomed = test_provider(&pool, "doomed").await;
    let kept = test_provider(&pool, "kept").await;
    set_default_model(&pool, kept.id, "m2").await.expect("default");

    assert!(delete_inference_provider(&pool, doomed.id).await.expect("delete"));

    assert_eq!(get_default_model(&pool).await.expect("default"), Some((kept.id, "m2".to_string())));
}

#[sqlx::test]
async fn test_setting_a_conversations_model_leaves_the_switch_point_to_its_next_turn(pool: PgPool) {
    let provider = test_provider(&pool, "p").await;
    let conversation = create_conversation(&pool).await.expect("conversation");
    let text = [ContentBlock::Text { text: "hi".to_string() }];
    create_message(&pool, conversation.id, "user", &text).await.expect("message");

    assert!(set_conversation_model(&pool, conversation.id, provider.id, "m1").await.expect("set"));
    let set = get_conversation_model(&pool, conversation.id).await.expect("get").expect("exists");
    assert_eq!(
        set,
        ConversationModelRow {
            provider_id: Some(provider.id),
            model: Some("m1".to_string()),
            model_changed_at_message_id: None,
        },
        "a running turn may still write for the old model, so the pick doesn't mark the switch"
    );
    assert!(!set_conversation_model(&pool, 999_999, provider.id, "m1").await.expect("set"));
}

/// SME-72 review: the switch point is where a turn starts on a
/// different backend than the last turn, recorded under the turn lock.
#[sqlx::test]
async fn test_a_turn_on_a_different_backend_records_the_switch_point(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let text = [ContentBlock::Text { text: "hi".to_string() }];
    let first = create_message(&pool, conversation.id, "user", &text).await.expect("message");

    assert_eq!(record_turn_model(&pool, conversation.id, "1 http://a m1").await.expect("record"), Some(first.id));
    let reply = create_message(&pool, conversation.id, "assistant", &text).await.expect("message");
    assert_eq!(
        record_turn_model(&pool, conversation.id, "1 http://a m1").await.expect("record"),
        Some(first.id),
        "the same backend again: unchanged"
    );
    assert_eq!(
        record_turn_model(&pool, conversation.id, "1 http://b m1").await.expect("record"),
        Some(reply.id),
        "another base URL is another backend"
    );
}

#[sqlx::test]
async fn test_setting_a_deleted_providers_model_is_refused(pool: PgPool) {
    let provider = test_provider(&pool, "p").await;
    let conversation = create_conversation(&pool).await.expect("conversation");
    delete_inference_provider(&pool, provider.id).await.expect("delete");

    assert!(set_conversation_model(&pool, conversation.id, provider.id, "m1").await.is_err());
    assert!(set_default_model(&pool, provider.id, "m1").await.is_err());
}

#[sqlx::test]
async fn test_a_conversation_adopts_the_default_only_when_it_has_no_model(pool: PgPool) {
    let provider = test_provider(&pool, "p").await;
    let conversation = create_conversation(&pool).await.expect("conversation");

    assert_eq!(adopt_default_model(&pool, conversation.id).await.expect("adopt"), None, "no default yet");

    set_default_model(&pool, provider.id, "m1").await.expect("default");
    assert_eq!(
        adopt_default_model(&pool, conversation.id).await.expect("adopt"),
        Some((provider.id, "m1".to_string()))
    );
    let adopted = get_conversation_model(&pool, conversation.id).await.expect("get").expect("exists");
    assert_eq!((adopted.provider_id, adopted.model.as_deref()), (Some(provider.id), Some("m1")));

    set_default_model(&pool, provider.id, "m2").await.expect("default");
    assert_eq!(adopt_default_model(&pool, conversation.id).await.expect("adopt"), None, "it has one now");
    let kept = get_conversation_model(&pool, conversation.id).await.expect("get").expect("exists");
    assert_eq!(kept.model.as_deref(), Some("m1"));
}

/// SME-72 review 2: Ollama reports a window from `/api/ps` only while
/// the model is loaded; asking again when it isn't keeps what it said.
#[sqlx::test]
async fn test_a_report_without_a_window_keeps_the_last_one(pool: PgPool) {
    let provider = test_provider(&pool, "p").await;
    set_provider_model_reported(&pool, provider.id, "m", Some(8192), Some(true), Some(true), Some(4096), None).await.expect("loaded");
    set_provider_model_reported(&pool, provider.id, "m", None, Some(false), Some(true), None, None).await.expect("unloaded");
    let row = get_provider_model(&pool, provider.id, "m").await.expect("get").expect("exists");
    assert_eq!((row.reported_context_window, row.reported_thinking), (Some(8192), Some(false)));
    assert_eq!(row.reported_max_output, Some(4096), "kept like the window (SME-111)");
}

#[sqlx::test]
async fn test_model_overrides_and_reported_details_leave_each_other_alone(pool: PgPool) {
    let provider = test_provider(&pool, "p").await;
    set_provider_model_reported(&pool, provider.id, "m", Some(4096), Some(false), Some(true), Some(1024), None).await.expect("reported");
    set_provider_model_overrides(&pool, provider.id, "m", Some(true), Some(32_768), None, Some(2048), Some(1024)).await.expect("overrides");
    set_provider_model_reported(&pool, provider.id, "m", Some(8192), None, Some(true), Some(4096), None).await.expect("reported");

    let row = get_provider_model(&pool, provider.id, "m").await.expect("get").expect("exists");
    assert_eq!(
        row,
        ProviderModelRow {
            provider_id: provider.id,
            model: "m".to_string(),
            thinking: Some(true),
            context_window: Some(32_768),
            reported_context_window: Some(8192),
            reported_thinking: None,
            reported_tools: Some(true),
            effort: None,
            max_output: Some(2048),
            reported_max_output: Some(4096),
            reasoning_budget: Some(1024),
            reported_adaptive_thinking: None,
            added_by_hand: false,
        }
    );
    assert_eq!(list_provider_models(&pool, provider.id).await.expect("list"), vec![row]);
}

/// SME-162: a provider's max concurrent requests is kept as set: none
/// (no limit), `0` (the count its server reports) or a count; an update
/// sets it outright, and a negative count is refused.
#[sqlx::test]
async fn test_a_providers_max_concurrent_requests_is_kept_as_set(pool: PgPool) {
    let unset = test_provider(&pool, "unset").await;
    assert_eq!(unset.max_concurrent_requests, None, "no limit unless set");
    let two = create_inference_provider(&pool, "two", "llama_cpp", "http://two", "bearer", "k", false, None, true, Some(2))
        .await
        .expect("create");
    assert_eq!(get_inference_provider(&pool, two.id).await.expect("get").expect("exists").max_concurrent_requests, Some(2));
    let auto = create_inference_provider(&pool, "auto", "llama_cpp", "http://auto", "bearer", "k", false, None, true, Some(0))
        .await
        .expect("create");
    assert_eq!(auto.max_concurrent_requests, Some(0), "0 is kept: the server's own count");

    let update = |max| update_inference_provider(&pool, two.id, "two", "llama_cpp", "http://two", "bearer", None, false, None, true, max);
    assert_eq!(update(Some(5)).await.expect("update").expect("exists").max_concurrent_requests, Some(5));
    assert_eq!(update(None).await.expect("update").expect("exists").max_concurrent_requests, None, "cleared");

    let refused = create_inference_provider(&pool, "neg", "other", "http://neg", "bearer", "k", false, None, true, Some(-1)).await;
    assert!(
        matches!(&refused, Err(sqlx::Error::Database(e)) if e.is_check_violation()),
        "a negative count is refused: {refused:?}"
    );
}
