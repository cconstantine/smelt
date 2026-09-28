//! The `/language-servers` page's server functions (SME-35).

use dioxus::prelude::*;

use crate::models::{LanguageServer, LanguageServerConfig};

#[cfg(feature = "server")]
use crate::db;

#[get("/api/language-servers")]
pub async fn list_language_servers() -> ServerFnResult<Vec<LanguageServer>> {
    db::list_language_servers(db::get()).await.map_err(ServerFnError::new)
}

#[get("/api/language-servers/{id}")]
pub async fn get_language_server(id: i64) -> ServerFnResult<LanguageServer> {
    db::get_language_server(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("That language server no longer exists."))
}

/// Creates a language server. A server already running for a conversation
/// isn't affected by later edits until it's restarted.
#[post("/api/language-servers")]
pub async fn create_language_server(config: LanguageServerConfig) -> ServerFnResult<LanguageServer> {
    crate::lsp::config::save(db::get(), None, &config).await.map_err(ServerFnError::new)
}

#[post("/api/language-servers/{id}")]
pub async fn update_language_server(id: i64, config: LanguageServerConfig) -> ServerFnResult<LanguageServer> {
    crate::lsp::config::save(db::get(), Some(id), &config).await.map_err(ServerFnError::new)
}

#[delete("/api/language-servers/{id}")]
pub async fn delete_language_server(id: i64) -> ServerFnResult<()> {
    db::delete_language_server(db::get(), id).await.map_err(ServerFnError::new)?;
    Ok(())
}
