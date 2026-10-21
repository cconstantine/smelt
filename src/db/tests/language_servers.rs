use super::*;

/// SME-35: a language server's settings go in and come back whole,
/// and a second server can't take a name that's in use.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_language_servers_round_trip(pool: PgPool) {
    let mut config = crate::models::LanguageServerConfig {
        name: "pyright".to_string(),
        image: "node:22-slim".to_string(),
        install_command: "npm install --prefix $HOME/lsp pyright".to_string(),
        command: "pyright-langserver".to_string(),
        args: vec!["--stdio".to_string()],
        env: [("NODE_OPTIONS".to_string(), "--max-old-space-size=1024".to_string())].into(),
        file_types: [("py".to_string(), "python".to_string()), ("pyi".to_string(), "python".to_string())].into(),
        root_markers: vec!["pyproject.toml".to_string()],
        initialization_options: None,
        settings: Some(serde_json::json!({"python": {"analysis": {"typeCheckingMode": "basic"}}})),
        memory_limit: "1Gi".to_string(),
        cpu_limit: "1".to_string(),
        enabled: true,
    };
    let created = create_language_server(&pool, &config).await.expect("create");
    assert_eq!(created.config, config);
    assert_eq!(get_language_server(&pool, created.id).await.expect("get"), Some(created.clone()));

    let duplicate = create_language_server(&pool, &config).await;
    assert!(duplicate.as_ref().is_err_and(|e| e.as_database_error().is_some_and(|d| d.is_unique_violation())));

    config.enabled = false;
    config.args.push("--verbose".to_string());
    let updated = update_language_server(&pool, created.id, &config).await.expect("update").expect("exists");
    assert_eq!(updated.config, config);
    assert_eq!(list_language_servers(&pool).await.expect("list"), vec![updated.clone()]);

    assert_eq!(delete_language_server(&pool, created.id).await.expect("delete").map(|s| s.id), Some(created.id));
    assert!(list_language_servers(&pool).await.expect("list").is_empty());
    assert_eq!(update_language_server(&pool, created.id, &config).await.expect("update"), None);
}
