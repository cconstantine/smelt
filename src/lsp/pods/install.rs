//! A server pod's install, waited on (SME-452's split of `pods.rs`): the
//! record it's read from, the bound, and what a timed-out install says.

use super::*;

pub(super) const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// What `wait_installed` makes of each read of its pod.
pub(super) struct InstallWatch<'a> {
    name: &'a str,
    uid: &'a str,
    /// The last read's error, when the last read failed: a failed read
    /// isn't a death (the API server may be busy), and goes in the
    /// timeout's message only while it's still the latest news.
    read_error: Option<String>,
    /// How many reads of the install's record failed, and the last one's
    /// error (SME-452): a record that couldn't be read for the whole
    /// bound reads as an install still running.
    rc_read_failures: u32,
    rc_read_error: Option<String>,
}

impl<'a> InstallWatch<'a> {
    pub(super) fn new(name: &'a str, uid: &'a str) -> Self {
        InstallWatch { name, uid, read_error: None, rc_read_failures: 0, rc_read_error: None }
    }

    /// A read of the install's record that didn't get through.
    pub(super) fn rc_read_failed(&mut self, error: String) {
        self.rc_read_failures += 1;
        self.rc_read_error = Some(error);
    }

    /// The error to end the wait with, if the pod as read says so.
    pub(super) fn read(&mut self, pod: Result<Option<Pod>, String>) -> Option<String> {
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

    /// The error for an install still running after `bound`, with its
    /// output so far (`log`, or why it couldn't be read).
    pub(super) fn timed_out(&self, bound: std::time::Duration, log: Result<String, String>) -> String {
        let within = match bound.as_secs() / 60 {
            0 => format!("{} s", bound.as_secs()),
            1 => "1 minute".to_string(),
            minutes => format!("{minutes} minutes"),
        };
        let record = match (self.rc_read_failures, &self.rc_read_error) {
            (0, _) | (_, None) => String::new(),
            (n, Some(e)) => format!("; its record couldn't be read {n} times (last: {e})"),
        };
        let output = match log {
            // npm writes nothing to a file until it ends (SME-452).
            Ok(log) if log.trim().is_empty() => ". It wrote no output.".to_string(),
            Ok(log) => format!(". Its output so far:\n{}", log.trim_end()),
            Err(e) => format!(" (its output couldn't be read: {e})"),
        };
        format!(
            "The install didn't finish within {within}{}{record}{output}",
            self.read_error.as_ref().map(|e| format!(" (last error reading the pod: {e})")).unwrap_or_default()
        )
    }
}

/// Waits for the pod's install to finish; a failure comes back with its
/// output. Pod `uid` stopping or going ends the wait at once, saying why
/// (SME-171): its install can't finish then, and every exec into it
/// fails, which reads as "not finished yet".
pub(super) async fn wait_installed(client: &kube::Client, pods: &kube::Api<Pod>, name: &str, uid: &str) -> Result<(), String> {
    wait_installed_within(client, pods, name, uid, INSTALL_TIMEOUT).await
}

/// The install's output so far (its last 4,000 bytes).
async fn install_log(client: &kube::Client, name: &str) -> Result<String, String> {
    log_of(
        crate::sandbox::exec_with(client, name, "server", &["tail", "-c", "4000", INSTALL_LOG], None)
            .await
            .map_err(|e| e.to_string())?,
    )
}

/// What a `tail` of the install's log says. A `tail` that failed (no log
/// at all, or an exec the runtime refused) is an error saying why, not
/// an empty log (SME-452).
pub(super) fn log_of(result: crate::sandbox::ExecResult) -> Result<String, String> {
    match result.exit_code {
        0 => Ok(result.stdout),
        code => Err(format!("tail exited {code}: {}", result.stderr.trim())),
    }
}

/// How long a timed-out install's log read may take (SME-452 review 1):
/// an exec has no bound of its own, and the message mustn't hold the
/// call past the install's bound.
pub(super) const LOG_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `read`, given up on after `limit`.
pub(super) async fn bounded_read(
    read: impl std::future::Future<Output = Result<String, String>>,
    limit: std::time::Duration,
) -> Result<String, String> {
    tokio::time::timeout(limit, read)
        .await
        .unwrap_or_else(|_| Err(format!("the read didn't answer within {} s", limit.as_secs())))
}

/// `wait_installed` with its bound: only a test passes another than
/// `INSTALL_TIMEOUT`, to reach the timeout without waiting it out. A
/// timed-out install says what it was doing (SME-452): its output so
/// far, and its record's failed reads, since its pod is deleted after.
pub(super) async fn wait_installed_within(
    client: &kube::Client,
    pods: &kube::Api<Pod>,
    name: &str,
    uid: &str,
    bound: std::time::Duration,
) -> Result<(), String> {
    #[cfg(test)]
    test_hooks::reached_install_wait(name);
    let started = std::time::Instant::now();
    let mut watch = InstallWatch::new(name, uid);
    loop {
        if let Some(error) = watch.read(pods.get_opt(name).await.map_err(|e| e.to_string())) {
            return Err(error);
        }
        match install_rc(client, name).await {
            Ok(Some(rc)) => {
                if rc == "0" {
                    return Ok(());
                }
                let log = install_log(client, name).await.unwrap_or_default();
                return Err(format!("The install failed (exit {rc}):\n{}", log.trim_end()));
            }
            // Ok(None): the install is still running. Err: this read didn't
            // get through — say so and ask again next second; the timeout
            // below is the bound.
            Ok(None) => {}
            Err(e) => {
                tracing::info!(pod = name, error = %e, "couldn't read the install's exit code");
                watch.rc_read_failed(e);
            }
        }
        if started.elapsed() > bound {
            return Err(watch.timed_out(bound, bounded_read(install_log(client, name), LOG_READ_TIMEOUT).await));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// The install's exit code, once it has finished. `Err` means the check
/// itself couldn't run (the exec failed): the install's state is then
/// unknown — *not* "not finished" — and no caller may report liveness from
/// it the way it once did (SME-426: one dropped exec read a Ready server
/// as not running).
pub(super) async fn install_rc(client: &kube::Client, name: &str) -> Result<Option<String>, String> {
    rc_of(crate::sandbox::exec_with(client, name, "server", &["cat", INSTALL_RC], None).await.map_err(|e| e.to_string())?)
}

/// What a `cat` of the install's record says. An exec that never ran
/// (-1: no status, or the runtime refused it) is a failed read, never
/// "still running" (SME-452): a pod whose execs were all refused once
/// timed out with nothing to say, even when its install had finished.
pub(super) fn rc_of(result: crate::sandbox::ExecResult) -> Result<Option<String>, String> {
    if result.exit_code == -1 {
        let why = result.stderr.trim();
        return Err(if why.is_empty() { "the exec ended without a status".to_string() } else { why.to_string() });
    }
    let rc = result.stdout.trim();
    Ok((result.exit_code == 0 && !rc.is_empty()).then(|| rc.to_string()))
}
