use super::*;

/// SME-12: a pod deleted out from under a live connection is noticed and
/// reported with no further tool call, and its terminal closed. The
/// terminal is idle, so the pod-level notice is all it gets.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_crashed_pod_is_reported_without_a_tool_call(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");

        pods_api(&client).delete(&pod_name(pod), &immediate_delete_params()).await.expect("delete the pod directly");

        // Waits for the pod-level message itself, not `list_terminals`'
        // "disconnected" status: that flips (via `deregister_if_current`)
        // before `handle_crash_cleanup` finishes the rest of its work
        // (closing the terminal, saving this message).
        assert!(
            message_arrives(&pool, conversation.id, "stopped unexpectedly", Duration::from_secs(30)).await,
            "a crash with no reason to report should get the plain pod-level notice, with no further tool call"
        );
        // `list_terminals` lists only live terminals: a closed one is absent.
        let terminals = list_terminals(&pool, conversation.id).await.expect("list_terminals");
        assert!(
            terminals.iter().all(|t| t.terminal_id != terminal),
            "the same crash cleanup should close the terminal"
        );
    })
    .await;
}

/// A deliberate `terminate_pod` is never reported as a crash: the
/// `Arc::ptr_eq` check in `deregister_if_current` keeps the reader task,
/// seeing the same connection drop, from misreporting it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deliberate_terminate_is_never_reported_as_a_crash(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        terminate_pod(&pool, conversation.id).await.expect("terminate_pod");
        tokio::time::sleep(Duration::from_secs(2)).await; // a wrongly firing reader task's chance to misfire
        assert!(
            !any_message_contains(&pool, conversation.id, "stopped unexpectedly").await,
            "a deliberate terminate_pod must never produce a crash notice"
        );
    })
    .await;
}

/// SME-26: the user stops a pod with an open terminal and a running
/// command. Everything is torn down, the command is marked lost, and the
/// model is told the user stopped it, not that it crashed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_stopping_a_pod_for_the_user_closes_everything_and_says_so(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        // `PodsChanged` names no pod, and the other tests' pods starting
        // and stopping alongside publish it too, so these two checks can
        // pass on another test's event (SME-94). The pods page's refresh
        // is also covered by the browser tier.
        let mut app_events = events::subscribe_app();
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        assert!(received_pods_changed(&mut app_events).await, "creating a pod should tell app-wide listeners");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        db::create_terminal_command(&pool, conversation.id, terminal, "long", "sleep 300", 300)
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal, "long", "sleep 300").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await;

        stop_pod_for_user(&pool, pod).await.expect("stop_pod_for_user");
        assert!(received_pods_changed(&mut app_events).await, "stopping a pod should tell app-wide listeners");

        let command = db::get_terminal_command(&pool, "long").await.expect("get").expect("the command");
        assert_eq!(command.status, "lost", "a running command should be marked lost");
        assert!(
            list_terminals(&pool, conversation.id).await.expect("list_terminals").is_empty(),
            "the pod's terminal should be closed"
        );
        assert!(
            db::list_sandbox_pods(&pool, conversation.id).await.expect("list pods").is_empty(),
            "the pod should be marked terminated"
        );
        assert!(
            pods_api(&client)
                .get_opt(&pod_name(pod))
                .await
                .expect("get pod")
                .is_none_or(|p| p.metadata.deletion_timestamp.is_some()),
            "the Kubernetes pod should be gone or going"
        );
        assert!(
            message_arrives(&pool, conversation.id, "The user stopped sandbox pod", Duration::from_secs(10)).await,
            "the model should be told the user stopped the pod"
        );
        assert!(
            !any_message_contains(&pool, conversation.id, "stopped unexpectedly").await,
            "a user stop must not be reported as a crash"
        );
    })
    .await;
}

/// SME-26, SME-33, SME-77: the pods view shows a busy pod's conversation,
/// status, limits (the sandbox's and the Docker sidecar's memory added up,
/// no CPU limit), terminals and activity, and its live usage once
/// metrics-server has sampled it, which smelt's RBAC lets it read.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_pods_view_shows_a_busy_pods_limits_and_live_usage(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    // metrics-server samples a new pod within a minute or so.
    run_then_tear_down(&pool, &client, Duration::from_secs(240), async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        db::create_terminal_command(&pool, conversation.id, terminal, "long", "sleep 300", 300)
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal, "long", "sleep 300").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await;

        let overviews = crate::api::pods::pod_overviews(&pool).await.expect("pod_overviews");
        let overview = overviews.iter().find(|o| o.pod_id == pod).expect("the pods view should list the pod");
        assert_eq!(overview.conversation_id, conversation.id);
        assert_eq!(overview.status.as_deref(), Some("Running"));
        assert_eq!(
            overview.memory_limit,
            crate::api::pods::sum_memory_limits(&[default_memory_limit(), default_docker_memory_limit()])
        );
        assert_eq!(overview.cpu_limit, None, "sandbox pods have no CPU limit (SME-77)");
        assert_eq!(overview.terminals, 1);
        assert_eq!(overview.activity, crate::api::pods::PodActivity::Busy, "a command is running");
        let usage = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let overviews = crate::api::pods::pod_overviews(&pool).await.expect("pod_overviews");
                if let Some(usage) = overviews.iter().find(|o| o.pod_id == pod).and_then(|o| o.usage.clone()) {
                    return usage;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        })
        .await
        .expect("the pods view should show the pod's live usage once metrics-server has sampled it");
        assert!(usage.memory_bytes > 0, "a running pod uses some memory: {usage:?}");
    })
    .await;
}

/// SME-26: `watch_pods` keeps records in step with the cluster. A record
/// whose pod vanished before the watch started is closed by its first
/// listing, a pod deleted while it runs is closed after the grace period,
/// and a young record whose pod may still be starting is left alone. All
/// quietly.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_watch_pods_closes_records_of_pods_gone_from_the_cluster(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let updated_at = |id: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, chrono::NaiveDateTime>("SELECT updated_at FROM conversations WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("the conversation's updated_at")
            }
        };
        let wait_until_closed = |pod_id: i64| {
            let pool = pool.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(30), async {
                    while db::sandbox_pod_is_live(&pool, pod_id).await.expect("is live") {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                })
                .await
                .is_ok()
            }
        };
        let wait_until_gone_from_kubernetes = |pod_id: i64| {
            let client = client.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(60), async {
                    while pods_api(&client).get_opt(&pod_name(pod_id)).await.ok().flatten().is_some() {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                })
                .await
                .is_ok()
            }
        };

        // Before the watch starts: this pod is gone from Kubernetes, and
        // old enough that its record should have a pod.
        let gone_before = db::create_conversation(&pool).await.expect("create conversation");
        let pod_before = create_pod(&pool, gone_before.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = db::create_sandbox_terminal(&pool, pod_before).await.expect("a terminal record");
        db::create_terminal_command(&pool, gone_before.id, terminal.id, "stale", "sleep 300", 300)
            .await
            .expect("a running command record");
        sqlx::query("UPDATE sandbox_pods SET created_at = now() - interval '10 minutes' WHERE id = $1")
            .bind(pod_before)
            .execute(&pool)
            .await
            .expect("age the pod past the reconciliation cut-off");
        pods_api(&client).delete(&pod_name(pod_before), &immediate_delete_params()).await.expect("delete the pod directly");
        assert!(wait_until_gone_from_kubernetes(pod_before).await, "the pod should leave Kubernetes");
        let conversation_young = db::create_conversation(&pool).await.expect("create conversation");
        let young = db::create_sandbox_pod(&pool, conversation_young.id).await.expect("a young pod record");
        let updated_before = updated_at(gone_before.id).await;

        let watch = tokio::spawn(watch_pods(pool.clone()));

        assert!(wait_until_closed(pod_before).await, "the watch's first listing should close the record");
        let command = db::get_terminal_command(&pool, "stale").await.expect("get").expect("the command");
        assert_eq!(command.status, "lost", "its running command should be marked lost");
        assert!(
            db::list_sandbox_terminals_for_pod(&pool, pod_before).await.expect("terminals").is_empty(),
            "its terminal should be closed"
        );
        assert!(
            db::list_messages(&pool, gone_before.id).await.expect("messages").is_empty(),
            "records are closed quietly, with no notice"
        );
        assert_eq!(updated_at(gone_before.id).await, updated_before, "the conversation shouldn't move in the sidebar");

        // While the watch runs: this pod is deleted outside smelt, with no
        // connection open, so crash detection never sees it.
        let gone_during = db::create_conversation(&pool).await.expect("create conversation");
        let pod_during = create_pod(&pool, gone_during.id, PodLimitOverrides::default()).await.expect("create_pod");
        pods_api(&client).delete(&pod_name(pod_during), &immediate_delete_params()).await.expect("delete the pod directly");
        assert!(wait_until_closed(pod_during).await, "a pod deleted while the watch runs should have its record closed");
        assert!(
            db::list_messages(&pool, gone_during.id).await.expect("messages").is_empty(),
            "closed quietly, with no notice"
        );

        assert!(db::sandbox_pod_is_live(&pool, young.id).await.expect("is live"), "a young record is left alone");
        watch.abort();
    })
    .await;
}

/// Exhausting reconnect attempts without Kubernetes ever confirming the
/// pod's death still cleans up and deletes the pod, so `create_pod` works
/// again at once instead of waiting behind a stale live record. A
/// hand-built pod with no agent in it (a `create_pod` pod's agent runs as
/// soon as it's `Running`, as the image's `ENTRYPOINT`, SME-17) makes every
/// connect fail while the pod stays `Running`: the "reports Running but
/// unreachable" case this path exists for.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_exhausted_reconnects_force_terminate_an_unreachable_pod(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create_sandbox_pod").id;
        // Explicit and small, on purpose (see
        // `sandbox_image_import::loader_pod_spec`'s comment): unspecified,
        // they'd default to the namespace `LimitRange`'s `max` (64Gi, 16
        // cores), which no CI runner can schedule.
        let mut no_agent_limits = std::collections::BTreeMap::new();
        no_agent_limits.insert("cpu".to_string(), Quantity("250m".to_string()));
        no_agent_limits.insert("memory".to_string(), Quantity("128Mi".to_string()));
        // This database's (SME-115): only such a pod is ever deleted.
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let no_agent_pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name(pod)),
                namespace: Some(NAMESPACE.to_string()),
                labels: Some(instance_labels(&instance.id)),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "sandbox".to_string(),
                    image: Some("busybox:1.36".to_string()),
                    command: Some(vec!["sleep".to_string(), "300".to_string()]),
                    resources: Some(ResourceRequirements { limits: Some(no_agent_limits), ..Default::default() }),
                    ..Default::default()
                }],
                restart_policy: Some("Never".to_string()),
                ..Default::default()
            }),
            status: None,
        };
        pods_api(&client).create(&PostParams::default(), &no_agent_pod).await.expect("create the agentless pod");
        wait_for_running(&pods_api(&client), &pod_name(pod)).await.expect("the agentless pod should reach Running");

        let gave_up = connect_with_retry(pool.clone(), pod, ConnectMode::Reconnect).await;
        assert!(
            matches!(gave_up, Err(TerminalError::AgentUnreachable)),
            "should give up as AgentUnreachable once reconnect attempts are exhausted, got {:?}",
            gave_up.is_ok()
        );
        // Deleted, though this agentless pod's `sleep` may take its grace
        // period to stop (SME-51 B6).
        let pod_now = pods_api(&client).get_opt(&pod_name(pod)).await.expect("get_opt");
        assert!(
            pod_now.is_none_or(|p| p.metadata.deletion_timestamp.is_some()),
            "exhausting reconnect attempts should delete the k8s pod"
        );
        let recreated = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await;
        assert!(
            recreated.is_ok(),
            "create_pod should succeed again at once, not stay blocked behind a stale live record, got {recreated:?}"
        );
    })
    .await;
}
