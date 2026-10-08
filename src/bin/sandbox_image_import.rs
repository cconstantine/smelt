//! Streams a `docker save` tarball into the cluster's node and
//! `ctr images import`s it — the registry-free delivery mechanism for the
//! custom sandbox image (see
//! SME-17's Phase 1). Run by
//! `scripts/build-sandbox-image.sh` after `docker build`/`docker save`,
//! never by the main `smelt` server process at runtime. With `--check
//! [reference...]` instead of a tarball, it only checks that the node has
//! those images (default: `SANDBOX_IMAGES`), with the read-only
//! `ctr images ls -q` (`scripts/cluster-doctor`, SME-102).
//!
//! A short-lived pod gets the node's containerd socket hostPath-mounted
//! (`/run/k3s/containerd` — the path this project's own `rancher/k3s`
//! image, and a real k3s install, both use; not configurable here since
//! this binary isn't meant to run against anything else), receives the
//! tarball over `pods.exec` stdin, imports it, and is deleted either way.
//!
//! Standalone rather than reusing `src/sandbox.rs`'s `Sandbox`/
//! `SandboxManager`: this crate has no `lib.rs`, so a `src/bin/*.rs` binary
//! (a separate crate root, same as `sandbox_agent.rs`) can't reach
//! `main.rs`'s private module tree. Small enough to duplicate directly
//! rather than restructuring the crate for one script's sake.

use std::error::Error;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    Container, HostPathVolumeSource, Pod, PodSpec, ResourceRequirements, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, AttachParams, DeleteParams, ListParams, PostParams};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

type BoxError = Box<dyn Error + Send + Sync>;

const NAMESPACE: &str = "smelt-park";
const LOADER_POD_NAME: &str = "sandbox-image-import";
/// Every loader pod carries this label, so a run can find finished ones
/// to clean up without touching another run's live loader.
const LOADER_LABEL: (&str, &str) = ("smelt/role", "sandbox-image-loader");
/// Pinned to match `docker-compose.yml`'s `k3s` service image — keep the
/// two in sync; this is only the loader pod's own image (needs a `ctr`
/// binary matching the cluster's containerd), not the sandbox image itself.
const LOADER_IMAGE: &str = "rancher/k3s:v1.34.6-k3s1";
const CONTAINERD_SOCKET: &str = "/hostcontainerd/containerd.sock";
const REMOTE_TAR_PATH: &str = "/tmp/sandbox-image.tar";
/// How long the loader pod runs unless the tool deletes it first, as it
/// does on the way out: an hour, the CI job's own limit, so a slow runner's
/// stream can't outlast it (SME-89: 300 s once did). A run that's killed
/// leaves its pod until then; the first run after it has finished deletes
/// it (`finished_loaders`).
const LOADER_LIFETIME: Duration = Duration::from_secs(3600);

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let arg = std::env::args()
        .nth(1)
        .ok_or("usage: sandbox_image_import <path-to-docker-save-tarball> | --check [image-reference...]")?;

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let client = kube::Client::try_default().await?;
    let pods: Api<Pod> = Api::namespaced(client, NAMESPACE);

    let loader = loader_name(std::process::id(), run_nonce());
    let result = if arg == "--check" {
        let refs: Vec<String> = std::env::args().skip(2).collect();
        check(&pods, &loader, &check_targets(&refs)).await
    } else {
        import(&pods, &loader, &arg).await
    };
    // Best-effort cleanup regardless of how `import` above went — this is
    // a one-shot CLI tool, not a long-lived process with its own cleanup
    // queue, so a plain delete-on-the-way-out is enough (same "disposable,
    // no graceful shutdown needed" posture `sandbox.rs`'s own
    // `immediate_delete_params` already documents).
    let _ = pods.delete(&loader, &immediate_delete()).await;
    result
}

async fn import(pods: &Api<Pod>, loader: &str, tar_path: &str) -> Result<(), BoxError> {
    let tar_bytes = std::fs::read(tar_path).map_err(|e| format!("reading {tar_path}: {e}"))?;
    println!(
        "sandbox_image_import: read {} bytes from {tar_path}",
        tar_bytes.len()
    );

    start_loader(pods, loader, LOADER_LIFETIME).await?;
    let started = std::time::Instant::now();
    let sent = stream_tarball(pods, loader, LOADER_LIFETIME, tar_bytes.as_slice()).await?;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    println!(
        "sandbox_image_import: streamed {sent} bytes in {secs:.1}s ({:.1} MB/s)",
        sent as f64 / secs / 1_000_000.0
    );
    run_import(pods, loader).await?;
    println!("sandbox_image_import: done");
    Ok(())
}

/// This run's loader pod: `sandbox-image-import-<run>-<nonce>`, `run` being
/// the process id and `nonce` a per-run number, since process ids repeat
/// across containers. A run used to share one fixed name with every other, and
/// creating its loader deleted whichever was there, so a cluster check in
/// one worktree could kill another's import mid-stream (SME-102 review).
fn loader_name(run: u32, nonce: u32) -> String {
    format!("{LOADER_POD_NAME}-{run}-{nonce:08x}")
}

/// A per-run number to go with the process id, which is only unique
/// within one container: two `docker compose run`s can both be pid 7
/// (SME-102 review 2).
fn run_nonce() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0)
}

/// The loaders in `listed` no run can still be using: those whose `sleep`
/// ran out, and failed ones older than a loader's lifetime. A live one
/// belongs to a run still going, and so may a failed one that's younger:
/// its run reads why it failed to report it (SME-89; SME-102 review 2).
fn finished_loaders(listed: &[Pod], now: k8s_openapi::jiff::Timestamp) -> Vec<String> {
    let older_than_a_lifetime = |pod: &Pod| {
        pod.metadata
            .creation_timestamp
            .as_ref()
            .is_some_and(|t| now.duration_since(t.0).as_secs() > LOADER_LIFETIME.as_secs() as i64)
    };
    listed
        .iter()
        .filter(|pod| match pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
            // Its `sleep` ran out: no run can still be using it.
            Some("Succeeded") => true,
            // Evicted, say: its run may still be reading why, so only once
            // that run has certainly ended.
            Some("Failed") => older_than_a_lifetime(pod),
            _ => false,
        })
        .filter_map(|pod| pod.metadata.name.clone())
        .collect()
}

/// A fresh loader pod named `name`, Running, that ends on its own after
/// `lifetime`.
async fn start_loader(pods: &Api<Pod>, name: &str, lifetime: Duration) -> Result<(), BoxError> {
    // Finished loaders left by runs that were killed; never a live one,
    // which another run is still using.
    let selector = format!("{}={}", LOADER_LABEL.0, LOADER_LABEL.1);
    if let Ok(listed) = pods.list(&ListParams::default().labels(&selector)).await {
        for finished in finished_loaders(&listed.items, k8s_openapi::jiff::Timestamp::now()) {
            let _ = pods.delete(&finished, &immediate_delete()).await;
        }
    }
    let _ = pods.delete(name, &immediate_delete()).await;
    wait_gone(pods, name).await;
    pods.create(&PostParams::default(), &loader_pod_spec(name, lifetime)).await?;
    wait_running(pods, name).await?;
    println!("sandbox_image_import: loader pod Running");
    Ok(())
}

fn loader_pod_spec(name: &str, lifetime: Duration) -> Pod {
    // Explicit and small, on purpose: `smelt-park`'s `LimitRange` sets only
    // a `max` (64Gi/16 cores, see k8s/smelt-park-rbac.yaml), no `default` —
    // Kubernetes fills that gap by making an unspecified request/limit
    // default to `max` itself, which no CI runner can schedule. Found via a
    // real CI failure (`FailedScheduling ... Insufficient cpu, Insufficient
    // memory`) that a bigger real cluster's spare capacity had masked.
    let mut requests = std::collections::BTreeMap::new();
    requests.insert("cpu".to_string(), Quantity("100m".to_string()));
    requests.insert("memory".to_string(), Quantity("128Mi".to_string()));
    let mut limits = std::collections::BTreeMap::new();
    limits.insert("cpu".to_string(), Quantity("500m".to_string()));
    limits.insert("memory".to_string(), Quantity("512Mi".to_string()));

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some([(LOADER_LABEL.0.to_string(), LOADER_LABEL.1.to_string())].into()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "loader".to_string(),
                image: Some(LOADER_IMAGE.to_string()),
                command: Some(vec!["sleep".to_string(), lifetime.as_secs().to_string()]),
                resources: Some(ResourceRequirements {
                    requests: Some(requests),
                    limits: Some(limits),
                    ..Default::default()
                }),
                volume_mounts: Some(vec![VolumeMount {
                    name: "k3s-run".to_string(),
                    mount_path: "/hostcontainerd".to_string(),
                    sub_path: Some("containerd".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "k3s-run".to_string(),
                host_path: Some(HostPathVolumeSource {
                    path: "/run/k3s".to_string(),
                    type_: Some("Directory".to_string()),
                }),
                ..Default::default()
            }]),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    }
}

fn immediate_delete() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    }
}

async fn wait_running(pods: &Api<Pod>, name: &str) -> Result<(), BoxError> {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let pod = pods.get(name).await?;
            let phase = pod
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");
            if phase == "Running" {
                return Ok::<(), BoxError>(());
            }
            if phase == "Failed" {
                return Err(format!("pod {name} Failed: {:?}", pod.status).into());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(_) => Err(format!("timed out waiting for pod {name} to be Running").into()),
    }
}

/// A stale loader pod from a previous run may still be `Terminating` —
/// `wait_running` alone can't tell that apart from "never existed," so
/// this waits for it to actually disappear before `create` is retried.
async fn wait_gone(pods: &Api<Pod>, name: &str) {
    let _ = tokio::time::timeout(Duration::from_secs(30), async {
        while pods.get_opt(name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
}

async fn exec_capture(
    pods: &Api<Pod>,
    pod_name: &str,
    command: &[&str],
) -> Result<String, BoxError> {
    let mut attached = pods
        .exec(pod_name, command.iter().copied(), &AttachParams::default())
        .await?;
    #[expect(clippy::expect_used, reason = "AttachParams::default() asks for stdout")]
    let mut stdout_reader = attached
        .stdout()
        .expect("stdout requested by AttachParams::default()");
    #[expect(clippy::expect_used, reason = "AttachParams::default() asks for stderr")]
    let mut stderr_reader = attached
        .stderr()
        .expect("stderr requested by AttachParams::default()");
    let mut stdout = String::new();
    let mut stderr = String::new();
    let (r1, r2) = tokio::join!(
        stdout_reader.read_to_string(&mut stdout),
        stderr_reader.read_to_string(&mut stderr)
    );
    r1.ok();
    r2.ok();
    attached.join().await.ok();
    if !stderr.trim().is_empty() {
        stdout.push_str("\n[stderr]\n");
        stdout.push_str(&stderr);
    }
    Ok(stdout)
}

/// Streams `src` into `name`'s `REMOTE_TAR_PATH` via stdin, returning how
/// many bytes went in. A failure says whether the loader pod had stopped by
/// then (see `stream_failure`). Draining stdout/stderr *concurrently* with the stdin
/// write is required, not cosmetic — see `docs/testing.md`: writing a
/// multi-MB payload while the executed command never produces any stdout
/// output reliably breaks the exec connection (`BrokenPipe`) otherwise.
/// Confirmed by spike on the real cluster before this binary was written.
async fn stream_tarball(
    pods: &Api<Pod>,
    name: &str,
    lifetime: Duration,
    src: impl AsyncRead + Unpin,
) -> Result<u64, BoxError> {
    let started = std::time::Instant::now();
    let sent = std::sync::atomic::AtomicU64::new(0);
    match stream_into(pods, name, src, &sent).await {
        Ok(()) => Ok(sent.into_inner()),
        Err(e) => {
            let elapsed = started.elapsed();
            let phase = settled_loader_state(pods, name).await;
            let sent = sent.load(std::sync::atomic::Ordering::Relaxed);
            Err(stream_failure(name, &phase, lifetime, elapsed, sent, &e.to_string()).into())
        }
    }
}

/// `name`'s state once a stream into it has failed. The pod's status lags
/// its container's exit by a few seconds (the stream breaks while the API
/// still says Running), so a Running pod is read again for up to 10 s
/// before it's believed.
async fn settled_loader_state(pods: &Api<Pod>, name: &str) -> LoaderState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = match pods.get_opt(name).await {
            Ok(Some(pod)) => {
                let status = pod.status.unwrap_or_default();
                LoaderState::Phase {
                    phase: status.phase.unwrap_or_else(|| "Unknown".to_string()),
                    reason: status.reason,
                }
            }
            Ok(None) => LoaderState::Gone,
            Err(_) => LoaderState::Unknown,
        };
        let running = matches!(&state, LoaderState::Phase { phase, .. } if phase == "Running");
        if !running || tokio::time::Instant::now() >= deadline {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// What a loader pod looked like once a stream into it failed.
#[derive(Debug)]
enum LoaderState {
    /// The pod's phase, and its status's reason (`Evicted`, say) if any.
    Phase { phase: String, reason: Option<String> },
    Gone,
    Unknown,
}

/// The error for a stream into `name` (a loader that runs for `lifetime`)
/// that failed with `error` after `elapsed` and `sent` bytes. A loader that has stopped is said so, since
/// the bare error is only a `BrokenPipe` (SME-89).
fn stream_failure(
    name: &str,
    loader: &LoaderState,
    lifetime: Duration,
    elapsed: Duration,
    sent: u64,
    error: &str,
) -> String {
    let progress = format!("{}s and {sent} bytes into streaming the tarball", elapsed.as_secs());
    match loader {
        LoaderState::Phase { phase, .. } if phase == "Running" || phase == "Pending" => {
            format!("streaming the tarball into loader pod {name} failed {progress}, with the pod still {phase}: {error}")
        }
        LoaderState::Phase { phase, .. } if phase == "Succeeded" => format!(
            "loader pod {name} stopped ({phase}) {progress}; it runs for at most {}s, so the stream outlasted it: {error}",
            lifetime.as_secs()
        ),
        LoaderState::Phase { phase, reason } if phase != "Unknown" => {
            let why = reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            format!("loader pod {name} stopped ({phase}{why}) {progress}: {error}")
        }
        LoaderState::Gone => format!("loader pod {name} stopped (it no longer exists) {progress}: {error}"),
        // Kubernetes's own Unknown phase: the node can't be reached, so the
        // pod may well still be running.
        LoaderState::Unknown | LoaderState::Phase { .. } => {
            format!("streaming the tarball into loader pod {name} failed {progress} (couldn't read the pod's state): {error}")
        }
    }
}

/// `stream_tarball`'s stream, counting the bytes written into `sent`.
async fn stream_into(
    pods: &Api<Pod>,
    name: &str,
    mut src: impl AsyncRead + Unpin,
    sent: &std::sync::atomic::AtomicU64,
) -> Result<(), BoxError> {
    let command = format!("cat > {REMOTE_TAR_PATH} && echo done");
    let mut attached = pods
        .exec(name, ["sh", "-c", command.as_str()], &AttachParams::default().stdin(true))
        .await?;
    let mut stdin = attached.stdin().ok_or("the loader's exec has no stdin")?;
    let mut stdout = attached.stdout().ok_or("the loader's exec has no stdout")?;
    let mut stderr = attached.stderr().ok_or("the loader's exec has no stderr")?;

    let write_fut = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = src.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            stdin.write_all(&buf[..n]).await?;
            sent.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        stdin.flush().await?;
        drop(stdin);
        Ok::<(), BoxError>(())
    };
    let drain_out = async {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
        buf
    };
    let drain_err = async {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    };
    let (write_res, out, err) = tokio::join!(write_fut, drain_out, drain_err);
    write_res?;
    attached.join().await.ok();
    if !err.trim().is_empty() {
        return Err(format!("streaming tarball into loader pod: {err}").into());
    }
    remote_write_finished(&out)?;
    Ok(())
}

/// Whether the loader's `cat > … && echo done` got to its `done`, given its
/// stdout. Every byte can be handed to the exec's stdin while the loader
/// dies before `cat` has written them, so a clean local write isn't enough
/// (SME-89 code review).
fn remote_write_finished(stdout: &str) -> Result<(), String> {
    if stdout.lines().any(|line| line.trim() == "done") {
        Ok(())
    } else {
        Err(format!("the loader's cat didn't finish writing the tarball (its output: {:?})", stdout.trim()))
    }
}

/// `ctr images import`s the streamed tarball.
async fn run_import(pods: &Api<Pod>, loader: &str) -> Result<(), BoxError> {
    println!("sandbox_image_import: tarball streamed, running ctr images import");
    let import_out = exec_capture(
        pods,
        loader,
        &[
            "ctr",
            "--address",
            CONTAINERD_SOCKET,
            "--namespace",
            "k8s.io",
            "images",
            "import",
            REMOTE_TAR_PATH,
        ],
    )
    .await?;
    println!("{import_out}");
    Ok(())
}

/// The images sandbox pods start from, as containerd names them. Pods use
/// `imagePullPolicy: Never`, so a missing one fails every pod
/// (`ErrImageNeverPull`); the kubelet deletes unused images when the node's
/// disk passes its garbage-collection threshold.
const SANDBOX_IMAGES: &[&str] = &[
    "docker.io/library/smelt-sandbox:latest",
    "docker.io/library/docker:29-dind",
];

/// The references `--check` looks for: the ones given after it, or
/// `SANDBOX_IMAGES` when none are (SME-102: `scripts/cluster-doctor` passes
/// the working tree's own `smelt-sandbox:src-<hash>`).
fn check_targets(given: &[String]) -> Vec<String> {
    if given.is_empty() {
        SANDBOX_IMAGES.iter().map(|s| s.to_string()).collect()
    } else {
        given.to_vec()
    }
}

/// Which of `wanted` aren't in `listed`, the output of `ctr images ls -q`
/// (one image reference per line).
fn missing_images<S: AsRef<str>>(listed: &str, wanted: &[S]) -> Vec<String> {
    let listed: std::collections::HashSet<&str> = listed.lines().map(str::trim).collect();
    wanted
        .iter()
        .map(AsRef::as_ref)
        .filter(|image| !listed.contains(image))
        .map(str::to_string)
        .collect()
}

/// `--check`: fails, naming them, if the node lacks any of `wanted`.
async fn check(pods: &Api<Pod>, loader: &str, wanted: &[String]) -> Result<(), BoxError> {
    start_loader(pods, loader, LOADER_LIFETIME).await?;
    let listed = exec_capture(
        pods,
        loader,
        &["ctr", "--address", CONTAINERD_SOCKET, "--namespace", "k8s.io", "images", "ls", "-q"],
    )
    .await?;
    let missing = missing_images(&listed, wanted);
    if missing.is_empty() {
        println!("sandbox_image_import: the node has {}", wanted.join(" and "));
        Ok(())
    } else {
        Err(format!(
            "the cluster's node is missing {} (the kubelet deletes unused images when its disk is \
             over 85% full); free space if need be, then run scripts/build-sandbox-image.sh",
            missing.join(" and ")
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_images_names_each_wanted_image_not_listed() {
        let listed = "docker.io/library/smelt-sandbox:latest\nsha256:0123abcd\ndocker.io/rancher/mirrored-pause:3.6\n";
        assert_eq!(missing_images(listed, SANDBOX_IMAGES), vec!["docker.io/library/docker:29-dind".to_string()]);
        assert!(missing_images("docker.io/library/smelt-sandbox:latest\ndocker.io/library/docker:29-dind\n", SANDBOX_IMAGES).is_empty());
        assert_eq!(missing_images("", SANDBOX_IMAGES).len(), 2);
    }

    /// A loader that ends while the tarball is still streaming in (SME-89:
    /// a slow CI runner outlasted the loader's lifetime) is reported as
    /// that, not as a bare `BrokenPipe`. Real cluster: its own short-lived
    /// loader, fed by a reader that trickles for longer than it lives.
    #[tokio::test]
    async fn test_a_loader_that_ends_mid_stream_says_so() -> Result<(), BoxError> {
        const NAME: &str = "sandbox-image-import-test";
        rustls::crypto::ring::default_provider().install_default().ok();
        let pods: Api<Pod> = Api::namespaced(kube::Client::try_default().await?, NAMESPACE);
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let trickle = tokio::spawn(async move {
            let chunk = vec![0u8; 16 * 1024];
            for _ in 0..100 {
                if writer.write_all(&chunk).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });

        let result = async {
            start_loader(&pods, NAME, Duration::from_secs(5)).await?;
            Ok::<_, BoxError>(stream_tarball(&pods, NAME, Duration::from_secs(5), reader).await)
        }
        .await;
        trickle.abort();
        let _ = pods.delete(NAME, &immediate_delete()).await;

        let error = match result? {
            Ok(sent) => panic!("streaming into an ended loader succeeded ({sent} bytes)"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("loader pod") && error.contains("stopped"), "error: {error}");
        Ok(())
    }

    /// Each run's loader has a name of its own, so one run can't delete
    /// another's (SME-102 review), even when their process ids match.
    #[test]
    fn test_each_run_gets_a_loader_of_its_own() {
        let (a, b) = (loader_name(1234, 1), loader_name(5678, 1));
        assert_ne!(a, b);
        // Two containers' runs can share a process id.
        assert_ne!(loader_name(7, 1), loader_name(7, 2));
        assert!(a.starts_with("sandbox-image-import-") && b.starts_with("sandbox-image-import-"), "{a} {b}");
        let labels = loader_pod_spec(&a, LOADER_LIFETIME).metadata.labels.unwrap_or_default();
        assert_eq!(labels.get(LOADER_LABEL.0).map(String::as_str), Some(LOADER_LABEL.1));
    }

    #[test]
    fn test_only_finished_loaders_are_cleaned_up() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let now = Timestamp::from_second(1_000_000).expect("a timestamp");
        let pod = |name: &str, phase: Option<&str>, age_secs: i64| Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                creation_timestamp: Some(Time(Timestamp::from_second(1_000_000 - age_secs).expect("a timestamp"))),
                ..Default::default()
            },
            status: phase.map(|p| k8s_openapi::api::core::v1::PodStatus { phase: Some(p.to_string()), ..Default::default() }),
            ..Default::default()
        };
        let old = LOADER_LIFETIME.as_secs() as i64 + 60;
        let listed = vec![
            pod("done", Some("Succeeded"), 10),
            pod("evicted-long-ago", Some("Failed"), old),
            // Its run may still be reading why it failed (SME-89's reason).
            pod("evicted-just-now", Some("Failed"), 10),
            pod("live", Some("Running"), old),
            pod("starting", Some("Pending"), 10),
            pod("unknown", None, old),
        ];
        assert_eq!(finished_loaders(&listed, now), vec!["done".to_string(), "evicted-long-ago".to_string()]);
    }

    /// The loader outlives any import CI can run: the job's own limit is
    /// 60 minutes (`.github/workflows/ci.yml`), and a slow runner once
    /// took longer than the old 300 s just to stream the tarball (SME-89).
    #[test]
    fn test_the_loader_lives_as_long_as_the_ci_job() {
        let spec = loader_pod_spec(LOADER_POD_NAME, LOADER_LIFETIME);
        let command = spec.spec.and_then(|s| s.containers.into_iter().next()).and_then(|c| c.command);
        assert_eq!(command, Some(vec!["sleep".to_string(), "3600".to_string()]));
    }

    #[test]
    fn test_a_stream_failure_says_whether_the_loader_stopped() {
        let t = Duration::from_secs(300);
        let ended = stream_failure("l", &phase("Succeeded", None), t, t, 42, "broken pipe");
        assert!(ended.starts_with("loader pod l stopped (Succeeded) 300s and 42 bytes into"), "{ended}");
        assert!(ended.contains("at most 300s") && ended.ends_with(": broken pipe"), "{ended}");
        let gone = stream_failure("l", &LoaderState::Gone, t, t, 42, "broken pipe");
        assert!(gone.starts_with("loader pod l stopped (it no longer exists)"), "{gone}");
        let running = stream_failure("l", &phase("Running", None), t, t, 42, "broken pipe");
        assert!(running.contains("with the pod still Running") && !running.contains("stopped"), "{running}");
        let unknown = stream_failure("l", &LoaderState::Unknown, t, t, 42, "broken pipe");
        assert!(unknown.contains("couldn't read the pod's state") && !unknown.contains("stopped"), "{unknown}");
    }

    #[test]
    fn test_a_stream_counts_only_once_the_loader_says_done() {
        assert!(remote_write_finished("done\n").is_ok());
        let cut_off = remote_write_finished("").expect_err("no done");
        assert!(cut_off.contains("didn't finish"), "{cut_off}");
        assert!(remote_write_finished("not done\n").is_err());
    }

    /// Kubernetes's `Unknown` phase (the node can't be reached) isn't a
    /// stopped pod (SME-89 code review 2).
    #[test]
    fn test_a_loader_in_an_unknown_phase_isnt_reported_as_stopped() {
        let t = Duration::from_secs(60);
        let unknown = stream_failure("l", &phase("Unknown", None), t, t, 42, "broken pipe");
        assert!(unknown.contains("couldn't read the pod's state") && !unknown.contains("stopped"), "{unknown}");
    }

    fn phase(phase: &str, reason: Option<&str>) -> LoaderState {
        LoaderState::Phase { phase: phase.to_string(), reason: reason.map(str::to_string) }
    }

    /// Only a loader whose `sleep` ran out (Succeeded) was outlasted; one
    /// that Failed was killed for some other reason, an eviction say, and
    /// is reported with that reason instead (SME-89 code review).
    #[test]
    fn test_a_failed_loader_is_reported_with_its_reason_not_as_outlasted() {
        let t = Duration::from_secs(3600);
        let evicted = stream_failure("l", &phase("Failed", Some("Evicted")), t, Duration::from_secs(60), 42, "broken pipe");
        assert!(evicted.starts_with("loader pod l stopped (Failed: Evicted) 60s and 42 bytes into"), "{evicted}");
        assert!(!evicted.contains("outlasted"), "{evicted}");
        let failed = stream_failure("l", &phase("Failed", None), t, Duration::from_secs(60), 42, "broken pipe");
        assert!(failed.starts_with("loader pod l stopped (Failed) 60s") && !failed.contains("outlasted"), "{failed}");
    }

    #[test]
    fn test_check_looks_for_the_references_given_or_the_defaults() {
        let given = vec!["docker.io/library/smelt-sandbox:src-0123456789abcdef".to_string()];
        assert_eq!(check_targets(&given), given);
        assert_eq!(check_targets(&[]), SANDBOX_IMAGES.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }

    /// A tag that only starts like a wanted one isn't it.
    #[test]
    fn test_missing_images_matches_whole_references() {
        let listed = "docker.io/library/smelt-sandbox:latest-old\ndocker.io/library/docker:29-dind\n";
        assert_eq!(missing_images(listed, SANDBOX_IMAGES), vec!["docker.io/library/smelt-sandbox:latest".to_string()]);
    }
}
