use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

/// SME-166: a sign-in's grant is saved only while the row still has the
/// URL the sign-in started for, so a token issued for one address never
/// lands on another after an edit, or on a deleted server.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_save_mcp_server_sign_in_saves_only_onto_the_url_it_started_for(pool: PgPool) {
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
    let stored = |pool: PgPool| async move {
        get_mcp_server_config(&pool, created.id)
            .await
            .expect("get mcp server config")
            .expect("row should exist")
            .oauth_credentials
            .map(|j| j.0)
    };

    let first = serde_json::json!({"client_id": "first"});
    let saved = save_mcp_server_sign_in(&pool, created.id, "https://example.com/mcp", first.clone())
        .await
        .expect("save on the row's own URL");
    assert!(saved, "a save on the row's own URL should match it");
    assert_eq!(stored(pool.clone()).await, Some(first.clone()));

    let saved = save_mcp_server_sign_in(
        &pool,
        created.id,
        "https://example.com/mcp-v2",
        serde_json::json!({"client_id": "for-another-url"}),
    )
    .await
    .expect("save on another URL");
    assert!(!saved, "a save for another URL must match nothing");
    assert_eq!(
        stored(pool.clone()).await,
        Some(first),
        "a save for another URL must leave the stored grant as it was"
    );

    let saved = save_mcp_server_sign_in(
        &pool,
        created.id + 1_000,
        "https://example.com/mcp",
        serde_json::json!({"client_id": "for-a-missing-row"}),
    )
    .await
    .expect("save on a missing id");
    assert!(!saved, "a save for a missing id must match nothing");
}

/// SME-166 review 1: whether an edit changes the URL is decided against
/// the row the edit writes over, not one it read earlier. Interleaving:
/// a rename reads the row at X; another edit (to Y) and a sign-in's save
/// for Y commit; then the rename writes X back. Deciding from its read,
/// the rename kept Y's grant on X.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_edit_racing_a_url_change_still_clears_the_grant(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://x.example/mcp",
        &HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create mcp server config");

    let mut moving = pool.begin().await.expect("begin the move to Y");
    sqlx::query(
        "UPDATE mcp_servers SET url = 'https://y.example/mcp',
             oauth_credentials = '{\"client_id\": \"for-y\"}' WHERE id = $1",
    )
    .bind(created.id)
    .execute(&mut *moving)
    .await
    .expect("move to Y and save Y's grant, uncommitted");

    let rename = tokio::spawn({
        let pool = pool.clone();
        let id = created.id;
        async move {
            update_mcp_server_config(
                &pool,
                id,
                "renamed",
                "https://x.example/mcp",
                &HashMap::new(),
                &[],
                "oauth",
            )
            .await
        }
    });
    // Commit only once the rename has read the row (at X) and is waiting
    // on the move's row lock to write it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the rename never waited on the move");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    moving.commit().await.expect("commit the move");

    let updated = rename
        .await
        .expect("rename task")
        .expect("rename should succeed")
        .expect("row should exist");
    assert_eq!(updated.url, "https://x.example/mcp");
    assert!(
        updated.oauth_credentials.is_none(),
        "Y's grant must not stay on the row once it's back at X"
    );
}

/// SME-166 review 2: a refresh is saved only while the row still has the
/// URL the refreshing connection was built for, as well as the client.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_is_saved_only_onto_the_url_its_connection_was_built_for(pool: PgPool) {
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
    let grant = serde_json::json!({"client_id": "abc"});
    set_mcp_server_oauth_credentials(&pool, created.id, Some(grant.clone()))
        .await
        .expect("set oauth credentials");

    let saved = save_refreshed_mcp_server_oauth_credentials(
        &pool,
        created.id,
        "https://example.com/old-mcp",
        &grant.to_string(),
        "abc",
        serde_json::json!({"client_id": "abc", "refreshed": "for-the-old-url"}),
    )
    .await
    .expect("refresh for another URL");
    assert!(!saved, "a refresh for another URL must match nothing");

    let saved = save_refreshed_mcp_server_oauth_credentials(
        &pool,
        created.id,
        "https://example.com/mcp",
        &grant.to_string(),
        "abc",
        serde_json::json!({"client_id": "abc", "refreshed": true}),
    )
    .await
    .expect("refresh for the row's URL");
    assert!(saved, "a refresh for the row's URL and client should save");
}

/// SME-177 B24: a refresh is saved only over the exact grant it refreshed.
/// A Reconnect with a pre-registered client (GitHub) keeps the client id,
/// so a client-id match alone let the old grant's refresh overwrite the
/// new grant.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_is_saved_only_over_the_grant_it_refreshed(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "oauth",
        Some("gh-client"),
        None,
    )
    .await
    .expect("create mcp server config");
    let old = serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "old"}});
    let new = serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "new"}});
    set_mcp_server_oauth_credentials(&pool, created.id, Some(new.clone()))
        .await
        .expect("the Reconnect's grant");

    let saved = save_refreshed_mcp_server_oauth_credentials(
        &pool,
        created.id,
        "https://example.com/mcp",
        &old.to_string(),
        "gh-client",
        serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "old-refreshed"}}),
    )
    .await
    .expect("refresh of the old grant");
    assert!(!saved, "a refresh of a grant no longer stored must match nothing");
    assert_eq!(stored_grant(&pool, created.id).await, Some(new.clone()), "the new grant stays");

    let refreshed = serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "new-refreshed"}});
    let saved = save_refreshed_mcp_server_oauth_credentials(
        &pool,
        created.id,
        "https://example.com/mcp",
        &new.to_string(),
        "gh-client",
        refreshed.clone(),
    )
    .await
    .expect("refresh of the stored grant");
    assert!(saved, "a refresh of the stored grant saves");
    assert_eq!(stored_grant(&pool, created.id).await, Some(refreshed));
}

async fn stored_grant(pool: &PgPool, id: i64) -> Option<serde_json::Value> {
    get_mcp_server_config(pool, id)
        .await
        .expect("get mcp server config")
        .expect("row should exist")
        .oauth_credentials
        .map(|json| json.0)
}

/// SME-177 B24, the race itself, forced with a row lock: the refresh read
/// X; a Reconnect's save of W (same client) is uncommitted when the
/// refresh's `UPDATE` runs, so Postgres makes it wait, then re-checks its
/// `WHERE` against the committed W.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_racing_a_reconnect_keeps_the_new_grant(pool: PgPool) {
    let created = create_mcp_server_config(
        &pool,
        "github",
        "https://example.com/mcp",
        &HashMap::new(),
        "oauth",
        Some("gh-client"),
        None,
    )
    .await
    .expect("create mcp server config");
    let x = serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "x"}});
    let w = serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "w"}});
    set_mcp_server_oauth_credentials(&pool, created.id, Some(x.clone()))
        .await
        .expect("the grant the refresh reads");

    let mut reconnect = pool.begin().await.expect("begin the Reconnect");
    sqlx::query("UPDATE mcp_servers SET oauth_credentials = $2 WHERE id = $1")
        .bind(created.id)
        .bind(sqlx::types::Json(w.clone()))
        .execute(&mut *reconnect)
        .await
        .expect("save W, uncommitted");

    let mut refresh = tokio::spawn({
        let pool = pool.clone();
        let id = created.id;
        async move {
            save_refreshed_mcp_server_oauth_credentials(
                &pool,
                id,
                "https://example.com/mcp",
                &x.to_string(),
                "gh-client",
                serde_json::json!({"client_id": "gh-client", "token_response": {"access_token": "y"}}),
            )
            .await
        }
    });
    // Commit only once the refresh is waiting on the Reconnect's row lock.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            break;
        }
        assert!(!refresh.is_finished(), "the refresh ended without waiting on the Reconnect");
        assert!(std::time::Instant::now() < deadline, "the refresh never waited on the Reconnect");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    reconnect.commit().await.expect("commit the Reconnect");

    let saved = tokio::time::timeout(std::time::Duration::from_secs(10), &mut refresh)
        .await
        .expect("the refresh finishes once the Reconnect commits")
        .expect("refresh task")
        .expect("refresh");
    assert!(!saved, "the refresh must not save over the Reconnect's grant");
    assert_eq!(stored_grant(&pool, created.id).await, Some(w), "the Reconnect's grant stays");
}
