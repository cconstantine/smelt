use super::*;

// --- Model providers (SME-72) ---
// Configuration a person edits on /providers, like mcp_servers: plain
// CRUD. `kind` and `auth_kind` are plain strings, the table's `CHECK`s
// being the source of truth (`McpServerConfig::auth_mode`'s precedent);
// `providers.rs` turns them into enums at its boundary.

#[derive(Clone, PartialEq, sqlx::FromRow)]
pub struct InferenceProvider {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub auth_kind: String,
    /// Never sent to the browser; `providers::secret_hint` is.
    pub secret: String,
    /// Mark turn requests for prompt caching (SME-106).
    pub prompt_caching: bool,
    /// The models.dev provider that prices its calls (SME-106).
    pub price_catalog_provider: Option<String>,
    /// llama.cpp only: send the template's `preserve_thinking` (SME-111).
    pub keep_reasoning: bool,
    /// llama.cpp only: its `/props`, as `providers::LlamaServerInfo`.
    pub server_caps: Option<serde_json::Value>,
    /// How many requests it's sent at once: `None` no limit, `0`
    /// the count its `/props` reports, else that many (SME-162).
    pub max_concurrent_requests: Option<i32>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// By hand, so the secret never reaches a log line.
impl std::fmt::Debug for InferenceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceProvider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("auth_kind", &self.auth_kind)
            .field("prompt_caching", &self.prompt_caching)
            .field("price_catalog_provider", &self.price_catalog_provider)
            .field("keep_reasoning", &self.keep_reasoning)
            .field("server_caps", &self.server_caps)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("secret", &"..")
            .finish_non_exhaustive()
    }
}

pub async fn create_inference_provider(
    pool: &PgPool,
    name: &str,
    kind: &str,
    base_url: &str,
    auth_kind: &str,
    secret: &str,
    prompt_caching: bool,
    price_catalog_provider: Option<&str>,
    keep_reasoning: bool,
    max_concurrent_requests: Option<i32>,
) -> Result<InferenceProvider, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>(
        "INSERT INTO inference_providers (name, kind, base_url, auth_kind, secret, prompt_caching, price_catalog_provider, keep_reasoning,
                                          max_concurrent_requests)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *",
    )
    .bind(name)
    .bind(kind)
    .bind(base_url)
    .bind(auth_kind)
    .bind(secret)
    .bind(prompt_caching)
    .bind(price_catalog_provider)
    .bind(keep_reasoning)
    .bind(max_concurrent_requests)
    .fetch_one(pool)
    .await
}

pub async fn list_inference_providers(pool: &PgPool) -> Result<Vec<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>("SELECT * FROM inference_providers ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn get_inference_provider(
    pool: &PgPool,
    id: i64,
) -> Result<Option<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>("SELECT * FROM inference_providers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Sets every field outright, except `secret`: `None` keeps the stored
/// one, so the edit form never needs to know it.
pub async fn update_inference_provider(
    pool: &PgPool,
    id: i64,
    name: &str,
    kind: &str,
    base_url: &str,
    auth_kind: &str,
    secret: Option<&str>,
    prompt_caching: bool,
    price_catalog_provider: Option<&str>,
    keep_reasoning: bool,
    max_concurrent_requests: Option<i32>,
) -> Result<Option<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>(
        "UPDATE inference_providers
         SET name = $2, kind = $3, base_url = $4, auth_kind = $5,
             secret = COALESCE($6, secret), prompt_caching = $7, price_catalog_provider = $8,
             keep_reasoning = $9, max_concurrent_requests = $10,
             -- Another server's caps don't describe this one (SME-111 review 2).
             server_caps = CASE WHEN kind = $3 AND base_url = $4 THEN server_caps END,
             updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(kind)
    .bind(base_url)
    .bind(auth_kind)
    .bind(secret)
    .bind(prompt_caching)
    .bind(price_catalog_provider)
    .bind(keep_reasoning)
    .bind(max_concurrent_requests)
    .fetch_optional(pool)
    .await
}

/// Keeps what a llama.cpp server last said about itself (SME-111).
pub async fn set_inference_provider_server_caps(
    pool: &PgPool,
    id: i64,
    server_caps: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE inference_providers SET server_caps = $2 WHERE id = $1")
        .bind(id)
        .bind(server_caps)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes a provider, first clearing it as the default and from every
/// conversation using it, all in one transaction: those conversations take
/// the default at their next turn. The foreign keys are `RESTRICT`, so a
/// reference this missed fails the delete instead of leaving a model with
/// no provider. Returns whether the provider existed.
pub async fn delete_inference_provider(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE inference_settings SET default_provider_id = NULL, default_model = NULL
         WHERE default_provider_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET provider_id = NULL, model = NULL WHERE provider_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let deleted = sqlx::query("DELETE FROM inference_providers WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(deleted == 1)
}

/// A model's row in `provider_models`: the user's overrides and what the
/// provider last reported. Null means unset or not reported.
#[derive(Clone, Debug, Default, PartialEq, sqlx::FromRow)]
pub struct ProviderModelRow {
    pub provider_id: i64,
    pub model: String,
    pub thinking: Option<bool>,
    pub context_window: Option<i32>,
    pub reported_context_window: Option<i32>,
    pub reported_thinking: Option<bool>,
    pub reported_tools: Option<bool>,
    /// The user's effort, `anthropic::Effort` as text (SME-106).
    pub effort: Option<String>,
    /// The user's cap on a reply (SME-111).
    pub max_output: Option<i32>,
    /// The provider's cap on a reply, as last reported.
    pub reported_max_output: Option<i32>,
    /// The user's cap on a reply's thinking, llama.cpp only (SME-111).
    pub reasoning_budget: Option<i32>,
    /// Whether the model takes adaptive thinking, as last reported (B9,
    /// SME-122).
    pub reported_adaptive_thinking: Option<bool>,
    /// Added on the provider's page as a model its listing doesn't show,
    /// so it's shown even when the listing works and lacks it.
    pub added_by_hand: bool,
}

pub async fn get_provider_model(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
) -> Result<Option<ProviderModelRow>, sqlx::Error> {
    sqlx::query_as::<_, ProviderModelRow>(
        "SELECT * FROM provider_models WHERE provider_id = $1 AND model = $2",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_optional(pool)
    .await
}

pub async fn list_provider_models(
    pool: &PgPool,
    provider_id: i64,
) -> Result<Vec<ProviderModelRow>, sqlx::Error> {
    sqlx::query_as::<_, ProviderModelRow>(
        "SELECT * FROM provider_models WHERE provider_id = $1 ORDER BY model ASC",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await
}

/// Marks a model as added by hand, adding its row if it has none and
/// leaving its settings as they are.
pub async fn ensure_provider_model(pool: &PgPool, provider_id: i64, model: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, added_by_hand) VALUES ($1, $2, true)
         ON CONFLICT (provider_id, model) DO UPDATE SET added_by_hand = true",
    )
    .bind(provider_id)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sets the user's overrides for a model, leaving what the provider
/// reported alone. `None` clears an override.
pub async fn set_provider_model_overrides(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    thinking: Option<bool>,
    context_window: Option<i32>,
    effort: Option<&str>,
    max_output: Option<i32>,
    reasoning_budget: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, thinking, context_window, effort, max_output, reasoning_budget)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (provider_id, model) DO UPDATE
         SET thinking = $3, context_window = $4, effort = $5, max_output = $6, reasoning_budget = $7",
    )
    .bind(provider_id)
    .bind(model)
    .bind(thinking)
    .bind(context_window)
    .bind(effort)
    .bind(max_output)
    .bind(reasoning_budget)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces what the provider reported about a model, leaving the user's
/// overrides alone. A report without a context window keeps the last one:
/// Ollama only says while the model is loaded. The same goes for a reply
/// cap (SME-111).
pub async fn set_provider_model_reported(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    context_window: Option<i32>,
    thinking: Option<bool>,
    tools: Option<bool>,
    max_output: Option<i32>,
    adaptive_thinking: Option<bool>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models
             (provider_id, model, reported_context_window, reported_thinking, reported_tools, reported_max_output,
              reported_adaptive_thinking)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (provider_id, model) DO UPDATE
         SET reported_context_window = COALESCE($3, provider_models.reported_context_window),
             reported_thinking = $4, reported_tools = $5,
             reported_max_output = COALESCE($6, provider_models.reported_max_output),
             reported_adaptive_thinking = $7",
    )
    .bind(provider_id)
    .bind(model)
    .bind(context_window)
    .bind(thinking)
    .bind(tools)
    .bind(max_output)
    .bind(adaptive_thinking)
    .execute(pool)
    .await?;
    Ok(())
}

/// The default provider and model, if one is set.
pub async fn get_default_model(pool: &PgPool) -> Result<Option<(i64, String)>, sqlx::Error> {
    let row: Option<(Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT default_provider_id, default_model FROM inference_settings")
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        Some((Some(provider_id), Some(model))) => Some((provider_id, model)),
        _ => None,
    })
}

pub async fn set_default_model(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO inference_settings (default_provider_id, default_model) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE SET default_provider_id = $1, default_model = $2",
    )
    .bind(provider_id)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// A conversation's own provider and model (both `None` until its first
/// turn takes the default), and its last message when they last changed.
#[derive(Clone, Debug, Default, PartialEq, sqlx::FromRow)]
pub struct ConversationModelRow {
    pub provider_id: Option<i64>,
    pub model: Option<String>,
    pub model_changed_at_message_id: Option<i64>,
}

/// `None` when the conversation doesn't exist.
pub async fn get_conversation_model(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<ConversationModelRow>, sqlx::Error> {
    sqlx::query_as::<_, ConversationModelRow>(
        "SELECT provider_id, model, model_changed_at_message_id FROM conversations WHERE id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
}

/// Sets a conversation's provider and model, from its next turn on. The
/// switch point for its thinking blocks is recorded when that turn starts
/// (`record_turn_model`), since a turn still running keeps writing for the
/// old model. Returns whether the conversation exists.
pub async fn set_conversation_model(
    pool: &PgPool,
    conversation_id: i64,
    provider_id: i64,
    model: &str,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query("UPDATE conversations SET provider_id = $2, model = $3 WHERE id = $1")
    .bind(conversation_id)
    .bind(provider_id)
    .bind(model)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Records that a turn is starting on the backend `model_key` names
/// (provider, base URL and model), and returns the message up to which
/// thinking must be left out: when it differs from the last turn's
/// backend, that's the conversation's last message now. Call under the
/// conversation's turn lock, so no turn is writing meanwhile.
pub async fn record_turn_model(
    pool: &PgPool,
    conversation_id: i64,
    model_key: &str,
) -> Result<Option<i64>, sqlx::Error> {
    // The right-hand sides read the row's values from before the update.
    let stamp: Option<(Option<i64>,)> = sqlx::query_as(
        "UPDATE conversations
         SET model_changed_at_message_id = CASE
                 WHEN last_turn_model_key IS DISTINCT FROM $2
                 THEN (SELECT max(id) FROM messages WHERE conversation_id = $1)
                 ELSE model_changed_at_message_id
             END,
             last_turn_model_key = $2
         WHERE id = $1
         RETURNING model_changed_at_message_id",
    )
    .bind(conversation_id)
    .bind(model_key)
    .fetch_optional(pool)
    .await?;
    Ok(stamp.and_then(|(id,)| id))
}

/// Gives a conversation with no model the default, in one statement so a
/// racing change isn't overwritten. Returns the provider and model it took,
/// or `None` when it already had one or there's no default.
pub async fn adopt_default_model(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    let adopted: Option<(i64, String)> = sqlx::query_as(
        "UPDATE conversations c
         SET provider_id = s.default_provider_id, model = s.default_model
         FROM inference_settings s
         WHERE c.id = $1 AND c.provider_id IS NULL AND s.default_provider_id IS NOT NULL
         RETURNING c.provider_id, c.model",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    Ok(adopted)
}
