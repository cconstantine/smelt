//! kube exec into a pod's containers, and `Sandbox`, a pod the real-cluster tests own.

use super::*;

pub struct Sandbox {
    pub(super) pod_name: String,
    // Only read by `exec` below, which is itself real-cluster-test-only
    // (see its own cfg) — production code talks to the sandbox through
    // `sandbox_agent`'s WebSocket protocol instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) client: kube::Client,
    pub(super) cleanup_tx: mpsc::UnboundedSender<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl Sandbox {
    /// Lower-level kube-exec path used only by the real-cluster tests below
    /// to check a raw property of the pod itself (its mounted volumes, its
    /// non-root user, ...) independent of `sandbox_agent`'s own WebSocket
    /// terminal protocol, which is what production code actually uses.
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

/// Writes git's files (`git::pod_git_files`) into a pod's sandbox
/// container. The keys directory is replaced wholesale, so a key deleted
/// since the last install goes too.
pub async fn install_git_files(
    client: &kube::Client,
    pod_name: &str,
    files: &[crate::git::PodFile],
) -> Result<(), SandboxError> {
    // Never emptied: a clone or push running during a reinstall must
    // still find its key. Keys are written over in place, and only the
    // ones no longer in `files` removed afterwards.
    let keys_dir = format!("{}/keys", crate::git::POD_GIT_DIR);
    let made = exec_with(
        client,
        pod_name,
        "sandbox",
        &["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", &keys_dir],
        None,
    )
    .await?;
    if made.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't make {keys_dir}: {}",
            made.stderr.trim()
        )));
    }
    for file in files {
        // Written beside the target and renamed over it, so ssh or git
        // never reads half a file; umask keeps a key private from its
        // first byte.
        let mode = format!("{:o}", file.mode);
        let written = exec_with(
            client,
            pod_name,
            "sandbox",
            &[
                "sh",
                "-c",
                r#"umask 077 && cat > "$1.new" && chmod "$2" "$1.new" && mv "$1.new" "$1""#,
                "sh",
                &file.path,
                &mode,
            ],
            Some(file.content.as_bytes()),
        )
        .await?;
        if written.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't write {}: {}",
                file.path,
                written.stderr.trim()
            )));
        }
    }
    let keep: Vec<&str> = files
        .iter()
        .filter_map(|f| f.path.strip_prefix(&format!("{keys_dir}/")))
        .collect();
    let mut prune = vec![
        "sh",
        "-c",
        r#"cd "$1" && shift && for f in * .[!.]*; do
               [ -e "$f" ] || continue
               keep=; for k in "$@"; do [ "$f" = "$k" ] && keep=1; done
               [ -n "$keep" ] || rm -f -- "$f"
           done"#,
        "sh",
        &keys_dir,
    ];
    prune.extend(keep);
    let pruned = exec_with(client, pod_name, "sandbox", &prune, None).await?;
    if pruned.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't remove deleted keys from {keys_dir}: {}",
            pruned.stderr.trim()
        )));
    }
    Ok(())
}

/// `install_git_files` for a live pod of smelt's own, by id.
pub async fn install_git_files_in_pod(
    pod_id: i64,
    files: &[crate::git::PodFile],
) -> Result<(), SandboxError> {
    install_git_files(&get().client, &pod_name(pod_id), files).await
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
