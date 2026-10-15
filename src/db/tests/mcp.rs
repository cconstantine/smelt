use super::*;

#[sqlx::test]
async fn test_create_list_get_delete_mcp_server_config_round_trip(pool: PgPool) {
    let headers = HashMap::from([("Authorization".to_string(), "Bearer secret".to_string())]);
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://api.githubcopilot.com/mcp/",
        &headers,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");
    assert_eq!(created.name, "github");
    assert_eq!(created.extra_headers.0, headers);

    let all = list_mcp_server_configs(&pool)
        .await
        .expect("list mcp server configs");
    assert!(all.iter().any(|s| s.id == created.id));

    let fetched = get_mcp_server_config(&pool, created.id)
        .await
        .expect("get mcp server config")
        .expect("row should exist");
    assert_eq!(fetched.url, "https://api.githubcopilot.com/mcp/");

    delete_mcp_server_config(&pool, created.id)
        .await
        .expect("delete mcp server config");
    let after_delete = get_mcp_server_config(&pool, created.id)
        .await
        .expect("get should still succeed");
    assert!(after_delete.is_none(), "row should be gone after delete");
}

#[sqlx::test]
async fn test_update_mcp_server_config_renames_without_touching_headers(pool: PgPool) {
    let original_headers = HashMap::from([(
        "Authorization".to_string(),
        "Bearer original-token".to_string(),
    )]);
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &original_headers,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let updated = update_mcp_server_config(
        &pool,
        created.id,
        "github-renamed",
        "https://example.com/mcp-v2",
        &HashMap::new(),
        &[],
        "static_headers",
    )
    .await
    .expect("update should succeed")
    .expect("row should exist");

    assert_eq!(updated.name, "github-renamed");
    assert_eq!(updated.url, "https://example.com/mcp-v2");
    assert_eq!(
        updated.extra_headers.0, original_headers,
        "an empty upsert/remove must never touch stored headers"
    );
}

#[sqlx::test]
async fn test_get_mcp_server_config_by_name_round_trips(pool: PgPool) {
    let headers = HashMap::new();
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://api.githubcopilot.com/mcp/",
        &headers,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let fetched = get_mcp_server_config_by_name(&pool, "github")
        .await
        .expect("query should succeed")
        .expect("row should exist");
    assert_eq!(fetched.id, created.id);

    let missing = get_mcp_server_config_by_name(&pool, "does-not-exist")
        .await
        .expect("query should succeed");
    assert!(missing.is_none());
}

#[sqlx::test]
async fn test_update_mcp_server_config_upserts_headers_without_touching_others(pool: PgPool) {
    let original = HashMap::from([
        ("Authorization".to_string(), "Bearer old".to_string()),
        ("X-Untouched".to_string(), "keep-me".to_string()),
    ]);
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &original,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let upsert = HashMap::from([
        ("Authorization".to_string(), "Bearer new".to_string()),
        ("X-New".to_string(), "brand-new".to_string()),
    ]);
    let updated = update_mcp_server_config(
        &pool,
        created.id,
        "github",
        "https://example.com/mcp",
        &upsert,
        &[],
        "static_headers",
    )
    .await
    .expect("update should succeed")
    .expect("row should exist");

    assert_eq!(
        updated.extra_headers.0,
        HashMap::from([
            ("Authorization".to_string(), "Bearer new".to_string()),
            ("X-Untouched".to_string(), "keep-me".to_string()),
            ("X-New".to_string(), "brand-new".to_string()),
        ]),
        "upsert should update/add given keys and leave every other existing header alone"
    );
}

#[sqlx::test]
async fn test_update_mcp_server_config_removes_named_headers(pool: PgPool) {
    let original = HashMap::from([
        ("Authorization".to_string(), "Bearer old".to_string()),
        ("X-Doomed".to_string(), "bye".to_string()),
    ]);
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &original,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let updated = update_mcp_server_config(
        &pool,
        created.id,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        &["X-Doomed".to_string()],
        "static_headers",
    )
    .await
    .expect("update should succeed")
    .expect("row should exist");

    assert_eq!(
        updated.extra_headers.0,
        HashMap::from([("Authorization".to_string(), "Bearer old".to_string())]),
        "the removed header must be gone, everything else untouched"
    );
}

#[sqlx::test]
async fn test_update_mcp_server_config_upsert_wins_over_remove_for_the_same_name(pool: PgPool) {
    let original = HashMap::from([("Authorization".to_string(), "Bearer old".to_string())]);
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &original,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let upsert = HashMap::from([("Authorization".to_string(), "Bearer new".to_string())]);
    let updated = update_mcp_server_config(
        &pool,
        created.id,
        "github",
        "https://example.com/mcp",
        &upsert,
        &["Authorization".to_string()],
        "static_headers",
    )
    .await
    .expect("update should succeed")
    .expect("row should exist");

    assert_eq!(
        updated.extra_headers.0,
        HashMap::from([("Authorization".to_string(), "Bearer new".to_string())]),
        "a name present in both upsert and remove should end up set, not removed"
    );
}

#[sqlx::test]
async fn test_update_mcp_server_config_returns_none_for_unknown_id(pool: PgPool) {
    let result = update_mcp_server_config(
        &pool,
        999_999,
        "name",
        "https://example.com",
        &HashMap::new(),
        &[],
        "static_headers",
    )
    .await
    .expect("query should succeed");
    assert!(result.is_none());
}

#[sqlx::test]
async fn test_ensure_mcp_server_inserts_a_missing_server_once(pool: PgPool) {
    let inserted = ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
        .await
        .expect("ensure mcp server");
    assert!(inserted, "the first call should insert");
    let again = ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
        .await
        .expect("ensure mcp server again");
    assert!(!again, "the second call should insert nothing");

    let configs = list_mcp_server_configs(&pool).await.expect("list");
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].name, "exa");
    assert_eq!(configs[0].url, "https://mcp.example.com/mcp");
    assert_eq!(configs[0].auth_mode, "static_headers");
    assert!(configs[0].extra_headers.0.is_empty());
}

#[sqlx::test]
async fn test_ensure_mcp_server_leaves_an_edited_entry_alone(pool: PgPool) {
    let headers = HashMap::from([("x-api-key".to_string(), "my-key".to_string())]);
    let existing = create_mcp_server_config(
        &pool,
        "exa",
        "https://edited.example.com/mcp",
        &headers,
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let inserted = ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
        .await
        .expect("ensure mcp server");
    assert!(!inserted);

    let after = get_mcp_server_config(&pool, existing.id)
        .await
        .expect("get")
        .expect("the entry should still exist");
    assert_eq!(after.url, "https://edited.example.com/mcp");
    assert_eq!(after.extra_headers.0, headers);
    assert_eq!(after.updated_at, existing.updated_at);
}

#[sqlx::test]
async fn test_create_mcp_server_config_defaults_auth_mode_to_static_headers(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create mcp server config");
    assert_eq!(created.auth_mode, "static_headers");
    assert!(created.oauth_credentials.is_none());
}

#[sqlx::test]
async fn test_create_mcp_server_config_stores_a_preregistered_oauth_client(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://api.githubcopilot.com/mcp/",
        &HashMap::new(),
        "oauth",
        Some("client-123"),
        Some("secret-456"),
    )
    .await
    .expect("create mcp server config");
    assert_eq!(created.oauth_client_id.as_deref(), Some("client-123"));
    assert_eq!(created.oauth_client_secret.as_deref(), Some("secret-456"));
}

#[sqlx::test]
async fn test_create_mcp_server_config_oauth_mode_round_trips(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create mcp server config");
    assert_eq!(created.auth_mode, "oauth");
}

#[sqlx::test]
async fn test_set_mcp_server_oauth_credentials_round_trips(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let credentials =
        serde_json::json!({"client_id": "abc", "token_response": null, "granted_scopes": []});
    set_mcp_server_oauth_credentials(&pool, created.id, Some(credentials.clone()))
        .await
        .expect("set oauth credentials");

    let fetched = get_mcp_server_config(&pool, created.id)
        .await
        .expect("get mcp server config")
        .expect("row should exist");
    assert_eq!(fetched.oauth_credentials.map(|j| j.0), Some(credentials));

    set_mcp_server_oauth_credentials(&pool, created.id, None)
        .await
        .expect("clear oauth credentials");
    let cleared = get_mcp_server_config(&pool, created.id)
        .await
        .expect("get mcp server config")
        .expect("row should exist");
    assert!(
        cleared.oauth_credentials.is_none(),
        "None should clear the stored credentials"
    );
}

#[sqlx::test]
async fn test_update_mcp_server_config_clears_oauth_credentials_when_url_changes(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create mcp server config");
    set_mcp_server_oauth_credentials(
        &pool,
        created.id,
        Some(serde_json::json!({"client_id": "abc"})),
    )
    .await
    .expect("set oauth credentials");

    // Changing the URL invalidates a grant that was issued for the old
    // audience — see SME-16's "Data model."
    let updated = update_mcp_server_config(
        &pool,
        created.id,
        "github",
        "https://example.com/mcp-v2",
        &HashMap::new(),
        &[],
        "oauth",
    )
    .await
    .expect("update should succeed")
    .expect("row should exist");

    assert!(
        updated.oauth_credentials.is_none(),
        "a URL change must clear stored OAuth credentials"
    );
}

#[sqlx::test]
async fn test_update_mcp_server_config_keeps_oauth_credentials_when_url_is_unchanged(
    pool: PgPool,
) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create mcp server config");
    let credentials = serde_json::json!({"client_id": "abc"});
    set_mcp_server_oauth_credentials(&pool, created.id, Some(credentials.clone()))
        .await
        .expect("set oauth credentials");

    let updated = update_mcp_server_config(
        &pool,
        created.id,
        "github-renamed",
        "https://example.com/mcp",
        &HashMap::new(),
        &[],
        "oauth",
    )
    .await
    .expect("update should succeed")
    .expect("row should exist");

    assert_eq!(
        updated.oauth_credentials.map(|j| j.0),
        Some(credentials),
        "renaming without changing the URL must not disturb stored OAuth credentials"
    );
}
