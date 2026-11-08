//! kube exec into a pod's containers: `PodShell` for a conversation's
//! sandbox container, and `Sandbox`, a pod the real-cluster tests own.

use super::*;

pub struct Sandbox {
    pub(super) pod_name: String,
    /// The uid of the pod this `Sandbox` made or reused: deleting it is
    /// held to that pod, never one that took its name since.
    pub(super) uid: Option<String>,
    // Only read by the test-only `exec` and `shell` below; production
    // code reaches a pod's sandbox container through `PodShell` or the
    // sandbox agent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) client: kube::Client,
    /// The instance the pod was made for, which `shell` checks it against.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) instance: db::SmeltInstance,
    pub(super) cleanup_tx: mpsc::UnboundedSender<(String, Option<String>)>,
}

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    /// -1 when the command never ran or its status never came; `refused`
    /// tells those two apart.
    pub exit_code: i32,
    /// The API server sent a status that wasn't an exit (the runtime
    /// refused the command, SME-452): it certainly never ran, where a
    /// status that never came (-1, not refused) leaves it unknown.
    pub refused: bool,
}

impl Sandbox {
    /// kube exec in the pod's sandbox container, for the real-cluster
    /// tests to check a raw property of the pod itself (its mounted
    /// volumes, its non-root user, ...) independent of the sandbox agent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn exec(&self, command: &[&str]) -> Result<ExecResult, SandboxError> {
        self.exec_in("sandbox", command).await
    }

    /// `exec` in a named container of the pod — `"docker"` for the Docker
    /// sidecar (SME-33).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn exec_in(
        &self,
        container: &str,
        command: &[&str],
    ) -> Result<ExecResult, SandboxError> {
        exec_with(&self.client, &self.pod_name, container, command, None).await
    }

    /// A `PodShell` into this pod, with the test's own client.
    #[cfg(test)]
    pub fn shell(&self) -> PodShell {
        PodShell::new(self.client.clone(), self.pod_name.clone(), self.instance.clone())
    }
}

/// Commands run in a pod's sandbox container through kube exec, which
/// sees its exit code and can feed it stdin: how git clones, reads a
/// checkout's `AGENTS.md` files and installs keys. The model's own
/// commands go through the sandbox agent's terminals instead.
///
/// Every `run` first reads the pod and refuses one that isn't this
/// database's (SME-180), as the agent dial does: never a cached answer,
/// since a pod's name can be taken by another database's pod once ours
/// is gone.
pub struct PodShell {
    client: kube::Client,
    pod_name: String,
    instance: db::SmeltInstance,
}

impl PodShell {
    /// A shell into the Kubernetes pod named `pod_name`, through `client`
    /// (a real-cluster test brings its own), for a server of `instance`.
    pub fn new(client: kube::Client, pod_name: String, instance: db::SmeltInstance) -> Self {
        PodShell { client, pod_name, instance }
    }

    /// A shell into smelt's pod `pod_id`.
    pub async fn for_pod(pool: &PgPool, pod_id: i64) -> Result<Self, SandboxError> {
        let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
        Ok(Self::new(kube_client()?, pod_name(pod_id), instance))
    }

    /// A shell into `conversation_id`'s live pod.
    pub async fn for_conversation(pool: &PgPool, conversation_id: i64) -> Result<Self, TerminalError> {
        Ok(Self::for_pod(pool, live_pod_id(pool, conversation_id).await?).await?)
    }

    /// Runs `command` in the sandbox container, with `stdin` written and
    /// closed when given (see `exec_with`), once the pod reads as this
    /// database's: another database's is `NotOurs`, and nothing runs.
    pub async fn run(&self, command: &[&str], stdin: Option<&[u8]>) -> Result<ExecResult, SandboxError> {
        refuse_foreign(&pods_api(&self.client), &self.pod_name, &self.instance).await?;
        #[cfg(test)]
        if let Some(stall) = stall_hook::take(&self.pod_name, command) {
            return stall_hook::stall(&self.client, &self.pod_name, command, stdin, stall).await;
        }
        exec_with(&self.client, &self.pod_name, "sandbox", command, stdin).await
    }
}

#[cfg(test)]
pub(crate) mod stall_hook;

/// kube exec in `pod_name`'s `container`, unchecked: the caller makes
/// sure the pod is this database's first (`PodShell::run` reads it before
/// every exec; the language servers' callers check where each request
/// starts). `stdin`, when given, is written and then closed, so a command
/// like `cat > file` sees its end. kube closes just the stdin stream on a
/// v5 connection (k3s has it); an older server closes the whole
/// connection and the exit code comes back missing, which callers treat
/// as a failure.
pub(crate) async fn exec_with(
    client: &kube::Client,
    pod_name: &str,
    container: &str,
    command: &[&str],
    stdin: Option<&[u8]>,
) -> Result<ExecResult, SandboxError> {
    let pods = pods_api(client);
    let mut attached = pods
        .exec(
            pod_name,
            command.iter().copied(),
            &AttachParams::default()
                .container(container)
                .stdin(stdin.is_some()),
        )
        .await?;
    if let Some(input) = stdin {
        use tokio::io::AsyncWriteExt;
        let mut writer = require_stream(attached.stdin(), "the exec's stdin")?;
        writer.write_all(input).await.map_err(SandboxError::Io)?;
        writer.shutdown().await.map_err(SandboxError::Io)?;
        drop(writer);
    }

    let mut stdout_reader = require_stream(attached.stdout(), "the exec's stdout")?;
    let mut stderr_reader = require_stream(attached.stderr(), "the exec's stderr")?;
    // Bytes, decoded leniently: a file's contents need not be UTF-8, and
    // a `head -c` cut can split a character (SME-32).
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (stdout_res, stderr_res) = tokio::join!(
        stdout_reader.read_to_end(&mut stdout),
        stderr_reader.read_to_end(&mut stderr),
    );
    stdout_res.map_err(SandboxError::Io)?;
    stderr_res.map_err(SandboxError::Io)?;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();

    let status = attached.take_status();
    attached.join().await.ok();
    let status = match status {
        Some(fut) => fut.await,
        None => None,
    };

    let exit_code = extract_exit_code(status.clone());
    // A command the runtime never ran says why, for callers that show
    // stderr (SME-452).
    let mut refused = false;
    if exit_code == -1
        && let Some(status) = status
    {
        refused = true;
        append_refusal(&mut stderr, &status);
    }
    Ok(ExecResult { stdout, stderr, exit_code, refused })
}

/// `stderr` with why the runtime refused the command (`status`) after it,
/// on a line of its own (SME-452 round 1).
pub(super) fn append_refusal(
    stderr: &mut String,
    status: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Status,
) {
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(&format!(
        "exec failed: {}: {}",
        status.reason.as_deref().unwrap_or_default(),
        status.message.as_deref().unwrap_or_default()
    ));
}

/// On success the exec protocol's terminal `Status` is `Success`, with no
/// exit code at all; on a non-zero exit it's a `StatusCause` with
/// `reason == "ExitCode"` and the code itself, as a string, in `message`.
/// Verified against a real cluster, not assumed — see SME-7's plan.
///
/// Anything else is a failure (-1), not a success: no status at all (the
/// connection ended before the API server said how the command did), or
/// another status, such as the kubelet's `InternalError` when the
/// container runtime couldn't run the command (captured live, SME-452).
pub(super) fn extract_exit_code(
    status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>,
) -> i32 {
    let Some(status) = status else {
        return -1;
    };
    let code = status
        .details
        .as_ref()
        .and_then(|d| d.causes.as_ref())
        .into_iter()
        .flatten()
        .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
        .and_then(|cause| cause.message.as_deref())
        .and_then(|message| message.parse().ok());
    match code {
        Some(code) => code,
        None if status.status.as_deref() == Some("Success") => 0,
        None => -1,
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        tracing::info!(pod = %self.pod_name, "Sandbox dropped, queuing cleanup");
        let _ = self.cleanup_tx.send((self.pod_name.clone(), self.uid.clone()));
    }
}
