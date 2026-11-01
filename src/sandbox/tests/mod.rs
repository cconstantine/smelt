use super::*;

pub(super) async fn test_client() -> kube::Client {
    // Only under the cluster lock (SME-212): every real-cluster test
    // gets its client here, or through `lsp::pods::tests::cluster`'s.
    crate::test_lock::require();
    // Same call `main()` makes before building any TLS-using client
    // (see its own comment) — but `main()` never runs under `cargo
    // test`, so without this, the first `kube::Client` built here hits
    // rustls's auto-detect instead of an explicit choice. That
    // auto-detect only works when exactly one crypto-provider backend
    // is compiled into the binary; it started panicking
    // ("Could not automatically determine the process-level
    // CryptoProvider... make sure exactly one of the 'aws-lc-rs' and
    // 'ring' features is enabled") once `rmcp`'s own `reqwest` feature
    // (added for the MCP client, see Cargo.toml) pulled a second
    // candidate backend into the shared `reqwest` dependency. `let _
    // =` because a second call in the same test binary (multiple
    // tests each calling `test_client()`) is no-op-safe, not an error
    // worth surfacing.
    let _ = rustls::crypto::ring::default_provider().install_default();
    kube::Client::try_default()
        .await
        .expect("KUBECONFIG must point at a reachable cluster for sandbox tests")
}

fn fake_pod(id: i64) -> db::SandboxPod {
    db::SandboxPod {
        id,
        conversation_id: 1,
        created_at: chrono::Utc::now().naive_utc(),
        terminated_at: None,
    }
}

fn docker_for_conversation(conversation_id: i64) -> DockerSidecar {
    DockerSidecar {
        memory: "2Gi".to_string(),
        storage: PodStorage::Conversation(conversation_id),
    }
}

fn container<'a>(containers: &'a Option<Vec<Container>>, name: &str) -> &'a Container {
    containers
        .as_ref()
        .and_then(|cs| cs.iter().find(|c| c.name == name))
        .unwrap_or_else(|| panic!("pod spec should have a `{name}` container"))
}

fn mount_path_of<'a>(c: &'a Container, volume: &str) -> Option<&'a str> {
    c.volume_mounts
        .as_ref()?
        .iter()
        .find(|m| m.name == volume)
        .map(|m| m.mount_path.as_str())
}

fn meta_with_instance(instance: Option<&str>) -> ObjectMeta {
    ObjectMeta {
        name: Some("sandbox-workspace-1528".to_string()),
        labels: Some(
            [(CONVERSATION_LABEL.to_string(), "1528".to_string())]
                .into_iter()
                .chain(instance.map(|i| (INSTANCE_LABEL.to_string(), i.to_string())))
                .collect(),
        ),
        ..Default::default()
    }
}

fn named(mut pod: Pod, name: &str) -> Pod {
    pod.metadata.name = Some(name.to_string());
    pod
}

use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
    PodStatus,
};

fn pod_with_phase(phase: &str) -> Pod {
    Pod {
        status: Some(PodStatus {
            phase: Some(phase.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(super) fn unique_session_id(label: &str) -> String {
    format!("test-{label}-{}", crate::id_alloc::FIXTURE_IDS.next_base())
}

/// A conversation id no other test or run uses: claims are named after
/// it. One cursor per process (SME-255): the fold this replaces
/// (`3e9 + nanos % 1e9`) handed two calls in one clock tick the same id.
pub(super) fn unused_conversation_id() -> i64 {
    crate::id_alloc::FIXTURE_IDS.next_base()
}

/// SME-255 (T5): the fixture helpers never hand two calls the same id or
/// name. 1000 back-to-back calls each — the mechanism of a raw fixture
/// pod 409 (no database sequences involved): two calls in one clock tick
/// returned one id, because the fold read the clock on every call.
#[test]
fn test_the_fixture_helpers_never_repeat() {
    let ids: std::collections::HashSet<i64> = (0..1000).map(|_| unused_conversation_id()).collect();
    assert_eq!(ids.len(), 1000, "the fixture id helper repeated an id across 1000 calls");
    let names: std::collections::HashSet<String> = (0..1000).map(|_| unique_session_id("t5")).collect();
    assert_eq!(names.len(), 1000, "the session-name helper repeated a name across 1000 calls");
}

/// A small claim for `conversation_id` labelled with `instance` (or with
/// no instance, like one from before SME-115).
async fn make_claim(client: &kube::Client, name: &str, conversation_id: i64, instance: Option<&str>) {
    let mut spec = build_conversation_pvc_spec(name.to_string(), conversation_id, "1Mi".to_string(), "unused");
    let labels = spec.metadata.labels.get_or_insert_default();
    match instance {
        Some(instance) => labels.insert(INSTANCE_LABEL.to_string(), instance.to_string()),
        None => labels.remove(INSTANCE_LABEL),
    };
    pvc_api(client).create(&PostParams::default(), &spec).await.expect("create a claim");
}

/// Whether `name` is still there and not being deleted.
async fn claim_kept(client: &kube::Client, name: &str) -> bool {
    match pvc_api(client).get_opt(name).await.expect("read a claim") {
        Some(claim) => claim.metadata.deletion_timestamp.is_none(),
        None => false,
    }
}

/// Real-cluster tests, apart from the pure ones so a filter on a pure
/// test's path can't run them (SME-134). Each runs only by exact name,
/// under the cluster lock.
mod cluster {
    use super::*;
    use k8s_openapi::api::core::v1::ConfigMapVolumeSource;

    /// The flake's stage, held on purpose (SME-132): a pod whose volume
    /// names a ConfigMap that doesn't exist stays at
    /// `PodReadyToStartContainers: False` with `FailedMount` events, every
    /// time, on an idle node. A timed-out wait says so, the containers'
    /// states and the event naming the ConfigMap included.
    ///
    /// Harmless if killed: a unique name, the tests' instance, no
    /// conversation label, and `activeDeadlineSeconds`, so the kubelet
    /// fails a leftover (`DeadlineExceeded`). It checks the deadline only
    /// once its 2-minute wait for the mount gives up: a run killed after
    /// the create left a pod that failed at 127 s. The sweep (SME-134)
    /// removes the object.
    #[tokio::test]
    async fn test_a_pod_stuck_before_its_containers_says_what_it_waits_on() {
        let client = test_client().await;
        let pods = pods_api(&client);
        let name = format!("sandbox-{}", unique_session_id("stuck-at-mount"));
        let missing = format!("sme-132-missing-{}", crate::id_alloc::FIXTURE_IDS.next_base());
        let docker = DockerSidecar { memory: "1Gi".to_string(), storage: PodStorage::Ephemeral };
        let mut pod = build_pod_spec(&name, "1Gi", &docker, &[], &[], TEST_INSTANCE);
        let spec = pod.spec.as_mut().expect("a pod spec");
        spec.active_deadline_seconds = Some(60);
        spec.volumes.get_or_insert_with(Vec::new).push(Volume {
            name: "missing".to_string(),
            config_map: Some(ConfigMapVolumeSource { name: missing.clone(), ..Default::default() }),
            ..Default::default()
        });
        // The kubelet mounts only the volumes a container uses.
        spec.containers[0].volume_mounts.get_or_insert_with(Vec::new).push(VolumeMount {
            name: "missing".to_string(),
            mount_path: "/sme-132-missing".to_string(),
            ..Default::default()
        });
        pods.create(&PostParams::default(), &pod).await.expect("create the pod");

        let (waited, spans) =
            crate::telemetry::capture_spans(wait_for_running_with_timeout(&pods, &name, Duration::from_secs(20))).await;
        pods.delete(&name, &immediate_delete_params()).await.ok();

        // SME-137: the timed-out wait is a failed span naming the pod.
        let span = spans.iter().find(|span| span.name == "wait_for_running").expect("the wait's span");
        assert!(span.attributes.iter().any(|kv| kv.key.as_str() == "pod" && kv.value == name.clone().into()), "{span:?}");
        assert!(span.attributes.iter().any(|kv| kv.key.as_str() == "timeout_s" && kv.value == 20_i64.into()), "{span:?}");
        let opentelemetry::trace::Status::Error { description } = &span.status else { panic!("{:?}", span.status) };
        assert!(description.contains("FailedMount"), "the wait's detail: {description}");

        let detail = match waited {
            Err(SandboxError::Timeout(Some(detail))) => detail,
            other => panic!("expected a timeout with a detail, got {other:?}"),
        };
        println!("{detail}");
        assert!(detail.contains("PodReadyToStartContainers"), "got {detail}");
        assert!(detail.contains("FailedMount"), "got {detail}");
        assert!(detail.contains(&missing), "got {detail}");
        for container in ["docker", "sandbox"] {
            assert!(
                detail.contains(&format!("{container} not created")) || detail.contains(&format!("{container} waiting")),
                "{container}'s state is missing: {detail}"
            );
        }
    }
}

mod spec;
mod ownership;
mod docker_restarts;
mod guards;
mod volumes;
mod start_reports;
mod death;
mod manager;
mod git;
mod child_checkout;
mod docker;
mod starting;
mod teardown;
mod adoption;
mod image;
mod idle_notice;
mod quiet_stop;
