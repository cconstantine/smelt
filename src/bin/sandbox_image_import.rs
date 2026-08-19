//! Streams a `docker save` tarball into the cluster's node and
//! `ctr images import`s it — the registry-free delivery mechanism for the
//! custom sandbox image (see
//! docs/projects/plans/sandbox-native-environment.md's Phase 1). Run by
//! `scripts/build-sandbox-image.sh` after `docker build`/`docker save`,
//! never by the main `smelt` server process at runtime.
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type BoxError = Box<dyn Error + Send + Sync>;

const NAMESPACE: &str = "smelt-park";
const LOADER_POD_NAME: &str = "sandbox-image-import";
/// Pinned to match `docker-compose.yml`'s `k3s` service image — keep the
/// two in sync; this is only the loader pod's own image (needs a `ctr`
/// binary matching the cluster's containerd), not the sandbox image itself.
const LOADER_IMAGE: &str = "rancher/k3s:v1.34.6-k3s1";
const CONTAINERD_SOCKET: &str = "/hostcontainerd/containerd.sock";
const REMOTE_TAR_PATH: &str = "/tmp/sandbox-image.tar";

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let tar_path = std::env::args().nth(1).ok_or("usage: sandbox_image_import <path-to-docker-save-tarball>")?;

    rustls::crypto::ring::default_provider().install_default().ok();
    let client = kube::Client::try_default().await?;
    let pods: Api<Pod> = Api::namespaced(client, NAMESPACE);

    let result = import(&pods, &tar_path).await;
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
    println!("sandbox_image_import: read {} bytes from {tar_path}", tar_bytes.len());

    let _ = pods.delete(LOADER_POD_NAME, &immediate_delete()).await;
    wait_gone(pods, LOADER_POD_NAME).await;

    pods.create(&PostParams::default(), &loader_pod_spec()).await?;
    wait_running(pods, LOADER_POD_NAME).await?;
    println!("sandbox_image_import: loader pod Running");

    stream_and_import(pods, &tar_bytes).await?;
    println!("sandbox_image_import: done");
    Ok(())
}

fn loader_pod_spec() -> Pod {
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
        metadata: ObjectMeta { name: Some(LOADER_POD_NAME.to_string()), namespace: Some(NAMESPACE.to_string()), ..Default::default() },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "loader".to_string(),
                image: Some(LOADER_IMAGE.to_string()),
                command: Some(vec!["sleep".to_string(), "300".to_string()]),
                resources: Some(ResourceRequirements { requests: Some(requests), limits: Some(limits), ..Default::default() }),
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
                host_path: Some(HostPathVolumeSource { path: "/run/k3s".to_string(), type_: Some("Directory".to_string()) }),
                ..Default::default()
            }]),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    }
}

fn immediate_delete() -> DeleteParams {
    DeleteParams { grace_period_seconds: Some(0), ..Default::default() }
}

async fn wait_running(pods: &Api<Pod>, name: &str) -> Result<(), BoxError> {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let pod = pods.get(name).await?;
            let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("");
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

async fn exec_capture(pods: &Api<Pod>, pod_name: &str, command: &[&str]) -> Result<String, BoxError> {
    let mut attached = pods.exec(pod_name, command.iter().copied(), &AttachParams::default()).await?;
    let mut stdout_reader = attached.stdout().expect("stdout requested by AttachParams::default()");
    let mut stderr_reader = attached.stderr().expect("stderr requested by AttachParams::default()");
    let mut stdout = String::new();
    let mut stderr = String::new();
    let (r1, r2) = tokio::join!(stdout_reader.read_to_string(&mut stdout), stderr_reader.read_to_string(&mut stderr));
    r1.ok();
    r2.ok();
    attached.join().await.ok();
    if !stderr.trim().is_empty() {
        stdout.push_str("\n[stderr]\n");
        stdout.push_str(&stderr);
    }
    Ok(stdout)
}

/// Streams `data` in via stdin, then runs `ctr images import` against it.
/// Draining stdout/stderr *concurrently* with the stdin write is required,
/// not cosmetic — see `docs/testing.md`: writing a multi-MB payload while
/// the executed command never produces any stdout output reliably breaks
/// the exec connection (`BrokenPipe`) otherwise. Confirmed by spike on the
/// real cluster before this binary was written.
async fn stream_and_import(pods: &Api<Pod>, data: &[u8]) -> Result<(), BoxError> {
    let command = format!("cat > {REMOTE_TAR_PATH} && echo done");
    let mut attached =
        pods.exec(LOADER_POD_NAME, ["sh", "-c", command.as_str()], &AttachParams::default().stdin(true)).await?;
    let mut stdin = attached.stdin().expect("stdin requested");
    let mut stdout = attached.stdout().expect("stdout requested by default");
    let mut stderr = attached.stderr().expect("stderr requested by default");

    let write_fut = async {
        stdin.write_all(data).await?;
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

    println!("sandbox_image_import: tarball streamed, running ctr images import");
    let import_out = exec_capture(
        pods,
        LOADER_POD_NAME,
        &["ctr", "--address", CONTAINERD_SOCKET, "--namespace", "k8s.io", "images", "import", REMOTE_TAR_PATH],
    )
    .await?;
    println!("{import_out}");
    Ok(())
}
