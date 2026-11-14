//! SME-345: the storage-grace pass's `sandbox_pods` queries — the quiet
//! stop's mark, the due list, the re-check — against the database, no
//! cluster.
//!
//! The clock is faked the way SME-78's tests fake theirs: an `UPDATE`
//! that moves `quietly_stopped_at` back.

use super::*;

/// `pod_id`'s mark, read back.
async fn mark(pool: &PgPool, pod_id: i64) -> Option<chrono::NaiveDateTime> {
    sqlx::query_scalar("SELECT quietly_stopped_at FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_one(pool)
        .await
        .expect("read quietly_stopped_at")
}

/// A pod's row is stamped with the database's `now()`, and stays stamped
/// until something clears it (the pass, whose clear is commit 3's).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_marking_a_pod_quietly_stopped_stamps_now(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    assert!(mark(&pool, pod.id).await.is_none(), "a live row is unmarked");

    mark_pod_quietly_stopped(&pool, pod.id).await.expect("mark it");

    let stamped = mark(&pool, pod.id).await.expect("the mark");
    let now = sqlx::query_scalar::<_, chrono::NaiveDateTime>("SELECT now()::timestamp")
        .fetch_one(&pool)
        .await
        .expect("the database's now()");
    let age = now - stamped;
    assert!(
        age.num_seconds() >= 0 && age.num_seconds() < 60,
        "the mark is now(), not a stray time: {stamped} vs {now}"
    );
}

/// SME-521: the grace inputs are each listed conversation's latest pod row
/// — whatever ended it, marked or not — and whether it's a sub-agent
/// that's done. A conversation with no row reads `None`; one that no
/// longer exists isn't in the answer at all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_grace_inputs_are_each_conversations_latest_row_and_agent_state(pool: PgPool) {
    let stopped = create_conversation(&pool).await.expect("conversation");
    let old = create_sandbox_pod(&pool, stopped.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, old.id, PodEnd::ByUser).await.expect("an older Stop");
    let latest = create_sandbox_pod(&pool, stopped.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, latest.id, PodEnd::Quiet).await.expect("a quiet stop");

    let live = create_conversation(&pool).await.expect("conversation");
    let live_pod = create_sandbox_pod(&pool, live.id).await.expect("a live pod");
    let never = create_conversation(&pool).await.expect("conversation");
    let agent = create_conversation(&pool).await.expect("conversation");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'b', 'finished')")
        .bind(agent.id)
        .bind(stopped.id)
        .execute(&pool)
        .await
        .expect("a finished agent");
    let gone = create_conversation(&pool).await.expect("conversation");
    sqlx::query("DELETE FROM conversations WHERE id = $1").bind(gone.id).execute(&pool).await.expect("delete it");

    let inputs = storage_grace_inputs(&pool, &[stopped.id, live.id, never.id, agent.id, gone.id]).await.expect("read");
    let row = &inputs.rows[&stopped.id];
    assert_eq!(row.pod_id, Some(latest.id), "the latest row, not the older Stop");
    assert!(row.terminated_at.is_some() && row.quiet && row.stopped_by_user == Some(false), "{row:?}");
    assert!(!row.agent_done);
    let row = &inputs.rows[&live.id];
    assert_eq!((row.pod_id, row.terminated_at), (Some(live_pod.id), None), "a live row has no end");
    let row = &inputs.rows[&never.id];
    assert_eq!((row.pod_id, row.quiet, row.agent_done), (None, false, false), "no row at all");
    assert!(inputs.rows[&agent.id].agent_done, "a finished sub-agent");
    assert!(!inputs.rows.contains_key(&gone.id), "a deleted conversation is gone from the answer");
}

/// The upgrade shape: `quietly_stopped_at` is added nullable with no
/// default, so a row written before the migration — and every stopped
/// row the upgrade finds already there — keeps NULL and is never swept.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_quietly_stopped_at_is_added_nullable_with_no_default(pool: PgPool) {
    let (nullable, default): (String, Option<String>) = sqlx::query_as(
        "SELECT is_nullable, column_default FROM information_schema.columns
          WHERE table_name = 'sandbox_pods' AND column_name = 'quietly_stopped_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("the column's shape");
    assert_eq!(nullable, "YES", "NULL for rows the upgrade didn't write");
    assert_eq!(default, None, "no default that could stamp a row retroactively");
}

/// SME-123 DR9: the Sandboxes page's stored volumes say when each
/// conversation's sandbox stopped and whether that was a quiet stop: its
/// latest pod row, once that row has ended. A live pod, and a
/// conversation that never had one, give nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_conversations_last_stop_is_its_latest_ended_pod(pool: PgPool) {
    let quiet = create_conversation(&pool).await.expect("conversation");
    let old = create_sandbox_pod(&pool, quiet.id).await.expect("pod");
    terminate_sandbox_pod(&pool, old.id).await.expect("end it");
    let latest = create_sandbox_pod(&pool, quiet.id).await.expect("pod");
    mark_pod_quietly_stopped(&pool, latest.id).await.expect("mark it");
    terminate_sandbox_pod(&pool, latest.id).await.expect("end it");

    let stopped_by_user = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, stopped_by_user.id).await.expect("pod");
    terminate_sandbox_pod(&pool, pod.id).await.expect("end it");

    let live = create_conversation(&pool).await.expect("conversation");
    create_sandbox_pod(&pool, live.id).await.expect("a live pod");

    let (stops, _) = last_pod_stops(&pool).await.expect("read");
    assert!(stops.get(&quiet.id).is_some_and(|(_, quiet_stop)| quiet_stop.is_some()), "{stops:?}");
    assert!(stops.get(&stopped_by_user.id).is_some_and(|(_, quiet_stop)| quiet_stop.is_none()), "{stops:?}");
    assert!(!stops.contains_key(&live.id), "a live pod hasn't stopped: {stops:?}");
}

/// `pod_id`'s `stopped_by_user`, read back (SME-521).
async fn stopped_by_user(pool: &PgPool, pod_id: i64) -> Option<bool> {
    sqlx::query_scalar("SELECT stopped_by_user FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_one(pool)
        .await
        .expect("read stopped_by_user")
}

/// SME-521: every way a row is closed says whether the user's Stop closed
/// it, so the storage-grace rule can give a Stop its own grace. A live row
/// says neither, and a `Plain` close after a Stop (a failed create's
/// cleanup inside SME-122's B1 window) must not turn the Stop into
/// something else.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_pod_end_records_whether_the_user_stopped_it(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let plain = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    assert_eq!(stopped_by_user(&pool, plain.id).await, None, "a live row says neither");
    terminate_sandbox_pod_as(&pool, plain.id, PodEnd::Plain).await.expect("plain end");
    assert_eq!(stopped_by_user(&pool, plain.id).await, Some(false), "terminate_pod, a lost pod, a failed create");

    let quiet = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, quiet.id, PodEnd::Quiet).await.expect("quiet end");
    assert_eq!(stopped_by_user(&pool, quiet.id).await, Some(false), "an idle or finish stop");

    let by_user = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, by_user.id, PodEnd::ByUser).await.expect("the user's Stop");
    assert_eq!(stopped_by_user(&pool, by_user.id).await, Some(true), "the user's Stop");
    terminate_sandbox_pod_as(&pool, by_user.id, PodEnd::Plain).await.expect("a plain close after it");
    assert_eq!(stopped_by_user(&pool, by_user.id).await, Some(true), "a later plain close keeps the Stop");

    let quiet_then_user = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, quiet_then_user.id, PodEnd::Quiet).await.expect("quiet end");
    terminate_sandbox_pod_as(&pool, quiet_then_user.id, PodEnd::ByUser).await.expect("then the user's Stop");
    assert_eq!(stopped_by_user(&pool, quiet_then_user.id).await, Some(true), "a Stop landing after a quiet stop wins (B3)");
}

/// SME-354 B14: a stop's age is measured on the database's clock, the one
/// its `terminated_at` was written on. The connection's time zone puts
/// that clock 14 hours from the server's UTC, so a mixed reading is off
/// by hours rather than by a drift too small to see.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_stops_come_with_the_databases_own_clock(pool: PgPool) {
    // Set after connecting: sqlx sends `TimeZone=UTC` at startup, which
    // overrides a `-c timezone` given in the connect options.
    let shifted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET TIME ZONE 'Pacific/Kiritimati'").await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("a +14 pool");
    let offset: String = sqlx::query_scalar("SELECT current_setting('TimeZone')").fetch_one(&shifted).await.expect("tz");
    assert_eq!(offset, "Pacific/Kiritimati", "the session's clock is 14 hours from UTC");
    let conversation = create_conversation(&shifted).await.expect("conversation");
    let pod = create_sandbox_pod(&shifted, conversation.id).await.expect("pod");
    terminate_sandbox_pod(&shifted, pod.id).await.expect("end it");

    let (stops, observed_at) = last_pod_stops(&shifted).await.expect("read");
    let (stopped, _) = stops.get(&conversation.id).copied().expect("its stop");
    let age = observed_at - stopped;
    assert!(age >= chrono::Duration::zero() && age < chrono::Duration::minutes(1), "stopped {stopped}, observed {observed_at}: {age}");

    // SME-521: the grace inputs too, and a claim's UTC creation time put
    // on that same clock.
    let inputs = storage_grace_inputs(&shifted, &[conversation.id]).await.expect("read");
    let stopped = inputs.rows[&conversation.id].terminated_at.expect("its end");
    let age = inputs.observed_at - stopped;
    assert!(age >= chrono::Duration::zero() && age < chrono::Duration::minutes(1), "stopped {stopped}, observed {}: {age}", inputs.observed_at);
    let claim_made_now = inputs.from_utc(chrono::Utc::now().naive_utc());
    let skew = (inputs.observed_at - claim_made_now).num_seconds().abs();
    assert!(skew < 60, "a claim made now reads as now on the database's clock: {claim_made_now} vs {}", inputs.observed_at);
    shifted.close().await;
}
