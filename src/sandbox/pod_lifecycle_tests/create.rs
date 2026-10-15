use super::*;

/// SME-121: a pod whose image isn't on the node fails within seconds with
/// the kubelet's `ErrImageNeverPull`, naming the image and what to do,
/// instead of waiting out the running timeout. A hand-built pod
/// with the sandbox's pull policy and an image no one built; its deadline
/// bounds its life if the test is killed before teardown.
#[sqlx::test]
async fn test_a_pod_whose_image_is_missing_fails_fast_saying_how_to_build_it(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create_sandbox_pod").id;
        let mut limits = std::collections::BTreeMap::new();
        limits.insert("cpu".to_string(), Quantity("100m".to_string()));
        limits.insert("memory".to_string(), Quantity("64Mi".to_string()));
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let missing = "docker.io/library/smelt-sandbox:src-sme121neverbuilt";
        let missing_image_pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name(pod)),
                namespace: Some(NAMESPACE.to_string()),
                labels: Some(instance_labels(&instance.id)),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "sandbox".to_string(),
                    image: Some(missing.to_string()),
                    image_pull_policy: Some("Never".to_string()),
                    resources: Some(ResourceRequirements { limits: Some(limits), ..Default::default() }),
                    ..Default::default()
                }],
                restart_policy: Some("Never".to_string()),
                active_deadline_seconds: Some(300),
                ..Default::default()
            }),
            status: None,
        };
        pods_api(&client).create(&PostParams::default(), &missing_image_pod).await.expect("create the pod");

        let started = std::time::Instant::now();
        let result = wait_for_running_with_timeout(&pods_api(&client), &pod_name(pod), Duration::from_secs(60)).await;
        let Err(SandboxError::StartFailed(reason)) = result else {
            panic!("expected a startup failure, got {result:?}");
        };
        assert!(started.elapsed() < Duration::from_secs(45), "took {:?}", started.elapsed());
        assert!(reason.contains("ErrImageNeverPull") && reason.contains(missing), "{reason}");
        // The advice goes by the spec's image; for a container that never
        // started, the status names the same one.
        let status_image = pods_api(&client).get(&pod_name(pod)).await.expect("get the pod").status
            .and_then(|s| s.container_statuses)
            .and_then(|c| c.into_iter().next())
            .map(|c| c.image);
        assert_eq!(status_image.as_deref(), Some(missing));
        // Not an image the build script makes, so the advice says whose it is.
        assert!(reason.contains("SANDBOX_IMAGE or SANDBOX_DOCKER_IMAGE"), "{reason}");
    })
    .await;
}

/// SME-33, SME-32: `create_pod` gives the pod its conversation's own
/// Docker data claim (so its images outlive the pod) and `/workspace`
/// claim, and `teardown_conversation` deletes both with the pod.
#[sqlx::test]
async fn test_create_pod_mounts_the_conversations_docker_and_workspace_claims(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let pvcs = pvc_api(&client);
        let docker_claim = docker_pvc_name(conversation.id);
        assert!(
            pvcs.get_opt(&docker_claim).await.expect("get_opt").is_some(),
            "create_pod should create the conversation's docker data claim {docker_claim}"
        );
        let workspace_claim = workspace_pvc_name(conversation.id);
        assert!(
            pvcs.get_opt(&workspace_claim).await.expect("get_opt").is_some(),
            "create_pod should create the conversation's workspace claim {workspace_claim}"
        );
        let volumes = pods_api(&client)
            .get(&pod_name(pod))
            .await
            .expect("get the pod")
            .spec
            .expect("spec")
            .volumes
            .unwrap_or_default();
        let claim_of = |volume: &str| {
            volumes
                .iter()
                .find(|v| v.name == volume)
                .and_then(|v| v.persistent_volume_claim.as_ref())
                .map(|c| c.claim_name.clone())
        };
        assert_eq!(claim_of(DOCKER_DATA_VOLUME).as_deref(), Some(docker_claim.as_str()), "the pod mounts its conversation's claim");
        assert_eq!(claim_of(WORKSPACE_VOLUME).as_deref(), Some(workspace_claim.as_str()), "the pod's /workspace is its conversation's claim");

        teardown_conversation(&pool, conversation.id, &[]).await;
        // A claim's `pvc-protection` finalizer holds it until the pod is
        // really gone.
        for claim in [&docker_claim, &workspace_claim] {
            let gone = tokio::time::timeout(Duration::from_secs(60), async {
                while pvcs.get_opt(claim).await.ok().flatten().is_some() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await;
            assert!(gone.is_ok(), "teardown_conversation should delete the claim {claim}");
        }
    })
    .await;
}

/// SME-51 B7: a `create_pod` whose caller goes away (a Stop drops the turn
/// mid-call) still finishes: the pod gets its git setup and is announced
/// Running, rather than being left at whatever step it had reached.
#[sqlx::test]
async fn test_create_pod_finishes_when_its_caller_goes_away(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let mut conversation_events = events::subscribe(conversation.id);
        let dropped =
            tokio::time::timeout(Duration::from_secs(2), create_pod(&pool, conversation.id, PodLimitOverrides::default()))
                .await;
        assert!(dropped.is_err(), "the create should still be under way after 2s");
        // As long as smelt itself waits for the pod to run (CI raises it),
        // plus time for the git setup that comes before the announcement.
        let announced = tokio::time::timeout(running_wait_timeout() + Duration::from_secs(30), async {
            loop {
                if let Ok(events::ConversationEvent::SandboxPodUpdate { status, .. }) = conversation_events.recv().await
                    && status == "Running"
                {
                    return;
                }
            }
        })
        .await;
        assert!(announced.is_ok(), "a create_pod whose caller went away never finished");
    })
    .await;
}

/// The `unique_session_id` label the volume test's pod uses, so the sweep
/// of earlier runs' pods can't drift from it.
const VOLUME_MOUNT_SESSION_LABEL: &str = "volume-mount";

/// SME-17 phase 4: `create_volume`/`delete_volume` manage a real claim
/// alongside the `sandbox_volumes` row, and the volume mounted into a real
/// pod is writable and readable.
#[sqlx::test]
async fn test_volumes_are_backed_by_claims_and_mounted_into_pods(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    // Earlier runs' volume-mount pods, found by prefix since each name
    // ends in a nanosecond suffix. A pile of them left `Pending` once
    // starved `local-path`'s `WaitForFirstConsumer` binding, which waits
    // on *a* pod using the claim reaching `Scheduled`. Safe to sweep:
    // nothing else in the suite uses this label.
    let pods = pods_api(&client);
    let volume_mount_pod_prefix = format!("sandbox-test-{VOLUME_MOUNT_SESSION_LABEL}-");
    if let Ok(existing) = pods.list(&ListParams::default()).await {
        for name in existing.items.into_iter().filter_map(|p| p.metadata.name) {
            if name.starts_with(&volume_mount_pod_prefix) {
                pods.delete(&name, &immediate_delete_params()).await.ok();
            }
        }
    }
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let volume_id = create_volume(&pool, "test-volume", "/data/testvol").await.expect("create_volume");
        let pvcs = pvc_api(&client);
        let pvc_name = sandbox_volume_pvc_name(volume_id);
        assert!(pvcs.get_opt(&pvc_name).await.expect("get_opt").is_some(), "create_volume should create the backing claim");

        let volumes = db::list_sandbox_volumes(&pool).await.expect("list_sandbox_volumes");
        // As this database's: its volume's claim is labelled with its
        // instance (SME-115), and a pod of another instance can't mount it.
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let docker = DockerSidecar { memory: "512Mi".to_string(), storage: PodStorage::Ephemeral };
        let sandbox = get()
            .expect("this test's manager")
            .create_with_docker(&super::super::tests::unique_session_id(VOLUME_MOUNT_SESSION_LABEL), "128Mi", &docker, &volumes, &instance)
            .await
            .expect("create a pod with the volume");
        let write = sandbox.exec(&["sh", "-c", "echo hello > /data/testvol/marker.txt"]).await;
        let read = sandbox.exec(&["cat", "/data/testvol/marker.txt"]).await;

        // The pod is deleted here, before anything is asserted, not through
        // the cleanup queue. The claim's `pvc-protection` finalizer holds it
        // until the pod is really gone, not just marked for deletion.
        let pod_name = sandbox.pod_name.clone();
        pods.delete(&pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
        let pod_gone = tokio::time::timeout(Duration::from_secs(30), async {
            while pods.get_opt(&pod_name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;

        assert_eq!(write.expect("exec").exit_code, 0, "writing into the mounted volume should succeed");
        assert_eq!(read.expect("exec").stdout.trim(), "hello", "the mounted volume should be writable and readable");
        assert!(pod_gone.is_ok(), "the volume-mounting pod should disappear, not just be marked for deletion");

        delete_volume(&pool, volume_id).await.expect("delete_volume");
        let pvc_gone = tokio::time::timeout(Duration::from_secs(15), async {
            while pvcs.get_opt(&pvc_name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(pvc_gone.is_ok(), "delete_volume should delete the backing claim");
    })
    .await;
}

/// A pod gone (deleted, evicted, node lost) while smelt still thinks it's
/// live: the next call to it gets `AgentUnreachable`, and the same crash
/// cleanup a failed connect runs closes its terminals and the pod's record,
/// so the conversation has no live pod left.
#[sqlx::test]
async fn test_a_pod_deleted_behind_smelts_back_is_cleaned_up_on_next_use(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");

        pods_api(&client).delete(&pod_name(pod), &immediate_delete_params()).await.expect("delete the pod directly");
        deregister(pod);

        let err = terminate_terminal(&pool, terminal).await;
        assert!(matches!(err, Err(TerminalError::AgentUnreachable)), "expected AgentUnreachable, got {err:?}");
        let live = db::list_sandbox_terminals_for_pod(&pool, pod).await.expect("list_sandbox_terminals_for_pod");
        assert!(
            live.is_empty(),
            "crash cleanup should close the terminal even though the pod was never found, not just on a failed connect"
        );
        let already_gone = terminate_pod(&pool, conversation.id).await;
        assert!(
            matches!(already_gone, Err(TerminalError::NoPod)),
            "a confirmed crash should already have closed the pod's record, got {already_gone:?}"
        );
    })
    .await;
}

/// A healthy pod whose connection smelt lost (a restart, simulated with
/// `forget_connection`; the pod is left alone) shows its terminal
/// connected again once `try_reconnect` runs, without waiting for some
/// other tool call.
#[sqlx::test]
async fn test_try_reconnect_restores_a_lost_connection_to_a_healthy_pod(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        forget_connection(pod);

        let status_of = |listed: Vec<TerminalInfo>| {
            listed.into_iter().find(|t| t.terminal_id == terminal).map(|t| t.status)
        };
        let disconnected = list_terminals(&pool, conversation.id).await.expect("list_terminals");
        assert_eq!(
            status_of(disconnected).as_deref(),
            Some("disconnected"),
            "a forgotten connection should list its terminal as disconnected"
        );

        try_reconnect(&pool, pod).await;

        let reconnected = list_terminals(&pool, conversation.id).await.expect("list_terminals");
        assert_eq!(
            status_of(reconnected).as_deref(),
            Some("connected"),
            "try_reconnect should reconnect to the still-healthy pod"
        );
        // And the connection works: the terminal and pod close through it.
        terminate_terminal(&pool, terminal).await.expect("terminate_terminal over the new connection");
        terminate_pod(&pool, conversation.id).await.expect("terminate_pod after reconnecting");
    })
    .await;
}
