use sqlx::PgPool;

use super::super::*;
use crate::anthropic::stream::{Auth, Endpoint};
use crate::db;

/// Whether `event` is among what `rx` has received so far. The
/// buses are process-wide, so other tests' events may come first.
fn received<T: Clone + PartialEq>(rx: &mut tokio::sync::broadcast::Receiver<T>, event: &T) -> bool {
    let mut seen = false;
    loop {
        match rx.try_recv() {
            Ok(e) => seen |= &e == event,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => return seen,
        }
    }
}

fn row(
    thinking: Option<bool>,
    context_window: Option<i32>,
    reported_context_window: Option<i32>,
    reported_thinking: Option<bool>,
) -> db::ProviderModelRow {
    db::ProviderModelRow {
        provider_id: 1,
        model: "m".to_string(),
        thinking,
        context_window,
        reported_context_window,
        reported_thinking,
        reported_tools: None,
        effort: None,
        max_output: None,
        reported_max_output: None,
        reasoning_budget: None,
        reported_adaptive_thinking: None,
        added_by_hand: false,
    }
}

fn settings(
    thinking: Option<bool>,
    context_window: Option<u32>,
    effort: Option<crate::anthropic::Effort>,
) -> ModelSettings {
    ModelSettings { thinking, context_window, effort, ..Default::default() }
}

async fn provider(pool: &PgPool, name: &str, auth_kind: &str) -> db::InferenceProvider {
    db::create_inference_provider(pool, name, "ollama", "http://ollama:11434", auth_kind, "the-secret", false, None, true)
        .await
        .expect("create provider")
}

fn input(name: &str, base_url: &str, secret: &str) -> ProviderInput {
    ProviderInput {
        name: name.to_string(),
        kind: ProviderKind::Ollama,
        base_url: base_url.to_string(),
        auth_kind: AuthKind::ApiKey,
        secret: secret.to_string(),
        prompt_caching: false,
        price_catalog_provider: None,
        keep_reasoning: true,
    }
}

mod resolution;
mod forms;
mod llama;
mod listing;
