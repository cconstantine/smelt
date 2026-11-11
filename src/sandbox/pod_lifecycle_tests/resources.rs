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
