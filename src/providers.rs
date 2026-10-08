//! Model providers (SME-72): the Anthropic-compatible endpoints the user
//! configures on `/providers`, the default model, and which model each
//! conversation uses. Every turn goes to a provider's `/v1/messages`; its
//! `kind` decides how its models are listed and described
//! (`anthropic::models`), and which of a turn's controls it's sent: effort
//! as `output_config` for Anthropic, the chat template's own settings for
//! llama.cpp (SME-111).
//!
//! The wire types here cross to the browser; the rest is server-only.

use serde::{Deserialize, Serialize};

/// The error a turn ends with when its conversation has no model and there
/// is no default. Not server-only: the chat page recognizes it to point at
/// the providers page instead of showing a red error.
pub const NO_MODEL_CONFIGURED: &str = "No model is chosen for this conversation. Add a model provider or choose a model, then send again.";

/// The context window assumed for a model no source sizes.
pub const ASSUMED_CONTEXT_WINDOW: u32 = 200_000;

/// The reply cap of a model on an Anthropic or Other provider that nothing
/// sized (SME-111): a Claude model refuses a `max_tokens` above its own
/// cap, so these keep what every turn asked for before. Ollama and
/// llama.cpp have no cap of their own; the window bounds them, once
/// something sized it (else this too).
#[cfg(feature = "server")]
pub const UNKNOWN_OUTPUT_CAP: u32 = 16_384;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    Ollama,
    /// A llama.cpp server (`llama-server`), SME-111.
    LlamaCpp,
    Other,
}

impl ProviderKind {
    /// In the order the new-provider form lists them.
    pub const ALL: [ProviderKind; 4] = [Self::Anthropic, Self::Ollama, Self::LlamaCpp, Self::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama_cpp",
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
            Self::LlamaCpp => "llama.cpp",
            Self::Other => "Other Anthropic-compatible server",
        }
    }

    /// The new-provider form's base URL placeholder.
    pub fn base_url_placeholder(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Ollama => "http://localhost:11434",
            Self::LlamaCpp => "http://localhost:8080",
            Self::Other => "https://gateway.example.com",
        }
    }

    /// The auth the new-provider form starts with. Local Ollama wants an
    /// `x-api-key` it ignores; its cloud endpoint wants `bearer`.
    pub fn default_auth_kind(self) -> AuthKind {
        match self {
            Self::Anthropic | Self::Ollama => AuthKind::ApiKey,
            Self::LlamaCpp | Self::Other => AuthKind::Bearer,
        }
    }

    /// Whether the new-provider form starts with prompt caching on: for
    /// Anthropic, which bills cache reads at a tenth of the input price.
    /// Another server may refuse the `cache_control` field; llama.cpp
    /// ignores it and keeps its own prefix cache.
    pub fn default_prompt_caching(self) -> bool {
        matches!(self, Self::Anthropic)
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
    pub prompt_caching: bool,
    pub price_catalog_provider: Option<String>,
    /// Send llama.cpp's template `preserve_thinking` (SME-111).
    pub keep_reasoning: bool,
    /// What a llama.cpp server last said about itself (`/props`).
    pub server_caps: Option<LlamaServerInfo>,
}

/// What a llama.cpp server says about itself on `/props` (SME-111): read
/// with its model list, kept per provider, shown on its page, and read
/// when a turn starts for which template settings to send.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LlamaServerInfo {
    /// Each slot's context window.
    pub n_ctx: Option<u32>,
    /// How many requests it serves at once.
    pub total_slots: Option<u32>,
    pub model_alias: Option<String>,
    pub build_info: Option<String>,
    /// The chat template's capabilities (`chat_template_caps`), such as
    /// `supports_reasoning_effort`.
    pub template_caps: std::collections::BTreeMap<String, bool>,
}

impl LlamaServerInfo {
    /// Whether the chat template reads `reasoning_effort`.
    pub fn supports_reasoning_effort(&self) -> bool {
        self.template_caps.get("supports_reasoning_effort") == Some(&true)
    }

    /// Whether the chat template reads `preserve_thinking`.
    #[cfg(feature = "server")]
    pub fn supports_preserve_reasoning(&self) -> bool {
        self.template_caps.get("supports_preserve_reasoning") == Some(&true)
    }
}

/// The efforts a llama.cpp model's row offers (SME-111): its template
/// decides what each means, and `high` is the top level the user's
/// template takes (`xhigh` and `max` aren't offered; it refuses `max`).
pub const LLAMA_CPP_EFFORTS: [crate::anthropic::Effort; 3] = [
    crate::anthropic::Effort::Low,
    crate::anthropic::Effort::Medium,
    crate::anthropic::Effort::High,
];

/// What the new- and edit-provider forms send. On an edit, an empty
/// `secret` keeps the stored one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderInput {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub auth_kind: AuthKind,
    pub secret: String,
    /// Mark turn requests for prompt caching (SME-106).
    pub prompt_caching: bool,
    /// The catalog provider that prices its calls, or `None` (SME-106).
    pub price_catalog_provider: Option<String>,
    /// llama.cpp only: keep earlier turns' reasoning (SME-111).
    pub keep_reasoning: bool,
}

/// A models.dev catalog provider, for the provider form's "Prices from"
/// choice (SME-106).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PriceSource {
    pub id: String,
    pub name: String,
    pub api: Option<String>,
}

/// The catalog provider to suggest for a provider of `kind` at
/// `base_url`: the one whose API is at the same host, nearest by path.
/// `None` when nothing matches, or when two match equally well (a service
/// and a plan on it share an address, and only the user knows which key
/// they have).
pub fn suggest_price_source(sources: &[PriceSource], kind: ProviderKind, base_url: &str) -> Option<String> {
    let (host, path) = host_and_path(base_url)?;
    // The catalog gives Anthropic no address of its own.
    if kind == ProviderKind::Anthropic && host == "api.anthropic.com" {
        return sources.iter().any(|s| s.id == "anthropic").then(|| "anthropic".to_string());
    }
    let mut best: Option<(usize, &PriceSource)> = None;
    let mut tied = false;
    for source in sources {
        let Some((api_host, api_path)) = source.api.as_deref().and_then(host_and_path) else {
            continue;
        };
        if api_host != host {
            continue;
        }
        let shared = path.iter().zip(&api_path).take_while(|(a, b)| a == b).count();
        match best {
            Some((score, _)) if score > shared => {}
            Some((score, _)) if score == shared => tied = true,
            _ => {
                best = Some((shared, source));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied).map(|(_, source)| source.id.clone())
}

/// `url`'s host, lowercased, and its path's segments; `None` for anything
/// but an http(s) address.
fn host_and_path(url: &str) -> Option<(String, Vec<String>)> {
    let rest = url.trim();
    let rest = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if host.is_empty() {
        return None;
    }
    let segments = path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    Some((host.to_ascii_lowercase(), segments))
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
    /// The user's effort, if set (SME-106).
    pub effort_override: Option<crate::anthropic::Effort>,
    /// The user's cap on a reply, if set (SME-111).
    pub max_output_override: Option<u32>,
    /// The user's cap on a reply's thinking, if set (SME-111).
    pub reasoning_budget_override: Option<u32>,
    /// What the provider reported, if it did.
    pub reported_context_window: Option<u32>,
    pub reported_tools: Option<bool>,
    /// What a turn uses.
    pub thinking: bool,
    pub context_window: u32,
    /// The cap on a reply, if anything but the window caps it (SME-111).
    pub max_output: Option<u32>,
    /// False when `context_window` is `ASSUMED_CONTEXT_WINDOW` because
    /// nothing sized the model.
    pub context_window_known: bool,
    /// Why this model's details couldn't be fetched, if they couldn't.
    pub details_error: Option<String>,
}

/// The user's settings for one model, as its row on the provider's page
/// saves them. `None` goes back to what the provider says.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelSettings {
    pub thinking: Option<bool>,
    pub context_window: Option<u32>,
    pub effort: Option<crate::anthropic::Effort>,
    /// "Max reply tokens" (SME-111).
    pub max_output: Option<u32>,
    /// "Reasoning budget", llama.cpp only (SME-111).
    pub reasoning_budget: Option<u32>,
}

/// A provider's models: the listing merged with every model that has
/// stored settings, and the listing's error if it failed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderModels {
    pub models: Vec<ModelInfo>,
    pub listing_error: Option<String>,
    /// The provider's kind, for which settings its models' rows offer.
    pub kind: ProviderKind,
    /// A llama.cpp server's `/props`, as last read (SME-111).
    pub server_caps: Option<LlamaServerInfo>,
    /// Why `/props` couldn't be read just now, if it couldn't: the turn
    /// then uses what was last read (`server_caps`).
    pub server_error: Option<String>,
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
                prompt_caching: provider.prompt_caching,
                keep_reasoning: provider.keep_reasoning,
                server_caps: server_caps(&provider),
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
            input.prompt_caching,
            input.price_catalog_provider.as_deref(),
            input.keep_reasoning,
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
            to_column(details.max_output),
        )
        .await
        .map_err(|e| e.to_string())
    }

    fn has_any(details: &crate::anthropic::models::ModelDetails) -> bool {
        details.context_window.is_some()
            || details.thinking.is_some()
            || details.tools.is_some()
            || details.max_output.is_some()
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
                db::set_inference_provider_server_caps(pool, provider.id, &value)
                    .await
                    .map_err(|e| e.to_string())?;
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
    fn source(id: &str, api: Option<&str>) -> PriceSource {
        PriceSource { id: id.to_string(), name: id.to_string(), api: api.map(str::to_string) }
    }

    fn catalog_sources() -> Vec<PriceSource> {
        vec![
            source("anthropic", None),
            source("deepseek", Some("https://api.deepseek.com")),
            source("minimax", Some("https://api.minimax.io/anthropic/v1")),
            source("minimax-coding-plan", Some("https://api.minimax.io/anthropic/v1")),
            source("moonshotai", Some("https://api.moonshot.ai/v1")),
            source("zai", Some("https://api.z.ai/api/paas/v4")),
            source("zai-coding-plan", Some("https://api.z.ai/api/coding/paas/v4")),
        ]
    }

    /// SME-111: a llama.cpp server wants a bearer token if any, and
    /// ignores `cache_control` (it keeps its own prefix cache).
    #[test]
    fn test_a_llama_cpp_provider_starts_with_bearer_auth_and_no_caching() {
        assert_eq!(ProviderKind::parse("llama_cpp"), Some(ProviderKind::LlamaCpp));
        assert_eq!(ProviderKind::LlamaCpp.label(), "llama.cpp");
        assert_eq!(ProviderKind::LlamaCpp.default_auth_kind(), AuthKind::Bearer);
        assert!(!ProviderKind::LlamaCpp.default_prompt_caching());
        assert_eq!(
            ProviderKind::ALL,
            [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::LlamaCpp, ProviderKind::Other],
            "listed between Ollama and Other"
        );
    }

    /// The server's info goes to the browser on the provider's summary.
    #[test]
    fn test_llama_server_info_round_trips_through_json() {
        let info = LlamaServerInfo {
            n_ctx: Some(262_144),
            total_slots: Some(1),
            model_alias: Some("flash-next".to_string()),
            build_info: Some("b1".to_string()),
            template_caps: [("supports_reasoning_effort".to_string(), true)].into_iter().collect(),
        };
        let json = serde_json::to_string(&info).expect("serializes");
        assert_eq!(serde_json::from_str::<LlamaServerInfo>(&json).expect("deserializes"), info);
        assert!(info.supports_reasoning_effort());
        assert!(!info.supports_preserve_reasoning(), "not reported");
    }

    #[test]
    fn test_anthropics_own_api_is_priced_as_anthropic() {
        let sources = catalog_sources();
        assert_eq!(
            suggest_price_source(&sources, ProviderKind::Anthropic, "https://api.anthropic.com").as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            suggest_price_source(&sources, ProviderKind::Anthropic, "https://API.anthropic.com/").as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            suggest_price_source(&sources, ProviderKind::Anthropic, "http://localhost:4000"),
            None,
            "a gateway isn't Anthropic's prices"
        );
    }

    #[test]
    fn test_another_service_is_matched_by_host_then_path() {
        let sources = catalog_sources();
        let suggest = |url| suggest_price_source(&sources, ProviderKind::Other, url);
        assert_eq!(suggest("https://api.deepseek.com/anthropic").as_deref(), Some("deepseek"));
        assert_eq!(suggest("https://api.moonshot.ai/anthropic").as_deref(), Some("moonshotai"));
        assert_eq!(suggest("https://api.z.ai/api/coding/paas/v4").as_deref(), Some("zai-coding-plan"), "nearest path");
        assert_eq!(suggest("https://api.z.ai/api/anthropic"), None, "two equally near");
        assert_eq!(suggest("https://api.minimax.io/anthropic"), None, "a service and its plan share the address");
        assert_eq!(suggest("http://llama:8080"), None);
        assert_eq!(suggest("not a url"), None);
    }

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
                effort: None,
                max_output: None,
                reported_max_output: None,
                reasoning_budget: None,
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

        fn settings(
            thinking: Option<bool>,
            context_window: Option<u32>,
            effort: Option<crate::anthropic::Effort>,
        ) -> ModelSettings {
            ModelSettings { thinking, context_window, effort, ..Default::default() }
        }

        /// SME-111: a reply's cap is the user's, else the provider's, else
        /// 16,384 where a Claude model may sit behind the provider.
        #[test]
        fn test_the_output_cap_prefers_the_override_then_the_report_then_the_kinds() {
            // Sized, as the window is unless a case says otherwise.
            let capped = |max_output, reported_max_output| db::ProviderModelRow {
                max_output,
                reported_max_output,
                ..row(None, None, Some(262_144), None)
            };
            assert_eq!(output_cap(ProviderKind::Anthropic, "m", Some(&capped(Some(8192), Some(128_000)))), Some(8192));
            assert_eq!(output_cap(ProviderKind::Anthropic, "m", Some(&capped(None, Some(128_000)))), Some(128_000));
            assert_eq!(output_cap(ProviderKind::Anthropic, "m", None), Some(UNKNOWN_OUTPUT_CAP));
            assert_eq!(output_cap(ProviderKind::Other, "m", Some(&capped(Some(0), Some(-1)))), Some(UNKNOWN_OUTPUT_CAP), "not sizes");
            assert_eq!(output_cap(ProviderKind::LlamaCpp, "m", Some(&capped(None, None))), None, "the window bounds it");
            // SME-111 review 1: a window nothing sized (assumed 200,000)
            // doesn't bound a local server's real one, so it keeps 16,384.
            assert_eq!(output_cap(ProviderKind::LlamaCpp, "m", None), Some(UNKNOWN_OUTPUT_CAP));
            assert_eq!(output_cap(ProviderKind::Ollama, "m", Some(&row(None, None, None, None))), Some(UNKNOWN_OUTPUT_CAP));
            assert_eq!(output_cap(ProviderKind::Ollama, "m", Some(&capped(None, None))), None);
            assert_eq!(output_cap(ProviderKind::LlamaCpp, "m", Some(&capped(Some(32_768), None))), Some(32_768));
            // SME-111 review 1: Anthropic refuses more than the model's cap.
            assert_eq!(output_cap(ProviderKind::Anthropic, "m", Some(&capped(Some(200_000), Some(128_000)))), Some(128_000));
            assert_eq!(output_cap(ProviderKind::Other, "m", Some(&capped(Some(200_000), Some(128_000)))), Some(200_000), "a gateway's report may not be the model's");
            let info = model_info(ProviderKind::LlamaCpp, "m".to_string(), None, Some(&capped(Some(32_768), None)), None);
            assert_eq!((info.max_output_override, info.max_output), (Some(32_768), Some(32_768)));
        }

        #[test]
        fn test_thinking_prefers_the_override_then_the_report_then_on() {
            assert!(thinking(None));
            assert!(!thinking(Some(&row(None, None, None, Some(false)))));
            assert!(thinking(Some(&row(Some(true), None, None, Some(false)))));
            assert!(!thinking(Some(&row(Some(false), None, None, None))));
        }

        async fn provider(pool: &PgPool, name: &str, auth_kind: &str) -> db::InferenceProvider {
            db::create_inference_provider(pool, name, "ollama", "http://ollama:11434", auth_kind, "the-secret", false, None, true)
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
            db::set_provider_model_reported(&pool, ollama.id, "gemma4", Some(4096), Some(true), Some(false), None)
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
            db::set_provider_model_overrides(&pool, bearer.id, "m1", Some(false), Some(64_000), None, None, None)
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
                    provider_id: bearer.id,
                    model: "m1".to_string(),
                    thinking: false,
                    prompt_caching: false,
                    price_catalog_provider: None,
                    effort: None,
                    kind: ProviderKind::Ollama,
                    template: None,
                    output_cap: None,
                    reasoning_cap: None,
                    output_cap_is_models_own: false,
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
                prompt_caching: false,
                price_catalog_provider: None,
                keep_reasoning: true,
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
                    prompt_caching: false,
                    price_catalog_provider: None,
                    keep_reasoning: true,
                    server_caps: None,
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
            let caching = update_provider(&pool, created.id, ProviderInput {
                prompt_caching: true,
                price_catalog_provider: Some(" deepseek ".to_string()),
                ..input("Home", "http://ollama:11434", "")
            })
            .await
            .expect("turn caching on");
            assert!(caching.prompt_caching, "the form's choice is saved and shown");
            assert_eq!(caching.price_catalog_provider.as_deref(), Some("deepseek"));
            let unpriced = update_provider(&pool, created.id, ProviderInput {
                price_catalog_provider: Some("  ".to_string()),
                ..input("Home", "http://ollama:11434", "")
            })
            .await
            .expect("no prices");
            assert_eq!(unpriced.price_catalog_provider, None, "a blank choice is none");
            assert_eq!(
                update_provider(&pool, created.id + 100, input("Z", "http://h", "")).await,
                Err("That provider no longer exists.".to_string())
            );
        }

        /// Effort is kept per model and sent only on an Anthropic provider:
        /// another server may refuse the field (SME-106).
        #[sqlx::test]
        async fn test_a_models_effort_is_kept_and_sent_only_to_anthropic(pool: PgPool) {
            use crate::anthropic::Effort;
            let anthropic = db::create_inference_provider(&pool, "a", "anthropic", "http://127.0.0.1:1", "api_key", "k", false, None, true)
                .await
                .expect("anthropic provider");
            let ollama = provider(&pool, "o", "api_key").await;
            for p in [&anthropic, &ollama] {
                set_model_settings(&pool, p.id, "m", settings(None, None, Some(Effort::Low))).await.expect("set");
                let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
                assert_eq!(model_info(ProviderKind::Anthropic, "m".to_string(), None, Some(&row), None).effort_override, Some(Effort::Low));
            }

            let on_anthropic = db::create_conversation_with_id(&pool, 9_172_000_010).await.expect("conversation");
            db::set_conversation_model(&pool, on_anthropic.id, anthropic.id, "m").await.expect("pick");
            let turn = resolve_turn_model(&pool, on_anthropic.id).await.expect("resolves");
            assert_eq!(turn.effort, Some(Effort::Low));

            let on_ollama = db::create_conversation_with_id(&pool, 9_172_000_011).await.expect("conversation");
            db::set_conversation_model(&pool, on_ollama.id, ollama.id, "m").await.expect("pick");
            let turn = resolve_turn_model(&pool, on_ollama.id).await.expect("resolves");
            assert_eq!(turn.effort, None, "not sent to an Ollama server");

            set_model_settings(&pool, anthropic.id, "m", settings(None, None, None)).await.expect("clear");
            let turn = resolve_turn_model(&pool, on_anthropic.id).await.expect("resolves");
            assert_eq!(turn.effort, None, "unset sends none");
        }

        fn llama_turn_model(template: Option<TemplateSettings>) -> TurnModel {
            TurnModel {
                endpoint: Endpoint { base_url: "http://llama".to_string(), auth: Auth::Bearer("k".to_string()) },
                provider_id: 1,
                model: "flash-next".to_string(),
                thinking: true,
                prompt_caching: false,
                price_catalog_provider: None,
                effort: None,
                kind: ProviderKind::LlamaCpp,
                template,
                output_cap: None,
                reasoning_cap: None,
                output_cap_is_models_own: false,
                context_window: 262_144,
                thinking_stripped_through: None,
            }
        }

        /// SME-111: thinking may take three quarters of the reply budget,
        /// leaving 4,096 for the answer, and no more than the user's cap.
        #[test]
        fn test_the_reasoning_budget_leaves_room_to_answer() {
            assert_eq!(reasoning_budget(131_072, None), 98_304, "three quarters");
            assert_eq!(reasoning_budget(131_072, Some(20_000)), 20_000, "the user's cap");
            assert_eq!(reasoning_budget(131_072, Some(200_000)), 98_304, "a cap above it changes nothing");
            assert_eq!(reasoning_budget(12_000, None), 7_904, "4,096 left to answer");
            assert_eq!(reasoning_budget(2_048, None), 0, "too small to think in");
        }

        /// SME-111: only llama.cpp is sent a budget; with thinking off,
        /// nothing.
        #[test]
        fn test_only_llama_cpp_is_sent_a_thinking_budget() {
            use crate::anthropic::ThinkingConfig;
            let llama = TurnModel { reasoning_cap: Some(50_000), ..llama_turn_model(None) };
            assert_eq!(llama.thinking_config(131_072), Some(ThinkingConfig::Enabled { budget_tokens: 50_000 }));
            assert_eq!(TurnModel { thinking: false, ..llama.clone() }.thinking_config(131_072), None);
            assert_eq!(llama.thinking_config(4_096), None, "no room to think in (review 1)");
            for kind in [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::Other] {
                assert_eq!(
                    TurnModel { kind, ..llama.clone() }.thinking_config(131_072),
                    Some(ThinkingConfig::Adaptive),
                    "{kind:?}"
                );
            }
        }

        /// SME-111: what a llama.cpp request tells its chat template, with
        /// thinking on and off; nothing for any other kind.
        #[test]
        fn test_a_llama_cpp_turn_sends_its_template_settings() {
            use crate::anthropic::{ChatTemplateKwargs, Effort};
            let set = TemplateSettings { effort: Some(Effort::Low), preserve_thinking: Some(false) };
            let turn = llama_turn_model(Some(set.clone()));
            assert_eq!(
                turn.chat_template_kwargs(true),
                Some(ChatTemplateKwargs { enable_thinking: None, reasoning_effort: Some(Effort::Low), preserve_thinking: Some(false) })
            );
            assert_eq!(
                turn.chat_template_kwargs(false),
                Some(ChatTemplateKwargs { enable_thinking: Some(false), reasoning_effort: None, preserve_thinking: Some(false) }),
                "thinking off says so, and effort means nothing without it"
            );
            assert_eq!(llama_turn_model(Some(TemplateSettings::default())).chat_template_kwargs(true), None, "nothing to send");
            assert_eq!(
                llama_turn_model(None).chat_template_kwargs(false),
                Some(ChatTemplateKwargs { enable_thinking: Some(false), ..Default::default() })
            );
            for kind in [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::Other] {
                let other = TurnModel { kind, ..llama_turn_model(Some(set.clone())) };
                assert_eq!(other.chat_template_kwargs(false), None, "{kind:?} isn't sent them");
            }
        }

        fn llama_provider(caps: Option<serde_json::Value>, keep_reasoning: bool) -> db::InferenceProvider {
            db::InferenceProvider {
                id: 1,
                name: "llama".to_string(),
                kind: "llama_cpp".to_string(),
                base_url: "http://llama".to_string(),
                auth_kind: "bearer".to_string(),
                secret: "k".to_string(),
                prompt_caching: false,
                price_catalog_provider: None,
                keep_reasoning,
                server_caps: caps,
                created_at: chrono::NaiveDateTime::default(),
                updated_at: chrono::NaiveDateTime::default(),
            }
        }

        /// SME-111: a setting goes only to a template whose caps say it
        /// reads it; one with no caps read yet is sent none.
        #[test]
        fn test_template_settings_follow_the_templates_caps() {
            use crate::anthropic::Effort;
            let row = db::ProviderModelRow { effort: Some("medium".to_string()), ..row(None, None, None, None) };
            let both = serde_json::json!({"template_caps": {"supports_reasoning_effort": true, "supports_preserve_reasoning": true}});
            assert_eq!(
                template_settings(&llama_provider(Some(both.clone()), false), Some(&row)),
                TemplateSettings { effort: Some(Effort::Medium), preserve_thinking: Some(false) }
            );
            assert_eq!(
                template_settings(&llama_provider(Some(both), true), None),
                TemplateSettings { effort: None, preserve_thinking: Some(true) }
            );
            // SME-111 review 1: a level the row doesn't offer for llama.cpp
            // (set while the provider was another kind) isn't sent: the
            // user's template refuses `max`, failing every turn.
            let effort_caps = serde_json::json!({"template_caps": {"supports_reasoning_effort": true}});
            for (stored, sent) in [("max", None), ("xhigh", None), ("high", Some(Effort::High))] {
                let stale = db::ProviderModelRow { effort: Some(stored.to_string()), ..self::row(None, None, None, None) };
                assert_eq!(template_settings(&llama_provider(Some(effort_caps.clone()), false), Some(&stale)).effort, sent, "{stored}");
            }
            let neither = serde_json::json!({"template_caps": {"supports_reasoning_effort": false}});
            assert_eq!(template_settings(&llama_provider(Some(neither), false), Some(&row)), TemplateSettings::default());
            assert_eq!(template_settings(&llama_provider(None, false), Some(&row)), TemplateSettings::default(), "never read");
            assert_eq!(
                template_settings(&llama_provider(Some(serde_json::json!("garbage")), false), Some(&row)),
                TemplateSettings::default(),
                "unreadable"
            );
        }

        /// A llama.cpp-shaped mock: `/v1/models` lists one model, and
        /// `/props` answers with the real server's fixture until
        /// `props_up` is cleared, then fails.
        async fn mock_llama_cpp(props_up: std::sync::Arc<std::sync::atomic::AtomicBool>) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let app = axum::Router::new()
                .route(
                    "/v1/models",
                    axum::routing::get(|| async {
                        ([(axum::http::header::CONTENT_TYPE, "application/json")],
                         include_str!("anthropic/fixtures/llama_cpp_v1_models.json"))
                    }),
                )
                .route(
                    "/props",
                    axum::routing::get(move || {
                        let up = props_up.load(std::sync::atomic::Ordering::SeqCst);
                        async move {
                            if up {
                                (axum::http::StatusCode::OK, include_str!("anthropic/fixtures/llama_cpp_props.json").to_string())
                            } else {
                                (axum::http::StatusCode::SERVICE_UNAVAILABLE, "loading model".to_string())
                            }
                        }
                    }),
                );
            tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            format!("http://{addr}")
        }

        /// SME-111: a llama.cpp provider's page reads `/props` with its
        /// models, keeps it, and a turn sends what its template supports.
        #[sqlx::test]
        async fn test_a_llama_cpp_providers_props_are_kept_and_used_by_its_turns(pool: PgPool) {
            use crate::anthropic::Effort;
            let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let base = mock_llama_cpp(up.clone()).await;
            let p = db::create_inference_provider(&pool, "llama", "llama_cpp", &base, "bearer", "k", false, None, false)
                .await
                .expect("create");
            set_model_settings(&pool, p.id, "flash-next", settings(None, None, Some(Effort::Low))).await.expect("effort");

            let listing = provider_models(&pool, p.id, true).await.expect("models");
            assert_eq!(listing.kind, ProviderKind::LlamaCpp);
            assert_eq!(listing.server_error, None);
            let caps = listing.server_caps.clone().expect("read /props");
            assert_eq!((caps.n_ctx, caps.total_slots), (Some(262_144), Some(1)));
            let model = &listing.models[0];
            assert_eq!((model.id.as_str(), model.context_window, model.reported_tools), ("flash-next", 262_144, Some(true)));
            let summary: ProviderSummary = db::get_inference_provider(&pool, p.id).await.expect("get").expect("exists").into();
            assert_eq!(summary.server_caps, Some(caps.clone()), "kept on the provider");

            let conversation = db::create_conversation_with_id(&pool, 9_172_000_020).await.expect("conversation");
            db::set_conversation_model(&pool, conversation.id, p.id, "flash-next").await.expect("pick");
            let turn = resolve_turn_model(&pool, conversation.id).await.expect("resolves");
            assert_eq!(turn.kind, ProviderKind::LlamaCpp);
            assert_eq!(turn.effort, None, "not as output_config");
            assert_eq!(turn.template, Some(TemplateSettings { effort: Some(Effort::Low), preserve_thinking: Some(false) }));

            // The server is down: the page says why, and keeps what it read.
            up.store(false, std::sync::atomic::Ordering::SeqCst);
            let listing = provider_models(&pool, p.id, true).await.expect("models");
            assert!(listing.server_error.as_deref().is_some_and(|e| e.contains("503")), "{listing:?}");
            assert_eq!(listing.server_caps, Some(caps));
        }

        /// SME-111 review 2: the picker's quick listing (no `/props`)
        /// doesn't replace the slot window and tool support `/props` gave
        /// with `/v1/models`' whole-server `meta.n_ctx` and no tool flag.
        #[sqlx::test]
        async fn test_a_quick_llama_cpp_listing_keeps_what_props_said(pool: PgPool) {
            let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let base = mock_llama_cpp(up).await;
            let p = db::create_inference_provider(&pool, "llama", "llama_cpp", &base, "bearer", "k", false, None, true)
                .await
                .expect("create");
            // A four-slot server: each slot has a quarter of the 262,144.
            let caps = serde_json::json!({
                "n_ctx": 65536, "total_slots": 4, "model_alias": "flash-next",
                "template_caps": {"supports_tool_calls": false}
            });
            db::set_inference_provider_server_caps(&pool, p.id, &caps).await.expect("caps");

            let listing = provider_models(&pool, p.id, false).await.expect("models");

            let model = &listing.models[0];
            assert_eq!((model.context_window, model.reported_tools), (65_536, Some(false)), "{model:?}");
            let stored = db::get_provider_model(&pool, p.id, "flash-next").await.expect("get").expect("stored");
            assert_eq!((stored.reported_context_window, stored.reported_tools), (Some(65_536), Some(false)));
        }

        #[sqlx::test]
        async fn test_model_settings_are_checked(pool: PgPool) {
            let p = provider(&pool, "p", "api_key").await;
            assert!(set_model_settings(&pool, p.id, " ", settings(None, None, None)).await.is_err());
            assert!(set_model_settings(&pool, p.id, "m", settings(None, Some(10), None)).await.is_err(), "too small");
            assert!(set_model_settings(&pool, p.id, "m", settings(None, Some(u32::MAX), None)).await.is_err(), "too big");
            assert_eq!(
                set_model_settings(&pool, p.id + 1, "m", settings(Some(true), None, None)).await,
                Err("That provider no longer exists.".to_string())
            );
            set_model_settings(&pool, p.id, "m", settings(Some(false), Some(32_768), None)).await.expect("set");
            let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
            assert_eq!((row.thinking, row.context_window), (Some(false), Some(32_768)));
            let reply = |max_output| ModelSettings { max_output: Some(max_output), ..Default::default() };
            assert!(set_model_settings(&pool, p.id, "m", reply(100)).await.is_err(), "too small");
            assert!(set_model_settings(&pool, p.id, "m", reply(u32::MAX)).await.is_err(), "too big");
            set_model_settings(&pool, p.id, "m", reply(65_536)).await.expect("set");
            let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
            assert_eq!((row.max_output, row.context_window), (Some(65_536), None), "every setting saved together");
            let thinking = |reasoning_budget| ModelSettings { reasoning_budget: Some(reasoning_budget), ..Default::default() };
            assert!(set_model_settings(&pool, p.id, "m", thinking(0)).await.is_err(), "zero");
            set_model_settings(&pool, p.id, "m", thinking(20_000)).await.expect("set");
            let row = db::get_provider_model(&pool, p.id, "m").await.expect("get").expect("exists");
            assert_eq!(row.reasoning_budget, Some(20_000));
            assert_eq!(model_info(ProviderKind::LlamaCpp, "m".to_string(), None, Some(&row), None).reasoning_budget_override, Some(20_000));
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
            let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k", false, None, true)
                .await
                .expect("create");
            set_model_settings(&pool, p.id, "typed-by-hand", settings(None, Some(16_384), None)).await.expect("set");

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
            let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k", false, None, true)
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
            set_model_settings(&pool, p.id, "llama3", settings(Some(false), Some(32_768), None)).await.expect("set");
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
            let p = db::create_inference_provider(&pool, "o", "ollama", &base, "api_key", "k", false, None, true)
                .await
                .expect("create");
            db::set_provider_model_reported(&pool, p.id, "removed", Some(4096), None, Some(true), None)
                .await
                .expect("seen once");
            add_model(&pool, p.id, "by-hand").await.expect("add");
            set_model_settings(&pool, p.id, "tuned", settings(None, Some(16_384), None)).await.expect("set");

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
            let p = db::create_inference_provider(&pool, "o", "ollama", &format!("http://{addr}"), "api_key", "k", false, None, true)
                .await
                .expect("create");

            let listing = provider_models(&pool, p.id, true).await.expect("models");

            assert_eq!(ps_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            let b = listing.models.iter().find(|m| m.id == "b").expect("b");
            assert_eq!((b.context_window, b.context_window_known), (8192, true));
        }

        /// SME-111: Anthropic's listing's `max_tokens` is kept as the
        /// model's reported cap, and a turn on it is capped by it.
        #[sqlx::test]
        async fn test_an_anthropic_models_reported_cap_is_kept_and_used(pool: PgPool) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let app = axum::Router::new().route("/v1/models", axum::routing::get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "application/json")],
                 r#"{"data":[{"id":"claude-x","max_input_tokens":1000000,"max_tokens":64000}],"has_more":false}"#)
            }));
            tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            let p = db::create_inference_provider(&pool, "a", "anthropic", &format!("http://{addr}"), "api_key", "k", false, None, true)
                .await
                .expect("create");
            let listing = provider_models(&pool, p.id, true).await.expect("models");
            assert_eq!((listing.models[0].max_output, listing.models[0].max_output_override), (Some(64_000), None));
            let conversation = db::create_conversation_with_id(&pool, 9_172_000_021).await.expect("conversation");
            db::set_conversation_model(&pool, conversation.id, p.id, "claude-x").await.expect("pick");
            assert_eq!(resolve_turn_model(&pool, conversation.id).await.expect("resolves").output_cap, Some(64_000));
        }

        #[sqlx::test]
        async fn test_an_unreachable_providers_models_still_list_what_is_stored(pool: PgPool) {
            let p = db::create_inference_provider(&pool, "gone", "anthropic", "http://127.0.0.1:1", "api_key", "k", false, None, true)
                .await
                .expect("create");
            set_model_settings(&pool, p.id, "claude-opus-5", settings(Some(true), None, None)).await.expect("set");
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

    /// Serializes the tests that run turns, here and in `sandbox`'s
    /// Docker-restart notice test. They
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
            false,
            None,
            true,
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
