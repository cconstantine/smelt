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

/// One `window_learn` span per decision about a model's window
/// (SME-439's Expected 6, and PR 2's two self-clears): a records-carrier,
/// ended on the spot, never a duration. Every field is a number, an id,
/// or a fixed enum — no reply text, no model output, no URLs (SME-137's
/// rule) — so the incident can be read off a trace without having sat in
/// the conversation. Lives beside the columns it reports on so the
/// self-clears — `window_beaten` and `bigger_report`, round 1 H1: "why
/// did mine vanish?" gets answered here or nowhere — speak on the same
/// span as the learners; `turn::overflow` writes through it. `value` is
/// what was, would be, or disproved the claim; the clears carry no
/// `conversation_id` — neither has one in hand, and neither reaches.
#[allow(clippy::too_many_arguments)]
pub fn trace_window_learn(
    conversation_id: Option<i64>,
    model: &str,
    source: &str,
    decision: &str,
    value: i64,
    usage: Option<&crate::anthropic::TokenUsage>,
    reported: Option<i64>,
    reported_per_request: Option<i64>,
) {
    let (measured_input, measured_output, cache_creation, cache_read) = match usage {
        Some(u) => (
            Some(u.input_tokens),
            Some(u.output_tokens),
            Some(u.cache_creation_input_tokens),
            Some(u.cache_read_input_tokens),
        ),
        None => (None, None, None, None),
    };
    tracing::info_span!(
        "window_learn",
        conversation_id,
        smelt.model = model,
        smelt.window.source = source,
        smelt.window.decision = decision,
        smelt.window.value = value,
        smelt.window.measured_input = measured_input,
        smelt.window.measured_output = measured_output,
        smelt.window.cache_creation = cache_creation,
        smelt.window.cache_read = cache_read,
        smelt.window.reported = reported,
        smelt.window.reported_per_request = reported_per_request,
    )
    .in_scope(|| {});
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

/// A save of the model's row forgets what smelt learned about its window
/// (SME-423): a raise made on the server itself moves no listing and no
/// learner, and the row's Save is the user's word that they just looked.
/// Clears through the Forget button's own statement — all four learned
/// columns together, the user's override and the report nobody's to
/// touch — and says so on `window_learn` (source `settings`, decision
/// `user_save`, value the number that went) only when the UPDATE really
/// cleared: fired on the answer, never on the attempt, like
/// `bigger_report` and `window_beaten`. Called between the overrides
/// write and `providers_changed()`, so the caller's notice covers the
/// clear; a failed read or clear proves nothing and fails no save.
pub async fn forget_learned_on_model_save(pool: &PgPool, provider_id: i64, model: &str) {
    let Ok(Some(row)) = db::get_provider_model(pool, provider_id, model).await else { return };
    let Some(learned) = positive(row.learned_context_window) else { return };
    if db::clear_learned_context_window(pool, provider_id, model).await.unwrap_or(false) {
        trace_window_learn(None, model, "settings", "user_save", i64::from(learned), None, None, None);
    }
}

/// A whole reply that filled more than the learned window disproves it
/// (SME-439 PR 2, Expected 3b): what just fit is bigger than what smelt
/// learned, so the learned number goes — one statement, strictly bigger
/// only, the learner's base_url guard and four columns like every other
/// clear. The measured size is the window-stop learner's own sum of the
/// whole call. Called fire-and-forget from the turn loop after the reply
/// is finished: a failed forget proves nothing and stops nothing, so the
/// error is swallowed here rather than reaching `run_turn`'s. When the
/// UPDATE really clears (round 1 H1) the clear says so on `window_learn`
/// with an info line — fired on the answer, never on the attempt, since
/// a no-op proves nothing.
pub async fn learned_window_beaten(pool: &PgPool, model: &TurnModel, usage: &crate::anthropic::TokenUsage) {
    let measured = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    let cleared = db::clear_learned_context_window_below(pool, model.provider_id, &model.endpoint.base_url, &model.model, measured).await;
    if matches!(cleared, Ok(true)) {
        providers_changed();
        tracing::info!(model = %model.model, measured, decision = "window_beaten", "learned context window forgotten: a whole reply measured past it");
        trace_window_learn(None, &model.model, "reply", "window_beaten", measured, Some(usage), None, None);
    }
}
