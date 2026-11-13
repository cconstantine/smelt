use super::*;

// --- The Claude subscription's usage windows (SME-486) ---

/// One window's latest reading for a provider: `claude_cli::usage`
/// writes it, and serves it with its pace.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ProviderUsageWindowRow {
    pub window_name: String,
    pub utilization: f64,
    pub status: String,
    pub resets_at: Option<chrono::DateTime<chrono::Utc>>,
    pub seen_at: chrono::DateTime<chrono::Utc>,
}

/// Upserts `rows` as `provider_id`'s, each replacing its window's row
/// only when it was read at the same time or later: a late, older
/// reading (two conversations racing on one provider) never wins.
pub async fn record_usage_windows(pool: &PgPool, provider_id: i64, rows: &[ProviderUsageWindowRow]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for row in rows {
        sqlx::query(
            "INSERT INTO provider_usage_windows (provider_id, window_name, utilization, status, resets_at, seen_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (provider_id, window_name) DO UPDATE SET
                 utilization = EXCLUDED.utilization,
                 status = EXCLUDED.status,
                 resets_at = EXCLUDED.resets_at,
                 seen_at = EXCLUDED.seen_at
             WHERE provider_usage_windows.seen_at <= EXCLUDED.seen_at",
        )
        .bind(provider_id)
        .bind(&row.window_name)
        .bind(row.utilization)
        .bind(&row.status)
        .bind(row.resets_at)
        .bind(row.seen_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// `provider_id`'s windows, by name.
pub async fn provider_usage_windows(pool: &PgPool, provider_id: i64) -> Result<Vec<ProviderUsageWindowRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT window_name, utilization, status, resets_at, seen_at FROM provider_usage_windows
         WHERE provider_id = $1 ORDER BY window_name",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await
}
