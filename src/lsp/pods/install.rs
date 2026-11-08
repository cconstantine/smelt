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
    /// How many reads of the record there were, and the slowest one
    /// (SME-452's recurrence): a slow read shows here, and one that
    /// never answers is given up on at `RC_READ_TIMEOUT` (round 1).
    rc_reads: u32,
    slowest_read: std::time::Duration,
    /// What was still running at the timeout, once read.
    processes: Option<Result<String, String>>,
}

impl<'a> InstallWatch<'a> {
    pub(super) fn new(name: &'a str, uid: &'a str) -> Self {
        InstallWatch {
            name,
            uid,
            read_error: None,
            rc_read_failures: 0,
            rc_read_error: None,
            rc_reads: 0,
            slowest_read: std::time::Duration::ZERO,
            processes: None,
        }
    }

    /// A read of the install's record, got through or not, that took
    /// `took`.
    pub(super) fn rc_read_took(&mut self, took: std::time::Duration) {
        self.rc_reads += 1;
        self.slowest_read = self.slowest_read.max(took);
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
        let reads = match self.rc_reads {
            0 => String::new(),
            n => format!(
                "; {n} record reads, the slowest {} s",
                // f64's Display drops a whole number's ".0": "2 s", "2.345 s".
                self.slowest_read.as_millis() as f64 / 1000.0
            ),
        };
        let output = match log {
            // npm writes nothing to a file until it ends (SME-452).
            Ok(log) if log.trim().is_empty() => ". It wrote no output.".to_string(),
            Ok(log) => format!(". Its output so far:\n{}", log.trim_end()),
            Err(e) => format!(" (its output couldn't be read: {e})"),
        };
        let processes = match &self.processes {
            None => String::new(),
            Some(Ok(listing)) => format!("\nStill running then:\n{}", listing.trim_end()),
            Some(Err(e)) => format!("\n(its processes couldn't be read: {e})"),
        };
        format!(
            "The install didn't finish within {within}{}{record}{reads}{output}{processes}",
            self.read_error.as_ref().map(|e| format!(" (last error reading the pod: {e})")).unwrap_or_default()
        )
    }

    /// What was running in the pod when the install ran out its bound
    /// (`PROCESS_LISTING`'s output, or why it couldn't be read).
    pub(super) fn saw_processes(&mut self, listing: Result<String, String>) {
        self.processes = Some(listing);
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

/// How long one read of the install's record may take (SME-452 round 1,
/// its (B′)): an exec has no bound of its own, and the wait checks its
/// bound only after a read returns, so one read that never answered hung
/// the wait, and its caller, for good, with nothing said. A record read
/// answers in well under a second (the slowest of T5's 84 was 0.05 s).
/// It's a bound inside the install's, not a widening of it.
pub(super) const RC_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `read`, given up on after `limit`.
pub(super) async fn bounded_read<T>(
    read: impl std::future::Future<Output = Result<T, String>>,
    limit: std::time::Duration,
) -> Result<T, String> {
    tokio::time::timeout(limit, read)
        .await
        .unwrap_or_else(|_| Err(format!("the read didn't answer within {} s", limit.as_secs())))
}

/// The reads `wait_install` makes of a server pod: the pod itself, its
/// install's record and log, and what's running in it. A trait so a test
/// can make one of them hang (SME-452 round 1).
pub(super) trait InstallReads {
    async fn pod(&self) -> Result<Option<Pod>, String>;
    async fn rc(&self) -> Result<Option<String>, String>;
    async fn log(&self) -> Result<String, String>;
    async fn processes(&self) -> Result<String, String>;
}

/// `InstallReads` of pod `name` through the cluster.
struct KubeReads<'a> {
    client: &'a kube::Client,
    pods: &'a kube::Api<Pod>,
    name: &'a str,
}

impl InstallReads for KubeReads<'_> {
    async fn pod(&self) -> Result<Option<Pod>, String> {
        self.pods.get_opt(self.name).await.map_err(|e| e.to_string())
    }
    async fn rc(&self) -> Result<Option<String>, String> {
        install_rc(self.client, self.name).await
    }
    async fn log(&self) -> Result<String, String> {
        install_log(self.client, self.name).await
    }
    async fn processes(&self) -> Result<String, String> {
        install_processes(self.client, self.name).await
    }
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
    wait_install(&KubeReads { client, pods, name }, name, uid, bound).await
}

/// `wait_installed_within`'s loop, over `reads` of pod `name`.
pub(super) async fn wait_install(
    reads: &impl InstallReads,
    name: &str,
    uid: &str,
    bound: std::time::Duration,
) -> Result<(), String> {
    // tokio's clock, so a test can run the loop on paused time.
    let started = tokio::time::Instant::now();
    let mut watch = InstallWatch::new(name, uid);
    loop {
        if let Some(error) = watch.read(reads.pod().await) {
            return Err(error);
        }
        let asked = tokio::time::Instant::now();
        // Bounded, so a read that never answers is a failed read and the
        // bound below is still checked (SME-452 round 1).
        let rc = bounded_read(reads.rc(), RC_READ_TIMEOUT).await;
        watch.rc_read_took(asked.elapsed());
        match rc {
            Ok(Some(rc)) => {
                if rc == "0" {
                    return Ok(());
                }
                let log = reads.log().await.unwrap_or_default();
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
            // Both reads at once, each bounded: the message waits at most
            // one `LOG_READ_TIMEOUT` past the install's bound.
            let (log, processes) = tokio::join!(
                bounded_read(reads.log(), LOG_READ_TIMEOUT),
                bounded_read(reads.processes(), LOG_READ_TIMEOUT),
            );
            watch.saw_processes(processes);
            return Err(watch.timed_out(bound, log));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// What was running in a server pod (SME-452's recurrence), for a
/// timed-out install to name, since its pod is deleted after: its record
/// (`install.rc`, if written), its log's size, and each process but the
/// pod script (PID 1, whose command line is the install command itself)
/// and this script's own shell, as `pid [state] <the first two words of
/// its command line>` (`npm install`, `curl -fsSL`, `go install`), the
/// second only when it's a plain word, `…` otherwise (SME-452 round 1: a
/// `curl http://user:SECRET@…` was listed whole). Never `environ`, and no
/// more than 2,000 bytes. POSIX `sh` and `/proc` only,
/// with `sed`, `tr`, `wc` and `head`, so it runs in Debian, busybox and
/// Alpine images.
pub(super) const PROCESS_LISTING: &str = r#"if [ -e /tmp/install.rc ]; then echo "install.rc: $(head -c 100 /tmp/install.rc)"; else echo "install.rc: not written"; fi
echo "install.log: $(wc -c < /tmp/install.log 2>/dev/null || echo missing) bytes"
out=""
for d in /proc/[0-9]*; do
  p=${d#/proc/}
  if [ "$p" = 1 ] || [ "$p" = "$$" ]; then continue; fi
  s=$(sed -n 's/^State:[[:space:]]*//p' "$d/status" 2>/dev/null) || continue
  [ -n "$s" ] || continue
  # The words split, never globbed (the loop's list is already expanded).
  set -f
  set -- $(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)
  # The second word only when it's a plain word (`install`, `-fsSL`): a
  # URL, a `K=V` or anything else could carry a credential.
  w=${2:-}
  case "$w" in *[!A-Za-z0-9._/@-]*|*//*) w='…';; esac
  out="$out$p [$s] ${1:-} $w
"
done
printf '%s' "$out" | head -c 2000"#;

/// `PROCESS_LISTING`'s output in pod `name`'s server container.
async fn install_processes(client: &kube::Client, name: &str) -> Result<String, String> {
    let result = crate::sandbox::exec_with(client, name, "server", &["sh", "-c", PROCESS_LISTING], None)
        .await
        .map_err(|e| e.to_string())?;
    match result.exit_code {
        0 => Ok(result.stdout),
        code => Err(format!("the listing exited {code}: {}", result.stderr.trim())),
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
