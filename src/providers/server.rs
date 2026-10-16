use sqlx::PgPool;

use super::*;
use crate::anthropic::stream::{Auth, Endpoint};
use crate::db;

/// Everything a turn needs about its model, read once when the turn
/// starts: a change made while it runs applies from the next turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnModel {
    pub endpoint: Endpoint,
    /// The provider the turn runs on, for its usage records.
    pub provider_id: i64,
    pub model: String,
    pub thinking: bool,
    /// The provider's prompt caching setting (SME-106).
    pub prompt_caching: bool,
    /// The catalog provider its calls are priced as (SME-106).
    pub price_catalog_provider: Option<String>,
    /// The model's effort, for an Anthropic provider only (SME-106).
    pub effort: Option<crate::anthropic::Effort>,
    /// The provider's kind: which controls a request carries.
    pub kind: ProviderKind,
    /// The most a reply may be, if anything caps it besides the
    /// window (SME-111).
    pub output_cap: Option<u32>,
    /// The user's cap on a reply's thinking (SME-111).
    pub reasoning_cap: Option<u32>,
    /// `output_cap` is an Anthropic model's own, which "Max reply
    /// tokens" can't raise (SME-111 review 2).
    pub output_cap_is_models_own: bool,
    /// For a llama.cpp provider, the chat template's settings it sends
    /// (SME-111).
    pub template: Option<TemplateSettings>,
    pub context_window: u32,
    /// Thinking blocks in messages up to this id were written for
    /// another provider or model, and aren't replayed.
    pub thinking_stripped_through: Option<i64>,
    /// The model takes adaptive thinking; false when its listing said
    /// it doesn't (Haiku 4.5), so it's sent a budget (B9, SME-122).
    pub adaptive_thinking: bool,
}

/// A llama.cpp model's chat-template settings, already limited to
/// what its template supports (`LlamaServerInfo`), so a template that
/// doesn't read one is never sent it (SME-111).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TemplateSettings {
    pub effort: Option<crate::anthropic::Effort>,
    pub preserve_thinking: Option<bool>,
}

/// A reply's share of its budget that thinking may take by default
/// (SME-111): three quarters, leaving at least `MIN_ANSWER_TOKENS`.
pub const REASONING_SHARE: (u32, u32) = (3, 4);

/// The smallest `budget_tokens` Anthropic takes (B9, SME-122).
pub const MIN_THINKING_BUDGET: u32 = 1_024;

/// What a reply keeps for its answer and any tool call, at least.
pub const MIN_ANSWER_TOKENS: u32 = 4_096;

/// How many of a `reply_budget` thinking may use: `REASONING_SHARE`
/// of it, leaving `MIN_ANSWER_TOKENS`, capped by the user's `cap`.
pub fn reasoning_budget(reply_budget: u32, cap: Option<u32>) -> u32 {
    let (share, of) = REASONING_SHARE;
    let automatic = (u64::from(reply_budget) * u64::from(share) / u64::from(of)) as u32;
    let budget = automatic.min(reply_budget.saturating_sub(MIN_ANSWER_TOKENS));
    cap.map_or(budget, |cap| budget.min(cap))
}

impl TurnModel {
    /// The `thinking` a request to this model with `reply_budget`
    /// carries: none with thinking off, a budget for llama.cpp, which
    /// enforces it, and adaptive for the rest. Anthropic refuses a
    /// budget on current models, Ollama doesn't enforce one, and Other
    /// may be a gateway in front of Claude.
    pub fn thinking_config(&self, reply_budget: u32) -> Option<crate::anthropic::ThinkingConfig> {
        if !self.thinking {
            return None;
        }
        Some(match self.kind {
            // No room to think in: thinking off, said so (the turn
            // tells the template too), rather than a budget of 0 with
            // thinking on (SME-111 review 1).
            ProviderKind::LlamaCpp => match reasoning_budget(reply_budget, self.reasoning_cap) {
                0 => return None,
                budget_tokens => crate::anthropic::ThinkingConfig::Enabled { budget_tokens },
            },
            // A model that can't take adaptive thinking (Haiku 4.5) is
            // given a budget, which the API wants at least 1,024 and
            // under the reply's (B9, SME-122).
            _ if !self.adaptive_thinking => match reasoning_budget(reply_budget, self.reasoning_cap) {
                budget_tokens if budget_tokens >= MIN_THINKING_BUDGET => {
                    crate::anthropic::ThinkingConfig::Enabled { budget_tokens }
                }
                _ => return None,
            },
            ProviderKind::Anthropic | ProviderKind::Ollama | ProviderKind::Other => {
                crate::anthropic::ThinkingConfig::Adaptive
            }
        })
    }

    /// The `chat_template_kwargs` a request to this model carries, with
    /// thinking on or off: `None` but for a llama.cpp provider, and
    /// when there's nothing to send.
    pub fn chat_template_kwargs(&self, thinking: bool) -> Option<crate::anthropic::ChatTemplateKwargs> {
        if self.kind != ProviderKind::LlamaCpp {
            return None;
        }
        let template = self.template.clone().unwrap_or_default();
        let kwargs = crate::anthropic::ChatTemplateKwargs {
            // llama.cpp reads this itself as well as the template.
            enable_thinking: (!thinking).then_some(false),
            reasoning_effort: template.effort.filter(|_| thinking),
            preserve_thinking: template.preserve_thinking,
        };
        (!kwargs.is_empty()).then_some(kwargs)
    }
}

/// The server info stored for `provider`, if it's readable.
pub fn server_caps(provider: &db::InferenceProvider) -> Option<LlamaServerInfo> {
    serde_json::from_value(provider.server_caps.clone()?).ok()
}

/// What a turn on `provider`'s model (`row`) sends its chat template.
pub fn template_settings(provider: &db::InferenceProvider, row: Option<&db::ProviderModelRow>) -> TemplateSettings {
    let Some(caps) = server_caps(provider) else {
        return TemplateSettings::default();
    };
    TemplateSettings {
        effort: row
            .and_then(|r| r.effort.as_deref())
            .and_then(crate::anthropic::Effort::parse)
            // Only a level the row offers for llama.cpp: one set while
            // the provider was another kind (`max`) can be one the
            // template refuses, failing every turn (SME-111 review 1).
            .filter(|effort| LLAMA_CPP_EFFORTS.contains(effort))
            .filter(|_| caps.supports_reasoning_effort()),
        preserve_thinking: caps.supports_preserve_reasoning().then_some(provider.keep_reasoning),
    }
}

/// How many requests at once `provider` is sent (SME-162): its max
/// concurrent requests, or with `0` the slot count its server last
/// reported (`server_caps`). `None` is no limit: none set, or automatic
/// with no count read yet.
pub fn resolved_max(provider: &db::InferenceProvider) -> Option<u32> {
    match provider.max_concurrent_requests? {
        0 => server_caps(provider)?.total_slots,
        n => u32::try_from(n).ok(),
    }
    // A pool of none would never send anything.
    .filter(|n| *n > 0)
}

/// The provider's address, credential and request slots (SME-162).
pub fn endpoint(provider: &db::InferenceProvider) -> Endpoint {
    let auth = match AuthKind::parse(&provider.auth_kind) {
        Some(AuthKind::Bearer) => Auth::Bearer(provider.secret.clone()),
        _ => Auth::ApiKey(provider.secret.clone()),
    };
    limiter::set(provider.id, provider.updated_at, resolved_max(provider));
    Endpoint {
        base_url: provider.base_url.clone(),
        auth,
        slots_of: Some(provider.id),
    }
}

impl From<db::InferenceProvider> for ProviderSummary {
    fn from(provider: db::InferenceProvider) -> Self {
        Self {
            id: provider.id,
            secret_hint: secret_hint(&provider.secret),
            prompt_caching: provider.prompt_caching,
            keep_reasoning: provider.keep_reasoning,
            server_caps: server_caps(&provider),
            max_concurrent_requests: provider.max_concurrent_requests.and_then(|n| u32::try_from(n).ok()),
            price_catalog_provider: provider.price_catalog_provider,
            kind: ProviderKind::parse(&provider.kind).unwrap_or(ProviderKind::Other),
            auth_kind: AuthKind::parse(&provider.auth_kind).unwrap_or(AuthKind::ApiKey),
            name: provider.name,
            base_url: provider.base_url,
        }
    }
}

fn positive(value: Option<i32>) -> Option<u32> {
    value.and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0)
}

/// The context window a turn uses and whether anything sized it: the
/// user's override, else what the provider reported, else a known
/// `claude-*` model's, else `ASSUMED_CONTEXT_WINDOW`.
pub fn context_window(model: &str, row: Option<&db::ProviderModelRow>) -> (u32, bool) {
    row.and_then(|r| positive(r.context_window).or(positive(r.reported_context_window)))
        .or_else(|| crate::anthropic::context_window_for(model))
        .map_or((ASSUMED_CONTEXT_WINDOW, false), |window| (window, true))
}

/// The cap on a reply to a model on a `kind` provider (SME-111): the
/// user's, else what the provider reported, else `UNKNOWN_OUTPUT_CAP`
/// on an Anthropic or Other provider. `None`: only the window caps it.
pub fn output_cap(kind: ProviderKind, model: &str, row: Option<&db::ProviderModelRow>) -> Option<u32> {
    let reported = row.and_then(|r| positive(r.reported_max_output));
    let users = row.and_then(|r| positive(r.max_output)).map(|users| match (kind, reported) {
        // Anthropic refuses a `max_tokens` above the model's own cap,
        // so the user's can lower it, not raise it (SME-111 review 1).
        (ProviderKind::Anthropic, Some(reported)) => users.min(reported),
        _ => users,
    });
    users
        .or(reported)
        .or_else(|| {
            // A window nothing sized is the assumed 200,000, which
            // doesn't bound a local server's real one (SME-111 review 1).
            let window_known = context_window(model, row).1;
            (matches!(kind, ProviderKind::Anthropic | ProviderKind::Other) || !window_known)
                .then_some(UNKNOWN_OUTPUT_CAP)
        })
}

/// Whether a turn asks for thinking: the user's override, else what
/// the provider reported, else on.
pub fn thinking(row: Option<&db::ProviderModelRow>) -> bool {
    row.and_then(|r| r.thinking.or(r.reported_thinking)).unwrap_or(true)
}

/// Builds one model's `ModelInfo` from its stored row, if any.
pub fn model_info(
    kind: ProviderKind,
    id: String,
    display_name: Option<String>,
    row: Option<&db::ProviderModelRow>,
    details_error: Option<String>,
) -> ModelInfo {
    let (context_window, context_window_known) = context_window(&id, row);
    ModelInfo {
        display_name,
        thinking_override: row.and_then(|r| r.thinking),
        context_window_override: row.and_then(|r| positive(r.context_window)),
        effort_override: row.and_then(|r| r.effort.as_deref()).and_then(crate::anthropic::Effort::parse),
        max_output_override: row.and_then(|r| positive(r.max_output)),
        reasoning_budget_override: row.and_then(|r| positive(r.reasoning_budget)),
        reported_context_window: row.and_then(|r| positive(r.reported_context_window)),
        reported_tools: row.and_then(|r| r.reported_tools),
        thinking: thinking(row),
        max_output: output_cap(kind, &id, row),
        context_window,
        context_window_known,
        details_error,
        id,
    }
}

async fn model_choice(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
) -> Result<Option<ModelChoice>, sqlx::Error> {
    let Some(provider) = db::get_inference_provider(pool, provider_id).await? else {
        return Ok(None);
    };
    let row = db::get_provider_model(pool, provider_id, model).await?;
    let (context_window, context_window_known) = context_window(model, row.as_ref());
    Ok(Some(ModelChoice {
        provider_id,
        provider_name: provider.name,
        model: model.to_string(),
        context_window,
        context_window_known,
        tools: row.and_then(|r| r.reported_tools),
    }))
}

/// Which model `conversation_id`'s next turn uses, without changing
/// anything. `None` when the conversation doesn't exist.
pub async fn conversation_model(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<ConversationModel>, sqlx::Error> {
    let Some(row) = db::get_conversation_model(pool, conversation_id).await? else {
        return Ok(None);
    };
    if let (Some(provider_id), Some(model)) = (row.provider_id, row.model.as_deref())
        && let Some(choice) = model_choice(pool, provider_id, model).await?
    {
        return Ok(Some(ConversationModel::Chosen { choice }));
    }
    if let Some((provider_id, model)) = db::get_default_model(pool).await?
        && let Some(choice) = model_choice(pool, provider_id, &model).await?
    {
        return Ok(Some(ConversationModel::Default { choice }));
    }
    Ok(Some(if db::list_inference_providers(pool).await?.is_empty() {
        ConversationModel::NoProviders {}
    } else {
        ConversationModel::NoDefault {}
    }))
}

/// The context window the context indicator shows for a conversation:
/// its model's, else the default's, else `ASSUMED_CONTEXT_WINDOW`.
pub async fn conversation_context_window(pool: &PgPool, conversation_id: i64) -> u32 {
    match conversation_model(pool, conversation_id).await {
        Ok(model) => model
            .as_ref()
            .and_then(ConversationModel::choice)
            .map_or(ASSUMED_CONTEXT_WINDOW, |choice| choice.context_window),
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't read the conversation's model");
            ASSUMED_CONTEXT_WINDOW
        }
    }
}

/// Resolves the model `conversation_id`'s turn runs on. A conversation
/// with no model takes the default (and keeps it); with no default it's
/// `NO_MODEL_CONFIGURED`.
pub async fn resolve_turn_model(pool: &PgPool, conversation_id: i64) -> Result<TurnModel, String> {
    // A provider deleted between reading the conversation and reading
    // the provider has cleared the conversation too: once more takes
    // the default (SME-72 review 3).
    match resolve_once(pool, conversation_id).await? {
        Some(model) => Ok(model),
        None => resolve_once(pool, conversation_id)
            .await?
            .ok_or_else(|| NO_MODEL_CONFIGURED.to_string()),
    }
}

/// `resolve_turn_model`'s one attempt: `None` when the conversation's
/// provider disappeared while it read.
async fn resolve_once(pool: &PgPool, conversation_id: i64) -> Result<Option<TurnModel>, String> {
    let db_error = |e: sqlx::Error| e.to_string();
    let mut row = db::get_conversation_model(pool, conversation_id)
        .await
        .map_err(db_error)?
        .ok_or("conversation not found")?;
    if row.provider_id.is_none() {
        if db::adopt_default_model(pool, conversation_id)
            .await
            .map_err(db_error)?
            .is_some()
        {
            crate::events::publish(conversation_id, crate::events::ConversationEvent::ModelChanged {});
        }
        // Read again either way: the picker may have set one meanwhile,
        // which the adoption then (rightly) left alone.
        row = db::get_conversation_model(pool, conversation_id)
            .await
            .map_err(db_error)?
            .ok_or("conversation not found")?;
    }
    let (Some(provider_id), Some(model)) = (row.provider_id, row.model) else {
        return Err(NO_MODEL_CONFIGURED.to_string());
    };
    // The foreign key keeps the provider while a conversation uses it,
    // so gone means it was deleted just now.
    let Some(provider) = db::get_inference_provider(pool, provider_id)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    let settings = db::get_provider_model(pool, provider_id, &model)
        .await
        .map_err(db_error)?;
    // Thinking is signed by the backend that wrote it: a provider moved
    // to another address is another backend.
    let model_key = format!("{provider_id} {} {model}", provider.base_url);
    let thinking_stripped_through = db::record_turn_model(pool, conversation_id, &model_key)
        .await
        .map_err(db_error)?;
    let kind = ProviderKind::parse(&provider.kind).unwrap_or(ProviderKind::Other);
    Ok(Some(TurnModel {
        endpoint: endpoint(&provider),
        provider_id,
        prompt_caching: provider.prompt_caching,
        price_catalog_provider: provider.price_catalog_provider.clone(),
        // Sent to Anthropic only: another server may refuse the field.
        effort: (kind == ProviderKind::Anthropic)
            .then(|| settings.as_ref().and_then(|r| r.effort.as_deref()).and_then(crate::anthropic::Effort::parse))
            .flatten(),
        template: (kind == ProviderKind::LlamaCpp).then(|| template_settings(&provider, settings.as_ref())),
        output_cap: output_cap(kind, &model, settings.as_ref()),
        reasoning_cap: settings.as_ref().and_then(|r| positive(r.reasoning_budget)),
        output_cap_is_models_own: kind == ProviderKind::Anthropic
            && settings.as_ref().and_then(|r| positive(r.reported_max_output)).is_some()
            && settings.as_ref().and_then(|r| positive(r.reported_max_output)) == output_cap(kind, &model, settings.as_ref()),
        kind,
        thinking: thinking(settings.as_ref()),
        context_window: context_window(&model, settings.as_ref()).0,
        thinking_stripped_through,
        adaptive_thinking: settings.as_ref().and_then(|r| r.reported_adaptive_thinking) != Some(false),
        model,
    }))
}

fn providers_changed() {
    crate::events::publish_app(crate::events::AppEvent::ProvidersChanged);
}

/// Checks a form's fields, returning them trimmed.
fn checked(input: ProviderInput) -> Result<ProviderInput, String> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err("Give the provider a name.".to_string());
    }
    let base_url = input.base_url.trim().to_string();
    match reqwest::Url::parse(&base_url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {}
        _ => {
            return Err(format!(
                "The base URL must be an http:// or https:// address, like {}.",
                input.kind.base_url_placeholder()
            ));
        }
    }
    let price_catalog_provider = input
        .price_catalog_provider
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    // The column's range; 0 is the server's own count (SME-162).
    if input.max_concurrent_requests.is_some_and(|n| i32::try_from(n).is_err()) {
        return Err(format!("Max concurrent requests can be at most {}.", i32::MAX));
    }
    // Only a llama.cpp server reports its slots (SME-162 review 1).
    if input.max_concurrent_requests == Some(0) && input.kind != ProviderKind::LlamaCpp {
        return Err("Max concurrent requests 0 (the server's own count) needs a llama.cpp server, which reports it; type a number instead.".to_string());
    }
    Ok(ProviderInput {
        name,
        base_url,
        secret: input.secret.trim().to_string(),
        price_catalog_provider,
        ..input
    })
}

/// A unique-name violation, said plainly; anything else as it is.
fn save_error(name: &str, error: sqlx::Error) -> String {
    match &error {
        sqlx::Error::Database(e) if e.is_unique_violation() => {
            format!("There's already a provider named \u{201c}{name}\u{201d}.")
        }
        _ => error.to_string(),
    }
}

pub async fn create_provider(pool: &PgPool, input: ProviderInput) -> Result<ProviderSummary, String> {
    let input = checked(input)?;
    if input.secret.is_empty() {
        return Err("Enter the API key or token (any value, for a local Ollama).".to_string());
    }
    let provider = db::create_inference_provider(
        pool,
        &input.name,
        input.kind.as_str(),
        &input.base_url,
        input.auth_kind.as_str(),
        &input.secret,
        input.prompt_caching,
        input.price_catalog_provider.as_deref(),
        input.keep_reasoning,
        to_column(input.max_concurrent_requests),
    )
    .await
    .map_err(|e| save_error(&input.name, e))?;
    limiter::set(provider.id, provider.updated_at, resolved_max(&provider));
    providers_changed();
    Ok(provider.into())
}

/// Saves an edit; an empty secret keeps the stored one.
pub async fn update_provider(
    pool: &PgPool,
    id: i64,
    input: ProviderInput,
) -> Result<ProviderSummary, String> {
    let input = checked(input)?;
    if input.secret.is_empty() {
        let stored = db::get_inference_provider(pool, id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("That provider no longer exists.")?;
        // The stored key only ever goes where it was entered for (the
        // MCP servers' OAuth credentials follow the same rule).
        if stored.base_url != input.base_url {
            return Err("Enter the key again to use it at a new address.".to_string());
        }
    }
    let secret = (!input.secret.is_empty()).then_some(input.secret.as_str());
    let provider = db::update_inference_provider(
        pool,
        id,
        &input.name,
        input.kind.as_str(),
        &input.base_url,
        input.auth_kind.as_str(),
        secret,
        input.prompt_caching,
        input.price_catalog_provider.as_deref(),
        input.keep_reasoning,
        to_column(input.max_concurrent_requests),
    )
    .await
    .map_err(|e| save_error(&input.name, e))?
    .ok_or("That provider no longer exists.")?;
    // From each running turn's next model call (SME-162 review 1).
    limiter::set(provider.id, provider.updated_at, resolved_max(&provider));
    providers_changed();
    Ok(provider.into())
}

/// Deletes a provider; conversations using it take the default at
/// their next turn (`db::delete_inference_provider`).
pub async fn delete_provider(pool: &PgPool, id: i64) -> Result<(), String> {
    // A turn taking the default, or a pick, can point a conversation
    // back at the provider between the delete's clearing and its
    // delete; the foreign key then refuses it, and trying again clears
    // that too (SME-72 review 3).
    let mut attempts = 0;
    loop {
        attempts += 1;
        match db::delete_inference_provider(pool, id).await {
            Ok(_) => break,
            Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() && attempts < 3 => continue,
            Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => {
                return Err("The provider is being used right now; try again.".to_string());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    limiter::forget(id);
    providers_changed();
    Ok(())
}

/// A foreign-key violation means the provider was deleted meanwhile.
fn provider_gone(error: sqlx::Error) -> String {
    match &error {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
            "That provider no longer exists.".to_string()
        }
        _ => error.to_string(),
    }
}

fn to_column(value: Option<u32>) -> Option<i32> {
    value.and_then(|v| i32::try_from(v).ok())
}

pub(super) async fn store_details(
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
    .map_err(|e| e.to_string())
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
            let updated_at = db::set_inference_provider_server_caps(pool, provider.id, &value)
                .await
                .map_err(|e| e.to_string())?;
            // An automatic limit takes the new count from each running
            // turn's next model call (SME-162 review 1).
            if let Some(updated_at) = updated_at {
                let read = db::InferenceProvider { server_caps: Some(value), updated_at, ..provider.clone() };
                limiter::set(provider.id, read.updated_at, resolved_max(&read));
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
    providers_changed();
    Ok(())
}

/// The default provider and model, if one is set.
pub async fn default_model(pool: &PgPool) -> Result<Option<ModelChoice>, String> {
    match db::get_default_model(pool).await.map_err(|e| e.to_string())? {
        Some((provider_id, model)) => model_choice(pool, provider_id, &model)
            .await
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

pub async fn set_default_model(pool: &PgPool, provider_id: i64, model: &str) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Choose a model.".to_string());
    }
    db::set_default_model(pool, provider_id, model)
        .await
        .map_err(provider_gone)?;
    providers_changed();
    Ok(())
}

/// Sets `conversation_id`'s model, telling its tabs.
pub async fn set_conversation_model(
    pool: &PgPool,
    conversation_id: i64,
    provider_id: i64,
    model: &str,
) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Choose a model.".to_string());
    }
    if db::get_inference_provider(pool, provider_id)
        .await
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err("That provider no longer exists.".to_string());
    }
    if !db::set_conversation_model(pool, conversation_id, provider_id, model)
        .await
        .map_err(provider_gone)?
    {
        return Err("conversation not found".to_string());
    }
    crate::events::publish(conversation_id, crate::events::ConversationEvent::ModelChanged {});
    Ok(())
}
