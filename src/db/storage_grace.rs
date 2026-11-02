use super::*;

// --- Storage grace (SME-345) ---
//
// The `sandbox_pods` queries for the storage-grace pass: the mark a quiet
// stop leaves, the due list it reads, and the re-check it makes under the
// locks. These queries live here, not in `sandbox.rs`, because they all
// speak about terminated rows — the section in `sandbox.rs` speaks about
// live ones. `sandbox_pods` must be aliased `p`.

/// Stamps `pod_id`'s row as ended by a quiet stop, so the storage-grace
/// pass can count its grace from that moment. Only
/// `quiet_stop::stop_locked` calls it: every other end — a user's Stop,
/// the model's `terminate_pod`, a lost pod — leaves NULL, and only the
/// pass clears it again.
pub async fn mark_pod_quietly_stopped(pool: &PgPool, pod_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = now() WHERE id = $1")
        .bind(pod_id)
        .execute(pool)
        .await?;
    Ok(())
}
