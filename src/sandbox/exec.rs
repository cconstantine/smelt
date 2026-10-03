//! kube exec into a pod's containers: `PodShell` for a conversation's
//! sandbox container, and `Sandbox`, a pod the real-cluster tests own.

use super::*;

pub struct Sandbox {
    pub(super) pod_name: String,
    // Only read by the test-only `exec` and `shell` below; production
    // code reaches a pod's sandbox container through `PodShell` or the
    // sandbox agent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) client: kube::Client,
    pub(super) cleanup_tx: mpsc::UnboundedSender<String>,
}

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
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
        PodShell::new(self.client.clone(), self.pod_name.clone())
    }
}

/// Commands run in a pod's sandbox container through kube exec, which
/// sees its exit code and can feed it stdin: how git clones, reads a
/// checkout's `AGENTS.md` files and installs keys. The model's own
/// commands go through the sandbox agent's terminals instead.
pub struct PodShell {
    client: kube::Client,
    pod_name: String,
}

impl PodShell {
    /// A shell into the Kubernetes pod named `pod_name`, through `client`
    /// (a real-cluster test brings its own).
    pub fn new(client: kube::Client, pod_name: String) -> Self {
        PodShell { client, pod_name }
    }

    /// A shell into smelt's pod `pod_id`.
    pub fn for_pod(pod_id: i64) -> Result<Self, SandboxError> {
        Ok(Self::new(kube_client()?, pod_name(pod_id)))
    }

    /// A shell into `conversation_id`'s live pod.
    pub async fn for_conversation(pool: &PgPool, conversation_id: i64) -> Result<Self, TerminalError> {
        Ok(Self::for_pod(live_pod_id(pool, conversation_id).await?)?)
    }

    /// Runs `command` in the sandbox container, with `stdin` written and
    /// closed when given (see `exec_with`).
    pub async fn run(&self, command: &[&str], stdin: Option<&[u8]>) -> Result<ExecResult, SandboxError> {
        exec_with(&self.client, &self.pod_name, "sandbox", command, stdin).await
    }
}

/// kube exec in `pod_name`'s `container`. `stdin`, when given, is written
/// and then closed, so a command like `cat > file` sees its end. kube
/// closes just the stdin stream on a v5 connection (k3s has it); an older
/// server closes the whole connection and the exit code comes back
/// missing, which callers treat as a failure.
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
        let mut writer = attached.stdin().expect("stdin requested above");
        writer.write_all(input).await.map_err(SandboxError::Io)?;
        writer.shutdown().await.map_err(SandboxError::Io)?;
        drop(writer);
    }

    let mut stdout_reader = attached
        .stdout()
        .expect("stdout requested by AttachParams::default()");
    let mut stderr_reader = attached
        .stderr()
        .expect("stderr requested by AttachParams::default()");
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
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    let status = attached.take_status();
    attached.join().await.ok();
    let status = match status {
        Some(fut) => fut.await,
        None => None,
    };

    Ok(ExecResult {
        stdout,
        stderr,
        exit_code: extract_exit_code(status),
    })
}

/// On success the exec protocol's terminal `Status` carries no exit code at
/// all (implying 0); on a non-zero exit it's a `StatusCause` with
/// `reason == "ExitCode"` and the code itself, as a string, in `message`.
/// Verified against a real cluster, not assumed — see SME-7's plan.
///
/// No status at all means the connection ended before the API server said
/// how the command did, so it counts as a failure (-1), not a success.
pub(super) fn extract_exit_code(
    status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>,
) -> i32 {
    let Some(status) = status else {
        return -1;
    };
    status
        .details
        .and_then(|d| d.causes)
        .into_iter()
        .flatten()
        .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
        .and_then(|cause| cause.message)
        .and_then(|message| message.parse().ok())
        .unwrap_or(0)
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        tracing::info!(pod = %self.pod_name, "Sandbox dropped, queuing cleanup");
        let _ = self.cleanup_tx.send(self.pod_name.clone());
    }
}
