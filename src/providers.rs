//! Model providers (SME-72): the Anthropic-compatible endpoints the user
//! configures on `/providers`, the default model, and which model each
//! conversation uses. Every turn goes to a provider's `/v1/messages`; its
//! `kind` only decides how its models are listed and described
//! (`anthropic::models`).
//!
//! The wire types here cross to the browser; the rest is server-only.

use serde::{Deserialize, Serialize};

/// The error a turn ends with when its conversation has no model and there
/// is no default. Not server-only: the chat page recognizes it to point at
/// the providers page instead of showing a red error.
pub const NO_MODEL_CONFIGURED: &str = "No model is chosen for this conversation. Add a model provider or choose a model, then send again.";

/// The context window assumed for a model no source sizes.
pub const ASSUMED_CONTEXT_WINDOW: u32 = 200_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    Ollama,
    Other,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [Self::Anthropic, Self::Ollama, Self::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::Ollama => "Ollama",
            Self::Other => "Other Anthropic-compatible server",
        }
    }

    /// The new-provider form's base URL placeholder.
    pub fn base_url_placeholder(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Ollama => "http://localhost:11434",
            Self::Other => "https://gateway.example.com",
        }
    }

    /// The auth the new-provider form starts with. Local Ollama wants an
    /// `x-api-key` it ignores; its cloud endpoint wants `bearer`.
    pub fn default_auth_kind(self) -> AuthKind {
        match self {
            Self::Anthropic | Self::Ollama => AuthKind::ApiKey,
            Self::Other => AuthKind::Bearer,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// An `x-api-key` header.
    ApiKey,
    /// An `Authorization: Bearer` header.
    Bearer,
}

impl AuthKind {
    pub const ALL: [AuthKind; 2] = [Self::ApiKey, Self::Bearer];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Bearer => "bearer",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ApiKey => "API key (x-api-key)",
            Self::Bearer => "Bearer token (Authorization)",
        }
    }
}

/// A provider as the browser sees it: never the secret, only its last few
/// characters (`secret_hint`) so keys can be told apart.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderSummary {
    pub id: i64,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub auth_kind: AuthKind,
    pub secret_hint: Option<String>,
}

/// What the new- and edit-provider forms send. On an edit, an empty
/// `secret` keeps the stored one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderInput {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub auth_kind: AuthKind,
    pub secret: String,
}

/// The last four characters of `secret`, shown as `…abcd`, or `None` for a
/// secret shorter than 12 characters, which four would give away too much
/// of.
#[cfg(feature = "server")]
pub fn secret_hint(secret: &str) -> Option<String> {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 12 {
        return None;
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    Some(format!("\u{2026}{tail}"))
}

/// What's known about one of a provider's models: the user's overrides,
/// what the provider reported, and what a turn will use.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    /// The user's override, if any.
    pub thinking_override: Option<bool>,
    /// The user's override, if any.
    pub context_window_override: Option<u32>,
    /// What the provider reported, if it did.
    pub reported_context_window: Option<u32>,
    pub reported_tools: Option<bool>,
    /// What a turn uses.
    pub thinking: bool,
    pub context_window: u32,
    /// False when `context_window` is `ASSUMED_CONTEXT_WINDOW` because
    /// nothing sized the model.
    pub context_window_known: bool,
    /// Why this model's details couldn't be fetched, if they couldn't.
    pub details_error: Option<String>,
}

/// A provider's models: the listing merged with every model that has
/// stored settings, and the listing's error if it failed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderModels {
    pub models: Vec<ModelInfo>,
    pub listing_error: Option<String>,
}

/// A provider and model, as the picker shows it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelChoice {
    pub provider_id: i64,
    pub provider_name: String,
    pub model: String,
    pub context_window: u32,
    pub context_window_known: bool,
    /// `Some(false)` when the provider says the model can't call tools.
    pub tools: Option<bool>,
}

/// Which model a conversation's next turn uses, for the picker.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state")]
pub enum ConversationModel {
    /// The conversation has its own.
    Chosen { choice: ModelChoice },
    /// It has none yet, and its next turn takes the default.
    Default { choice: ModelChoice },
    /// It has none, there are providers, but no default.
    NoDefault {},
    /// There are no providers at all.
    NoProviders {},
}

impl ConversationModel {
    /// Whether the next turn has a model to run on.
    pub fn is_ready(&self) -> bool {
        self.choice().is_some()
    }

    /// The model the next turn runs on, if there is one.
    pub fn choice(&self) -> Option<&ModelChoice> {
        match self {
            Self::Chosen { choice } | Self::Default { choice } => Some(choice),
            Self::NoDefault {} | Self::NoProviders {} => None,
        }
    }
}

#[cfg(feature = "server")]
pub use server::*;

#[cfg(feature = "server")]
mod server {
    use sqlx::PgPool;

    use super::*;
    use crate::anthropic::stream::{Auth, Endpoint};
    use crate::db;

    /// Everything a turn needs about its model, read once when the turn
    /// starts: a change made while it runs applies from the next turn.
    #[derive(Clone, Debug, PartialEq)]
    pub struct TurnModel {
        pub endpoint: Endpoint,
        pub model: String,
        pub thinking: bool,
        pub context_window: u32,
        /// Thinking blocks in messages up to this id were written for
        /// another provider or model, and aren't replayed.
        pub thinking_stripped_through: Option<i64>,
    }

    pub fn endpoint(provider: &db::InferenceProvider) -> Endpoint {
        let auth = match AuthKind::parse(&provider.auth_kind) {
            Some(AuthKind::Bearer) => Auth::Bearer(provider.secret.clone()),
            _ => Auth::ApiKey(provider.secret.clone()),
        };
        Endpoint {
            base_url: provider.base_url.clone(),
            auth,
        }
    }

    impl From<db::InferenceProvider> for ProviderSummary {
        fn from(provider: db::InferenceProvider) -> Self {
            Self {
                id: provider.id,
                secret_hint: secret_hint(&provider.secret),
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

    /// Whether a turn asks for thinking: the user's override, else what
    /// the provider reported, else on.
    pub fn thinking(row: Option<&db::ProviderModelRow>) -> bool {
        row.and_then(|r| r.thinking.or(r.reported_thinking)).unwrap_or(true)
    }

    /// Builds one model's `ModelInfo` from its stored row, if any.
    pub fn model_info(
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
            reported_context_window: row.and_then(|r| positive(r.reported_context_window)),
            reported_tools: row.and_then(|r| r.reported_tools),
            thinking: thinking(row),
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
        Ok(Some(TurnModel {
            endpoint: endpoint(&provider),
            thinking: thinking(settings.as_ref()),
            context_window: context_window(&model, settings.as_ref()).0,
            thinking_stripped_through,
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
        Ok(ProviderInput {
            name,
            base_url,
            secret: input.secret.trim().to_string(),
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
        )
        .await
        .map_err(|e| save_error(&input.name, e))?;
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
        )
        .await
        .map_err(|e| save_error(&input.name, e))?
        .ok_or("That provider no longer exists.")?;
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

    async fn store_details(
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
        )
        .await
        .map_err(|e| e.to_string())
    }

    fn has_any(details: &crate::anthropic::models::ModelDetails) -> bool {
        details.context_window.is_some() || details.thinking.is_some() || details.tools.is_some()
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
        let (listed, listing_error) =
            match crate::anthropic::models::list_models(kind, &endpoint).await {
                Ok(listed) => (listed, None),
                Err(e) => (Vec::new(), Some(e)),
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
                model_info(m.id, m.display_name, row.as_ref(), error)
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
                    listing_failed || row.added_by_hand || row.thinking.is_some() || row.context_window.is_some()
                })
                .map(|row| model_info(row.model.clone(), None, Some(&row), None)),
        );
        Ok(ProviderModels { models, listing_error })
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
        let error = match crate::anthropic::models::model_details(kind, &endpoint(&provider), model).await {
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
        Ok(model_info(model.to_string(), None, row.as_ref(), error))
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

    /// Sets the user's overrides for a model: thinking on or off, and its
    /// context window. `None` goes back to what the provider says.
    pub async fn set_model_settings(
        pool: &PgPool,
        provider_id: i64,
        model: &str,
        thinking: Option<bool>,
        context_window: Option<u32>,
    ) -> Result<(), String> {
        let model = model.trim();
        if model.is_empty() {
            return Err("Enter the model's id.".to_string());
        }
        let context_window = match context_window {
            None => None,
            Some(window) => Some(
                i32::try_from(window)
                    .ok()
                    .filter(|w| *w >= 1024)
                    .ok_or("A context window is a number of tokens, at least 1024.")?,
            ),
        };
        db::set_provider_model_overrides(pool, provider_id, model, thinking, context_window)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "server")]
    #[test]
    fn test_a_secret_hint_shows_only_the_last_four_characters() {
        assert_eq!(secret_hint("sk-ant-api03-abcdefWXYZ"), Some("\u{2026}WXYZ".to_string()));
        assert_eq!(secret_hint("short-key"), None, "too short to show any of");
        assert_eq!(secret_hint(""), None);
        // Characters, not bytes: a multi-byte secret doesn't split mid-char.
        assert_eq!(secret_hint("ключ-ключ-ключ"), Some("\u{2026}ключ".to_string()));
    }

    #[test]
    fn test_kinds_and_auth_kinds_round_trip_their_names() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        for kind in AuthKind::ALL {
            assert_eq!(AuthKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProviderKind::parse("openai"), None);
    }

    /// Wire types go to the browser as JSON; each shape round-trips.
    #[test]
    fn test_conversation_models_round_trip_through_json() {
        let choice = ModelChoice {
            provider_id: 1,
            provider_name: "Anthropic".to_string(),
            model: "claude-opus-5".to_string(),
            context_window: 200_000,
            context_window_known: true,
            tools: None,
        };
        for value in [
            ConversationModel::Chosen { choice: choice.clone() },
            ConversationModel::Default { choice },
            ConversationModel::NoDefault {},
            ConversationModel::NoProviders {},
        ] {
            let json = serde_json::to_string(&value).expect("serializes");
            assert_eq!(serde_json::from_str::<ConversationModel>(&json).expect("deserializes"), value);
        }
    }

    #[cfg(feature = "server")]
    mod server_tests {
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
                added_by_hand: false,
            }
        }

        #[test]
        fn test_the_context_window_prefers_the_override_then_the_report_then_a_known_model() {
            assert_eq!(context_window("m", Some(&row(None, Some(32_000), Some(4096), None))), (32_000, true));
            assert_eq!(context_window("m", Some(&row(None, None, Some(4096), None))), (4096, true));
            assert_eq!(context_window("claude-opus-5", None), (200_000, true), "a known claude-* model");
            assert_eq!(context_window("llama3", None), (ASSUMED_CONTEXT_WINDOW, false));
            assert_eq!(
                context_window("llama3", Some(&row(None, Some(0), Some(-1), None))),
                (ASSUMED_CONTEXT_WINDOW, false),
                "a zero or negative size isn't one"
            );
        }

        #[test]
        fn test_thinking_prefers_the_override_then_the_report_then_on() {
            assert!(thinking(None));
            assert!(!thinking(Some(&row(None, None, None, Some(false)))));
            assert!(thinking(Some(&row(Some(true), None, None, Some(false)))));
            assert!(!thinking(Some(&row(Some(false), None, None, None))));
        }

        async fn provider(pool: &PgPool, name: &str, auth_kind: &str) -> db::InferenceProvider {
            db::create_inference_provider(pool, name, "ollama", "http://ollama:11434", auth_kind, "the-secret")
                .await
                .expect("create provider")
        }

        #[sqlx::test]
        async fn test_the_picker_states_from_no_providers_to_chosen(pool: PgPool) {
            let conversation = db::create_conversation(&pool).await.expect("conversation");
            assert_eq!(
                conversation_model(&pool, conversation.id).await.expect("read"),
                Some(ConversationModel::NoProviders {})
            );
            assert_eq!(conversation_model(&pool, 999_999).await.expect("read"), None);

            let ollama = provider(&pool, "Home Ollama", "api_key").await;
            assert_eq!(
                conversation_model(&pool, conversation.id).await.expect("read"),
                Some(ConversationModel::NoDefault {})
            );

            db::set_default_model(&pool, ollama.id, "gemma4").await.expect("default");
            db::set_provider_model_reported(&pool, ollama.id, "gemma4", Some(4096), Some(true), Some(false))
                .await
                .expect("reported");
            let gemma = ModelChoice {
                provider_id: ollama.id,
                provider_name: "Home Ollama".to_string(),
                model: "gemma4".to_string(),
                context_window: 4096,
                context_window_known: true,
                tools: Some(false),
            };
            assert_eq!(
                conversation_model(&pool, conversation.id).await.expect("read"),
                Some(ConversationModel::Default { choice: gemma.clone() })
            );

            db::set_conversation_model(&pool, conversation.id, ollama.id, "llama3").await.expect("set");
            assert_eq!(
                conversation_model(&pool, conversation.id).await.expect("read"),
                Some(ConversationModel::Chosen {
                    choice: ModelChoice {
                        model: "llama3".to_string(),
                        context_window: ASSUMED_CONTEXT_WINDOW,
                        context_window_known: false,
                        tools: None,
                        ..gemma
                    }
                })
            );
            assert_eq!(conversation_context_window(&pool, conversation.id).await, ASSUMED_CONTEXT_WINDOW);
        }

        #[sqlx::test]
        async fn test_a_turn_takes_the_default_and_keeps_it(pool: PgPool) {
            // Its own id: event buses are process-wide, keyed by id.
            let conversation = db::create_conversation_with_id(&pool, 9_172_000_001).await.expect("conversation");
            let bearer = provider(&pool, "gateway", "bearer").await;
            db::set_default_model(&pool, bearer.id, "m1").await.expect("default");
            db::set_provider_model_overrides(&pool, bearer.id, "m1", Some(false), Some(64_000))
                .await
                .expect("overrides");
            let mut events = crate::events::subscribe(conversation.id);

            let turn = resolve_turn_model(&pool, conversation.id).await.expect("resolves");

            assert_eq!(
                turn,
                TurnModel {
                    endpoint: Endpoint {
                        base_url: "http://ollama:11434".to_string(),
                        auth: Auth::Bearer("the-secret".to_string()),
                    },
                    model: "m1".to_string(),
                    thinking: false,
                    context_window: 64_000,
                    thinking_stripped_through: None,
                }
            );
            let kept = db::get_conversation_model(&pool, conversation.id).await.expect("read").expect("exists");
            assert_eq!((kept.provider_id, kept.model.as_deref()), (Some(bearer.id), Some("m1")));
            assert!(received(&mut events, &crate::events::ConversationEvent::ModelChanged {}), "tabs are told");

            // A later change of default doesn't move it.
            db::set_default_model(&pool, bearer.id, "m2").await.expect("default");
            assert_eq!(resolve_turn_model(&pool, conversation.id).await.expect("resolves").model, "m1");
        }

        #[sqlx::test]
        async fn test_a_turn_with_no_model_and_no_default_says_so(pool: PgPool) {
            let conversation = db::create_conversation(&pool).await.expect("conversation");
            provider(&pool, "p", "api_key").await;
            assert_eq!(
                resolve_turn_model(&pool, conversation.id).await,
                Err(NO_MODEL_CONFIGURED.to_string())
            );
            assert_eq!(
                resolve_turn_model(&pool, 999_999).await,
                Err("conversation not found".to_string())
            );
        }

        #[sqlx::test]
        async fn test_a_switched_conversations_turn_strips_thinking_up_to_the_switch(pool: PgPool) {
            let conversation = db::create_conversation(&pool).await.expect("conversation");
            let p = provider(&pool, "p", "api_key").await;
            let text = [crate::anthropic::ContentBlock::Text { text: "hi".to_string() }];
            db::set_conversation_model(&pool, conversation.id, p.id, "m1").await.expect("model");
            resolve_turn_model(&pool, conversation.id).await.expect("the first turn, on m1");
            // The user picks m2 while that turn is still running and saving
            // messages m1 signed.
            set_conversation_model(&pool, conversation.id, p.id, "m2").await.expect("switch");
            let during = db::create_message(&pool, conversation.id, "assistant", &text).await.expect("message");

            let turn = resolve_turn_model(&pool, conversation.id).await.expect("the next turn, on m2");
            assert_eq!(turn.thinking_stripped_through, Some(during.id), "includes what m1 wrote after the pick");
        }

        /// SME-72 review: a provider moved to another address is another
        /// backend, whose signatures don't match.
        #[sqlx::test]
        async fn test_a_provider_moved_to_another_address_strips_old_thinking(pool: PgPool) {
            let conversation = db::create_conversation(&pool).await.expect("conversation");
            let p = provider(&pool, "p", "api_key").await;
            let text = [crate::anthropic::ContentBlock::Text { text: "hi".to_string() }];
            db::set_conversation_model(&pool, conversation.id, p.id, "m1").await.expect("model");
            resolve_turn_model(&pool, conversation.id).await.expect("first turn");
            let reply = db::create_message(&pool, conversation.id, "assistant", &text).await.expect("message");
            update_provider(&pool, p.id, input("p", "http://elsewhere:11434", "new-key")).await.expect("move it");

            let turn = resolve_turn_model(&pool, conversation.id).await.expect("next turn");
            assert_eq!(turn.thinking_stripped_through, Some(reply.id));
        }

        /// SME-72 review: the stored key only ever goes to the address it
        /// was entered for.
        #[sqlx::test]
        async fn test_moving_a_provider_needs_its_key_again(pool: PgPool) {
            let created = create_provider(&pool, input("p", "http://one:11434", "secret-1234567890"))
                .await
                .expect("create");
            let refused = update_provider(&pool, created.id, input("p", "http://two:11434", ""))
                .await
                .expect_err("a new address without the key");
            assert!(refused.contains("key"), "{refused}");
            let stored = db::get_inference_provider(&pool, created.id).await.expect("get").expect("exists");
            assert_eq!(stored.base_url, "http://one:11434", "nothing changed");
            // The same address with a blank key still keeps it.
            update_provider(&pool, created.id, input("renamed", " http://one:11434 ", ""))
                .await
                .expect("a rename keeps the key");
        }

        fn input(name: &str, base_url: &str, secret: &str) -> ProviderInput {
            ProviderInput {
                name: name.to_string(),
                kind: ProviderKind::Ollama,
                base_url: base_url.to_string(),
                auth_kind: AuthKind::ApiKey,
                secret: secret.to_string(),
            }
        }

        #[sqlx::test]
        async fn test_a_provider_form_is_checked_and_the_secret_never_comes_back(pool: PgPool) {
            let mut app_events = crate::events::subscribe_app();
            assert_eq!(
                create_provider(&pool, input("  ", "http://h", "k")).await,
                Err("Give the provider a name.".to_string())
            );
            let bad_url = create_provider(&pool, input("x", "ftp://h", "k")).await.expect_err("scheme");
            assert!(bad_url.contains("http://"), "{bad_url}");
            assert!(create_provider(&pool, input("x", "localhost:11434", "k")).await.is_err(), "no scheme");
            assert!(create_provider(&pool, input("x", "http://h", "  ")).await.is_err(), "a new one needs a secret");

            let created = create_provider(&pool, input(" Home ", " http://ollama:11434 ", "secret-1234567890"))
                .await
                .expect("create");
            assert_eq!(
                created,
                ProviderSummary {
                    id: created.id,
                    name: "Home".to_string(),
                    kind: ProviderKind::Ollama,
                    base_url: "http://ollama:11434".to_string(),
                    auth_kind: AuthKind::ApiKey,
                    secret_hint: Some("\u{2026}7890".to_string()),
                }
            );
            assert!(received(&mut app_events, &crate::events::AppEvent::ProvidersChanged));
            assert_eq!(
                create_provider(&pool, input("Home", "http://h", "k")).await,
                Err("There's already a provider named \u{201c}Home\u{201d}.".to_string())
            );

            let kept = update_provider(&pool, created.id, input("Home", "http://ollama:11434", ""))
                .await
                .expect("update");
            assert_eq!(kept.secret_hint, created.secret_hint, "a blank secret keeps the stored one");
            let stored = db::get_inference_provider(&pool, created.id).await.expect("get").expect("exists");
            assert_eq!(stored.secret, "secret-1234567890");
            assert_eq!(
                update_provider(&pool, created.id + 100, input("Z", "http://h", "")).await,
                Err("That provider no longer exists.".to_string())
            );
        }

        #[sqlx::test]
        async fn test_model_settings_are_checked(pool: PgPool) {
            let p = provider(&pool, "p", "api_key").await;
            assert!(set_model_settings(&pool, p.id, " ", None, None).await.is_err());
            assert!(set_model_settings(&pool, p.id, "m", None, Some(10)).await.is_err(), "too small");
            assert!(set_model_settings(&pool, p.id, "m", None, Some(u32::MAX)).await.is_err(), "too big");
            assert_eq!(
                set_model_settings(&pool, p.id + 1, "m", Some(true), None).await,
                Err("That provider no longer exists.".to_string())
            );
            set_model_settings(&pool, p.id, "m", Some(false), Some(32_768)).await.expect("set");
            let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
            assert_eq!((row.thinking, row.context_window), (Some(false), Some(32_768)));
        }

        /// An Ollama-shaped mock: `/api/tags` lists two models, `/api/show`
        /// knows one of them and fails for the other.
        async fn mock_ollama() -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let app = axum::Router::new()
                .route(
                    "/api/tags",
                    axum::routing::get(|| async {
                        ([(axum::http::header::CONTENT_TYPE, "application/json")],
                         r#"{"models":[{"name":"gemma4"},{"name":"broken"}]}"#)
                    }),
                )
                .route(
                    "/api/show",
                    axum::routing::post(|body: String| async move {
                        if body.contains("gemma4") {
                            (axum::http::StatusCode::OK,
                             r#"{"capabilities":["completion","tools"],"parameters":"num_ctx 8192"}"#.to_string())
                        } else {
                            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "model is corrupt".to_string())
                        }
                    }),
                );
            tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            format!("http://{addr}")
        }

        #[sqlx::test]
        async fn test_an_ollama_providers_models_come_with_their_details(pool: PgPool) {
            let base = mock_ollama().await;
            let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k")
                .await
                .expect("create");
            set_model_settings(&pool, p.id, "typed-by-hand", None, Some(16_384)).await.expect("set");

            let listing = provider_models(&pool, p.id, true).await.expect("models");

            assert_eq!(listing.listing_error, None);
            let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["gemma4", "broken", "typed-by-hand"]);
            let gemma = &listing.models[0];
            assert_eq!(
                (gemma.context_window, gemma.context_window_known, gemma.thinking, gemma.reported_tools),
                (8192, true, false, Some(true)),
                "num_ctx, and no thinking capability means thinking off"
            );
            let broken = &listing.models[1];
            assert!(broken.details_error.as_deref().is_some_and(|e| e.contains("500")), "{broken:?}");
            assert!(!broken.context_window_known);
            assert_eq!(listing.models[2].context_window, 16_384);
            // Stored, so a turn uses it without asking again.
            let stored = db::get_provider_model(&pool, p.id, "gemma4").await.expect("get").expect("stored");
            assert_eq!(stored.reported_context_window, Some(8192));
        }

        /// SME-72 review: the picker's list doesn't ask an Ollama server
        /// about every model each time it opens; it uses what's stored.
        #[sqlx::test]
        async fn test_a_quick_listing_skips_the_per_model_questions(pool: PgPool) {
            let base = mock_ollama().await;
            let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k")
                .await
                .expect("create");
            let listing = provider_models(&pool, p.id, false).await.expect("models");
            let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["gemma4", "broken"]);
            assert!(listing.models.iter().all(|m| m.details_error.is_none()), "nobody asked");
            assert_eq!(db::get_provider_model(&pool, p.id, "gemma4").await.expect("get"), None, "nothing stored");
        }

        /// SME-72 review: adding a model by id keeps any settings it has.
        #[sqlx::test]
        async fn test_adding_a_model_by_id_keeps_its_settings(pool: PgPool) {
            let p = provider(&pool, "p", "api_key").await;
            set_model_settings(&pool, p.id, "llama3", Some(false), Some(32_768)).await.expect("set");
            add_model(&pool, p.id, " llama3 ").await.expect("add");
            let row = db::get_provider_model(&pool, p.id, "llama3").await.expect("get").expect("exists");
            assert_eq!((row.thinking, row.context_window), (Some(false), Some(32_768)));
            add_model(&pool, p.id, "fresh").await.expect("add");
            assert!(db::get_provider_model(&pool, p.id, "fresh").await.expect("get").is_some());
            assert!(add_model(&pool, p.id, "  ").await.is_err());
            assert_eq!(
                add_model(&pool, p.id + 1, "m").await,
                Err("That provider no longer exists.".to_string())
            );
        }

        /// SME-72 review 2: a model the provider stopped listing goes away,
        /// unless it was added by hand or has settings of the user's.
        #[sqlx::test]
        async fn test_a_model_the_provider_dropped_isnt_shown(pool: PgPool) {
            let base = mock_ollama().await;
            let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k")
                .await
                .expect("create");
            db::set_provider_model_reported(&pool, p.id, "removed", Some(4096), None, Some(true))
                .await
                .expect("seen once");
            add_model(&pool, p.id, "by-hand").await.expect("add");
            set_model_settings(&pool, p.id, "tuned", None, Some(16_384)).await.expect("set");

            let listing = provider_models(&pool, p.id, false).await.expect("models");
            let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["gemma4", "broken", "by-hand", "tuned"]);
        }

        /// SME-72 review 2: `/api/ps` is one server-wide list, fetched once
        /// per page, not once per model.
        #[sqlx::test]
        async fn test_ollamas_loaded_models_are_fetched_once(pool: PgPool) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let ps_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = ps_calls.clone();
            let app = axum::Router::new()
                .route("/api/tags", axum::routing::get(|| async {
                    ([(axum::http::header::CONTENT_TYPE, "application/json")],
                     r#"{"models":[{"name":"a"},{"name":"b"},{"name":"c"}]}"#)
                }))
                .route("/api/show", axum::routing::post(|| async {
                    ([(axum::http::header::CONTENT_TYPE, "application/json")],
                     r#"{"capabilities":["completion","tools"]}"#)
                }))
                .route("/api/ps", axum::routing::get(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        ([(axum::http::header::CONTENT_TYPE, "application/json")],
                         r#"{"models":[{"name":"b","model":"b","context_length":8192}]}"#)
                    }
                }));
            tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            let p = db::create_inference_provider(&pool, "o", "ollama", &format!("http://{addr}"), "api_key", "k")
                .await
                .expect("create");

            let listing = provider_models(&pool, p.id, true).await.expect("models");

            assert_eq!(ps_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            let b = listing.models.iter().find(|m| m.id == "b").expect("b");
            assert_eq!((b.context_window, b.context_window_known), (8192, true));
        }

        #[sqlx::test]
        async fn test_an_unreachable_providers_models_still_list_what_is_stored(pool: PgPool) {
            let p = db::create_inference_provider(&pool, "gone", "anthropic", "http://127.0.0.1:1", "api_key", "k")
                .await
                .expect("create");
            set_model_settings(&pool, p.id, "claude-opus-5", Some(true), None).await.expect("set");
            let listing = provider_models(&pool, p.id, true).await.expect("models");
            assert!(listing.listing_error.is_some());
            assert_eq!(listing.models.len(), 1);
            assert_eq!(listing.models[0].id, "claude-opus-5");
        }

        #[sqlx::test]
        async fn test_the_default_is_checked_and_announced(pool: PgPool) {
            let p = provider(&pool, "p", "api_key").await;
            let mut app_events = crate::events::subscribe_app();
            assert!(set_default_model(&pool, p.id, "").await.is_err());
            assert_eq!(
                set_default_model(&pool, p.id + 1, "m").await,
                Err("That provider no longer exists.".to_string())
            );
            set_default_model(&pool, p.id, "m").await.expect("set");
            assert!(received(&mut app_events, &crate::events::AppEvent::ProvidersChanged));
            assert_eq!(default_model(&pool).await.expect("read").map(|c| c.model), Some("m".to_string()));
            delete_provider(&pool, p.id).await.expect("delete");
            assert_eq!(default_model(&pool).await.expect("read"), None);
        }

        #[sqlx::test]
        async fn test_setting_a_model_checks_it_and_tells_the_tabs(pool: PgPool) {
            let conversation = db::create_conversation_with_id(&pool, 9_172_000_002).await.expect("conversation");
            let p = provider(&pool, "p", "api_key").await;
            let mut events = crate::events::subscribe(conversation.id);
            assert_eq!(
                set_conversation_model(&pool, conversation.id, p.id, "  ").await,
                Err("Choose a model.".to_string())
            );
            assert_eq!(
                set_conversation_model(&pool, conversation.id, p.id + 1, "m").await,
                Err("That provider no longer exists.".to_string())
            );
            set_conversation_model(&pool, conversation.id, p.id, " m1 ").await.expect("set");
            let set = db::get_conversation_model(&pool, conversation.id).await.expect("read").expect("exists");
            assert_eq!(set.model.as_deref(), Some("m1"), "trimmed");
            assert!(received(&mut events, &crate::events::ConversationEvent::ModelChanged {}), "tabs are told");
        }
    }
}

/// A provider for tests that run turns against a mock upstream.
#[cfg(all(test, feature = "server"))]
pub(crate) mod test_support {
    use sqlx::PgPool;

    use crate::db;

    /// Serializes the tests that run turns, here and in `sandbox.rs`. They
    /// share process-wide state keyed by conversation id (the turn lock,
    /// the reply so far, a stop or pause), and every `#[sqlx::test]`
    /// database numbers conversations from 1. Recovers from poisoning, so
    /// one failing test doesn't fail the rest. (Until SME-72 this was the
    /// lock around the `ANTHROPIC_BASE_URL` variable, which serialized
    /// them as a side effect.)
    pub(crate) fn lock_turn_tests() -> std::sync::MutexGuard<'static, ()> {
        static TURN_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TURN_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The model `add_mock_provider` makes the default.
    pub(crate) const MOCK_MODEL: &str = "mock-model";

    /// Saves a provider at `addr` and makes `MOCK_MODEL` on it the
    /// default, and moves every conversation already in `pool`'s database
    /// onto it, so any turn there runs against this mock (a test that
    /// starts a second mock means the second). Panics on a database error:
    /// it's test setup.
    pub(crate) async fn add_mock_provider(pool: &PgPool, addr: std::net::SocketAddr) {
        // Named by address: a test may start a second mock, which then
        // becomes the default.
        let provider = db::create_inference_provider(
            pool,
            &format!("mock {addr}"),
            "anthropic",
            &format!("http://{addr}"),
            "api_key",
            "test-key",
        )
        .await
        .expect("create the mock provider");
        db::set_default_model(pool, provider.id, MOCK_MODEL)
            .await
            .expect("make the mock the default");
        for conversation in db::list_conversations(pool).await.expect("list conversations") {
            db::set_conversation_model(pool, conversation.id, provider.id, MOCK_MODEL)
                .await
                .expect("move the conversation onto the mock");
        }
    }
}
