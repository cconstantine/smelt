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
    let uid = match wait_running(&pods, &name).await {
        Ok(uid) => uid,
        Err(e) => {
            let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
            return Err(e);
        }
    };
    match wait_installed(client, &pods, &name, &uid).await {
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

/// Why the server stopped, when Kubernetes says: the pod's own reason
/// when its node stopped it (`Evicted`), else its container's
/// (`OOMKilled`).
fn stop_reason(pod: &Pod) -> Option<String> {
    let status = pod.status.as_ref()?;
    if let Some(reason) = &status.reason {
        return Some(reason.clone());
    }
    status
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

/// Why a server stopped, for a sentence about it: what an OOM kill
/// means and what to change, or Kubernetes' reason as is.
fn stop_explanation(reason: Option<&str>) -> String {
    match reason {
        Some("OOMKilled") => " (OOMKilled: it ran out of memory; the user can raise its memory limit on the \
                              Language servers page)"
            .to_string(),
        Some(reason) => format!(" ({reason})"),
        None => String::new(),
    }
}

/// What a start waiting on pod `name`'s install (the pod `uid` it saw
/// running) makes of the pod as read now: the error to end the wait
/// with, or `None` when the install may still be running (SME-171).
fn install_interrupted(name: &str, uid: &str, pod: Option<&Pod>) -> Option<String> {
    let deleted = || {
        Some(format!(
            "{name} was deleted during its install (its sandbox went, or the server was stopped or restarted)"
        ))
    };
    let Some(pod) = pod else { return deleted() };
    if pod.metadata.uid.as_deref() != Some(uid) || pod.metadata.deletion_timestamp.is_some() {
        return deleted();
    }
    has_stopped(pod)
        .then(|| format!("{name} stopped during its install{}", stop_explanation(stop_reason(pod).as_deref())))
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
/// image that can't be pulled). Returns the running pod's uid.
async fn wait_running(pods: &kube::Api<Pod>, name: &str) -> Result<String, String> {
    let started = std::time::Instant::now();
    loop {
        let pod = pods.get(name).await.map_err(|e| e.to_string())?;
        let status = pod.status.as_ref();
        match status.and_then(|s| s.phase.as_deref()) {
            Some("Running") => return pod.metadata.uid.clone().ok_or_else(|| format!("{name} is running without a uid")),
            Some("Failed" | "Succeeded") => {
                return Err(format!("{name} stopped before it started{}", stop_explanation(stop_reason(&pod).as_deref())));
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

/// What `wait_installed` makes of each read of its pod.
struct InstallWatch<'a> {
    name: &'a str,
    uid: &'a str,
    /// The last read's error, when the last read failed: a failed read
    /// isn't a death (the API server may be busy), and goes in the
    /// timeout's message only while it's still the latest news.
    read_error: Option<String>,
}

impl InstallWatch<'_> {
    /// The error to end the wait with, if the pod as read says so.
    fn read(&mut self, pod: Result<Option<Pod>, String>) -> Option<String> {
        match pod {
            Ok(pod) => {
                self.read_error = None;
                install_interrupted(self.name, self.uid, pod.as_ref())
            }
            Err(e) => {
                self.read_error = Some(e);
                None
            }
        }
    }

    fn timed_out(&self) -> String {
        format!(
            "The install didn't finish within {} minutes{}",
            INSTALL_TIMEOUT.as_secs() / 60,
            self.read_error.as_ref().map(|e| format!(" (last error reading the pod: {e})")).unwrap_or_default()
        )
    }
}

/// Waits for the pod's install to finish; a failure comes back with its
/// output. Pod `uid` stopping or going ends the wait at once, saying why
/// (SME-171): its install can't finish then, and every exec into it
/// fails, which reads as "not finished yet".
async fn wait_installed(client: &kube::Client, pods: &kube::Api<Pod>, name: &str, uid: &str) -> Result<(), String> {
    #[cfg(test)]
    test_hooks::reached_install_wait(name);
    let started = std::time::Instant::now();
    let mut watch = InstallWatch { name, uid, read_error: None };
    loop {
        if let Some(error) = watch.read(pods.get_opt(name).await.map_err(|e| e.to_string())) {
            return Err(error);
        }
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
            return Err(watch.timed_out());
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
    let why = stop_explanation(stop_reason(pod).as_deref());
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

/// Lets a real-cluster test act once a start is waiting on its install,
/// not still waiting for its pod to run (SME-171).
#[cfg(test)]
pub(crate) mod test_hooks {
    type Waiters = std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>;
    static WAITERS: std::sync::LazyLock<std::sync::Mutex<Waiters>> = std::sync::LazyLock::new(Default::default);

    /// Fires once the start of pod `name` begins waiting on its install.
    pub fn on_install_wait(name: &str) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        WAITERS.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), tx);
        rx
    }

    pub(super) fn reached_install_wait(name: &str) {
        if let Some(tx) = WAITERS.lock().unwrap_or_else(|e| e.into_inner()).remove(name) {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
