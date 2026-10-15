use super::*;

/// The manager before `init()`: an error saying so, not a panic (SME-56).
/// Read from a cell of the test's own, so it holds whatever else runs.
#[test]
fn test_the_manager_before_init_is_not_initialized() {
    let cell = OnceLock::new();
    let result = manager_in(&cell);
    assert!(matches!(result, Err(SandboxError::NotInitialized)), "expected NotInitialized");
}

/// A client for a cluster that isn't there: nothing is sent until a
/// request is made, so it builds a manager without a cluster.
fn unreachable_client() -> kube::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = "http://127.0.0.1:9".parse().expect("a url");
    kube::Client::try_from(kube::Config::new(url)).expect("a client")
}

/// SME-94: a test's own manager is what `get()` returns on the test's
/// thread, where `#[sqlx::test]`'s current-thread runtime runs the test
/// and every task it spawns, and nowhere else.
#[tokio::test]
async fn test_a_test_manager_is_what_get_returns_on_its_own_thread_only() {
    let manager = use_test_manager(unreachable_client());
    let here = get().map(|got| std::ptr::eq(got, manager));
    let elsewhere = std::thread::spawn(|| matches!(get(), Err(SandboxError::NotInitialized)))
        .join()
        .expect("the other thread");
    assert!(matches!(here, Ok(true)), "get() on the test's thread isn't the test's manager");
    assert!(elsewhere, "another thread sees the test's manager");
}

/// A conversation's Docker claim is created once and reused, and
/// deleting it removes it. No pod: `local-path` binds on first use, so
/// an unused claim stays `Pending`, which is fine here.
#[tokio::test]
async fn test_ensure_docker_pvc_creates_once_and_delete_removes_it() {
    let client = test_client().await;
    let pvcs = pvc_api(&client);
    // Far above any id a fresh test database hands out, and unique per run.
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let name = docker_pvc_name(conversation_id);

    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("first ensure should create");
    let first = pvcs.get_opt(&name).await.expect("get claim");
    let first_uid = first.and_then(|p| p.metadata.uid);
    assert!(first_uid.is_some(), "ensure_docker_pvc should create {name}");

    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("second ensure should reuse");
    let second_uid = pvcs.get_opt(&name).await.expect("get claim").and_then(|p| p.metadata.uid);
    assert_eq!(first_uid, second_uid, "a second ensure must reuse the claim, not replace it");

    delete_conversation_pvcs(&client, conversation_id, TEST_INSTANCE).await;
    let gone = tokio::time::timeout(Duration::from_secs(30), async {
        while pvcs.get_opt(&name).await.expect("get claim").is_some() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    // Delete directly too, so a failed assertion never leaves it behind.
    pvcs.delete(&name, &DeleteParams::default()).await.ok();
    assert!(gone.is_ok(), "delete_docker_pvc should remove {name}");
}

#[tokio::test]
async fn test_create_reaches_running_from_clean_slate() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("create");

    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let pods = pods_api(&client);
    let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
    assert_eq!(pod.status.and_then(|s| s.phase).as_deref(), Some("Running"));

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// A pod that never reaches `Running` (real cause seen in this
/// environment: `local-path`'s `WaitForFirstConsumer` provisioning
/// stuck behind an unrelated pileup — see the precheck below) must not
/// be left behind: nothing else in the system ever cleans up a pod
/// `create` gave up waiting on, since `Sandbox::drop`'s cleanup queue
/// only fires for a `Sandbox` that was actually returned. An orphaned
/// pod here isn't just wasted — a caller that derives the same pod
/// name deterministically (`sandbox_volume_pvc_name`'s pod-name
/// counterpart) collides with it on every later attempt, exactly what
/// happened to `test_terminal_lifecycle_end_to_end`'s volume-mount pod
/// before this fix.
///
/// Drives `create_with_running_timeout` directly with a 1ms timeout
/// rather than the real (30s+) default, or mutating
/// `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` — that env var is process-global
/// and read by every other concurrently-running test in this suite, so
/// overriding it here would risk timing them out too. 1ms guarantees
/// the timeout fires before the first status check ever completes,
/// independent of how fast this cluster actually schedules pods.
#[tokio::test]
async fn test_create_deletes_the_pod_it_just_created_if_it_never_reaches_running() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("timeout-cleanup");
    let name = format!("sandbox-{session_id}");

    let result = manager
        .create_with_running_timeout(
            &session_id,
            "128Mi",
            &DockerSidecar {
                memory: "512Mi".to_string(),
                storage: PodStorage::Ephemeral,
            },
            &[],
            Duration::from_millis(1),
            &test_instance(),
        )
        .await;
    match result {
        Err(SandboxError::Timeout(_)) => {}
        Err(e) => panic!("expected a Timeout error, got a different error: {e}"),
        Ok(sandbox) => {
            std::mem::forget(sandbox);
            panic!("expected create to time out waiting for Running, but it succeeded");
        }
    }

    let pods = pods_api(&client);
    let gone = tokio::time::timeout(Duration::from_secs(15), async {
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "the pod create() gave up waiting on should have been cleaned up, not left orphaned"
    );
}

/// Every configured volume is mounted into every pod, but its claim can
/// be missing from the pod's namespace — the cluster was rebuilt, the
/// claim was deleted, or (as the browser tier found) the volume was
/// created in another namespace. The pod must still start.
#[tokio::test]
async fn test_create_recreates_a_missing_volume_claim() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pvcs = pvc_api(&client);
    let volume_id = 990_003;
    let claim = sandbox_volume_pvc_name(volume_id);
    pvcs.delete(&claim, &DeleteParams::default()).await.ok();
    let now = chrono::Utc::now().naive_utc();
    let volume = db::SandboxVolume {
        id: volume_id,
        name: "missing-claim".to_string(),
        mount_path: "/missing-claim".to_string(),
        created_at: now,
        updated_at: now,
    };

    let result = manager
        .create(&unique_session_id("missing-claim"), "128Mi", &[volume])
        .await;
    let claim_exists = matches!(pvcs.get_opt(&claim).await, Ok(Some(_)));
    if let Ok(sandbox) = &result {
        pods_api(&client)
            .delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .ok();
    }
    pvcs.delete(&claim, &DeleteParams::default()).await.ok();

    assert!(result.is_ok(), "the pod should start: {:?}", result.err().map(|e| e.to_string()));
    assert!(claim_exists, "the missing claim should have been recreated");
}

#[tokio::test]
async fn test_create_applies_the_memory_limit_and_reserves_nothing() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("limits");

    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let pods = pods_api(&client);
    let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
    let limits = pod
        .spec
        .expect("pod should have a spec")
        .containers
        .into_iter()
        .next()
        .expect("pod should have a container")
        .resources
        .expect("container should have resources");
    let requests = limits.requests.clone().unwrap_or_default();
    let limits = limits.limits.expect("resources should have limits");
    assert_eq!(limits.get("memory"), Some(&Quantity("128Mi".to_string())));
    assert_eq!(requests.get("memory"), Some(&Quantity("0".to_string())), "no memory reserved (SME-77)");
    assert!(!limits.contains_key("cpu") && !requests.contains_key("cpu"), "no CPU limit or request (SME-77)");

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// A `memory_limit` over the `smelt-park` namespace's `LimitRange` max
/// (`k8s/smelt-park-rbac.yaml`, `64Gi`) is rejected by Kubernetes
/// itself — no app-side comparison logic to test, just that the
/// rejection actually happens and surfaces as an ordinary error.
/// Requires the `LimitRange` to actually be applied to the cluster
/// (`k3s-bootstrap`, or the equivalent on `homelab`) — see SME-12's
/// "Per-pod limit overrides".
#[tokio::test]
async fn test_create_rejects_a_memory_limit_over_the_limitrange_max() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("over-limit");

    let result = manager.create(&session_id, "128Gi", &[]).await;
    assert!(
        matches!(result, Err(SandboxError::Kube(_))),
        "a memory_limit over the LimitRange's 64Gi max should be rejected by Kubernetes, got is_ok={}",
        result.is_ok()
    );
}

/// The one real, permanent OOM trigger in this suite (see SME-12's
/// "Testing" on why this isn't a full `cargo build` like the design
/// spike used — a bash builtin growing memory directly in its own
/// process, no fork, needs no `rust:*` image). Proves the whole real
/// path end to end: a genuine kernel OOM kill, Kubernetes actually
/// reporting `OOMKilled`, and `pod_death_reason` extracting it from a
/// *real* API response — the pure unit tests already cover the
/// branching logic exhaustively, but only against constructed data.
#[tokio::test]
async fn test_pod_death_reason_reports_oomkilled_from_a_real_oom_kill() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("real-oom");
    // Small on purpose — fast and reliable to trigger, using SME-12's
    // own per-pod override rather than the real 8Gi default.
    let sandbox = manager
        .create(&session_id, "64Mi", &[])
        .await
        .expect("create should succeed");
    let pods = pods_api(&client);

    // The whole `AttachedProcess` — not just its split-off stdout/
    // stderr handles — is moved into the spawned task, so the exec
    // session it owns stays open for as long as *that task* runs, not
    // tied to this function's own scope. Letting `exec` drop early
    // (e.g. a `{ ... }` block that only spawns readers off split
    // handles) closes the underlying connection, and since this
    // process is directly attached (not `setsid`-detached the way the
    // real agent injection is), the container runtime kills it right
    // along with the disconnect — the remote command never gets a
    // chance to actually run.
    if let Ok(mut exec) = pods
        .exec(
            &sandbox.pod_name,
            ["bash", "-c", "printf -v x '%*s' 200000000 ''; sleep 5"],
            &AttachParams::default(),
        )
        .await
    {
        tokio::spawn(async move {
            let mut out = String::new();
            let mut err = String::new();
            if let Some(mut so) = exec.stdout() {
                let _ = so.read_to_string(&mut out).await;
            }
            if let Some(mut se) = exec.stderr() {
                let _ = se.read_to_string(&mut err).await;
            }
            exec.join().await.ok();
        });
    }

    let reason = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(reason) = pod_death_reason(&pods, &sandbox.pod_name, &test_instance()).await {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;

    // Delete the pod directly before asserting, not through the cleanup
    // queue: a Failed pod is never removed by Kubernetes, and every run
    // used to leave one behind.
    let name = sandbox.pod_name.clone();
    std::mem::forget(sandbox);
    pods.delete(&name, &immediate_delete_params()).await.ok();
    while pods.get_opt(&name).await.ok().flatten().is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let reason = reason.expect("pod should be confirmed dead within 30s of a real OOM trigger");
    assert_eq!(
        reason,
        Some("OOMKilled".to_string()),
        "a real OOM kill should surface as OOMKilled via the real Kubernetes API, not a constructed one"
    );
}

#[tokio::test]
async fn test_create_reuses_existing_running_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("reuse");

    let first = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("first create should succeed");
    let second = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("second create should reuse, not error");

    assert_eq!(first.pod_name, second.pod_name);

    let pods = pods_api(&client);
    pods.delete(&first.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(first);
    std::mem::forget(second);
}

#[tokio::test]
async fn test_exec_captures_stdout_and_exit_code() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("exec");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let result = sandbox
        .exec(&["echo", "hello"])
        .await
        .expect("exec should succeed");
    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, 0);

    let failing = sandbox
        .exec(&["bash", "-c", "exit 7"])
        .await
        .expect("exec should succeed even for nonzero exit");
    assert_eq!(failing.exit_code, 7);

    let pods = pods_api(&client);
    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// New observable behavior from
/// SME-17's Phase 3 —
/// commands run as the pre-created, unprivileged `sandbox` user by
/// default (not root, unlike the old `debian:trixie-slim` image with
/// no `USER` set), with passwordless `sudo` still available for
/// anything that genuinely needs root.
#[tokio::test]
async fn test_sandbox_commands_run_as_non_root_with_sudo_available() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("non-root");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let whoami = sandbox
        .exec(&["whoami"])
        .await
        .expect("exec should succeed");
    assert_eq!(
        whoami.stdout.trim(),
        "sandbox",
        "commands should run as the pre-created `sandbox` user, not root"
    );

    let sudo_whoami = sandbox
        .exec(&["sudo", "whoami"])
        .await
        .expect("exec should succeed");
    assert_eq!(
        sudo_whoami.stdout.trim(),
        "root",
        "sudo should still reach root for a command that genuinely needs it"
    );

    // The everyday tools are already there: a first coding task used
    // to fail with `python3: command not found` and spend a minute
    // installing it (SME-41 D10).
    for tool in ["python3", "git", "curl"] {
        let found = sandbox
            .exec(&["sh", "-c", &format!("command -v {tool}")])
            .await
            .expect("exec should succeed");
        assert!(!found.stdout.trim().is_empty(), "{tool} should be installed in the sandbox image");
    }

    let pods = pods_api(&client);
    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// Characterization test for `pod_death_reason`'s real fetch, not
/// `decide_pod_death_reason`'s branching (already exhaustively covered
/// without a cluster) — proves the wrapper actually calls through to a
/// live pod correctly in both directions: inconclusive while it's
/// genuinely running, confirmed (with no reason to report) once it's
/// gone entirely.
#[tokio::test]
async fn test_pod_death_reason_reflects_a_real_pod_then_its_absence() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("death-reason");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");
    let pods = pods_api(&client);

    assert_eq!(
        pod_death_reason(&pods, &sandbox.pod_name, &test_instance()).await,
        None,
        "a genuinely Running pod is inconclusive"
    );

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .expect("delete should succeed");
    assert_eq!(
        pod_death_reason(&pods, &sandbox.pod_name, &test_instance()).await,
        Some(None),
        "a pod that's gone entirely is confirmed dead with no reason to report"
    );
    std::mem::forget(sandbox);
}

// No standalone `force_terminate_pod` test: it's reached through
// `terminate_pod`, which `pod_lifecycle_tests` exercise many times over.
// A test of it would never set the process-global `MANAGER` (a kube
// client works only while the runtime that built it runs, and each test
// has its own: on SME-42 that broke the lifecycle test with
// `Kube(Service(Closed))`); it would call `use_test_manager` instead.

#[tokio::test]
async fn test_manager_delete_removes_the_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("delete");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");
    let pod_name = sandbox.pod_name.clone();

    manager
        .delete(sandbox)
        .await
        .expect("delete should succeed");

    // Pods are deleted with a grace period (SME-51 B6), and the agent
    // exits on SIGTERM, so the pod goes well within it.
    let pods = pods_api(&client);
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        while pods.get_opt(&pod_name).await.expect("get_opt should not error").is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the pod should stop within seconds of manager.delete, not wait out its grace");
}

#[tokio::test]
async fn test_dropping_without_delete_still_cleans_up_via_drain_task() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("drop");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");
    let pod_name = sandbox.pod_name.clone();

    drop(sandbox); // no manager.delete call — this is the path under test

    let pods = pods_api(&client);
    let gone = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if pods
                .get_opt(&pod_name)
                .await
                .expect("get_opt should not error")
                .is_none()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "drain task should have deleted the pod within the timeout"
    );
}

/// The futures of the sandbox calls that sit deepest in the dev server's
/// and the tests' call stacks stay small. A debug build runs them on a
/// 2 MB stack: on SME-115 holding a whole `Pod` across an await doubled
/// `force_terminate_pod`'s future and every caller's, and the first sign
/// was `test_terminal_lifecycle_end_to_end` overflowing its stack. Each
/// bound is about twice the size when this test was written; a failure
/// here means keep large values out of an await's scope or `Box::pin`
/// the large sub-future, not raise the bound. Nothing is polled, so this
/// needs neither a cluster nor a database.
#[tokio::test]
async fn test_sandbox_futures_stay_small() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/unused")
        .expect("lazy pool");
    let client = kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().expect("url"))).expect("client");
    let instance = db::SmeltInstance { id: "size-test".to_string(), owns_unlabelled: true };

    let sizes = [
        ("force_terminate_pod", std::mem::size_of_val(&force_terminate_pod(&pool, 1)), 1200),
        ("force_terminate_pod_with", std::mem::size_of_val(&force_terminate_pod_with(&pool, &client, 1)), 600),
        ("close_if_gone", std::mem::size_of_val(&watch::close_if_gone(&pool, 1)), 4200),
        ("close_if_gone_with", std::mem::size_of_val(&watch::close_if_gone_with(&pool, &client, 1)), 4000),
        ("create_pod", std::mem::size_of_val(&create_pod(&pool, 1, PodLimitOverrides::default())), 15000),
        ("ensure_conversation_pvcs", std::mem::size_of_val(&claims::ensure_conversation_pvcs(&client, 1, &instance)), 6700),
        // `create_pod` spawns its work, so its own size can't show what
        // the wait for `Running` holds; these two do (SME-132 review 1).
        // The wait's bound is tighter than twice its 2760 bytes: its
        // timed-out report, unboxed, takes it to 4520.
        // SME-137: these two are their spans' wrappers, which box the work,
        // so they stay small (unboxed, the wait's was 5984 bytes); the
        // work's own futures, `_unspanned`, keep the bounds they had.
        ("create_pod_attempt", std::mem::size_of_val(&create_pod_attempt(&pool, 1, PodLimitOverrides::default())), 1000),
        ("wait_for_running_with_timeout", std::mem::size_of_val(&wait_for_running_with_timeout(&pods_api(&client), "sandbox-1", Duration::from_secs(1))), 1000),
        ("wait_for_running_unspanned", std::mem::size_of_val(&wait_for_running_unspanned(&pods_api(&client), "sandbox-1", Duration::from_secs(1))), 3600),
        ("create_pod_attempt_unspanned", std::mem::size_of_val(&create_pod_attempt_unspanned(&pool, 1, PodLimitOverrides::default())), 19000),
        // Given spans on SME-137 (7664, 840 and 7200 bytes then).
        ("start_or_get_pod", std::mem::size_of_val(&start_or_get_pod(&pool, 1)), 15400),
        ("terminate_pod", std::mem::size_of_val(&terminate_pod(&pool, 1)), 1700),
        ("teardown_conversation", std::mem::size_of_val(&teardown_conversation(&pool, 1, &[])), 14400),
        // SME-144 moved its publishes around the send: 664 bytes then.
        ("run_command", std::mem::size_of_val(&run_command(&pool, 1, 1, "cmd", "true")), 1300),
        // B1 (SME-122): every reconnect awaits it.
        ("start_under_way_for_pod", std::mem::size_of_val(&start_under_way_for_pod(&pool, 1)), 700),
    ];
    for (name, size, bound) in sizes {
        println!("{name}: {size} bytes (bound {bound})");
    }
    for (name, size, bound) in sizes {
        assert!(size <= bound, "{name}'s future is {size} bytes, over its bound of {bound}");
    }
}

/// SME-137: a wait for `Running` is a `wait_for_running` span naming its
/// pod and timeout, failed when the wait fails.
#[tokio::test]
async fn test_a_wait_that_cant_reach_the_cluster_is_a_failed_span() {
    let client = unreachable_client();
    let (waited, spans) = crate::telemetry::capture_spans(wait_for_running_with_timeout(
        &pods_api(&client),
        "sandbox-9137000001",
        Duration::from_secs(5),
    ))
    .await;
    assert!(waited.is_err());
    let span = spans.iter().find(|span| span.name == "wait_for_running").expect("the wait's span");
    assert!(span.attributes.iter().any(|kv| kv.key.as_str() == "pod" && kv.value == "sandbox-9137000001".into()), "{span:?}");
    assert!(span.attributes.iter().any(|kv| kv.key.as_str() == "timeout_s" && kv.value == 5_i64.into()), "{span:?}");
    assert!(matches!(span.status, opentelemetry::trace::Status::Error { .. }), "{:?}", span.status);
}

/// SME-137: a pod event the watch acts on is a short `pod_event` span with
/// the pod and its phase.
#[tokio::test]
async fn test_a_pod_event_span_names_the_pod_and_phase() {
    let mut pod = Pod::default();
    pod.metadata.name = Some("sandbox-9137000002".to_string());
    pod.status = Some(k8s_openapi::api::core::v1::PodStatus { phase: Some("Failed".to_string()), ..Default::default() });
    let ((), spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(watch::pod_event_span("finished", &pod), async {}).await;
    })
    .await;
    let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
    assert_eq!(span.name, "pod_event");
    let attribute = |key: &str| span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| kv.value.clone());
    assert_eq!(attribute("pod"), Some("sandbox-9137000002".into()));
    assert_eq!(attribute("phase"), Some("Failed".into()));
    assert_eq!(attribute("event"), Some("finished".into()));
}

/// SME-94 review 1: `use_test_manager` refuses a multi-thread runtime,
/// where a task on another worker would read the process-wide manager.
#[test]
fn test_a_test_manager_on_a_multi_thread_runtime_is_refused() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let refused = std::thread::spawn(move || {
        runtime.block_on(async { std::panic::catch_unwind(|| use_test_manager(unreachable_client())).is_err() })
    })
    .join()
    .expect("the test thread");
    assert!(refused, "use_test_manager accepted a multi-thread runtime");
}
