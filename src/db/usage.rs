use super::*;

/// The saved price catalog (SME-106): when it was fetched, and
/// `pricing::Providers` as JSON.
pub async fn get_price_catalog(
    pool: &PgPool,
) -> Result<Option<(NaiveDateTime, serde_json::Value)>, sqlx::Error> {
    sqlx::query_as("SELECT fetched_at, providers FROM price_catalog").fetch_optional(pool).await
}

/// Replaces the saved price catalog.
pub async fn save_price_catalog(
    pool: &PgPool,
    fetched_at: NaiveDateTime,
    providers: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO price_catalog (id, fetched_at, providers) VALUES (TRUE, $1, $2)
         ON CONFLICT (id) DO UPDATE SET fetched_at = EXCLUDED.fetched_at, providers = EXCLUDED.providers",
    )
    .bind(fetched_at)
    .bind(providers)
    .execute(pool)
    .await?;
    Ok(())
}

/// Each provider's models' calls since `since`, costliest first (SME-106).
/// A deleted provider's calls are kept, grouped under no provider.
pub async fn model_spend_since(
    pool: &PgPool,
    since: NaiveDateTime,
) -> Result<Vec<crate::models::ModelSpend>, sqlx::Error> {
    sqlx::query_as::<_, crate::models::ModelSpend>(
        "SELECT p.name AS provider_name, u.model,
                count(*) AS calls,
                sum(u.input_tokens)::BIGINT AS input_tokens,
                sum(u.output_tokens)::BIGINT AS output_tokens,
                sum(u.cache_creation_input_tokens)::BIGINT AS cache_creation_input_tokens,
                sum(u.cache_read_input_tokens)::BIGINT AS cache_read_input_tokens,
                sum(u.cost_usd) AS cost_usd,
                count(*) FILTER (WHERE u.cost_usd IS NULL) AS unpriced_calls
         FROM model_call_usage u
         LEFT JOIN inference_providers p ON p.id = u.provider_id
         WHERE u.created_at >= $1
         GROUP BY u.provider_id, p.name, u.model
         ORDER BY sum(u.cost_usd) DESC NULLS LAST, count(*) DESC, p.name, u.model",
    )
    .bind(since)
    .fetch_all(pool)
    .await
}

/// Which request a `model_call_usage` row is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallKind {
    Turn,
    Compaction,
}

impl ModelCallKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Compaction => "compaction",
        }
    }
}

/// One model call, for `record_model_call`: finished, or stopped or
/// failed with what it used (SME-178).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCall<'a> {
    pub conversation_id: i64,
    pub provider_id: i64,
    pub model: &'a str,
    pub kind: ModelCallKind,
    pub usage: crate::anthropic::TokenUsage,
    /// From the price catalog when the call finished; `None` without a price.
    pub cost_usd: Option<f64>,
    /// Whether the call finished, its reply saved (SME-178): a stopped or
    /// failed one is recorded with what it used, but isn't the
    /// conversation's last-known usage.
    pub finished: bool,
}

/// Appends `call` to `model_call_usage` (SME-106). A finished turn's call
/// is also the conversation's new last-known usage
/// (`upsert_conversation_usage`), written in the same transaction so the
/// two never disagree. A compaction's isn't: it described the old, long
/// history. Nor is an unfinished one's: its reply may not be saved.
///
/// A conversation deleted before or while the call is recorded gives a
/// row with no conversation, as the deletion's own SET NULL would
/// (SME-178), and no last-known usage. Only tests call it: production
/// records go through `record_model_call_if_current`.
#[cfg(test)]
pub async fn record_model_call(pool: &PgPool, call: &ModelCall<'_>) -> Result<(), sqlx::Error> {
    record_model_call_if_current(pool, call, || true).await.map(|_| ())
}

/// `record_model_call`, for a call whose turn may have been stopped while
/// it was recorded (code review 1): its record then runs on its own, and
/// can commit after the next turn's. A finished turn call is the
/// last-known usage only if `still_current` says its turn still is, asked
/// holding the conversation's row (`FOR NO KEY UPDATE`), which every
/// writer of the last-known usage takes first, this one and
/// `clear_conversation_usage`. A record that sees its turn current
/// therefore commits before any newer turn's write: that turn starts only
/// once this one has ended, and then waits for the row. Says whether it
/// wrote the last-known usage.
pub async fn record_model_call_if_current(
    pool: &PgPool,
    call: &ModelCall<'_>,
    still_current: impl FnOnce() -> bool + Send,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let conversation_id: Option<i64> = sqlx::query_scalar(
        // The provider as it is now: a turn reads it when it starts, and the
        // user may delete it before the reply finishes. A deleted one gives
        // null, as the deletion's own SET NULL would (code review 1). `FOR
        // KEY SHARE` waits for a delete still in progress and then reads
        // null; without it the id is read, and the foreign key fails once
        // the delete commits (code review 2). The conversation the same
        // way (SME-178), locked first, since VALUES is evaluated left to
        // right, and `FOR NO KEY UPDATE`, the lock that orders writes of
        // its last-known usage (SME-178 code review 1). Taken before the
        // provider's: a provider's delete locks its conversations first
        // too, so the two can't wait on each other.
        "INSERT INTO model_call_usage
             (conversation_id, provider_id, model, kind, input_tokens, output_tokens,
              cache_creation_input_tokens, cache_read_input_tokens, cost_usd)
         VALUES ((SELECT id FROM conversations WHERE id = $1 FOR NO KEY UPDATE),
                 (SELECT id FROM inference_providers WHERE id = $2 FOR KEY SHARE),
                 $3, $4, $5, $6, $7, $8, $9)
         RETURNING conversation_id",
    )
    .bind(call.conversation_id)
    .bind(call.provider_id)
    .bind(call.model)
    .bind(call.kind.as_str())
    .bind(call.usage.input_tokens)
    .bind(call.usage.output_tokens)
    .bind(call.usage.cache_creation_input_tokens)
    .bind(call.usage.cache_read_input_tokens)
    .bind(call.cost_usd)
    .fetch_one(&mut *tx)
    .await?;
    let current = if let Some(conversation_id) = conversation_id
        && call.kind == ModelCallKind::Turn
        && call.finished
        && still_current()
    {
        upsert_conversation_usage(&mut *tx, conversation_id, &call.usage).await?;
        true
    } else {
        false
    };
    tx.commit().await?;
    Ok(current)
}

pub async fn get_conversation_spend(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<crate::models::ConversationSpend, sqlx::Error> {
    sqlx::query_as::<_, crate::models::ConversationSpend>(
        "SELECT count(*) AS calls,
                COALESCE(sum(input_tokens), 0)::BIGINT AS input_tokens,
                COALESCE(sum(output_tokens), 0)::BIGINT AS output_tokens,
                COALESCE(sum(cache_creation_input_tokens), 0)::BIGINT AS cache_creation_input_tokens,
                COALESCE(sum(cache_read_input_tokens), 0)::BIGINT AS cache_read_input_tokens,
                sum(cost_usd) AS cost_usd,
                count(*) FILTER (WHERE cost_usd IS NULL) AS unpriced_calls
         FROM model_call_usage WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
}
