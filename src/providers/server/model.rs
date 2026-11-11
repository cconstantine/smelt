//! The model rows: a provider's listing and what it reports about each
//! model, the llama.cpp `/props` read, and the user's per-model overrides.
//! Split out of `mod.rs` under the file-ceiling rule (SME-423); every item
//! keeps its `providers::…` path through the module's re-export.

use sqlx::PgPool;

use super::super::*;
use super::{endpoint, model_info, provider_gone, providers_changed, server_caps, to_column};
use crate::anthropic::stream::Endpoint;
use crate::db;

pub async fn store_details(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    details: &crate::anthropic::models::ModelDetails,
) -> Result<(), String> {
    db::set_provider_model_reported(
        pool,
        provider_id,
        model,
        to_column(details.context_window),
        details.thinking,
        details.tools,
        to_column(details.max_output),
        details.adaptive_thinking,
    )
    .await
    .map_err(|e| e.to_string())?;
    // SME-439 PR 2 (Expected 3a): a report bigger than what smelt learned
    // says the learned window is too small — forget it. A llama.cpp report
    // is the server's whole context split across its slots, so what must
    // outgrow is one slot's share, the learner's own division. The picker
    // lists with the last `/props` unrefreshed, so its llama.cpp report can
    // over-clear — a lost re-learn, never a wrong keep (the plan's noted
    // trade-off).
    if details.context_window.filter(|w| *w > 0).is_some()
        && let Some(provider) = db::get_inference_provider(pool, provider_id).await.map_err(|e| e.to_string())?
    {
        let kind = ProviderKind::parse(&provider.kind).unwrap_or(ProviderKind::Other);
        let total_slots = server_caps(&provider).and_then(|caps| caps.total_slots);
        if let Some(per_request) = crate::turn::reported_per_request(to_column(details.context_window), kind, total_slots) {
            let forgotten = db::clear_learned_context_window_below(pool, provider_id, &provider.base_url, model, per_request)
                .await
                .map_err(|e| e.to_string())?;
            if forgotten {
                providers_changed();
                // Round 1 H1: a clear that cleared speaks on window_learn
                // — decision `bigger_report`, the report and its
                // per-request share, and no conversation id: a listing
                // has none. Fired on the UPDATE's answer, not the attempt.
                let reported = to_column(details.context_window).map(i64::from);
                tracing::info!(model = %model, reported = reported.unwrap_or_default(), reported_per_request = per_request, decision = "bigger_report", "learned context window forgotten: a report outgrew it");
                trace_window_learn(None, model, "listing", "bigger_report", per_request, None, reported, Some(per_request));
            }
        }
    }
    Ok(())
}

fn has_any(details: &crate::anthropic::models::ModelDetails) -> bool {
    details.context_window.is_some()
        || details.thinking.is_some()
        || details.tools.is_some()
        || details.max_output.is_some()
        || details.adaptive_thinking.is_some()
}

/// How many of an Ollama server's models to ask about at once.
const DETAIL_REQUESTS_AT_ONCE: usize = 4;

/// A provider's models: its listing (storing what it reports about
/// each), plus every model with stored settings the listing lacks (one
/// typed by hand, say). With `ask_each`, an Ollama server is also asked
/// about each listed model (the provider's page); without, what's
/// stored is used (the picker's suggestions, opened often). A listing
/// that fails still returns the stored models, with the error.
pub async fn provider_models(
    pool: &PgPool,
    provider_id: i64,
    ask_each: bool,
) -> Result<ProviderModels, String> {
    let provider = db::get_inference_provider(pool, provider_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("That provider no longer exists.")?;
    let kind = ProviderKind::parse(&provider.kind).unwrap_or(ProviderKind::Other);
    let endpoint = endpoint(&provider);
    let (mut listed, listing_error) =
        match crate::anthropic::models::list_models(kind, &endpoint).await {
            Ok(listed) => (listed, None),
            Err(e) => (Vec::new(), Some(e)),
        };
    let (server_caps, server_error) = if kind == ProviderKind::LlamaCpp && ask_each {
        read_llama_props(pool, &provider, &endpoint, &mut listed).await?
    } else {
        let stored = server_caps(&provider);
        // What `/props` said last still wins over `/v1/models`' whole-
        // server window and missing tool flag (SME-111 review 2).
        if kind == ProviderKind::LlamaCpp
            && let Some(info) = &stored
        {
            crate::anthropic::models::apply_llama_props(&mut listed, info);
        }
        (stored, None)
    };

    let mut details_errors = std::collections::HashMap::new();
    if kind == ProviderKind::Ollama && ask_each {
        let ids = listed.iter().map(|m| m.id.clone()).collect();
        let fetched =
            crate::anthropic::models::ollama_details(&endpoint, ids, DETAIL_REQUESTS_AT_ONCE).await;
        for (model, details) in fetched {
            match details {
                Ok(details) => store_details(pool, provider_id, &model, &details).await?,
                Err(e) => {
                    details_errors.insert(model, e);
                }
            }
        }
    } else {
        for model in listed.iter().filter(|m| has_any(&m.details)) {
            store_details(pool, provider_id, &model.id, &model.details).await?;
        }
    }

    let mut stored: std::collections::BTreeMap<String, db::ProviderModelRow> =
        db::list_provider_models(pool, provider_id)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|row| (row.model.clone(), row))
            .collect();
    let mut models: Vec<ModelInfo> = listed
        .into_iter()
        .map(|m| {
            let row = stored.remove(&m.id);
            let error = details_errors.remove(&m.id);
            model_info(kind, m.id, m.display_name, row.as_ref(), error)
        })
        .collect();
    // The rest of what's stored: a model added by hand or given a
    // setting always, one only ever seen in a listing only while the
    // listing can't be had (a model the provider dropped goes away).
    let listing_failed = listing_error.is_some();
    models.extend(
        stored
            .into_values()
            .filter(|row| {
                listing_failed
                    || row.added_by_hand
                    || row.thinking.is_some()
                    || row.context_window.is_some()
                    || row.effort.is_some()
                    || row.max_output.is_some()
                    || row.reasoning_budget.is_some()
            })
            .map(|row| model_info(kind, row.model.clone(), None, Some(&row), None)),
    );
    Ok(ProviderModels { models, listing_error, kind, server_caps, server_error })
}

/// Reads a llama.cpp server's `/props`, keeps it on the provider and
/// applies its window and tool support to `listed`. A failure keeps
/// what was read before and says why.
async fn read_llama_props(
    pool: &PgPool,
    provider: &db::InferenceProvider,
    endpoint: &Endpoint,
    listed: &mut [crate::anthropic::models::ListedModel],
) -> Result<(Option<LlamaServerInfo>, Option<String>), String> {
    match crate::anthropic::models::llama_cpp_props(endpoint).await {
        Ok(info) => {
            crate::anthropic::models::apply_llama_props(listed, &info);
            let value = serde_json::to_value(&info).map_err(|e| e.to_string())?;
            let written =
                db::set_inference_provider_server_caps(pool, provider.id, &provider.kind, &provider.base_url, &value)
                    .await
                    .map_err(|e| e.to_string())?;
            // An automatic limit takes the new count from each running
            // turn's next model call (SME-162 review 1), from the row as
            // written: a limit saved while this was read stands (review 2).
            if let Some(row) = written {
                limiter::set(row.id, row.updated_at, resolved_max(&row));
            }
            Ok((Some(info), None))
        }
        Err(e) => Ok((server_caps(provider), Some(e))),
    }
}

/// Asks the provider about one model and stores what it says. A
/// failure keeps what was stored and says why.
pub async fn refresh_model_details(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
) -> Result<ModelInfo, String> {
    let model = model.trim();
    let provider = db::get_inference_provider(pool, provider_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("That provider no longer exists.")?;
    let kind = ProviderKind::parse(&provider.kind).unwrap_or(ProviderKind::Other);
    let details = match kind {
        // The listing and `/props` together (SME-111).
        ProviderKind::LlamaCpp => {
            let endpoint = endpoint(&provider);
            match crate::anthropic::models::list_models(kind, &endpoint).await {
                Ok(mut listed) => {
                    read_llama_props(pool, &provider, &endpoint, &mut listed).await?;
                    Ok(listed.into_iter().find(|m| m.id == model).map(|m| m.details).unwrap_or_default())
                }
                Err(e) => Err(e),
            }
        }
        _ => crate::anthropic::models::model_details(kind, &endpoint(&provider), model).await,
    };
    let error = match details {
        Ok(details) => {
            if has_any(&details) {
                store_details(pool, provider_id, model, &details).await?;
            }
            None
        }
        Err(e) => Some(e),
    };
    let row = db::get_provider_model(pool, provider_id, model)
        .await
        .map_err(|e| e.to_string())?;
    Ok(model_info(kind, model.to_string(), None, row.as_ref(), error))
}

/// Adds a model the listing doesn't show, keeping any settings it has.
pub async fn add_model(pool: &PgPool, provider_id: i64, model: &str) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Enter the model's id.".to_string());
    }
    db::ensure_provider_model(pool, provider_id, model)
        .await
        .map_err(provider_gone)?;
    providers_changed();
    Ok(())
}

/// A token count a model's row sets, checked: at least `min`.
fn token_count(value: Option<u32>, min: i32, error: &str) -> Result<Option<i32>, String> {
    value
        .map(|n| i32::try_from(n).ok().filter(|n| *n >= min).ok_or_else(|| error.to_string()))
        .transpose()
}

/// Sets the user's overrides for a model (`ModelSettings`). `None`
/// goes back to what the provider says.
pub async fn set_model_settings(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    settings: ModelSettings,
) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Enter the model's id.".to_string());
    }
    let context_window = token_count(
        settings.context_window,
        1024,
        "A context window is a number of tokens, at least 1024.",
    )?;
    let max_output = token_count(
        settings.max_output,
        1024,
        "Max reply tokens is a number of tokens, at least 1024.",
    )?;
    let reasoning_budget = token_count(
        settings.reasoning_budget,
        1,
        "A reasoning budget is a number of tokens, at least 1.",
    )?;
    db::set_provider_model_overrides(
        pool,
        provider_id,
        model,
        settings.thinking,
        context_window,
        settings.effort.map(|e| e.as_str()),
        max_output,
        reasoning_budget,
    )
    .await
    .map_err(provider_gone)?;
    // Every save forgets what smelt learned about this model's window
    // (SME-423): a raise on the server itself moves no listing and no
    // learner, and this row's Save is the user saying they just looked.
    super::window::forget_learned_on_model_save(pool, provider_id, model).await;
    providers_changed();
    Ok(())
}
