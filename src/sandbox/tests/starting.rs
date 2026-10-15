use super::*;

/// A new pod must not mount a conversation's Docker claim while the old
/// one is still stopping: two dockerds on one /var/lib/docker corrupt
/// it. The old pod gets a grace period so it lingers while terminating.
#[tokio::test]
async fn test_wait_for_conversation_pods_gone_waits_out_a_stopping_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let docker = DockerSidecar {
        memory: "256Mi".to_string(),
        storage: PodStorage::Conversation(conversation_id),
    };
    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("ensure docker claim");
    let sandbox = manager
        .create_with_docker(&unique_session_id("stopping"), "128Mi", &docker, &[], &test_instance())
        .await
        .expect("create pod");
    let name = sandbox.pod_name.clone();
    std::mem::forget(sandbox);

    // The same delete production makes (SME-51 B6): a forced one (grace
    // 0) removes the pod object at once while dockerd is still
    // stopping, and the wait below would see nothing to wait for.
    pods.delete(&name, &pod_delete_params())
        .await
        .expect("start deleting the pod");
    let listed_while_stopping = pods.get_opt(&name).await.expect("get pod").is_some();
    let waited =
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(60), TEST_INSTANCE).await;
    let still_there = pods.get_opt(&name).await.expect("get pod").is_some();

    pods.delete(&name, &immediate_delete_params()).await.ok();
    while pods.get_opt(&name).await.ok().flatten().is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    pvc_api(&client).delete(&docker_pvc_name(conversation_id), &DeleteParams::default()).await.ok();

    assert!(listed_while_stopping, "the delete removed {name} before its containers stopped");
    assert!(waited.is_ok(), "the wait should finish once the pod is gone: {waited:?}");
    assert!(!still_there, "the wait returned while {name} was still stopping");
}

/// The pod `test_a_stopping_pod_from_another_run_doesnt_hold_up_the_tier`
/// holds in Terminating. A fixed name, so a stopped run's leftover is easy
/// to find, and the next run clears it first.
const HELD_POD: &str = "sme-99-held-stopping-pod";

/// How long the held pod stays in Terminating once deleted: `sleep` as
/// PID 1 ignores SIGTERM, so the kubelet waits out the whole grace period
/// before killing it. Longer than the test's 10 s wait, so the wait meets
/// a stopping pod; short, so a run killed mid-test (no cleanup) leaves a
/// pod that goes by itself within this. A finalizer held it at first, and
/// a killed run would have left it Terminating for good, failing every
/// unit test that uses conversation 1 (SME-99's code review).
const HELD_POD_GRACE_SECS: i64 = 30;

/// The held pod's conversation label until it's already deleting: no
/// conversation's, so nothing waits on the pod while it starts.
const HELD_POD_PLACEHOLDER_LABEL: &str = "sme-99-not-yet";

/// A pod that stays in Terminating for `HELD_POD_GRACE_SECS` once deleted.
/// It's created under `HELD_POD_PLACEHOLDER_LABEL` and only relabelled as
/// conversation 1's once its delete has started, so a run killed at any
/// point leaves nothing that blocks conversation 1 for longer than the
/// grace period: before the delete the pod is no conversation's, and
/// after it the kubelet ends it within the grace (SME-99's second code
/// review: a run killed while waiting for it to start had left a running
/// pod labelled as conversation 1's for good).
fn held_pod_spec() -> Result<Pod, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": HELD_POD,
            "labels": { CONVERSATION_LABEL: HELD_POD_PLACEHOLDER_LABEL },
        },
        "spec": {
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": HELD_POD_GRACE_SECS,
            "containers": [{
                "name": "held",
                "image": default_sandbox_image(),
                "imagePullPolicy": "Never",
                "command": ["sleep", "3600"],
                "resources": {
                    "requests": { "cpu": "10m", "memory": "16Mi" },
                    "limits": { "cpu": "100m", "memory": "64Mi" },
                },
            }],
        },
    }))
}

/// Removes the held pod at once (grace 0) and waits, up to a minute, for
/// it to be gone. Errors are only logged: this runs on every exit path,
/// and before a run, for a previous run's leftover.
async fn release_held_pod(pods: &Api<Pod>) {
    let _ = pods.delete(HELD_POD, &immediate_delete_params()).await;
    for _ in 0..120 {
        match pods.get_opt(HELD_POD).await {
            Ok(None) => return,
            Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                eprintln!("couldn't check {HELD_POD} is gone: {e}");
                return;
            }
        }
    }
    eprintln!("{HELD_POD} is still there after a minute");
}

/// Waits up to a minute for the held pod to run: a pod deleted before its
/// container starts goes at once, with nothing to wait out.
async fn wait_held_pod_running(pods: &Api<Pod>) -> Result<(), String> {
    for _ in 0..120 {
        let pod = pods.get_opt(HELD_POD).await.map_err(|e| format!("read the held pod: {e}"))?;
        let phase = pod.and_then(|p| p.status).and_then(|s| s.phase);
        if phase.as_deref() == Some("Running") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("the held pod didn't start running within a minute".to_string())
}

/// SME-99: the browser tier's scratch database counted conversations from
/// 1, like every real-cluster test's, so its first sandbox waited out any
/// pod for conversation 1 another run was still stopping, and failed after
/// the wait's limit. Holds such a pod in Terminating on purpose, then
/// takes the tier's first conversation the way the tier does: its pod
/// must not wait on the held one. Its own client, never the process-global
/// manager (see `test_open_pod_port_reaches_the_conversations_own_pod`).
/// `#[ignore]`d so it runs with the browser tier and CI's ignored tests,
/// not alongside the unit tests that use conversation 1 for real.
#[sqlx::test]
#[ignore]
async fn test_a_stopping_pod_from_another_run_doesnt_hold_up_the_tier(pool: PgPool) {
    let client = test_client().await;
    let pods = pods_api(&client);
    release_held_pod(&pods).await;
    let spec = held_pod_spec().expect("the held pod's spec");
    pods.create(&PostParams::default(), &spec).await.expect("create the held pod");

    let outcome: Result<i64, String> = async {
        wait_held_pod_running(&pods).await?;
        pods.delete(HELD_POD, &DeleteParams::default())
            .await
            .map_err(|e| format!("start deleting the held pod: {e}"))?;
        let relabel = serde_json::json!({ "metadata": { "labels": { CONVERSATION_LABEL: "1" } } });
        pods.patch(HELD_POD, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&relabel))
            .await
            .map_err(|e| format!("label the stopping pod as conversation 1's: {e}"))?;
        let held = pods.get_opt(HELD_POD).await.map_err(|e| format!("read the held pod: {e}"))?;
        let stopping_as_1 = held.is_some_and(|pod| {
            pod.metadata.deletion_timestamp.is_some()
                && pod.metadata.labels.and_then(|l| l.get(CONVERSATION_LABEL).cloned()).as_deref() == Some("1")
        });
        if !stopping_as_1 {
            return Err("the held pod isn't held in Terminating as conversation 1's".to_string());
        }
        db::test_support::start_ids_clear_of_other_runs(&pool)
            .await
            .map_err(|e| format!("move the id sequences: {e}"))?;
        let conversation = db::create_conversation(&pool).await.map_err(|e| format!("conversation: {e}"))?;
        let instance = db::smelt_instance(&pool).await.map_err(|e| format!("instance: {e}"))?.id;
        wait_for_conversation_pods_gone(&client, conversation.id, Duration::from_secs(10), &instance)
            .await
            .map_err(|e| format!("conversation {}'s pod would wait: {e:?}", conversation.id))?;
        Ok(conversation.id)
    }
    .await;
    release_held_pod(&pods).await;

    let conversation = outcome.expect("the tier's first conversation shouldn't meet another run's stopping pod");
    assert_ne!(conversation, 1);
}

/// SME-51 B8: the listening probe (`sandbox_preview_url`) waited as
/// long as the Kubernetes client would for a port-forward to open,
/// minutes when the API server hangs, holding the turn.
#[sqlx::test]
async fn test_the_listening_probe_gives_up_on_a_hanging_cluster(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    // An "API server" that accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = kube::Config::new(format!("http://{addr}").parse().expect("url"));
    let client = kube::Client::try_from(config).expect("client");
    let probed = tokio::time::timeout(
        LISTEN_OPEN_TIMEOUT + Duration::from_secs(5),
        pod_port_is_listening_with(&pool, conversation.id, PodHost::Localhost, 8000, || Ok(client)),
    )
    .await;
    assert!(matches!(probed, Ok(Ok(false))), "the probe should give up and say nothing's listening: {probed:?}");
}

/// SME-62 B15: a pod start mustn't give up before the Docker sidecar's
/// own startup probe would (60 checks a second apart), with time left
/// for a first pod's claims to be provisioned.
#[test]
fn test_a_pod_start_waits_longer_than_the_docker_probe() {
    let probe = DOCKER_STARTUP_PROBE_CHECKS * DOCKER_STARTUP_PROBE_PERIOD_SECS;
    assert!(DEFAULT_RUNNING_WAIT_TIMEOUT_SECS >= probe as u64 + 30, "{DEFAULT_RUNNING_WAIT_TIMEOUT_SECS}s vs a {probe}s probe");
}

/// SME-51 code review 1: a pod start that fails after its conversation
/// was deleted may have re-created the conversation's claims after the
/// delete's teardown ran; they're removed. A live conversation's stay.
#[sqlx::test]
async fn test_a_failed_start_for_a_deleted_conversation_leaves_no_claims(pool: PgPool) {
    let client = test_client().await;
    let pvcs = pvc_api(&client);
    let deleted = db::create_conversation(&pool).await.expect("conversation");
    let live = db::create_conversation(&pool).await.expect("conversation");
    // Ids from a fresh test database are tiny; offset them away from
    // other runs' claims in the shared test namespace.
    let offset = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64 + 2_000_000_000;
    db::delete_conversation(&pool, deleted.id).await.expect("delete");
    let instance = db::smelt_instance(&pool).await.expect("instance");
    for id in [deleted.id, live.id] {
        ensure_conversation_pvcs(&client, id + offset, &instance).await.expect("claims");
    }

    clean_up_after_failed_start(&pool, &client, deleted.id, deleted.id + offset).await;
    clean_up_after_failed_start(&pool, &client, live.id, live.id + offset).await;

    let gone = pvcs.get_opt(&docker_pvc_name(deleted.id + offset)).await.expect("get").is_none_or(|p| p.metadata.deletion_timestamp.is_some());
    let kept = pvcs.get_opt(&docker_pvc_name(live.id + offset)).await.expect("get").is_some();
    delete_conversation_pvcs(&client, live.id + offset, &instance.id).await;
    delete_conversation_pvcs(&client, deleted.id + offset, &instance.id).await;
    assert!(gone, "the deleted conversation's re-created claim was left behind");
    assert!(kept, "a live conversation's claims must stay");
}

/// SME-51 B7: "Work on a repo" while the sandbox is still starting
/// waits for it, rather than taking the half-made pod's record and
/// cloning into a pod that isn't running.
#[sqlx::test]
async fn test_start_or_get_pod_waits_for_a_pod_still_starting(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // A pod being made: its record exists, and its start holds the lock.
    let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let starting = pod_start_lock(conversation.id);
    let guard = starting.lock().await;
    let early = tokio::time::timeout(Duration::from_millis(300), start_or_get_pod(&pool, conversation.id)).await;
    assert!(early.is_err(), "it took the pod before it had started");
    drop(guard);
    let pod = start_or_get_pod(&pool, conversation.id).await.expect("the started pod");
    assert_eq!(pod, row.id);
}
