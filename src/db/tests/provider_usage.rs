//! `provider_usage_windows` (SME-486): a late, older reading never
//! replaces a newer one, the rows go with their provider, and a reading
//! held past a Stop keeps the time it was read.

use super::*;

use chrono::{DateTime, Duration, Utc};

async fn claude_provider(pool: &PgPool) -> i64 {
    crate::db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    create_inference_provider(pool, "Claude", "claude_cli", "claude", "api_key", "", false, None, true, None).await.expect("provider").id
}

fn row(utilization: f64, seen_at: DateTime<Utc>) -> ProviderUsageWindowRow {
    ProviderUsageWindowRow { window_name: "five_hour".into(), utilization, status: "allowed".into(), resets_at: None, seen_at }
}

/// Postgres keeps microseconds: a time that round-trips.
fn now() -> DateTime<Utc> {
    let now = Utc::now();
    now - Duration::nanoseconds(i64::from(now.timestamp_subsec_nanos() % 1_000))
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_older_reading_never_replaces_a_newer(pool: PgPool) {
    let provider = claude_provider(&pool).await;
    let older = row(0.10, now() - Duration::minutes(1));
    let newer = row(0.20, now());
    record_usage_windows(&pool, provider, std::slice::from_ref(&newer)).await.expect("record");
    record_usage_windows(&pool, provider, std::slice::from_ref(&older)).await.expect("record");
    assert_eq!(provider_usage_windows(&pool, provider).await.expect("read"), vec![newer.clone()], "the late, older one is refused");
    let other = claude_provider_named(&pool, "Claude 2").await;
    record_usage_windows(&pool, other, std::slice::from_ref(&older)).await.expect("record");
    record_usage_windows(&pool, other, std::slice::from_ref(&newer)).await.expect("record");
    assert_eq!(provider_usage_windows(&pool, other).await.expect("read"), vec![newer], "the newer one replaces");
}

async fn claude_provider_named(pool: &PgPool, name: &str) -> i64 {
    create_inference_provider(pool, name, "claude_cli", "claude", "api_key", "", false, None, true, None).await.expect("provider").id
}

/// G7 (SME-523): the gate's startup reading is the newest `five_hour` of
/// any Claude Code provider, never another kind's nor another window.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_gates_reading_is_the_newest_claude_code_five_hour(pool: PgPool) {
    let one = claude_provider(&pool).await;
    let two = claude_provider_named(&pool, "Claude 2").await;
    let http = create_inference_provider(&pool, "llama", "llama_cpp", "http://one", "bearer", "k", false, None, true, None).await.expect("provider").id;
    assert_eq!(newest_claude_code_five_hour(&pool).await.expect("read"), None, "nothing recorded");
    let older = row(0.30, now() - Duration::minutes(2));
    let newest = row(0.40, now() - Duration::minutes(1));
    record_usage_windows(&pool, one, std::slice::from_ref(&older)).await.expect("record");
    record_usage_windows(&pool, two, std::slice::from_ref(&newest)).await.expect("record");
    record_usage_windows(&pool, http, &[row(0.90, now())]).await.expect("record");
    let seven_day = ProviderUsageWindowRow { window_name: "seven_day".into(), ..row(0.80, now()) };
    record_usage_windows(&pool, one, &[seven_day]).await.expect("record");
    assert_eq!(newest_claude_code_five_hour(&pool).await.expect("read"), Some(newest));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_rows_go_with_their_provider(pool: PgPool) {
    let provider = claude_provider(&pool).await;
    record_usage_windows(&pool, provider, &[row(0.5, now())]).await.expect("record");
    assert!(delete_inference_provider(&pool, provider).await.expect("delete"));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_usage_windows").fetch_one(&pool).await.expect("count");
    assert_eq!(left, 0, "the rows go with their provider");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_calls_reading_is_recorded_by_the_next_and_keeps_its_time(pool: PgPool) {
    use crate::claude_cli::usage::{record, seen};
    let provider = claude_provider(&pool).await;
    let (stopped, other) = (-486_901, -486_902);
    let event = |utilization: f64| {
        serde_json::json!({"type": "rate_limit_event", "rate_limit_info": {"unifiedWindows": {
            "five_hour": {"utilization": utilization, "status": "allowed"}}}})
    };
    // Read at t0, then the call is dropped by a Stop: nothing records it.
    seen(stopped, &event(0.10));
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    // Another conversation on the provider reads and records at t1.
    seen(other, &event(0.20));
    record(&pool, other, provider).await;
    // The stopped conversation's next call records its held, older one.
    record(&pool, stopped, provider).await;
    let rows = provider_usage_windows(&pool, provider).await.expect("read");
    assert_eq!(rows.len(), 1);
    assert!((rows[0].utilization - 0.20).abs() < 1e-9, "the t1 reading stands: {rows:?}");
}
