//! Server functions for model providers (SME-72): the `/providers` pages.
//! The per-conversation picker's are in `api::chat`. The logic lives in
//! `crate::providers`, taking the pool so it can be tested.

use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::db;
use crate::providers::{ModelChoice, ModelInfo, PriceSource, ProviderInput, ProviderModels, ProviderSummary};

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

/// Each provider's models' use and cost over the last 30 days, for the
/// providers page (SME-106).
#[get("/api/providers/spend")]
pub async fn get_recent_spend() -> ServerFnResult<Vec<crate::models::ModelSpend>> {
    let since = chrono::Utc::now().naive_utc() - chrono::Duration::days(30);
    db::model_spend_since(db::get(), since)
        .await
        .map_err(ServerFnError::new)
}

/// The price catalog's providers, by name, for the form's "Prices from"
/// choice; empty until the catalog is first fetched (SME-106).
#[get("/api/price-sources")]
pub async fn list_price_sources() -> ServerFnResult<Vec<PriceSource>> {
    Ok(crate::pricing::price_sources())
}

#[delete("/api/providers/{id}")]
pub async fn delete_provider(id: i64) -> ServerFnResult<()> {
    crate::providers::delete_provider(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

/// The provider's models, asking the provider (up to 10 seconds a
/// request). `ask_each`: also ask an Ollama server about each model, for
/// the provider's page; the picker's suggestions skip that.
#[get("/api/providers/{id}/models?ask_each")]
pub async fn list_provider_models(id: i64, ask_each: bool) -> ServerFnResult<ProviderModels> {
    crate::providers::provider_models(db::get(), id, ask_each)
        .await
        .map_err(ServerFnError::new)
}

/// Adds a model the provider's listing doesn't show, keeping any settings
/// it already has.
#[post("/api/providers/{id}/models/add")]
pub async fn add_provider_model(id: i64, model: String) -> ServerFnResult<()> {
    crate::providers::add_model(db::get(), id, &model)
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
    effort: Option<crate::anthropic::Effort>,
) -> ServerFnResult<()> {
    crate::providers::set_model_settings(db::get(), id, &model, thinking, context_window, effort)
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
