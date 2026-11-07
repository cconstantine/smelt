//! The context-window belief: what a turn uses, and how the learned
//! number is saved, cleared and forgotten (SME-439). Split out of
//! `server.rs` under the file-ceiling rule when PR 2 added the Forget
//! endpoint; every item keeps its `providers::…` path through the
//! module's re-export.

use sqlx::PgPool;

use super::super::*;
use super::{positive, providers_changed, TurnModel};
use crate::db;

/// The context window a turn uses and whether anything sized it: the
/// user's override, else what smelt last learned (SME-439), else what
/// the provider reported, else a known `claude-*` model's, else
/// `ASSUMED_CONTEXT_WINDOW`.
pub fn context_window(model: &str, row: Option<&db::ProviderModelRow>) -> (u32, bool) {
    row.and_then(|r| {
        positive(r.context_window)
            .or(positive(r.learned_context_window))
            .or(positive(r.reported_context_window))
    })
        .or_else(|| crate::anthropic::context_window_for(model))
        .map_or((ASSUMED_CONTEXT_WINDOW, false), |window| (window, true))
}

/// Which learner saved a window (SME-439): recorded beside the learned
/// window, so it can always be said where a number came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowSource {
    /// An overflow refusal that named a window.
    Refusal,
    /// A reply the window stopped, measured through the gate in
    /// `turn::overflow::learn_window_stop`.
    WindowStop,
    /// Ollama's `/api/ps` answer for what's loaded.
    OllamaPs,
}

impl WindowSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refusal => "refusal",
            Self::WindowStop => "window_stop",
            Self::OllamaPs => "ollama_ps",
        }
    }
}

/// Saves a window a learner settled on (SME-73, SME-439) for the model
/// this turn ran on — into the learned column, never the user's own — and
/// tells the app, so the providers page and pickers refresh. The meter
/// takes the new window from the turn's own copy, which its caller
/// updates too.
pub async fn learn_context_window(
    pool: &PgPool,
    model: &TurnModel,
    window: u32,
    reason: WindowSource,
    conversation_id: i64,
) -> Result<bool, sqlx::Error> {
    let lowered = db::save_learned_context_window(
        pool,
        model.provider_id,
        &model.endpoint.base_url,
        &model.model,
        window,
        reason.as_str(),
        conversation_id,
        context_window(&model.model, None).0,
    )
    .await?;
    if lowered {
        providers_changed();
    }
    Ok(lowered)
}

/// Forgets what smelt learned about a model's window (SME-439 PR 2): the
/// Forget button's server side. The learned four go; the user's own
/// Context window, the provider's report, and this turn's belief stay
/// exactly where they were.
pub async fn clear_learned_context_window(pool: &PgPool, provider_id: i64, model: &str) -> Result<(), String> {
    let forgotten = db::clear_learned_context_window(pool, provider_id, model)
        .await
        .map_err(|e| e.to_string())?;
    if forgotten {
        providers_changed();
    }
    Ok(())
}

/// A whole reply that filled more than the learned window disproves it
/// (SME-439 PR 2, Expected 3b): what just fit is bigger than what smelt
/// learned, so the learned number goes — one statement, strictly bigger
/// only, the learner's base_url guard and four columns like every other
/// clear. The measured size is the window-stop learner's own sum of the
/// whole call. Called fire-and-forget from the turn loop after the reply
/// is finished: a failed forget proves nothing and stops nothing, so the
/// error is swallowed here rather than reaching `run_turn`'s.
pub async fn learned_window_beaten(pool: &PgPool, model: &TurnModel, usage: &crate::anthropic::TokenUsage) {
    let measured = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    let cleared = db::clear_learned_context_window_below(pool, model.provider_id, &model.endpoint.base_url, &model.model, measured).await;
    if matches!(cleared, Ok(true)) {
        providers_changed();
    }
}
