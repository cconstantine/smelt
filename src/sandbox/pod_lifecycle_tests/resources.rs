//! SME-484: a conversation's pod resources reach the pod it starts, and
//! a pod no node has room for says so.

use super::*;

/// The real caller's path, `start_or_get_pod`: the conversation's
/// `pod_resources` become its containers' requests and limits, the pod
/// carries the spread rule, and it schedules. Requests are small, so any
/// cluster the tests run on has room.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversations_pod_resources_reach_its_pod_and_it_schedules(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let id = db::create_conversation(&pool).await.expect("create conversation").id;
        let asks = serde_json::json!({
            "sandbox": {"memory_request": "64Mi", "cpu_request": "50m"},
            "docker": {"memory_request": "128Mi", "memory_limit": "4Gi"},
        });
        db::set_conversation_pod_resources(&pool, id, Some(&asks)).await.expect("set");
        let pod = start_or_get_pod(&pool, id).await.expect("the pod starts");
        let read = pods_api(&client).get(&pod_name(pod)).await.expect("read the pod");
        assert_eq!(read.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Running"));
        let spec = read.spec.expect("spec");
        let quantity = |q: &str| Some(Quantity(q.to_string()));
        let sandbox = spec.containers.iter().find(|c| c.name == "sandbox").and_then(|c| c.resources.clone()).expect("sandbox");
        let requests = sandbox.requests.expect("requests");
        assert_eq!((requests.get("memory").cloned(), requests.get("cpu").cloned()), (quantity("64Mi"), quantity("50m")));
        let docker = spec.init_containers.iter().flatten().find(|c| c.name == "docker").and_then(|c| c.resources.clone()).expect("docker");
        assert_eq!(docker.requests.and_then(|r| r.get("memory").cloned()), quantity("128Mi"));
        assert_eq!(docker.limits.and_then(|l| l.get("memory").cloned()), quantity("4Gi"));
        let spread = spec.topology_spread_constraints.expect("the spread rule");
        assert_eq!(spread.first().map(|s| s.when_unsatisfiable.as_str()), Some("ScheduleAnyway"));
        teardown_conversation(&pool, id, &[]).await;
    })
    .await;
}

/// SME-484 L2: the start's backstop, the real `PodCheckout::restart_pod`,
/// brings the pod back with the conversation's new requests: a new live
/// pod, the old one gone, Running, and its storage kept.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_restart_pod_brings_the_pod_back_with_the_conversations_new_requests(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let id = db::create_conversation(&pool).await.expect("create conversation").id;
        let first = start_or_get_pod(&pool, id).await.expect("the first pod starts");
        let claims = claim_uids(&client, id).await;
        let asks = serde_json::json!({
            "sandbox": {"memory_request": "64Mi"},
            "docker": {"memory_request": "128Mi"},
        });
        db::set_conversation_pod_resources(&pool, id, Some(&asks)).await.expect("set");

        crate::agents::ChildCheckout::restart_pod(&crate::agents::PodCheckout, &pool, id).await.expect("restart_pod");

        let live: Vec<i64> = db::list_sandbox_pods(&pool, id).await.expect("live pods").into_iter().map(|p| p.id).collect();
        assert_eq!(live.len(), 1, "one live pod: {live:?}");
        let second = live[0];
        assert_ne!(second, first, "a new pod");
        assert!(pods_api(&client).get_opt(&pod_name(first)).await.expect("read").map_or(true, |p| p.metadata.deletion_timestamp.is_some()), "the old pod is gone or going");
        let read = pods_api(&client).get(&pod_name(second)).await.expect("read the pod");
        assert_eq!(read.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Running"));
        let quantity = |q: &str| Some(Quantity(q.to_string()));
        assert_eq!(memory_requests(&client, second).await, (quantity("64Mi"), quantity("128Mi")));
        assert_eq!(claim_uids(&client, id).await, claims, "the storage outlives the pod");
        teardown_conversation(&pool, id, &[]).await;
    })
    .await;
}

/// The claims' uids, workspace's first.
async fn claim_uids(client: &kube::Client, id: i64) -> Vec<Option<String>> {
    let mut uids = Vec::new();
    for name in [workspace_pvc_name(id), docker_pvc_name(id)] {
        let claim = pvc_api(client).get_opt(&name).await.expect("read the claim");
        uids.push(claim.and_then(|c| c.metadata.uid));
    }
    uids
}

/// The requests a pod's `sandbox` and `docker` containers carry.
async fn memory_requests(client: &kube::Client, pod: i64) -> (Option<Quantity>, Option<Quantity>) {
    let spec = pods_api(client).get(&pod_name(pod)).await.expect("read the pod").spec.expect("spec");
    let sandbox = spec.containers.iter().find(|c| c.name == "sandbox").and_then(|c| c.resources.clone());
    let docker = spec.init_containers.iter().flatten().find(|c| c.name == "docker").and_then(|c| c.resources.clone());
    let memory = |r: Option<ResourceRequirements>| r.and_then(|r| r.requests).and_then(|r| r.get("memory").cloned());
    (memory(sandbox), memory(docker))
}

/// SME-484 M1: a starting child whose first pod asked for the wrong room
/// has its storage dropped, so its next pod is placed afresh: both
/// claims are new objects, the pod carries the new requests, and the
/// model, which hasn't run, is told nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_dropping_a_starting_conversations_storage_frees_its_claims(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let id = db::create_conversation(&pool).await.expect("create conversation").id;
        let first = start_or_get_pod(&pool, id).await.expect("the first pod starts");
        let before = claim_uids(&client, id).await;
        assert!(before.iter().all(Option::is_some), "both claims exist: {before:?}");
        let asks = serde_json::json!({
            "sandbox": {"memory_request": "64Mi"},
            "docker": {"memory_request": "128Mi"},
        });
        db::set_conversation_pod_resources(&pool, id, Some(&asks)).await.expect("set");

        drop_starting_storage(&pool, id).await.expect("the storage is dropped");
        let second = start_or_get_pod(&pool, id).await.expect("the second pod starts");

        assert_ne!(first, second, "a new pod");
        let after = claim_uids(&client, id).await;
        for (old, new) in before.iter().zip(&after) {
            assert!(new.is_some() && new != old, "a new claim object: {before:?} then {after:?}");
        }
        let quantity = |q: &str| Some(Quantity(q.to_string()));
        assert_eq!(memory_requests(&client, second).await, (quantity("64Mi"), quantity("128Mi")));
        let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("count the messages");
        assert_eq!(saved, 0, "the model is told nothing");
        teardown_conversation(&pool, id, &[]).await;
    })
    .await;
}
