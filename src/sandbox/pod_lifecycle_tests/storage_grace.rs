//! SME-345 against a real cluster: one pass of the storage-grace rules
//! deletes the two claims a stopped conversation kept — the way "remove
//! storage" does — and only once the grace has actually run out. Since
//! SME-521 the pass starts from the claims it lists in the cluster, so a
//! conversation with claims and no pod row is seen too. The clock is faked
//! the way SME-78's cluster tests fake theirs: an `UPDATE` that moves the
//! row's end back. No holder pods: unmounted claims delete outright.

use super::*;

/// The graces in production's defaults: a week, a day for a finished
/// agent, thirty days for the user's Stop.
const SETTINGS: GraceSettings = GraceSettings { default_days: 7, agent_days: 1, user_days: 30 };

/// A conversation with its two claims made for real and its pod row
/// closed by a quiet stop `days` ago.
async fn quiet_stopped_conversation(pool: &PgPool, client: &kube::Client, days: &str) -> i64 {
    let id = conversation_with_claims(pool, client).await;
    let pod = db::create_sandbox_pod(pool, id).await.expect("pod row");
    db::terminate_sandbox_pod_as(pool, pod.id, db::PodEnd::Quiet).await.expect("quiet-stop the row");
    age_end(pool, id, days).await;
    id
}

/// A conversation with its two claims made for real and no pod row.
async fn conversation_with_claims(pool: &PgPool, client: &kube::Client) -> i64 {
    let instance = db::smelt_instance(pool).await.expect("instance");
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    ensure_conversation_pvcs(client, conversation.id, &instance).await.expect("claims");
    conversation.id
}

/// Moves the conversation's pod rows' ends (and quiet marks) `days` back.
async fn age_end(pool: &PgPool, conversation_id: i64, days: &str) {
    sqlx::query(&format!(
        "UPDATE sandbox_pods SET terminated_at = now() - interval '{days}', \
         quietly_stopped_at = now() - interval '{days}' WHERE conversation_id = $1"
    ))
    .bind(conversation_id)
    .execute(pool)
    .await
    .expect("age the row's end");
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

/// One real pass: the real list, the real removal.
async fn real_pass(pool: &PgPool) -> Vec<i64> {
    remove_expired_storage_once_with(
        pool,
        &SETTINGS,
        STORAGE_REMOVAL_TIMEOUT,
        || list_stored_claims(pool),
        |conversation_id| remove_conversation_storage_locked(pool, conversation_id),
    )
    .await
}

/// The plan's cluster test: due, one real pass, both claims gone for
/// real, one saved notice, and the next pass does nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_pass_removes_the_claims_a_quiet_stop_kept(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let id = quiet_stopped_conversation(&pool, &client, "8 days").await;

        let settled = real_pass(&pool).await;
        assert_eq!(settled, vec![id], "the due conversation settled");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(!claim_there(&client, &name).await, "{name} should be gone");
        }
        let notices = removal_notices(&pool, id).await;
        assert_eq!(notices.len(), 1, "one notice: {notices:?}");
        assert!(notices[0].contains("starts fresh with empty volumes"), "{}", notices[0]);
        // SME-357: the saved storage-removal notice carries the mark before its words.
        assert!(notices[0].contains(&format!("{}Sandbox pod", crate::api::chat::NOTICE_MARK)), "{}", notices[0]);

        let settled = real_pass(&pool).await;
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

        let settled = real_pass(&pool).await;
        assert!(settled.is_empty(), "day one of a week's grace is not due");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(claim_there(&client, &name).await, "{name} should stand through the grace");
        }
        assert!(removal_notices(&pool, id).await.is_empty(), "and nothing is said yet");

        age_end(&pool, id, "8 days").await;
        let settled = real_pass(&pool).await;
        assert_eq!(settled, vec![id], "the same pass removes it once the grace has run out");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(!claim_there(&client, &name).await, "{name} should be gone");
        }
        assert_eq!(removal_notices(&pool, id).await.len(), 1, "one notice, for the stop");
    })
    .await;
}

/// SME-521 item 4: real claims with no pod row are listed by the real
/// list, and removed once their own age passes the grace, with no notice
/// (no pod ever ran). A claim's creation time can't be moved back, so the
/// listing is aged by eight days on its way to the pass.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_claims_with_no_pod_row_are_listed_and_removed(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let id = conversation_with_claims(&pool, &client).await;

        let listed = list_stored_claims(&pool).await.expect("list the claims");
        let set = listed.by_conversation.get(&id).expect("the no-row conversation is listed");
        let mut pair = vec![workspace_pvc_name(id), docker_pvc_name(id)];
        pair.sort();
        assert_eq!(set.names, pair, "both of its claims");
        assert!(set.oldest.is_some() && !set.all_deleting, "{set:?}");

        assert!(real_pass(&pool).await.is_empty(), "fresh claims are within the grace");
        let settled = remove_expired_storage_once_with(
            &pool,
            &SETTINGS,
            STORAGE_REMOVAL_TIMEOUT,
            || async {
                let mut claims = list_stored_claims(&pool).await?;
                for set in claims.by_conversation.values_mut() {
                    set.oldest = set.oldest.map(|at| at - chrono::Duration::days(8));
                }
                Ok(claims)
            },
            |conversation_id| remove_conversation_storage_locked(&pool, conversation_id),
        )
        .await;
        assert_eq!(settled, vec![id], "claims eight days old with no pod are due");
        for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
            assert!(!claim_there(&client, &name).await, "{name} should be gone");
        }
        assert!(removal_notices(&pool, id).await.is_empty(), "no pod ever ran: nothing to tell");
    })
    .await;
}
