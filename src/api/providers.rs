//! Server functions for model providers (SME-72): the `/providers` pages.
//! The per-conversation picker's are in `api::chat`. The logic lives in
//! `crate::providers`, taking the pool so it can be tested.

use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::db;
use crate::providers::{ModelChoice, ModelInfo, ProviderInput, ProviderModels, ProviderSummary};

#[get("/api/providers")]
pub async fn list_providers() -> ServerFnResult<Vec<ProviderSummary>> {
    let providers = db::list_inference_providers(db::get())
        .await
        .map_err(ServerFnError::new)?;
    Ok(providers.into_iter().map(ProviderSummary::from).collect())
}

#[get("/api/providers/{id}")]
pub async fn get_provider(id: i64) -> ServerFnResult<ProviderSummary> {
    db::get_inference_provider(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .map(ProviderSummary::from)
        .ok_or_else(|| ServerFnError::new("That provider no longer exists."))
}

#[post("/api/providers")]
pub async fn create_provider(input: ProviderInput) -> ServerFnResult<ProviderSummary> {
    crate::providers::create_provider(db::get(), input)
        .await
        .map_err(ServerFnError::new)
}

/// Saves the edit form; an empty secret keeps the stored one.
#[post("/api/providers/{id}")]
pub async fn update_provider(id: i64, input: ProviderInput) -> ServerFnResult<ProviderSummary> {
    crate::providers::update_provider(db::get(), id, input)
        .await
        .map_err(ServerFnError::new)
}

#[delete("/api/providers/{id}")]
pub async fn delete_provider(id: i64) -> ServerFnResult<()> {
    crate::providers::delete_provider(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

/// The provider's models, asking the provider (up to 10 seconds a request).
#[get("/api/providers/{id}/models")]
pub async fn list_provider_models(id: i64) -> ServerFnResult<ProviderModels> {
    crate::providers::provider_models(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

/// Asks the provider about one model, for the picker when it's chosen.
#[post("/api/providers/{id}/models/refresh")]
pub async fn refresh_model_details(id: i64, model: String) -> ServerFnResult<ModelInfo> {
    crate::providers::refresh_model_details(db::get(), id, &model)
        .await
        .map_err(ServerFnError::new)
}

/// The user's overrides for a model; `None` goes back to what the
/// provider says.
#[post("/api/providers/{id}/models/settings")]
pub async fn set_model_settings(
    id: i64,
    model: String,
    thinking: Option<bool>,
    context_window: Option<u32>,
) -> ServerFnResult<()> {
    crate::providers::set_model_settings(db::get(), id, &model, thinking, context_window)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/default-model")]
pub async fn get_default_model() -> ServerFnResult<Option<ModelChoice>> {
    crate::providers::default_model(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/default-model")]
pub async fn set_default_model(provider_id: i64, model: String) -> ServerFnResult<()> {
    crate::providers::set_default_model(db::get(), provider_id, &model)
        .await
        .map_err(ServerFnError::new)
}
