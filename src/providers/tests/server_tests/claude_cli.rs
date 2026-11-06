//! SME-429: a Claude Code provider is a binary on the server, with no
//! address or key; its models are a static list.

use super::*;

fn claude_code(name: &str, binary: &str) -> ProviderInput {
    ProviderInput {
        kind: ProviderKind::ClaudeCli,
        // What the form can't show for this kind, sent anyway: ignored.
        prompt_caching: true,
        ..input(name, binary, "")
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claude_code_provider_is_created_edited_and_deleted_with_no_key(pool: PgPool) {
    let created = create_provider(&pool, claude_code(" Claude ", "  ")).await.expect("no key needed");
    assert_eq!(created.kind, ProviderKind::ClaudeCli);
    assert_eq!(created.base_url, crate::claude_cli::DEFAULT_BINARY, "blank is claude on the PATH");
    assert_eq!(created.secret_hint, None);
    assert!(!created.prompt_caching, "the CLI marks its own cache breakpoints");

    let moved = update_provider(&pool, created.id, claude_code("Claude", "/opt/claude/bin/claude"))
        .await
        .expect("a new binary needs no key");
    assert_eq!(moved.base_url, "/opt/claude/bin/claude");
    let refused = update_provider(&pool, created.id, claude_code("Claude", "https://api.anthropic.com")).await;
    assert!(refused.is_err_and(|e| e.contains("binary")), "an address isn't a binary");

    delete_provider(&pool, created.id).await.expect("delete");
    assert!(db::get_inference_provider(&pool, created.id).await.expect("get").is_none());
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_provider_moved_to_claude_code_drops_its_key(pool: PgPool) {
    let keyed = create_provider(&pool, ProviderInput { kind: ProviderKind::Anthropic, ..input("A", "https://api.anthropic.com", "sk-ant-api-123456789012") })
        .await
        .expect("create");
    update_provider(&pool, keyed.id, claude_code("A", "claude")).await.expect("to Claude Code");
    let stored = db::get_inference_provider(&pool, keyed.id).await.expect("get").expect("exists");
    assert_eq!(stored.kind, "claude_cli");
    assert!(stored.secret.is_empty(), "the key isn't kept for a kind that never sends it");
    // And back: an address needs its key again.
    let back = update_provider(&pool, keyed.id, ProviderInput { kind: ProviderKind::Anthropic, ..input("A", "https://api.anthropic.com", "") }).await;
    assert!(back.is_err(), "no key to send to the address");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claude_code_providers_models_are_listed_and_picked(pool: PgPool) {
    let created = create_provider(&pool, claude_code("Claude", "claude")).await.expect("create");
    let listing = provider_models(&pool, created.id, true).await.expect("list");
    assert_eq!(listing.listing_error, None, "a static list can't fail");
    let opus = listing.models.iter().find(|m| m.id == "claude-opus-5-5").expect("Opus 5.5 is listed");
    assert_eq!((opus.context_window, opus.max_output), (1_000_000, Some(128_000)));
    assert!(opus.context_window_known && opus.thinking);
    assert_eq!(opus.reported_tools, Some(true));
    set_default_model(&pool, created.id, "claude-opus-5-5").await.expect("pick");
    let choice = default_model(&pool).await.expect("read").expect("set");
    assert_eq!((choice.provider_id, choice.context_window), (created.id, 1_000_000));
}

/// Review 1 L3: the status runs a provider's stored path only for a
/// Claude Code provider.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_cli_status_is_only_for_a_claude_code_provider(pool: PgPool) {
    let ollama = create_provider(&pool, input("Home", "http://ollama:11434", "k")).await.expect("create");
    let refused = crate::claude_cli::provider_status(&pool, ollama.id).await;
    assert!(refused.is_err_and(|e| e.contains("isn't a Claude Code provider")), "another kind's address is never run");
    let missing = crate::claude_cli::provider_status(&pool, ollama.id + 1000).await;
    assert!(missing.is_err_and(|e| e.contains("no longer exists")));
    let claude = create_provider(&pool, claude_code("Claude", "/nonexistent/claude")).await.expect("create");
    let status = crate::claude_cli::provider_status(&pool, claude.id).await.expect("a Claude Code provider's status");
    assert!(status.error.is_some_and(|e| e.contains("/nonexistent/claude")));
}
