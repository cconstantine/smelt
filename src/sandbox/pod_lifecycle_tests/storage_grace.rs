//! SME-345 against a real cluster: one pass of the storage-grace rules
//! deletes the two claims a quiet stop kept — the way "remove storage"
//! does — and only once the grace has actually run out. The clock is
//! faked the way SME-78's cluster tests fake theirs: an `UPDATE` that
//! moves the mark back. No holder pods: unmounted claims delete outright.

use super::*;

/// A conversation with its two claims made for real and its pod row
/// closed and marked, as a quiet stop aged `days` days would have left
/// it.
async fn quiet_stopped_conversation(pool: &PgPool, client: &kube::Client, days: &str) -> i64 {
    let instance = db::smelt_instance(pool).await.expect("instance");
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    let id = conversation.id;
    ensure_conversation_pvcs(client, id, &instance).await.expect("claims");
    let pod = db::create_sandbox_pod(pool, id).await.expect("pod row");
    sqlx::query(&format!(
        "UPDATE sandbox_pods SET terminated_at = now(), quietly_stopped_at = now() - interval '{days}' WHERE id = $1"
    ))
    .bind(pod.id)
    .execute(pool)
    .await
    .expect("quiet-stop the row");
    id
}

async fn claim_there(client: &kube::Client, name: &str) -> bool {
    pvc_api(client).get_opt(name).await.expect("read a claim").is_some()
}

async fn removal_notices(pool: &PgPool, conversation_id: i64) -> Vec<String> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("list messages")
        .into_iter()
        .filter(|m| m.content.contains("storage was removed"))
        .map(|m| m.content)
        .collect()
}

/// The plan's cluster test: due, one real pass, both claims gone for
/// real, one saved notice, and the next pass does nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_pass_removes_the_claims_a_quiet_stop_kept(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let id = quiet_stopped_conversation(&pool, &client, "8 days").await;

        let settled = remove_expired_storage_once_with(&pool, 7, STORAGE_REMOVAL_TIMEOUT, |conversation_id| {
            remove_conversation_storage_locked(&pool, conversation_id)
        })
        .await;
        assert_eq!(settled, vec![id], "the due conversation settled");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(!claim_there(&client, &name).await, "{name} should be gone");
        }
        let notices = removal_notices(&pool, id).await;
        assert_eq!(notices.len(), 1, "one notice: {notices:?}");
        assert!(notices[0].contains("starts fresh with empty volumes"), "{}", notices[0]);
        // SME-357: the saved storage-removal notice carries the mark before its words.
        assert!(notices[0].contains(&format!("{}Sandbox pod", crate::api::chat::NOTICE_MARK)), "{}", notices[0]);

        let settled = remove_expired_storage_once_with(&pool, 7, STORAGE_REMOVAL_TIMEOUT, |conversation_id| {
            remove_conversation_storage_locked(&pool, conversation_id)
        })
        .await;
        assert!(settled.is_empty(), "a settled stop is never revisited");
        assert_eq!(removal_notices(&pool, id).await.len(), 1, "and never announced twice");
    })
    .await;
}

/// Both directions of the clock gate at the cluster level: a day into a
/// week's grace the claims stand untouched with nothing said, and the
/// same pass after the week removes them.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_storage_woken_within_the_grace_stands_until_due(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let id = quiet_stopped_conversation(&pool, &client, "1 day").await;

        let settled = remove_expired_storage_once_with(&pool, 7, STORAGE_REMOVAL_TIMEOUT, |conversation_id| {
            remove_conversation_storage_locked(&pool, conversation_id)
        })
        .await;
        assert!(settled.is_empty(), "day one of a week's grace is not due");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(claim_there(&client, &name).await, "{name} should stand through the grace");
        }
        assert!(removal_notices(&pool, id).await.is_empty(), "and nothing is said yet");

        sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = now() - interval '8 days' WHERE conversation_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("let the week pass");
        let settled = remove_expired_storage_once_with(&pool, 7, STORAGE_REMOVAL_TIMEOUT, |conversation_id| {
            remove_conversation_storage_locked(&pool, conversation_id)
        })
        .await;
        assert_eq!(settled, vec![id], "the same pass removes it once the grace has run out");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(!claim_there(&client, &name).await, "{name} should be gone");
        }
        assert_eq!(removal_notices(&pool, id).await.len(), 1, "one notice, for the stop");
    })
    .await;
}
