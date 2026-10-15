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
mod server;

#[cfg(test)]
mod tests;

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
