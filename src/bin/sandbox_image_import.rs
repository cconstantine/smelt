//! Streams a `docker save` tarball into the cluster's node and
//! `ctr images import`s it — the registry-free delivery mechanism for the
//! custom sandbox image (see
//! SME-17's Phase 1). Run by
//! `scripts/build-sandbox-image.sh` after `docker build`/`docker save`,
//! never by the main `smelt` server process at runtime. With `--check`
//! instead of a tarball, it only checks that the node still has every
//! sandbox image (`scripts/cluster-doctor`).
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
use kube::api::{Api, AttachParams, DeleteParams, PostParams};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

type BoxError = Box<dyn Error + Send + Sync>;

const NAMESPACE: &str = "smelt-park";
const LOADER_POD_NAME: &str = "sandbox-image-import";
/// Pinned to match `docker-compose.yml`'s `k3s` service image — keep the
/// two in sync; this is only the loader pod's own image (needs a `ctr`
/// binary matching the cluster's containerd), not the sandbox image itself.
const LOADER_IMAGE: &str = "rancher/k3s:v1.34.6-k3s1";
const CONTAINERD_SOCKET: &str = "/hostcontainerd/containerd.sock";
const REMOTE_TAR_PATH: &str = "/tmp/sandbox-image.tar";
/// How long the loader pod runs unless the tool deletes it first, as it
/// does on the way out: an hour, the CI job's own limit, so a slow runner's
/// stream can't outlast it (SME-89: 300 s once did). A run that's killed
/// leaves the pod until then, or until the next run deletes it.
const LOADER_LIFETIME: Duration = Duration::from_secs(3600);

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let arg = std::env::args()
        .nth(1)
        .ok_or("usage: sandbox_image_import <path-to-docker-save-tarball> | --check")?;

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let client = kube::Client::try_default().await?;
    let pods: Api<Pod> = Api::namespaced(client, NAMESPACE);

    let result = if arg == "--check" { check(&pods).await } else { import(&pods, &arg).await };
    // Best-effort cleanup regardless of how `import` above went — this is
    // a one-shot CLI tool, not a long-lived process with its own cleanup
    // queue, so a plain delete-on-the-way-out is enough (same "disposable,
    // no graceful shutdown needed" posture `sandbox.rs`'s own
    // `immediate_delete_params` already documents).
    let _ = pods.delete(LOADER_POD_NAME, &immediate_delete()).await;
    result
}

async fn import(pods: &Api<Pod>, tar_path: &str) -> Result<(), BoxError> {
    let tar_bytes = std::fs::read(tar_path).map_err(|e| format!("reading {tar_path}: {e}"))?;
    println!(
        "sandbox_image_import: read {} bytes from {tar_path}",
        tar_bytes.len()
    );

    start_loader(pods, LOADER_POD_NAME, LOADER_LIFETIME).await?;
    let started = std::time::Instant::now();
    let sent = stream_tarball(pods, LOADER_POD_NAME, LOADER_LIFETIME, tar_bytes.as_slice()).await?;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    println!(
        "sandbox_image_import: streamed {sent} bytes in {secs:.1}s ({:.1} MB/s)",
        sent as f64 / secs / 1_000_000.0
    );
    run_import(pods).await?;
    println!("sandbox_image_import: done");
    Ok(())
}

/// A fresh loader pod named `name`, Running, that ends on its own after
/// `lifetime`.
async fn start_loader(pods: &Api<Pod>, name: &str, lifetime: Duration) -> Result<(), BoxError> {
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
    let mut stdout_reader = attached
        .stdout()
        .expect("stdout requested by AttachParams::default()");
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
            Ok(Some(pod)) => LoaderState::Phase(
                pod.status.and_then(|s| s.phase).unwrap_or_else(|| "Unknown".to_string()),
            ),
            Ok(None) => LoaderState::Gone,
            Err(_) => LoaderState::Unknown,
        };
        let running = matches!(&state, LoaderState::Phase(p) if p == "Running");
        if !running || tokio::time::Instant::now() >= deadline {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// What a loader pod looked like once a stream into it failed.
#[derive(Debug)]
enum LoaderState {
    Phase(String),
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
        LoaderState::Phase(phase) if phase == "Running" || phase == "Pending" => {
            format!("streaming the tarball into loader pod {name} failed {progress}, with the pod still {phase}: {error}")
        }
        LoaderState::Phase(phase) => format!(
            "loader pod {name} stopped ({phase}) {progress}; it runs for at most {}s, so the stream outlasted it: {error}",
            lifetime.as_secs()
        ),
        LoaderState::Gone => format!("loader pod {name} stopped (it no longer exists) {progress}: {error}"),
        LoaderState::Unknown => {
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
    let (write_res, _out, err) = tokio::join!(write_fut, drain_out, drain_err);
    write_res?;
    attached.join().await.ok();
    if !err.trim().is_empty() {
        return Err(format!("streaming tarball into loader pod: {err}").into());
    }
    Ok(())
}

/// `ctr images import`s the streamed tarball.
async fn run_import(pods: &Api<Pod>) -> Result<(), BoxError> {
    println!("sandbox_image_import: tarball streamed, running ctr images import");
    let import_out = exec_capture(
        pods,
        LOADER_POD_NAME,
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

/// Which of `wanted` aren't in `listed`, the output of `ctr images ls -q`
/// (one image reference per line).
fn missing_images(listed: &str, wanted: &[&str]) -> Vec<String> {
    let listed: std::collections::HashSet<&str> = listed.lines().map(str::trim).collect();
    wanted
        .iter()
        .filter(|image| !listed.contains(**image))
        .map(|image| image.to_string())
        .collect()
}

/// `--check`: fails, naming them, if the node lacks any of `SANDBOX_IMAGES`.
async fn check(pods: &Api<Pod>) -> Result<(), BoxError> {
    start_loader(pods, LOADER_POD_NAME, LOADER_LIFETIME).await?;
    let listed = exec_capture(
        pods,
        LOADER_POD_NAME,
        &["ctr", "--address", CONTAINERD_SOCKET, "--namespace", "k8s.io", "images", "ls", "-q"],
    )
    .await?;
    let missing = missing_images(&listed, SANDBOX_IMAGES);
    if missing.is_empty() {
        println!("sandbox_image_import: the node has every sandbox image");
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
        let ended = stream_failure("l", &LoaderState::Phase("Succeeded".to_string()), t, t, 42, "broken pipe");
        assert!(ended.starts_with("loader pod l stopped (Succeeded) 300s and 42 bytes into"), "{ended}");
        assert!(ended.contains("at most 300s") && ended.ends_with(": broken pipe"), "{ended}");
        let gone = stream_failure("l", &LoaderState::Gone, t, t, 42, "broken pipe");
        assert!(gone.starts_with("loader pod l stopped (it no longer exists)"), "{gone}");
        let running = stream_failure("l", &LoaderState::Phase("Running".to_string()), t, t, 42, "broken pipe");
        assert!(running.contains("with the pod still Running") && !running.contains("stopped"), "{running}");
        let unknown = stream_failure("l", &LoaderState::Unknown, t, t, 42, "broken pipe");
        assert!(unknown.contains("couldn't read the pod's state") && !unknown.contains("stopped"), "{unknown}");
    }

    /// A tag that only starts like a wanted one isn't it.
    #[test]
    fn test_missing_images_matches_whole_references() {
        let listed = "docker.io/library/smelt-sandbox:latest-old\ndocker.io/library/docker:29-dind\n";
        assert_eq!(missing_images(listed, SANDBOX_IMAGES), vec!["docker.io/library/smelt-sandbox:latest".to_string()]);
    }
}
