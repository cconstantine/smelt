use super::super::*;

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::{DeleteParams, PostParams};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) async fn client() -> kube::Client {
    crate::test_lock::require(); // only under the cluster lock (SME-212)
    let _ = rustls::crypto::ring::default_provider().install_default();
    kube::Client::try_default().await.expect("KUBECONFIG must point at a reachable cluster")
}

/// A conversation id no other test run is using: the fixture band of
/// `crate::id_alloc` — its cursor is seeded once per process from the
/// start time mixed with the pid (so runs don't meet leftovers from
/// earlier ones) and every call advances it a full stride (so tests
/// started at once in this run never share one). (SME-255: this used to
/// fold `as_secs` through a modulo beside a counter, and two processes
/// started in the same second shared ids.)
pub(crate) fn unique() -> i64 {
    crate::id_alloc::FIXTURE_IDS.next_base()
}

/// A sandbox stand-in for conversation `id`, with its workspace
/// claim. Deleted by `tear_down`.
pub(crate) async fn stand_in_sandbox(client: &kube::Client, id: i64) -> Result<SandboxRef, String> {
    stand_in_sandbox_with(client, id, &crate::sandbox::default_sandbox_image(), Duration::from_secs(120)).await
}

async fn stand_in_sandbox_with(
    client: &kube::Client,
    id: i64,
    image: &str,
    wait: Duration,
) -> Result<SandboxRef, String> {
    let pods = crate::sandbox::pods_api(client);
    let pvcs: kube::Api<PersistentVolumeClaim> = kube::Api::namespaced(client.clone(), pods_namespace(&pods));
    let claim_name = crate::sandbox::workspace_pvc_name(id);
    let claim: PersistentVolumeClaim = serde_json::from_value(json!({
        "metadata": {"name": claim_name},
        "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}},
    }))
    .map_err(|e| format!("the claim's spec: {e}"))?;
    if let Err(e) = pvcs.create(&PostParams::default(), &claim).await {
        if may_have_created(&e) {
            let _ = pvcs.delete(&claim_name, &DeleteParams::default()).await;
        }
        return Err(format!("creating the claim: {e}"));
    }
    let name = format!("sandbox-lsp-test-{id}");
    let mut guard = StandInGuard { name: name.clone(), claim_name: claim_name.clone(), armed: true };
    let started = start_stand_in(&pods, &name, id, image, &claim_name, wait).await;
    // Finished, either way: a success is the caller's to tear down,
    // and a failure is cleaned up just below, where it can report.
    guard.armed = false;
    if let Err(e) = started {
        // The caller never gets a `SandboxRef` to tear down, so this
        // has to, however it failed: on SME-53, every failed run left
        // its stand-ins behind. What it can't delete, it names.
        let gone = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
        let pod = pods.delete(&name, &gone).await.map(|_| ());
        let claim = pvcs.delete(&claim_name, &DeleteParams::default()).await.map(|_| ());
        let left: Vec<String> = [
            left_behind(&format!("pod {name}"), pod),
            left_behind(&format!("claim {claim_name}"), claim),
        ]
        .into_iter()
        .flatten()
        .collect();
        return Err(if left.is_empty() {
            e
        } else {
            format!("{e}; cleaning up failed, so delete by hand: {}", left.join("; "))
        });
    }
    started
}

/// Deletes a stand-in's pod and claim if it's dropped while armed:
/// when `stand_in_sandbox_with`'s future is dropped part-way, by a
/// caller that gave up or a panic that took its runtime down. `Drop`
/// can't await, and the runtime dropping it may be shutting down, so
/// the deletes run on a thread with a runtime and client of its own
/// (a client only works on the runtime it was made on), and `drop`
/// waits for them.
struct StandInGuard {
    name: String,
    claim_name: String,
    armed: bool,
}

impl Drop for StandInGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let name = std::mem::take(&mut self.name);
        let claim_name = std::mem::take(&mut self.claim_name);
        let cleanup = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("stand-in cleanup: no runtime ({e}); delete pod {name} and claim {claim_name} by hand");
                    return;
                }
            };
            runtime.block_on(async {
                let _ = rustls::crypto::ring::default_provider().install_default();
                let client = match kube::Client::try_default().await {
                    Ok(client) => client,
                    Err(e) => {
                        eprintln!("stand-in cleanup: no client ({e}); delete pod {name} and claim {claim_name} by hand");
                        return;
                    }
                };
                let pods = crate::sandbox::pods_api(&client);
                let pvcs: kube::Api<PersistentVolumeClaim> =
                    kube::Api::namespaced(client.clone(), pods_namespace(&pods));
                let gone = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
                let pod = pods.delete(&name, &gone).await.map(|_| ());
                let claim = pvcs.delete(&claim_name, &DeleteParams::default()).await.map(|_| ());
                for left in [left_behind(&format!("pod {name}"), pod), left_behind(&format!("claim {claim_name}"), claim)]
                    .into_iter()
                    .flatten()
                {
                    eprintln!("stand-in cleanup failed, delete by hand: {left}");
                }
            });
        });
        if cleanup.join().is_err() {
            eprintln!("stand-in cleanup: its thread panicked");
        }
    }
}

/// What a cleanup delete left behind, if anything: nothing when it
/// worked or the object was already gone.
fn left_behind(what: &str, deleted: Result<(), kube::Error>) -> Option<String> {
    match deleted {
        Ok(()) => None,
        Err(kube::Error::Api(status)) if status.is_not_found() => None,
        Err(e) => Some(format!("{what}: {e}")),
    }
}

#[test]
fn test_left_behind_names_what_a_failed_delete_left() {
    let api = |reason: &str, code: u16| {
        kube::Error::Api(Box::new(kube::core::Status::failure("no", reason).with_code(code)))
    };
    assert_eq!(left_behind("pod p", Ok(())), None);
    assert_eq!(left_behind("pod p", Err(api("NotFound", 404))), None, "already gone is gone");
    let left = left_behind("pod p", Err(api("InternalError", 500))).expect("a failed delete is named");
    assert!(left.starts_with("pod p: "), "{left}");
    assert!(left_behind("claim c", Err(kube::Error::Service(Box::new(std::io::Error::other("timed out"))))).is_some());
}

/// Whether a create that failed may still have made the object. An
/// API error is the server refusing (including "already exists",
/// which isn't ours to delete); anything else, a timeout or a dropped
/// connection, can come after the server made it.
fn may_have_created(error: &kube::Error) -> bool {
    !matches!(error, kube::Error::Api(_))
}

#[test]
fn test_only_a_failure_short_of_the_server_answering_may_have_created_the_object() {
    let refused = |reason: &str, code: u16| {
        kube::Error::Api(Box::new(kube::core::Status::failure("refused", reason).with_code(code)))
    };
    assert!(!may_have_created(&refused("AlreadyExists", 409)));
    assert!(!may_have_created(&refused("Forbidden", 403)));
    assert!(!may_have_created(&refused("Invalid", 422)));
    assert!(may_have_created(&kube::Error::Service(Box::new(std::io::Error::other("timed out")))));
}

/// The stand-in's pod, once the claim exists, waited on until Running.
async fn start_stand_in(
    pods: &kube::Api<Pod>,
    name: &str,
    id: i64,
    image: &str,
    claim_name: &str,
    wait: Duration,
) -> Result<SandboxRef, String> {
    let pod: Pod = serde_json::from_value(json!({
        "metadata": {"name": name},
        "spec": {
            "restartPolicy": "Never",
            "containers": [{
                "name": "sandbox",
                "image": image,
                "imagePullPolicy": "Never",
                "command": ["sh", "-c", "trap 'exit 0' TERM; while :; do sleep 1 & wait; done"],
                "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}],
                "resources": {"requests": {"memory": "64Mi", "cpu": "50m"}, "limits": {"memory": "256Mi", "cpu": "1"}},
            }],
            "volumes": [{"name": "workspace", "persistentVolumeClaim": {"claimName": claim_name}}],
        },
    }))
    .map_err(|e| format!("the pod's spec: {e}"))?;
    pods.create(&PostParams::default(), &pod).await.map_err(|e| format!("creating the pod: {e}"))?;
    let mut waiting_on = None;
    for _ in 0..wait.as_millis() / 500 {
        let p = pods.get(name).await.map_err(|e| format!("reading the pod: {e}"))?;
        waiting_on = p
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|statuses| statuses.iter().find_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.clone()));
        if p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running") {
            return Ok(SandboxRef {
                conversation_id: id,
                pod_id: id,
                pod_name: name.to_string(),
                pod_uid: p.metadata.uid.clone().ok_or("the Running pod has no uid")?,
                node: p.spec.as_ref().and_then(|s| s.node_name.clone()).ok_or("the Running pod has no node")?,
                instance: crate::sandbox::TEST_INSTANCE.to_string(),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "the stand-in sandbox pod didn't start within {}s{}",
        wait.as_secs(),
        waiting_on.map(|reason| format!(" (waiting: {reason})")).unwrap_or_default()
    ))
}

fn pods_namespace(pods: &kube::Api<Pod>) -> &str {
    pods.namespace().expect("namespaced")
}

pub(crate) async fn tear_down(client: &kube::Client, sandbox: &SandboxRef) {
    let pods = crate::sandbox::pods_api(client);
    let gone = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
    let _ = pods.delete(&sandbox.pod_name, &gone).await;
    let pvcs: kube::Api<PersistentVolumeClaim> = kube::Api::namespaced(client.clone(), pods_namespace(&pods));
    let _ = pvcs.delete(&crate::sandbox::workspace_pvc_name(sandbox.conversation_id), &DeleteParams::default()).await;
}

/// Runs `body` with a stand-in sandbox, and tears it down whether
/// or not the body panics.
pub(crate) async fn with_sandbox<F, Fut>(body: F)
where
    F: FnOnce(kube::Client, SandboxRef) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use futures_util::FutureExt;
    let client = client().await;
    let sandbox = stand_in_sandbox(&client, unique()).await.expect("a stand-in sandbox");
    let outcome = std::panic::AssertUnwindSafe(body(client.clone(), sandbox.clone())).catch_unwind().await;
    tear_down(&client, &sandbox).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// A stand-in whose pod never starts (here, an image the node
/// doesn't have) takes its pod and claim with it: nothing else can,
/// since the test never gets a `SandboxRef` to tear down.
#[tokio::test]
async fn test_a_stand_in_sandbox_that_never_starts_leaves_nothing_behind() {
    let client = client().await;
    let id = unique();
    let started =
        stand_in_sandbox_with(&client, id, "docker.io/library/smelt-no-such-image:never", Duration::from_secs(5)).await;
    started.map(|_| ()).expect_err("a pod with no image started");
    assert_left_nothing(&client, id).await;
}

/// The same when the stand-in fails any other way once its claim
/// exists: here its pod is deleted while it waits, so reading it fails.
#[tokio::test]
async fn test_a_stand_in_sandbox_that_fails_while_waiting_leaves_nothing_behind() {
    let client = client().await;
    let id = unique();
    let name = format!("sandbox-lsp-test-{id}");
    let pods = crate::sandbox::pods_api(&client);
    let starting = tokio::spawn({
        let client = client.clone();
        async move {
            stand_in_sandbox_with(&client, id, "docker.io/library/smelt-no-such-image:never", Duration::from_secs(60))
                .await
                .map(|_| ())
        }
    });
    for _ in 0..240 {
        if matches!(pods.get_opt(&name).await, Ok(Some(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let gone = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
    let deleted = pods.delete(&name, &gone).await;
    let started = tokio::time::timeout(Duration::from_secs(30), starting)
        .await
        .expect("the stand-in kept waiting on a deleted pod")
        .expect("join");
    assert_left_nothing(&client, id).await;
    deleted.expect("delete the pod under it");
    let err = started.expect_err("a deleted pod started");
    assert!(err.contains("reading the pod"), "{err}");
}

/// A caller that gives up on the stand-in (a timeout, a panic that
/// takes its runtime down) drops it mid-wait; that must not leave
/// the pod and claim behind either.
#[tokio::test]
async fn test_a_stand_in_sandbox_dropped_while_waiting_leaves_nothing_behind() {
    let client = client().await;
    let id = unique();
    let name = format!("sandbox-lsp-test-{id}");
    let pods = crate::sandbox::pods_api(&client);
    let starting = tokio::spawn({
        let client = client.clone();
        async move {
            stand_in_sandbox_with(&client, id, "docker.io/library/smelt-no-such-image:never", Duration::from_secs(120))
                .await
                .map(|_| ())
        }
    });
    for _ in 0..240 {
        if matches!(pods.get_opt(&name).await, Ok(Some(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    starting.abort();
    let _ = starting.await;
    assert_left_nothing(&client, id).await;
}

/// Waits up to 30s for stand-in `id`'s pod and claim to be gone,
/// deletes whatever is left, and fails if anything was.
async fn assert_left_nothing(client: &kube::Client, id: i64) {
    let pods = crate::sandbox::pods_api(client);
    let pvcs: kube::Api<PersistentVolumeClaim> = kube::Api::namespaced(client.clone(), pods_namespace(&pods));
    let name = format!("sandbox-lsp-test-{id}");
    let claim = crate::sandbox::workspace_pvc_name(id);
    let mut left = (true, true);
    for _ in 0..60 {
        left = (
            pods.get_opt(&name).await.expect("get pod").is_some(),
            pvcs.get_opt(&claim).await.expect("get claim").is_some(),
        );
        if left == (false, false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Clean up after this test itself, whatever it found.
    let _ = pods.delete(&name, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() }).await;
    let _ = pvcs.delete(&claim, &DeleteParams::default()).await;
    assert_eq!(left, (false, false), "(pod, claim) left behind");
}

fn echo_server(name: &str, install: &str) -> LanguageServerConfig {
    LanguageServerConfig {
        name: name.to_string(),
        image: "debian:trixie-slim".to_string(),
        install_command: install.to_string(),
        command: "cat".to_string(),
        file_types: [("txt".to_string(), "plaintext".to_string())].into(),
        memory_limit: "128Mi".to_string(),
        cpu_limit: "1".to_string(),
        enabled: true,
        ..Default::default()
    }
}

/// Starting installs, a second start finds it running (even two at
/// once), its stdio works, and it goes with its sandbox pod.
#[tokio::test]
async fn test_a_server_pod_starts_once_talks_and_goes_with_its_sandbox() {
    with_sandbox(|client, sandbox| async move {
    let config = echo_server("echo", "echo installing; touch $HOME/installed");

    let (first, second) = tokio::join!(
        start_with(&client, &sandbox, &config, "v1"),
        start_with(&client, &sandbox, &config, "v1"),
    );
    let mut outcomes = vec![first.clone(), second.clone()];
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(outcomes, vec![Ok(Started::AlreadyRunning), Ok(Started::Started)], "{first:?} {second:?}");

    let listed = list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].name.as_str(), &listed[0].state, listed[0].config_version.as_str()), ("echo", &ServerState::Ready, "v1"));
    let pod = crate::sandbox::pods_api(&client).get(&listed[0].pod_name).await.expect("pod");
    assert_eq!(pod.metadata.annotations.as_ref().and_then(|a| a.get(READY_ANNOTATION)).map(String::as_str), Some("true"));

    let mut io = open_stdio(&client, &listed[0].pod_name, &config).await.expect("stdio");
    io.stdin.write_all(b"Content-Length: 2\r\n\r\n{}").await.expect("write");
    io.stdin.flush().await.expect("flush");
    let mut echoed = vec![0u8; 23];
    tokio::time::timeout(Duration::from_secs(20), io.stdout.read_exact(&mut echoed))
        .await
        .expect("the server should answer")
        .expect("read");
    assert_eq!(&echoed, b"Content-Length: 2\r\n\r\n{}");

    // The sandbox pod goes; its server pod goes with it.
    let pods = crate::sandbox::pods_api(&client);
    pods.delete(&sandbox.pod_name, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() })
        .await
        .expect("delete the sandbox");
    let mut gone = false;
    for _ in 0..120 {
        if list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list").is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(gone, "the server pod outlived its sandbox pod");
    })
    .await;
}

/// A failed install comes back with its output, and leaves no pod.
#[tokio::test]
async fn test_a_failed_install_says_why_and_leaves_nothing() {
    with_sandbox(|client, sandbox| async move {
    let result = start_with(&client, &sandbox, &echo_server("broken", "echo no such package >&2; exit 3"), "v1").await;
    let listed = list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list");
    let error = result.expect_err("the install failed");
    assert!(error.contains("no such package") && error.contains('3'), "{error}");
    assert!(listed.is_empty(), "the failed server's pod was left: {listed:?}");
    })
    .await;
}

/// A server that runs out of memory takes only its own pod down
/// (the kernel kills the whole container): the sandbox keeps
/// running, the server shows as stopped, and starting it again
/// replaces the pod.
#[tokio::test]
async fn test_a_server_out_of_memory_leaves_the_sandbox_running() {
    with_sandbox(|client, sandbox| async move {
    let config = LanguageServerConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "head -c 1G /dev/zero | tail".to_string()],
        ..echo_server("hungry", "")
    };
    start_with(&client, &sandbox, &config, "v1").await.expect("start");
    let pod_name = server_pod_name(sandbox.pod_id, "hungry");
    let mut io = open_stdio(&client, &pod_name, &config).await.expect("stdio");
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), io.stdout.read_to_end(&mut rest))
        .await
        .expect("the server's output should end when it's killed")
        .expect("read");

    let mut state = None;
    for _ in 0..120 {
        state = list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list").pop().map(|p| p.state);
        if matches!(state, Some(ServerState::Stopped(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(state, Some(ServerState::Stopped(Some("OOMKilled".to_string()))));
    let pods = crate::sandbox::pods_api(&client);
    let sandbox_pod = pods.get(&sandbox.pod_name).await.expect("sandbox");
    assert_eq!(sandbox_pod.status.and_then(|s| s.phase).as_deref(), Some("Running"));

    assert_eq!(start_with(&client, &sandbox, &config, "v1").await, Ok(Started::Started));
    })
    .await;
}

/// SME-115: a server pod of the same name that is another
/// database's is refused and left alone, never reused or replaced.
#[tokio::test]
async fn test_another_databases_server_pod_is_left_alone() {
    let client = client().await;
    let pods = crate::sandbox::pods_api(&client);
    let id = unique();
    let sandbox = SandboxRef {
        conversation_id: id,
        pod_id: id,
        pod_name: crate::sandbox::pod_name(id),
        pod_uid: "unused".to_string(),
        node: "unused".to_string(),
        instance: crate::sandbox::TEST_INSTANCE.to_string(),
    };
    let name = server_pod_name(id, "theirs");
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {
            LSP_OF_LABEL: id.to_string(),
            LSP_SERVER_LABEL: "theirs",
            crate::sandbox::INSTANCE_LABEL: "another-smelt-database",
        }},
        "spec": {"containers": [{"name": "server", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    let started = start_with(&client, &sandbox, &echo_server("theirs", ""), "v1").await;
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() }).await.ok();
    assert!(started.as_ref().is_err_and(|e| e.contains("SME-115")), "{started:?}");
    assert!(kept, "another database's server pod was deleted");
}

/// SME-115: stopping a server everywhere (its config was deleted)
/// stops only this database's pods of it: another smelt sharing the
/// namespace has its own config of that name.
#[tokio::test]
async fn test_stopping_a_server_everywhere_leaves_another_databases() {
    let client = client().await;
    let pods = crate::sandbox::pods_api(&client);
    let id = unique();
    let server = format!("stop-{id}");
    let name = server_pod_name(id, &server);
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {
            LSP_OF_LABEL: id.to_string(),
            LSP_SERVER_LABEL: server,
            crate::sandbox::INSTANCE_LABEL: "another-smelt-database",
        }},
        "spec": {"containers": [{"name": "server", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    let stopped = stop_everywhere_with(&client, &server, crate::sandbox::TEST_INSTANCE).await;
    let stopped_in = stop_everywhere_in(&client, id, &server, crate::sandbox::TEST_INSTANCE).await;
    let listed = list_with(&client, id, crate::sandbox::TEST_INSTANCE).await;
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() }).await.ok();
    assert_eq!((stopped, stopped_in), (Ok(()), Ok(())));
    assert_eq!(listed, Ok(Vec::new()), "another database's server pod was listed as ours");
    assert!(kept, "stopping a server everywhere deleted another database's pod");
}

/// Closing a server's stdin over `pods/exec` reaches the process in
/// its pod, whether the stream is shut down or just dropped: that's
/// how a language server is told to exit.
#[tokio::test]
async fn test_closing_a_servers_stdin_reaches_it() {
    with_sandbox(|client, sandbox| async move {
    let config = |marker: &str| LanguageServerConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), format!("cat > /dev/null; touch /tmp/{marker}")],
        ..echo_server("reader", "")
    };
    start_with(&client, &sandbox, &config("x"), "v1").await.expect("start");
    let pod_name = server_pod_name(sandbox.pod_id, "reader");
    let exists = |marker: &'static str| {
        let client = client.clone();
        let pod_name = pod_name.clone();
        async move {
            for _ in 0..40 {
                let found = crate::sandbox::exec_with(&client, &pod_name, "server", &["test", "-e", &format!("/tmp/{marker}")], None)
                    .await
                    .expect("exec");
                if found.exit_code == 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            false
        }
    };

    let mut io = open_stdio(&client, &pod_name, &config("shut")).await.expect("stdio");
    io.stdin.shutdown().await.expect("shutdown");
    assert!(exists("shut").await, "shutting stdin down should end the server");

    let io = open_stdio(&client, &pod_name, &config("dropped")).await.expect("stdio");
    drop(io);
    assert!(exists("dropped").await, "dropping the stream should end the server");
    })
    .await;
}

/// Deleting a server's config stops its pods everywhere.
#[tokio::test]
async fn test_stopping_a_server_everywhere_deletes_its_pods() {
    with_sandbox(|client, sandbox| async move {
    let name = format!("gone-{}", sandbox.conversation_id % 100_000);
    start_with(&client, &sandbox, &echo_server(&name, ""), "v1").await.expect("start");
    stop_everywhere_with(&client, &name, crate::sandbox::TEST_INSTANCE).await.expect("stop");
    let mut gone = false;
    for _ in 0..120 {
        if list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list").is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(gone);
    })
    .await;
}

/// Starts `config`'s server, and once the start is waiting on the
/// install (past waiting for the pod to run), runs `meanwhile` with the
/// server pod's name. The start's outcome, which must come within 120 s
/// of `meanwhile`: the install itself would wait 10 minutes.
async fn start_and_meanwhile<F, Fut>(
    client: &kube::Client,
    sandbox: &SandboxRef,
    config: &LanguageServerConfig,
    meanwhile: F,
) -> Result<Started, String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let name = server_pod_name(sandbox.pod_id, &config.name);
    let waiting = test_hooks::on_install_wait(&name);
    let start = start_with(client, sandbox, config, "v1");
    tokio::pin!(start);
    tokio::select! {
        ended = &mut start => panic!("the start ended before it waited on the install: {ended:?}"),
        reached = tokio::time::timeout(Duration::from_secs(300), waiting) => {
            reached.expect("the start never waited on the install").expect("the hook was dropped");
        }
    }
    meanwhile(name).await;
    tokio::time::timeout(Duration::from_secs(120), start)
        .await
        .expect("the start kept waiting on an install whose pod had died")
}

/// SME-171: an install the kernel kills for running out of memory ends
/// the start at once, saying so, and leaves no pod. The install waits
/// for `/tmp/go`, made once the start waits on it, so the kill can't
/// land while the start is still waiting for the pod to run.
#[tokio::test]
async fn test_an_install_that_runs_out_of_memory_says_so_at_once() {
    with_sandbox(|client, sandbox| async move {
    let config = echo_server("hungry-install", "while [ ! -e /tmp/go ]; do sleep 0.2; done; head -c 1G /dev/zero | tail");
    let started = start_and_meanwhile(&client, &sandbox, &config, |name| {
        let client = client.clone();
        async move {
            let go = crate::sandbox::exec_with(&client, &name, "server", &["touch", "/tmp/go"], None).await.expect("exec");
            assert_eq!(go.exit_code, 0, "touch /tmp/go: {}", go.stderr);
        }
    })
    .await;
    let listed = list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list");
    let error = started.expect_err("an install killed for memory started");
    assert!(error.contains("OOMKilled") && error.contains("memory limit"), "{error}");
    assert!(listed.is_empty(), "the killed server's pod was left: {listed:?}");
    })
    .await;
}

/// SME-171: a server pod deleted while its install runs (its sandbox
/// went, the user stopped the server, or a restart replaced it) ends the
/// start at once, saying so.
#[tokio::test]
async fn test_a_server_pod_deleted_during_its_install_ends_the_start() {
    with_sandbox(|client, sandbox| async move {
    let config = echo_server("slow-install", "sleep 600");
    let started = start_and_meanwhile(&client, &sandbox, &config, |name| {
        let client = client.clone();
        async move {
            crate::sandbox::pods_api(&client)
                .delete(&name, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() })
                .await
                .expect("delete the server pod");
        }
    })
    .await;
    let listed = list_with(&client, sandbox.conversation_id, &sandbox.instance).await.expect("list");
    let error = started.expect_err("a deleted server pod started");
    assert!(error.contains("slow-install") && error.contains("deleted"), "{error}");
    assert!(listed.is_empty(), "{listed:?}");
    })
    .await;
}
