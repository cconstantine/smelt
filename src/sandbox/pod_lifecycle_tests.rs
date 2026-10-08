//! The sandbox's lifecycle against a real cluster, one feature per test
//! (SME-94): pods, terminals, commands, file tools, repos and volumes,
//! each run by the real agent in a real pod.
//!
//! Every test starts with `own_sandbox`, which gives it ids no other test
//! or run shares and a sandbox manager of its own behind `get()`, and runs
//! its scenario through `run_then_tear_down`, which bounds it in time and
//! deletes what its database made in the cluster, pass or fail. So the
//! tests run in parallel, and a failure names the feature that broke.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use super::tests::test_client;
use super::*;

/// Gives the test ids clear of every other test and run (pods, claims and
/// labels are named after them, in a namespace they all share) and a
/// sandbox manager of its own, which `get()` returns on the test's thread.
/// Returns the manager's client.
pub(super) async fn own_sandbox(pool: &PgPool) -> kube::Client {
    db::test_support::start_ids_clear_of_other_runs(pool)
        .await
        .expect("ids clear of other runs");
    let client = test_client().await;
    use_test_manager(client.clone());
    client
}

/// How many of these tests run their scenarios at once. Each starts a pod
/// or more, with a Docker sidecar each; a dozen starting together on one
/// node took longer than a pod start may (90 s), failing these and the
/// other real-cluster tests alike. CI runs four tests at a time anyway.
static SCENARIOS_AT_ONCE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Runs `scenario` for at most `limit`, then deletes everything the test's
/// database made in the cluster, whether it passed, failed or ran out of
/// time, and only then reports how it went. Waits its turn first (see
/// `SCENARIOS_AT_ONCE`), which `limit` doesn't count.
pub(super) async fn run_then_tear_down(
    pool: &PgPool,
    client: &kube::Client,
    limit: Duration,
    scenario: impl Future<Output = ()>,
) {
    let _turn = SCENARIOS_AT_ONCE.acquire().await.expect("the semaphore is never closed");
    // On the heap: a test thread's stack is small, and a scenario's future
    // holds every value it keeps across an await.
    let outcome = AssertUnwindSafe(Box::pin(tokio::time::timeout(limit, scenario)))
        .catch_unwind()
        .await;
    Box::pin(tear_down(pool, client)).await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(_)) => panic!("the scenario didn't finish within {limit:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Deletes every conversation's pods and claims, and every volume's claim,
/// in the test's database. Best-effort: a failure here mustn't hide the
/// scenario's own.
async fn tear_down(pool: &PgPool, client: &kube::Client) {
    let Ok(instance) = db::smelt_instance(pool).await else {
        eprintln!("couldn't read the test database's instance; its cluster objects are left");
        return;
    };
    let conversations: Vec<i64> = sqlx::query_scalar("SELECT id FROM conversations")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let pods: Vec<(i64, i64)> = sqlx::query_as("SELECT conversation_id, id FROM sandbox_pods")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for conversation_id in conversations {
        let pod_ids: Vec<i64> = pods.iter().filter(|(c, _)| *c == conversation_id).map(|(_, p)| *p).collect();
        teardown_conversation_with(client, conversation_id, &pod_ids, &instance).await;
    }
    for volume in db::list_sandbox_volumes(pool).await.unwrap_or_default() {
        delete_volume_claim(client, volume.id, &instance).await.ok();
    }
}

/// Creates and sends a command in one step, waits for it to finish,
/// returns its `command_id`.
pub(super) async fn run_and_wait(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> String {
    db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command)
        .await
        .expect("create_terminal_command");
    send_command(pool, terminal_id, command_id, command)
        .await
        .expect("send_command");
    poll_until_finished(pool, command_id).await;
    command_id.to_string()
}

pub(super) async fn first_stdout_line(pool: &PgPool, command_id: &str) -> String {
    let lines = db::read_terminal_output(pool, command_id, &["stdout"], 0, 1)
        .await
        .expect("read_terminal_output");
    lines.first().map(|l| l.data.clone()).unwrap_or_default()
}

pub(super) async fn poll_until_finished(pool: &PgPool, command_id: &str) -> db::TerminalCommandStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = db::terminal_command_status(pool, command_id)
            .await
            .expect("terminal_command_status")
            .expect("command should exist");
        if status.status != "running" {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "command {command_id} did not finish in time"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Whether a `PodsChanged` arrives on `rx` within a few seconds,
/// skipping any older ones still queued.
pub(super) async fn received_pods_changed(
    rx: &mut tokio::sync::broadcast::Receiver<events::AppEvent>,
) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(events::AppEvent::PodsChanged) => return true,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

pub(super) async fn any_message_contains(pool: &PgPool, conversation_id: i64, needle: &str) -> bool {
    db::list_messages(pool, conversation_id)
        .await
        .expect("list_messages")
        .iter()
        .any(|m| {
            m.blocks().ok().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| matches!(b, crate::anthropic::ContentBlock::Text { text } if text.contains(needle)))
            })
        })
}

/// How long a scenario may take unless it waits on purpose: a pod start or
/// two, under the load of the other tests' pods starting alongside.
const SCENARIO_LIMIT: Duration = Duration::from_secs(180);

/// Whether a message containing `needle` reaches `conversation_id` within
/// `within`.
async fn message_arrives(pool: &PgPool, conversation_id: i64, needle: &str, within: Duration) -> bool {
    tokio::time::timeout(within, async {
        while !any_message_contains(pool, conversation_id, needle).await {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .is_ok()
}

/// SME-12: a pod deleted out from under a live connection is noticed and
/// reported with no further tool call, and its terminal closed. The
/// terminal is idle, so the pod-level notice is all it gets.
#[sqlx::test]
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
#[sqlx::test]
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
#[sqlx::test]
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
        db::create_terminal_command(&pool, conversation.id, terminal, "long", "sleep 300")
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
#[sqlx::test]
async fn test_the_pods_view_shows_a_busy_pods_limits_and_live_usage(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    // metrics-server samples a new pod within a minute or so.
    run_then_tear_down(&pool, &client, Duration::from_secs(240), async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        db::create_terminal_command(&pool, conversation.id, terminal, "long", "sleep 300")
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
#[sqlx::test]
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
        db::create_terminal_command(&pool, gone_before.id, terminal.id, "stale", "sleep 300")
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
#[sqlx::test]
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
        let announced = tokio::time::timeout(Duration::from_secs(90), async {
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
            .create_with_docker(&super::tests::unique_session_id(VOLUME_MOUNT_SESSION_LABEL), "128Mi", &docker, &volumes, &instance)
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
    })
    .await;
}

/// Makes a repo in pod `pod` whose `AGENTS.md` says `Run make test.`, as
/// a bare origin at `origin`, and returns its commit. The commit names its
/// own author: the test stores no git identity.
async fn make_origin(client: &kube::Client, pod: i64, origin: &str) -> String {
    let script = format!(
        "set -e; git init -q -b main /tmp/src; cd /tmp/src; echo 'Run make test.' > AGENTS.md; \
         git add AGENTS.md; git -c user.name=t -c user.email=t@example.com commit -qm one; git clone -q --bare /tmp/src {origin}; git rev-parse HEAD"
    );
    let made = exec_with(client, &pod_name(pod), "sandbox", &["sh", "-c", &script], None)
        .await
        .expect("exec make origin");
    assert_eq!(made.exit_code, 0, "make origin: {}{}", made.stdout, made.stderr);
    made.stdout.trim().to_string()
}

/// Runs `script` in pod `pod`'s sandbox container, and asserts it worked.
async fn sh(client: &kube::Client, pod: i64, script: &str) {
    let ran = exec_with(client, &pod_name(pod), "sandbox", &["sh", "-c", script], None)
        .await
        .expect("exec");
    assert_eq!(ran.exit_code, 0, "{script}: {}", ran.stderr);
}

/// SME-32: `/workspace` is the conversation's own. The next pod has the
/// checkouts, their uncommitted work, the repo list and the instructions
/// loaded from them.
#[sqlx::test]
async fn test_a_conversations_workspace_outlives_its_pod(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        make_origin(&client, pod, "/tmp/origin.git").await;
        for dir in [None, Some("again")] {
            crate::git::clone_repo(&pool, conversation.id, "file:///tmp/origin.git", None, dir)
                .await
                .expect("clone_repo");
        }
        // Loaded, then edited in the checkout, alongside a file not
        // committed yet.
        crate::git::load_instructions(&pool, conversation.id, "/workspace/origin/AGENTS.md")
            .await
            .expect("load_instructions");
        let repos = crate::git::list_repos(&pool, conversation.id).await.expect("list repos");
        let shown = &repos[0].trust_requests[0];
        crate::git::decide_trust(&pool, conversation.id, shown.id, &shown.hash, true).await.expect("trust");
        sh(
            &client,
            pod,
            "echo 'not pushed yet' > /workspace/origin/uncommitted.txt && echo 'Run make lint too.' >> /workspace/origin/AGENTS.md",
        )
        .await;
        terminate_pod(&pool, conversation.id).await.expect("terminate the first pod");

        let next = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("a second pod");
        let kept = exec_with(
            &client,
            &pod_name(next),
            "sandbox",
            &["sh", "-c", "cat /workspace/origin/uncommitted.txt && git -C /workspace/origin status --porcelain"],
            None,
        )
        .await
        .expect("exec check workspace");
        assert_eq!(
            (kept.exit_code, kept.stdout.as_str()),
            (0, "not pushed yet\n M AGENTS.md\n?? uncommitted.txt\n"),
            "the checkout and its uncommitted file survive the pod: {}",
            kept.stderr
        );
        let repos = crate::git::list_repos(&pool, conversation.id).await.expect("list repos");
        assert_eq!(repos.len(), 2, "origin, again: {repos:?}");
        assert_eq!(repos[0].status, crate::git::RepoStatus::Ready, "{repos:?}");
        assert_eq!(repos[0].loaded_instructions, vec!["AGENTS.md".to_string()], "{repos:?}");
        assert_eq!(repos[1].status, crate::git::RepoStatus::Ready, "{repos:?}");
        terminate_pod(&pool, conversation.id).await.expect("terminate the second pod");
        let pods_after = list_pods(&pool, conversation.id).await.expect("list_pods");
        assert!(pods_after.is_empty(), "no pods should be listed after terminating it, got {pods_after:?}");
    })
    .await;
}

/// SME-32, SME-49: "work on a repo" and the model's `clone_repo`, on a
/// conversation with no sandbox, each start one. The repo is recorded
/// first, so a turn started while the sandbox starts waits for the clone.
#[sqlx::test]
async fn test_attach_repo_and_clone_repo_start_a_sandbox_when_there_is_none(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    // Three pod starts, one after another.
    run_then_tear_down(&pool, &client, Duration::from_secs(300), async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        // An origin in /workspace, which outlives this pod, for the model's
        // clone below.
        let setup = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        make_origin(&client, setup, "/workspace/seed.git").await;
        terminate_pod(&pool, conversation.id).await.expect("terminate the setup pod");

        let attaching = tokio::spawn({
            let pool = pool.clone();
            let id = conversation.id;
            async move { crate::git::attach_repo(&pool, id, "file:///tmp/missing.git", None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !crate::git::wait_for_clones(&pool, conversation.id, Duration::ZERO).await,
            "a turn started while the sandbox starts should see the clone coming"
        );
        let attached = attaching.await.expect("attach task").expect_err("this origin doesn't exist");
        assert!(attached.contains("does not appear to be a git repository"), "{attached}");
        // The user named it, so it counts as trusted.
        assert_eq!(db::get_repo_trust(&pool, "file/tmp/missing").await.expect("trust"), Some(true));
        assert_eq!(list_pods(&pool, conversation.id).await.expect("list_pods").len(), 1, "attach_repo started a sandbox");
        terminate_pod(&pool, conversation.id).await.expect("terminate the attach pod");

        let cloning = tokio::spawn({
            let pool = pool.clone();
            let id = conversation.id;
            async move { crate::git::clone_repo(&pool, id, "file:///workspace/seed.git", None, None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !crate::git::wait_for_clones(&pool, conversation.id, Duration::ZERO).await,
            "a turn started while the sandbox starts should see the clone coming"
        );
        let seed = cloning.await.expect("clone task").expect("clone_repo starts the sandbox and clones");
        assert_eq!(seed.path, "/workspace/seed");
        assert_eq!(seed.status, crate::git::RepoStatus::Ready, "{seed:?}");
        assert_eq!(list_pods(&pool, conversation.id).await.expect("list_pods").len(), 1, "clone_repo started a sandbox");
        terminate_pod(&pool, conversation.id).await.expect("terminate the clone pod");
        assert!(
            list_terminals(&pool, conversation.id).await.expect("list_terminals").is_empty(),
            "no terminals should be listed after terminating all of them"
        );
    })
    .await;
}
