//! Language server pods (SME-35): one per sandbox pod and server, next to
//! the sandbox on its node, sharing the conversation's `/workspace`.
//!
//! Kubernetes is their only record: they're found by label, owned by the
//! sandbox pod (so they go when it does) and deleted with the conversation
//! (`sandbox::teardown_conversation_with`). See SME-35's state model.

use k8s_openapi::api::core::v1::Pod;
use serde_json::json;

use crate::lsp::catalog::POD_HOME;
use crate::models::LanguageServerConfig;

/// The conversation a server pod works for. Kept apart from the sandbox
/// pod's own `smelt/conversation`, so waiting for a conversation's sandbox
/// pod to go doesn't wait on its servers too.
pub const LSP_OF_LABEL: &str = "smelt/lsp-of";
/// The server's name.
pub const LSP_SERVER_LABEL: &str = "smelt/lsp-server";
/// The sandbox pod it runs next to.
pub const LSP_POD_LABEL: &str = "smelt/lsp-pod";
/// When the config it started with was last saved, to tell the model a
/// running server's config has changed since.
pub const CONFIG_VERSION_ANNOTATION: &str = "smelt/lsp-config-version";
/// Set once a server's install has finished: only a pod that got that far
/// is reported when it stops.
pub const READY_ANNOTATION: &str = "smelt/lsp-ready";

/// Where the pod records its install's outcome.
pub const INSTALL_RC: &str = "/tmp/install.rc";
pub const INSTALL_LOG: &str = "/tmp/install.log";

/// The sandbox pod a server pod goes next to.
#[derive(Clone, Debug)]
pub struct SandboxRef {
    pub conversation_id: i64,
    pub pod_id: i64,
    pub pod_name: String,
    pub pod_uid: String,
    pub node: String,
    /// The database's instance (SME-115): the server pod is labelled with it.
    pub instance: String,
}

/// `lsp-<pod id>-<server>`.
pub fn server_pod_name(pod_id: i64, server: &str) -> String {
    format!("lsp-{pod_id}-{server}")
}

/// The script a server pod runs: the install, its outcome recorded, then
/// idle until deleted (the server itself is started over `pods/exec`).
fn pod_script(install_command: &str) -> String {
    let install = if install_command.trim().is_empty() { "true" } else { install_command };
    format!(
        "mkdir -p \"$HOME\" && ( {install} ) > {INSTALL_LOG} 2>&1; echo $? > {INSTALL_RC}; \
         trap 'exit 0' TERM; while :; do sleep 1 & wait; done"
    )
}

/// The server pod for `config` next to `sandbox`, its config version
/// `config_version`.
#[expect(clippy::expect_used, reason = "a fixed Pod shape; the values from the user (env, image, limits) are strings, which the fields they fill take")]
pub fn server_pod_spec(sandbox: &SandboxRef, config: &LanguageServerConfig, config_version: &str) -> Pod {
    let mut env = vec![
        json!({"name": "HOME", "value": POD_HOME}),
        json!({"name": "CARGO_HOME", "value": format!("{POD_HOME}/.cargo")}),
    ];
    env.extend(config.env.iter().map(|(name, value)| json!({"name": name, "value": value})));
    serde_json::from_value(json!({
        "metadata": {
            "name": server_pod_name(sandbox.pod_id, &config.name),
            "labels": {
                LSP_OF_LABEL: sandbox.conversation_id.to_string(),
                LSP_SERVER_LABEL: config.name,
                LSP_POD_LABEL: sandbox.pod_id.to_string(),
                crate::sandbox::INSTANCE_LABEL: sandbox.instance,
            },
            "annotations": {CONFIG_VERSION_ANNOTATION: config_version},
            "ownerReferences": [{
                "apiVersion": "v1",
                "kind": "Pod",
                "name": sandbox.pod_name,
                "uid": sandbox.pod_uid,
            }],
        },
        "spec": {
            // `local-path` claims are on one node's disk.
            "nodeName": sandbox.node,
            "restartPolicy": "Never",
            "securityContext": {"runAsUser": 1000, "runAsGroup": 1000},
            "containers": [{
                "name": "server",
                "image": config.image,
                "imagePullPolicy": "IfNotPresent",
                "command": ["sh", "-c", pod_script(&config.install_command)],
                "env": env,
                "workingDir": "/workspace",
                "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}],
                "resources": {
                    "requests": {"memory": "64Mi", "cpu": "50m"},
                    "limits": {"memory": config.memory_limit, "cpu": config.cpu_limit},
                },
            }],
            "volumes": [{
                "name": "workspace",
                "persistentVolumeClaim": {"claimName": crate::sandbox::workspace_pvc_name(sandbox.conversation_id)},
            }],
        },
    }))
    .expect("a server pod's spec is a valid Pod")
}

/// How long a server pod may take to start (an image pull included), and
/// its install to finish.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// What `start_with` did.
#[derive(Clone, Debug, PartialEq)]
pub enum Started {
    Started,
    AlreadyRunning,
}

/// A server pod's state, as `list_with` reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerState {
    Installing,
    Ready,
    /// It stopped, with Kubernetes' reason when it gave one (`OOMKilled`).
    Stopped(Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerPod {
    pub name: String,
    pub pod_name: String,
    pub state: ServerState,
    pub config_version: String,
    /// The server container's memory limit (`"2Gi"`).
    pub memory_limit: Option<String>,
}

/// Starts `config`'s server next to `sandbox`, or finds it already
/// running. Returns once its install is done; a failed install's output
/// comes back as the error, and its pod is deleted.
pub async fn start_with(
    client: &kube::Client,
    sandbox: &SandboxRef,
    config: &LanguageServerConfig,
    config_version: &str,
) -> Result<Started, String> {
    // A second start waits for the first (SME-35's state model).
    let lock = start_lock(sandbox.conversation_id, &config.name);
    let _starting = lock.lock().await;
    let pods = crate::sandbox::pods_api(client);
    let name = server_pod_name(sandbox.pod_id, &config.name);
    let existing = pods.get_opt(&name).await.map_err(|e| e.to_string())?;
    // Never another database's pod of the same name (SME-115).
    if let Some(pod) = &existing
        && crate::sandbox::ownership(&pod.metadata, &sandbox.instance) != crate::sandbox::Ownership::Ours
    {
        return Err(crate::sandbox::SandboxError::NotOurs {
            name,
            ownership: crate::sandbox::ownership(&pod.metadata, &sandbox.instance),
        }
        .to_string());
    }
    let created = match existing {
        Some(pod) if pod.metadata.deletion_timestamp.is_none() && !has_stopped(&pod) => false,
        stale => {
            // Stopped or stopping: out of the way first.
            if stale.is_some() {
                let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
                wait_gone(&pods, &name).await?;
            }
            pods.create(&kube::api::PostParams::default(), &server_pod_spec(sandbox, config, config_version))
                .await
                .map_err(|e| format!("Couldn't create {name}: {e}"))?;
            true
        }
    };
    if let Err(e) = wait_running(&pods, &name).await {
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        return Err(e);
    }
    match wait_installed(client, &name).await {
        Ok(()) => {
            let ready = json!({"metadata": {"annotations": {READY_ANNOTATION: "true"}}});
            if let Err(e) = pods.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&ready)).await {
                tracing::warn!(pod = %name, error = %e, "couldn't mark a language server pod ready");
            }
            Ok(if created { Started::Started } else { Started::AlreadyRunning })
        }
        Err(e) => {
            let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
            Err(e)
        }
    }
}

/// Each conversation and server's start lock.
fn start_lock(conversation_id: i64, server: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<(i64, String), std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry((conversation_id, server.to_string()))
        .or_default()
        .clone()
}

fn has_stopped(pod: &Pod) -> bool {
    matches!(pod.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Failed" | "Succeeded"))
}

/// Why the server container stopped, when Kubernetes says (`OOMKilled`).
fn stop_reason(pod: &Pod) -> Option<String> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .first()?
        .state
        .as_ref()?
        .terminated
        .as_ref()?
        .reason
        .clone()
}

async fn wait_gone(pods: &kube::Api<Pod>, name: &str) -> Result<(), String> {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while pods.get_opt(name).await.ok().flatten().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
    .await
    .map_err(|_| format!("{name} is still stopping; try again in a moment"))
}

/// Waits for the pod to run, failing early on what won't fix itself (an
/// image that can't be pulled).
async fn wait_running(pods: &kube::Api<Pod>, name: &str) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        let pod = pods.get(name).await.map_err(|e| e.to_string())?;
        let status = pod.status.as_ref();
        match status.and_then(|s| s.phase.as_deref()) {
            Some("Running") => return Ok(()),
            Some("Failed" | "Succeeded") => {
                return Err(format!("{name} stopped before it started ({})", stop_reason(&pod).unwrap_or_default()));
            }
            _ => {}
        }
        let waiting = status
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|c| c.first())
            .and_then(|c| c.state.as_ref())
            .and_then(|s| s.waiting.as_ref());
        if let Some(waiting) = waiting
            && matches!(waiting.reason.as_deref(), Some("ErrImagePull" | "ImagePullBackOff" | "InvalidImageName"))
        {
            return Err(format!(
                "Couldn't pull the server's image: {}",
                waiting.message.clone().unwrap_or_default()
            ));
        }
        if started.elapsed() > START_TIMEOUT {
            return Err(format!("{name} didn't start within {} minutes", START_TIMEOUT.as_secs() / 60));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Waits for the pod's install to finish; a failure comes back with its
/// output.
async fn wait_installed(client: &kube::Client, name: &str) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        if let Some(rc) = install_rc(client, name).await {
            if rc == "0" {
                return Ok(());
            }
            let log = crate::sandbox::exec_with(client, name, "server", &["tail", "-c", "4000", INSTALL_LOG], None)
                .await
                .map(|r| r.stdout)
                .unwrap_or_default();
            return Err(format!("The install failed (exit {rc}):\n{}", log.trim_end()));
        }
        if started.elapsed() > INSTALL_TIMEOUT {
            return Err(format!("The install didn't finish within {} minutes", INSTALL_TIMEOUT.as_secs() / 60));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// The install's exit code, once it has finished.
async fn install_rc(client: &kube::Client, name: &str) -> Option<String> {
    let result = crate::sandbox::exec_with(client, name, "server", &["cat", INSTALL_RC], None).await.ok()?;
    let rc = result.stdout.trim();
    (result.exit_code == 0 && !rc.is_empty()).then(|| rc.to_string())
}

/// The server pods working for `conversation_id`.
/// Only this database's (`instance`, SME-115).
pub async fn list_with(client: &kube::Client, conversation_id: i64, instance: &str) -> Result<Vec<ServerPod>, String> {
    let pods = crate::sandbox::pods_api(client);
    let selector = kube::api::ListParams::default()
        .labels(&format!("{LSP_OF_LABEL}={conversation_id},{}={instance}", crate::sandbox::INSTANCE_LABEL));
    let mut servers = Vec::new();
    for pod in pods.list(&selector).await.map_err(|e| e.to_string())? {
        if pod.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let Some(pod_name) = pod.metadata.name.clone() else { continue };
        let label = |key: &str| pod.metadata.labels.as_ref().and_then(|l| l.get(key)).cloned().unwrap_or_default();
        let state = match pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
            Some("Failed" | "Succeeded") => ServerState::Stopped(stop_reason(&pod)),
            Some("Running") => match install_rc(client, &pod_name).await.as_deref() {
                Some("0") => ServerState::Ready,
                _ => ServerState::Installing,
            },
            _ => ServerState::Installing,
        };
        servers.push(ServerPod {
            name: label(LSP_SERVER_LABEL),
            pod_name,
            state,
            config_version: pod
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(CONFIG_VERSION_ANNOTATION))
                .cloned()
                .unwrap_or_default(),
            memory_limit: pod
                .spec
                .as_ref()
                .and_then(|spec| spec.containers.first())
                .and_then(|c| c.resources.as_ref()?.limits.as_ref()?.get("memory"))
                .map(|q| q.0.clone()),
        });
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(servers)
}

/// Deletes every pod of server `name`, in every conversation: its config
/// was deleted or disabled. Only this database's (`instance`, SME-115):
/// another smelt sharing the namespace has its own config.
pub async fn stop_everywhere_with(client: &kube::Client, name: &str, instance: &str) -> Result<(), String> {
    // One by one: smelt's role can delete pods, not collections of them.
    let pods = crate::sandbox::pods_api(client);
    let selector = kube::api::ListParams::default()
        .labels(&format!("{LSP_SERVER_LABEL}={name},{}={instance}", crate::sandbox::INSTANCE_LABEL));
    for pod in pods.list(&selector).await.map_err(|e| e.to_string())? {
        if let Some(pod_name) = pod.metadata.name {
            pods.delete(&pod_name, &crate::sandbox::pod_delete_params()).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Deletes server `name`'s pod for `conversation_id` (to restart it with
/// newer settings).
pub async fn stop_everywhere_in(
    client: &kube::Client,
    conversation_id: i64,
    name: &str,
    instance: &str,
) -> Result<(), String> {
    let pods = crate::sandbox::pods_api(client);
    let selector = kube::api::ListParams::default().labels(&format!(
        "{LSP_SERVER_LABEL}={name},{LSP_OF_LABEL}={conversation_id},{}={instance}",
        crate::sandbox::INSTANCE_LABEL
    ));
    for pod in pods.list(&selector).await.map_err(|e| e.to_string())? {
        if let Some(pod_name) = pod.metadata.name {
            pods.delete(&pod_name, &crate::sandbox::pod_delete_params()).await.map_err(|e| e.to_string())?;
            wait_gone(&pods, &pod_name).await?;
        }
    }
    Ok(())
}

/// A server pod that stopped on its own, for `watch_pods` to tell the
/// model: its conversation, the server's name, and a notice.
#[derive(Debug, PartialEq)]
pub struct ServerStopped {
    pub conversation_id: i64,
    pub notice: String,
}

/// `watch_pods`' bookkeeping for server pods: remembers in `seen` each pod
/// (by uid) that stopped, and returns the stop to report the first time.
/// A pod being deleted (stopped by smelt, or going with its sandbox) isn't
/// reported, and neither is one already stopped when first listed
/// (`initial`, after smelt starts or the watch reconnects).
pub fn note_server_stop(seen: &mut std::collections::HashSet<String>, pod: &Pod, initial: bool) -> Option<ServerStopped> {
    let labels = pod.metadata.labels.as_ref()?;
    let conversation_id = labels.get(LSP_OF_LABEL)?.parse().ok()?;
    let server = labels.get(LSP_SERVER_LABEL)?;
    let phase = pod.status.as_ref()?.phase.as_deref()?;
    if pod.metadata.deletion_timestamp.is_some() || !matches!(phase, "Failed" | "Succeeded") {
        return None;
    }
    // One whose install never finished: its start already said why.
    if pod.metadata.annotations.as_ref().and_then(|a| a.get(READY_ANNOTATION)).is_none() {
        return None;
    }
    if !seen.insert(pod.metadata.uid.clone()?) || initial {
        return None;
    }
    let reason = stop_reason(pod);
    let why = match reason.as_deref() {
        Some("OOMKilled") => " (OOMKilled: it ran out of memory; the user can raise its memory limit on the \
                             Language servers page)"
            .to_string(),
        Some(reason) => format!(" ({reason})"),
        None => String::new(),
    };
    Some(ServerStopped {
        conversation_id,
        notice: format!(
            "Language server {server} stopped{why}. The sandbox is unaffected. start_language_server \
             starts it again."
        ),
    })
}

/// A running server's stdin and stdout, over `pods/exec` into its pod.
pub struct ServerIo {
    pub stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
}

/// Runs `config`'s command in its pod and connects to it.
pub async fn open_stdio(client: &kube::Client, pod_name: &str, config: &LanguageServerConfig) -> Result<ServerIo, String> {
    let mut argv = vec![config.command.clone()];
    argv.extend(config.args.iter().cloned());
    let mut attached = crate::sandbox::pods_api(client)
        .exec(
            pod_name,
            argv,
            &kube::api::AttachParams::default().container("server").stdin(true).stdout(true).stderr(false),
        )
        .await
        .map_err(|e| format!("Couldn't start {} in {pod_name}: {e}", config.command))?;
    let stdin = attached.stdin().ok_or("the server's stdin wasn't attached")?;
    let stdout = attached.stdout().ok_or("the server's stdout wasn't attached")?;
    // The exec lives as long as this task: until the server exits or its
    // pod goes, which ends the stdout stream too.
    tokio::spawn(async move {
        let _ = attached.join().await;
    });
    Ok(ServerIo { stdin: Box::new(stdin), stdout: Box::new(stdout) })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn sandbox() -> SandboxRef {
        SandboxRef {
            conversation_id: 42,
            pod_id: 7,
            pod_name: "sandbox-7".to_string(),
            pod_uid: "uid-7".to_string(),
            node: "node-a".to_string(),
            instance: "instance-a".to_string(),
        }
    }

    fn config() -> LanguageServerConfig {
        LanguageServerConfig {
            name: "rust-analyzer".to_string(),
            image: "rust:1".to_string(),
            install_command: "rustup component add rust-analyzer".to_string(),
            command: "rust-analyzer".to_string(),
            env: [("RA_LOG".to_string(), "info".to_string())].into(),
            file_types: [("rs".to_string(), "rust".to_string())].into(),
            memory_limit: "3Gi".to_string(),
            cpu_limit: "2".to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_a_server_pod_sits_next_to_its_sandbox_and_goes_with_it() {
        let pod = server_pod_spec(&sandbox(), &config(), "2026-09-28T08:00:00");
        let meta = &pod.metadata;
        assert_eq!(meta.name.as_deref(), Some("lsp-7-rust-analyzer"));
        let labels = meta.labels.as_ref().expect("labels");
        assert_eq!(labels.get(LSP_OF_LABEL).map(String::as_str), Some("42"));
        assert_eq!(labels.get(LSP_SERVER_LABEL).map(String::as_str), Some("rust-analyzer"));
        assert_eq!(labels.get(LSP_POD_LABEL).map(String::as_str), Some("7"));
        assert!(!labels.contains_key("smelt/conversation"), "the sandbox pod's own label would make create_pod wait on it");
        assert_eq!(labels.get(crate::sandbox::INSTANCE_LABEL).map(String::as_str), Some("instance-a"), "SME-115");
        assert_eq!(
            meta.annotations.as_ref().and_then(|a| a.get(CONFIG_VERSION_ANNOTATION)).map(String::as_str),
            Some("2026-09-28T08:00:00")
        );
        let owner = &meta.owner_references.as_ref().expect("owner")[0];
        assert_eq!((owner.kind.as_str(), owner.name.as_str(), owner.uid.as_str()), ("Pod", "sandbox-7", "uid-7"));

        let spec = pod.spec.as_ref().expect("spec");
        assert_eq!(spec.node_name.as_deref(), Some("node-a"), "local-path claims can only be shared on one node");
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        let security = spec.security_context.as_ref().expect("security context");
        assert_eq!((security.run_as_user, security.run_as_group), (Some(1000), Some(1000)));
        let volume = &spec.volumes.as_ref().expect("volumes")[0];
        assert_eq!(
            volume.persistent_volume_claim.as_ref().map(|c| c.claim_name.as_str()),
            Some("sandbox-workspace-42")
        );

        let container = &spec.containers[0];
        assert_eq!(container.image.as_deref(), Some("rust:1"));
        assert_eq!(container.volume_mounts.as_ref().expect("mounts")[0].mount_path, "/workspace");
        let env: Vec<(String, String)> = container
            .env
            .as_ref()
            .expect("env")
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        assert!(env.contains(&("HOME".to_string(), POD_HOME.to_string())));
        assert!(env.contains(&("RA_LOG".to_string(), "info".to_string())));
        let resources = container.resources.as_ref().expect("resources");
        let limits = resources.limits.as_ref().expect("limits");
        assert_eq!(limits.get("memory").map(|q| q.0.as_str()), Some("3Gi"));
        assert_eq!(limits.get("cpu").map(|q| q.0.as_str()), Some("2"));
        // Small requests: with only the namespace's defaults the spike's
        // pods couldn't be scheduled.
        assert_eq!(resources.requests.as_ref().and_then(|r| r.get("memory")).map(|q| q.0.as_str()), Some("64Mi"));
        let script = container.command.as_ref().expect("command").join(" ");
        assert!(script.contains("rustup component add rust-analyzer"), "{script}");
        assert!(script.contains(INSTALL_RC) && script.contains("trap 'exit 0' TERM"), "{script}");
    }

    fn server_pod(uid: &str, phase: &str, reason: Option<&str>, deleting: bool) -> Pod {
        let mut pod = unready_server_pod(uid, phase, reason, deleting);
        pod.metadata.annotations = Some([(READY_ANNOTATION.to_string(), "true".to_string())].into());
        pod
    }

    /// A server pod whose install never finished.
    fn unready_server_pod(uid: &str, phase: &str, reason: Option<&str>, deleting: bool) -> Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": "lsp-7-rust-analyzer",
                "uid": uid,
                "labels": {LSP_OF_LABEL: "42", LSP_SERVER_LABEL: "rust-analyzer", LSP_POD_LABEL: "7"},
                "deletionTimestamp": if deleting { Some("2026-09-28T00:00:00Z") } else { None },
            },
            "status": {
                "phase": phase,
                "containerStatuses": [{
                    "name": "server", "image": "rust:1", "imageID": "", "ready": false, "restartCount": 0,
                    "state": {"terminated": {"exitCode": 137, "reason": reason}},
                }],
            },
        }))
        .expect("a pod")
    }

    #[test]
    fn test_a_server_pod_that_stops_is_reported_once() {
        let mut seen = std::collections::HashSet::new();
        assert_eq!(note_server_stop(&mut seen, &server_pod("a", "Running", None, false), false), None);
        let stopped = note_server_stop(&mut seen, &server_pod("a", "Failed", Some("OOMKilled"), false), false).expect("reported");
        assert_eq!(stopped.conversation_id, 42);
        assert!(stopped.notice.contains("rust-analyzer") && stopped.notice.contains("OOMKilled"), "{}", stopped.notice);
        assert!(stopped.notice.contains("memory limit"), "an OOM says what to change: {}", stopped.notice);
        assert!(stopped.notice.contains("start_language_server"), "{}", stopped.notice);
        assert_eq!(note_server_stop(&mut seen, &server_pod("a", "Failed", Some("OOMKilled"), false), false), None);
        // A new pod for the same server is reported again.
        assert!(note_server_stop(&mut seen, &server_pod("b", "Failed", Some("Error"), false), false).is_some());
    }

    #[test]
    fn test_a_server_pod_stopped_by_smelt_or_before_is_not_reported() {
        let mut seen = std::collections::HashSet::new();
        assert_eq!(note_server_stop(&mut seen, &server_pod("a", "Failed", Some("Error"), true), false), None);
        assert_eq!(note_server_stop(&mut seen, &server_pod("b", "Failed", Some("Error"), false), true), None);
        assert_eq!(note_server_stop(&mut seen, &server_pod("b", "Failed", Some("Error"), false), false), None);
        // One that never got through its install: its start already said
        // what went wrong.
        assert_eq!(note_server_stop(&mut seen, &unready_server_pod("c", "Failed", Some("StartError"), false), false), None);
    }

    #[test]
    fn test_an_empty_install_still_records_success() {
        assert!(pod_script("  ").contains("( true )"));
    }

    /// Real-cluster tests: a stand-in sandbox pod (the real image, idle)
    /// and its conversation's workspace claim, with `cat` as the "server".
    pub(crate) mod cluster {
        use super::super::*;

        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        use kube::api::{DeleteParams, PostParams};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        pub(crate) async fn client() -> kube::Client {
            let _ = rustls::crypto::ring::default_provider().install_default();
            kube::Client::try_default().await.expect("KUBECONFIG must point at a reachable cluster")
        }

        /// A conversation id no other test run is using: the time (so runs
        /// don't meet leftovers from earlier ones) plus a count (so tests
        /// started at once in this run never share one).
        pub(crate) fn unique() -> i64 {
            static COUNT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
            3_000_000_000 + (seconds % 1_000_000) * 1_000 + COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % 1_000
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
    }
}
