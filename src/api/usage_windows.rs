//! Server functions for the Claude subscription's usage windows
//! (SME-486): read-only, the latest reading per window with its pace as
//! of the request. `claude_cli::usage` records them.

use dioxus::prelude::*;

use crate::claude_cli::usage::ProviderUsage;
#[cfg(feature = "server")]
use crate::db;
#[cfg(feature = "server")]
use sqlx::PgPool;

/// A provider's windows; empty when it has no reading yet. Served
/// whatever the provider's kind: the UI shows them for Claude Code only.
#[get("/api/providers/{id}/usage")]
pub async fn get_provider_usage(id: i64) -> ServerFnResult<ProviderUsage> {
    provider_usage(db::get(), id).await
}

/// `get_provider_usage`'s body, taking the pool so tests can call it.
#[cfg(feature = "server")]
pub(crate) async fn provider_usage(pool: &PgPool, id: i64) -> ServerFnResult<ProviderUsage> {
    if db::get_inference_provider(pool, id).await.map_err(ServerFnError::new)?.is_none() {
        return Err(ServerFnError::new("That provider no longer exists."));
    }
    let rows = db::provider_usage_windows(pool, id).await.map_err(ServerFnError::new)?;
    Ok(ProviderUsage::from_rows(id, rows, chrono::Utc::now()))
}

/// The windows of the provider `id`'s next turn runs on, for the chat's
/// indicator: `None` unless that provider is a Claude Code one.
#[get("/api/conversations/{id}/usage-windows")]
pub async fn get_conversation_usage_windows(id: i64) -> ServerFnResult<Option<ProviderUsage>> {
    conversation_usage_windows(db::get(), id).await
}

/// `get_conversation_usage_windows`'s body, taking the pool.
#[cfg(feature = "server")]
pub(crate) async fn conversation_usage_windows(pool: &PgPool, id: i64) -> ServerFnResult<Option<ProviderUsage>> {
    let choice = crate::providers::conversation_model(pool, id).await.map_err(ServerFnError::new)?;
    let Some(provider_id) = choice.as_ref().and_then(crate::providers::ConversationModel::choice).map(|c| c.provider_id) else {
        return Ok(None);
    };
    let claude = db::get_inference_provider(pool, provider_id)
        .await
        .map_err(ServerFnError::new)?
        .is_some_and(|p| crate::providers::ProviderKind::parse(&p.kind) == Some(crate::providers::ProviderKind::ClaudeCli));
    if !claude {
        return Ok(None);
    }
    provider_usage(pool, provider_id).await.map(Some)
}
