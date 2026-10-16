use super::*;

/// The conversation's live pod record, once a start has made one. Bounded
/// by the scenario's own time limit.
async fn wait_for_pod_record(pool: &PgPool, conversation_id: i64) -> i64 {
    loop {
        if let Ok(pod_id) = live_pod_id(pool, conversation_id).await {
            return pod_id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether `pod_id`'s pod is in the cluster and not being deleted.
async fn pod_is_up(client: &kube::Client, pod_id: i64) -> bool {
    pods_api(client)
        .get_opt(&pod_name(pod_id))
        .await
        .expect("read the pod")
        .is_some_and(|pod| pod.metadata.deletion_timestamp.is_none())
}

/// B1 (SME-122): opening the conversation while its pod starts (the
/// page load's `try_reconnect`, from `get_sandbox_state`) leaves the start
/// alone. Before the fix the reconnect found no agent, took the starting
/// pod for dead and closed its record, and the start went on to make a pod
/// that nothing would ever delete.
#[sqlx::test]
async fn test_a_page_load_while_the_pod_starts_leaves_the_start_alone(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let start = tokio::spawn({
            let pool = pool.clone();
            async move { create_pod(&pool, conversation.id, PodLimitOverrides::default()).await }
        });
        let pod = wait_for_pod_record(&pool, conversation.id).await;

        try_reconnect(&pool, pod).await;

        let started = start.await.expect("the start task").expect("the start should succeed");
        assert_eq!(started, pod);
        assert_eq!(
            live_pod_id(&pool, conversation.id).await.ok(),
            Some(pod),
            "the pod's record should still be live"
        );
        assert!(pod_is_up(&client, pod).await, "the pod should be running");
    })
    .await;
}

/// B1 (SME-122): a pod whose record is closed while it starts (the
/// sandbox panel's Stop) doesn't outlive its start: the start says it was
/// stopped and deletes the pod it made.
#[sqlx::test]
async fn test_a_pod_stopped_while_it_starts_is_not_left_running(pool: PgPool) {
    stop_while_starting(pool, false).await;
}

/// B1 (SME-122): a Stop once Kubernetes has the starting pod deletes it,
/// and the start, waiting for it to run, says it was stopped rather than
/// failing with Kubernetes' own 404 (as the bug bash saw in the UI).
#[sqlx::test]
async fn test_a_pod_stopped_once_kubernetes_has_it_says_it_was_stopped(pool: PgPool) {
    stop_while_starting(pool, true).await;
}

/// Stops a conversation's pod while it starts: as soon as its record is
/// made, or once Kubernetes has the pod too (`once_made`).
async fn stop_while_starting(pool: PgPool, once_made: bool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let start = tokio::spawn({
            let pool = pool.clone();
            async move { create_pod(&pool, conversation.id, PodLimitOverrides::default()).await }
        });
        let pod = wait_for_pod_record(&pool, conversation.id).await;
        while once_made && pods_api(&client).get_opt(&pod_name(pod)).await.expect("read the pod").is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        stop_pod_for_user(&pool, pod).await.expect("stop_pod_for_user");

        let started = start.await.expect("the start task");
        assert!(
            matches!(&started, Err(SandboxError::StartFailed(message)) if message.contains("stopped")),
            "the start should say the pod was stopped: {started:?}"
        );
        assert!(!pod_is_up(&client, pod).await, "the stopped pod shouldn't be left running");
    })
    .await;
}
