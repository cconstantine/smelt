use super::*;

pub(super) async fn test_client() -> kube::Client {
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

#[test]
fn test_pod_spec_runs_dockerd_in_a_privileged_native_sidecar() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    let spec = pod.spec.expect("pod should have a spec");
    let docker = container(&spec.init_containers, "docker");

    // A native sidecar: an init container that keeps running, and that
    // Kubernetes restarts on its own when it dies (an OOM kill).
    assert_eq!(docker.restart_policy.as_deref(), Some("Always"));
    assert_eq!(
        docker.security_context.as_ref().and_then(|s| s.privileged),
        Some(true)
    );
    // Our own start script, never the image's entrypoint, which
    // rearranges the node's root cgroup (see START_DOCKERD_SCRIPT).
    let command = docker.command.as_ref().expect("sidecar should override the entrypoint");
    assert_eq!(command[..2], ["sh".to_string(), "-c".to_string()]);
    assert_eq!(command[2], START_DOCKERD_SCRIPT);
    let args = docker.args.as_ref().expect("sidecar should pass dockerd args");
    assert!(
        args.contains(&"--host=unix:///run/docker-sock/docker.sock".to_string()),
        "dockerd should listen on the shared socket: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains("tcp://")),
        "dockerd must never listen on TCP: {args:?}"
    );
    let limits = docker
        .resources
        .as_ref()
        .and_then(|r| r.limits.as_ref())
        .expect("sidecar should have limits");
    assert_eq!(limits.get("memory"), Some(&Quantity("2Gi".to_string())));
    assert!(docker.startup_probe.is_some(), "the sandbox should wait until dockerd answers");
}

#[test]
fn test_pod_spec_leaves_the_sandbox_container_unprivileged() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    let spec = pod.spec.expect("pod should have a spec");
    let main = Some(spec.containers);
    let sandbox = container(&main, "sandbox");
    assert_ne!(
        sandbox.security_context.as_ref().and_then(|s| s.privileged),
        Some(true)
    );
}

#[test]
fn test_pod_spec_shares_workspace_and_socket_but_keeps_docker_data_in_the_sidecar() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    let spec = pod.spec.expect("pod should have a spec");
    let docker = container(&spec.init_containers, "docker");
    let main = Some(spec.containers.clone());
    let sandbox = container(&main, "sandbox");

    for c in [docker, sandbox] {
        assert_eq!(mount_path_of(c, "workspace"), Some("/workspace"), "{}", c.name);
        assert_eq!(mount_path_of(c, "docker-sock"), Some("/run/docker-sock"), "{}", c.name);
    }
    assert_eq!(mount_path_of(docker, "docker-data"), Some("/var/lib/docker"));
    assert_eq!(mount_path_of(sandbox, "docker-data"), None);
    // The sidecar (root, started first) hands /workspace to the sandbox user.
    let owner = docker
        .env
        .as_ref()
        .and_then(|env| env.iter().find(|e| e.name == "WORKSPACE_OWNER"))
        .and_then(|e| e.value.as_deref());
    assert_eq!(owner, Some(SANDBOX_OWNER));

    let volumes = spec.volumes.expect("pod should have volumes");
    let data = volumes.iter().find(|v| v.name == "docker-data").expect("docker-data volume");
    assert_eq!(
        data.persistent_volume_claim.as_ref().map(|p| p.claim_name.as_str()),
        Some("sandbox-docker-42")
    );
    // /workspace outlives the pod on a claim of its own (SME-32); the
    // socket dies with it.
    let workspace = volumes.iter().find(|v| v.name == "workspace").expect("workspace volume");
    assert_eq!(
        workspace.persistent_volume_claim.as_ref().map(|p| p.claim_name.as_str()),
        Some("sandbox-workspace-42")
    );
    let sock = volumes.iter().find(|v| v.name == "docker-sock").expect("docker-sock volume");
    assert!(sock.empty_dir.is_some(), "docker-sock should be an emptyDir");
}

#[test]
fn test_pod_spec_ephemeral_storage_is_empty_dirs() {
    let docker = DockerSidecar {
        storage: PodStorage::Ephemeral,
        ..docker_for_conversation(42)
    };
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker, &[], TEST_INSTANCE);
    let volumes = pod.spec.and_then(|s| s.volumes).expect("pod should have volumes");
    for name in ["docker-data", "workspace"] {
        let v = volumes.iter().find(|v| v.name == name).expect(name);
        assert!(v.empty_dir.is_some(), "{name}");
        assert!(v.persistent_volume_claim.is_none(), "{name}");
    }
}

#[test]
fn test_pod_spec_mounts_user_volumes_into_both_containers_at_the_same_path() {
    let volumes = vec![db::SandboxVolume {
        id: 7,
        name: "cache".to_string(),
        mount_path: "/data/cache".to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
    }];
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &volumes, TEST_INSTANCE);
    let spec = pod.spec.expect("pod should have a spec");
    let docker = container(&spec.init_containers, "docker");
    let main = Some(spec.containers.clone());
    let sandbox = container(&main, "sandbox");
    // So `docker run -v /data/cache:/c` sees the same files the model does.
    assert_eq!(mount_path_of(docker, "volume-7"), Some("/data/cache"));
    assert_eq!(mount_path_of(sandbox, "volume-7"), Some("/data/cache"));
}

#[test]
fn test_pod_spec_labels_a_pod_with_its_conversation_only_when_it_uses_the_claim() {
    let labelled = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    assert_eq!(
        labelled.metadata.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)).map(String::as_str),
        Some("42"),
        "create_pod finds a conversation's still-stopping pods by this label"
    );
    let ephemeral = DockerSidecar {
        storage: PodStorage::Ephemeral,
        ..docker_for_conversation(42)
    };
    let unlabelled = build_pod_spec("sandbox-1", "1Gi", &ephemeral, &[], TEST_INSTANCE);
    assert!(unlabelled.metadata.labels.and_then(|l| l.get(CONVERSATION_LABEL).cloned()).is_none());
}

/// SME-115: every object smelt creates says which database it belongs
/// to, so another smelt server sharing the namespace can leave it alone.
#[test]
fn test_every_pod_and_claim_carries_its_instance() {
    let instance = "0b7f3d1c-3a60-4c1e-9d55-2f0e1c7a9b42";
    let label = |meta: &ObjectMeta| meta.labels.as_ref().and_then(|l| l.get(INSTANCE_LABEL)).cloned();
    let ephemeral = DockerSidecar {
        storage: PodStorage::Ephemeral,
        ..docker_for_conversation(42)
    };
    for docker in [docker_for_conversation(42), ephemeral] {
        let pod = build_pod_spec("sandbox-1", "1Gi", &docker, &[], instance);
        assert_eq!(label(&pod.metadata).as_deref(), Some(instance), "a pod on {:?}", docker.storage);
    }
    for (_, claim) in conversation_pvc_specs(42, instance) {
        assert_eq!(label(&claim.metadata).as_deref(), Some(instance), "{:?}", claim.metadata.name);
        assert_eq!(
            claim.metadata.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)).map(String::as_str),
            Some("42"),
            "the conversation label stays"
        );
    }
    let volume = build_volume_pvc_spec(7, instance);
    assert_eq!(label(&volume.metadata).as_deref(), Some(instance));
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

#[test]
fn test_ownership_tells_ours_from_unlabelled_and_foreign() {
    assert_eq!(ownership(&meta_with_instance(Some("ours")), "ours"), Ownership::Ours);
    assert_eq!(ownership(&meta_with_instance(None), "ours"), Ownership::Unlabelled);
    assert_eq!(ownership(&ObjectMeta::default(), "ours"), Ownership::Unlabelled, "no labels at all");
    assert_eq!(ownership(&meta_with_instance(Some("theirs")), "ours"), Ownership::Foreign);
    // An instance that was never read must not match an empty label.
    assert_eq!(ownership(&meta_with_instance(Some("")), ""), Ownership::Foreign);
}

fn claim(name: &str, conversation: Option<&str>) -> PersistentVolumeClaim {
    claim_of(name, conversation, Some(TEST_INSTANCE))
}

fn claim_of(name: &str, conversation: Option<&str>, instance: Option<&str>) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            uid: Some(format!("uid-{name}")),
            labels: Some(
                conversation
                    .map(|c| (CONVERSATION_LABEL.to_string(), c.to_string()))
                    .into_iter()
                    .chain(instance.map(|i| (INSTANCE_LABEL.to_string(), i.to_string())))
                    .collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn names(orphans: &[OrphanedClaim]) -> Vec<&str> {
    orphans.iter().map(|o| o.name.as_str()).collect()
}

#[test]
fn test_orphaned_docker_claims_are_those_whose_conversation_is_gone() {
    let claims = vec![
        claim("sandbox-docker-1", Some("1")),
        claim("sandbox-docker-2", Some("2")),
        // A sandbox volume's claim has no conversation label.
        claim("sandbox-volume-3", None),
        claim("sandbox-docker-x", Some("not-a-number")),
    ];
    let live = std::collections::HashSet::from([1]);
    let orphans = orphaned_docker_claims(&claims, &live, TEST_INSTANCE);
    assert_eq!(names(&orphans), vec!["sandbox-docker-2"]);
    assert_eq!(orphans[0].conversation_id, 2);
    assert_eq!(orphans[0].uid, "uid-sandbox-docker-2", "deleted only while it's still this object");
    // A conversation has two claims (Docker data and /workspace): both go.
    let both = vec![
        claim("sandbox-docker-4", Some("4")),
        claim("sandbox-workspace-4", Some("4")),
    ];
    assert_eq!(
        names(&orphaned_docker_claims(&both, &live, TEST_INSTANCE)),
        vec!["sandbox-docker-4", "sandbox-workspace-4"]
    );
}

/// SME-115, the bug: a server on an empty database (a check server's
/// scratch one) took every conversation's claims in the namespace for
/// orphans. Only its own are; one from before the fix (no instance) and
/// another database's never are.
#[test]
fn test_only_our_own_claims_are_ever_orphans() {
    let claims = vec![
        claim_of("sandbox-workspace-1528", Some("1528"), None),
        claim_of("sandbox-docker-1528", Some("1528"), Some("the-dev-database")),
        claim_of("sandbox-workspace-7", Some("7"), Some("a-scratch-database")),
    ];
    let nothing_live = std::collections::HashSet::new();
    assert_eq!(
        names(&orphaned_docker_claims(&claims, &nothing_live, "a-scratch-database")),
        vec!["sandbox-workspace-7"]
    );
    assert!(orphaned_docker_claims(&claims, &nothing_live, "yet-another").is_empty());
    assert!(orphaned_docker_claims(&claims, &nothing_live, "").is_empty(), "an unread instance owns nothing");
}

#[test]
fn test_pod_limit_overrides_replace_only_the_limits_they_name() {
    let overrides = PodLimitOverrides {
        memory: Some("3Gi".to_string()),
        ..Default::default()
    };
    let (memory, docker) = overrides.resolve(7);
    assert_eq!(memory, "3Gi");
    assert_eq!(docker.memory, default_docker_memory_limit());
    assert_eq!(docker.storage, PodStorage::Conversation(7));
}

fn pod_with_docker_status(restarts: i32, last_reason: Option<&str>) -> Pod {
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
    };
    Pod {
        status: Some(PodStatus {
            init_container_statuses: Some(vec![ContainerStatus {
                name: "docker".to_string(),
                restart_count: restarts,
                last_state: last_reason.map(|reason| ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        reason: Some(reason.to_string()),
                        exit_code: 137,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn test_docker_restart_to_report_ignores_a_pod_whose_docker_never_restarted() {
    assert_eq!(docker_restart_to_report(&Pod::default(), 0), None);
    assert_eq!(docker_restart_to_report(&pod_with_docker_status(0, None), 0), None);
}

#[test]
fn test_docker_restart_to_report_gives_the_new_count_and_kubernetes_reason() {
    assert_eq!(
        docker_restart_to_report(&pod_with_docker_status(1, Some("OOMKilled")), 0),
        Some((1, Some("OOMKilled".to_string())))
    );
    assert_eq!(
        docker_restart_to_report(&pod_with_docker_status(3, Some("Error")), 2),
        Some((3, Some("Error".to_string())))
    );
}

#[test]
fn test_docker_restart_to_report_reports_each_restart_once() {
    assert_eq!(docker_restart_to_report(&pod_with_docker_status(1, Some("OOMKilled")), 1), None);
}

fn named(mut pod: Pod, name: &str) -> Pod {
    pod.metadata.name = Some(name.to_string());
    pod
}

#[test]
fn test_docker_restarts_are_reported_once_and_never_from_the_first_listing() {
    let mut seen = HashMap::new();
    // A restart from before smelt started, seen in the first listing.
    let old = named(pod_with_docker_status(2, Some("Error")), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &old, true), None);
    assert_eq!(note_docker_restarts(&mut seen, &old, false), None, "already known");

    let again = named(pod_with_docker_status(3, Some("OOMKilled")), "sandbox-7");
    assert_eq!(
        note_docker_restarts(&mut seen, &again, false),
        Some(DockerRestart::Report { pod_id: 7, reason: "OOMKilled".to_string() })
    );
    assert_eq!(note_docker_restarts(&mut seen, &again, false), None, "reported once");

    // Not smelt's pod.
    let other = named(pod_with_docker_status(1, Some("OOMKilled")), "something-else");
    assert_eq!(note_docker_restarts(&mut seen, &other, false), None);
}

/// A pod whose Docker sidecar has stopped but not restarted yet: the
/// update the kubelet publishes before the restart (SME-85's spike).
fn pod_with_docker_exited(restarts: i32, reason: &str) -> Pod {
    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateTerminated};
    let mut pod = pod_with_docker_status(restarts, None);
    if let Some(status) = pod
        .status
        .as_mut()
        .and_then(|s| s.init_container_statuses.as_mut())
        .and_then(|c| c.first_mut())
    {
        status.state = Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                reason: Some(reason.to_string()),
                exit_code: 137,
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    pod
}

/// The update that raises the count can lack `lastState`'s reason
/// (SME-85); the exit seen just before the restart still says why.
#[test]
fn test_a_restart_without_a_last_state_reason_takes_the_exit_seen_before_it() {
    let mut seen = HashMap::new();
    let running = named(pod_with_docker_status(0, None), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &running, true), None);
    let exited = named(pod_with_docker_exited(0, "OOMKilled"), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &exited, false), None, "not restarted yet");
    let restarted = named(pod_with_docker_status(1, None), "sandbox-7");
    assert_eq!(
        note_docker_restarts(&mut seen, &restarted, false),
        Some(DockerRestart::Report { pod_id: 7, reason: "OOMKilled".to_string() })
    );
}

/// With no update saying why, the restart is still reported, once,
/// after a re-read of the pod.
#[test]
fn test_a_restart_with_no_reason_anywhere_is_rechecked_once() {
    let mut seen = HashMap::new();
    let running = named(pod_with_docker_status(0, None), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &running, true), None);
    let restarted = named(pod_with_docker_status(1, None), "sandbox-7");
    assert_eq!(
        note_docker_restarts(&mut seen, &restarted, false),
        Some(DockerRestart::Recheck { pod_id: 7 })
    );
    let later = named(pod_with_docker_status(1, Some("OOMKilled")), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &later, false), None, "the re-read reports it");
}

/// A pod deleted while the watch was disconnected never gets a
/// `Delete`: the re-listing's end forgets it (SME-85 code review).
#[test]
fn test_a_relisting_forgets_pods_it_did_not_include() {
    let mut seen = HashMap::new();
    note_docker_restarts(&mut seen, &named(pod_with_docker_status(0, None), "sandbox-7"), true);
    note_docker_restarts(&mut seen, &named(pod_with_docker_status(0, None), "sandbox-8"), true);
    forget_unlisted_docker(&mut seen, &[8].into_iter().collect());
    assert_eq!(seen.keys().copied().collect::<Vec<_>>(), vec![8]);
}

/// SME-88: a restart that happened while the watch was disconnected
/// shows up in the re-listing after it reconnects, and is reported.
#[test]
fn test_a_restart_missed_while_disconnected_is_reported_from_the_relisting() {
    let mut seen = HashMap::new();
    let running = named(pod_with_docker_status(0, None), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &running, true), None);
    let relisted = named(pod_with_docker_status(1, Some("OOMKilled")), "sandbox-7");
    assert_eq!(
        note_docker_restarts(&mut seen, &relisted, true),
        Some(DockerRestart::Report { pod_id: 7, reason: "OOMKilled".to_string() })
    );
    assert_eq!(note_docker_restarts(&mut seen, &relisted, false), None, "reported once");
}

/// A pod first seen in a listing, before smelt started or while it was
/// disconnected, has nothing to compare with: only remembered.
#[test]
fn test_a_pod_first_seen_in_a_listing_is_only_remembered() {
    let mut seen = HashMap::new();
    let known = named(pod_with_docker_status(0, None), "sandbox-7");
    note_docker_restarts(&mut seen, &known, true);
    let new_pod = named(pod_with_docker_status(2, Some("OOMKilled")), "sandbox-8");
    assert_eq!(note_docker_restarts(&mut seen, &new_pod, true), None);
}

/// A first listing whose sidecar has already stopped again: its count
/// is a starting point, and the exit it shows explains the restart to
/// come (SME-85's second code review).
#[test]
fn test_an_exit_in_a_first_listing_explains_the_next_restart() {
    let mut seen = HashMap::new();
    let listed = named(pod_with_docker_exited(2, "OOMKilled"), "sandbox-7");
    assert_eq!(note_docker_restarts(&mut seen, &listed, true), None);
    let restarted = named(pod_with_docker_status(3, None), "sandbox-7");
    assert_eq!(
        note_docker_restarts(&mut seen, &restarted, false),
        Some(DockerRestart::Report { pod_id: 7, reason: "OOMKilled".to_string() })
    );
}

/// A pod that finished while the watch was disconnected isn't told its
/// Docker restarted: it's gone, and crash cleanup says so (SME-88 code
/// review).
#[test]
fn test_a_finished_pod_in_a_relisting_reports_no_docker_restart() {
    let mut seen = HashMap::new();
    note_docker_restarts(&mut seen, &named(pod_with_docker_status(0, None), "sandbox-7"), true);
    let mut failed = named(pod_with_docker_status(1, Some("OOMKilled")), "sandbox-7");
    if let Some(status) = failed.status.as_mut() {
        status.phase = Some("Failed".to_string());
    }
    assert_eq!(note_docker_restarts(&mut seen, &failed, true), None);
}

/// An exit remembered for one restart doesn't explain the next.
#[test]
fn test_a_remembered_exit_reason_explains_only_the_restart_after_it() {
    let mut seen = HashMap::new();
    let running = named(pod_with_docker_status(0, None), "sandbox-7");
    note_docker_restarts(&mut seen, &running, true);
    note_docker_restarts(&mut seen, &named(pod_with_docker_exited(0, "OOMKilled"), "sandbox-7"), false);
    note_docker_restarts(&mut seen, &named(pod_with_docker_status(1, None), "sandbox-7"), false);
    assert_eq!(
        note_docker_restarts(&mut seen, &named(pod_with_docker_status(2, None), "sandbox-7"), false),
        Some(DockerRestart::Recheck { pod_id: 7 })
    );
}

/// DB-only: the notice lands in the pod's conversation, saying what
/// was lost and what wasn't. The wake after it fails at once: the test
/// database has no model provider.
#[sqlx::test]
async fn test_report_docker_restart_tells_the_pods_conversation(pool: PgPool) {
    // The wake after the notice touches process-wide turn state keyed
    // by conversation id, as the chat tests' turns do.
    let _turns = crate::providers::test_support::lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create pod row");

    report_docker_restart(&pool, pod.id, Some("OOMKilled".to_string())).await;

    let saved = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let messages = db::list_messages(&pool, conversation.id).await.expect("list messages");
            if let Some(text) = messages.iter().find_map(|m| {
                m.blocks().ok()?.into_iter().find_map(|b| match b {
                    crate::anthropic::ContentBlock::Text { text } if text.contains("Docker") => {
                        Some(text)
                    }
                    _ => None,
                })
            }) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a Docker notice should be saved to the conversation");
    assert!(saved.contains("OOMKilled"), "{saved}");
    assert!(saved.contains("/workspace"), "should say what survived: {saved}");
}

#[test]
fn test_check_reachable_port_refuses_both_of_the_agents_ports() {
    assert!(matches!(check_reachable_port(AGENT_PORT), Err(TerminalError::AgentPort(8088))));
    assert!(matches!(check_reachable_port(RELAY_PORT), Err(TerminalError::AgentPort(8089))));
    assert!(check_reachable_port(3000).is_ok());
}

/// SME-77: a limit with no request is copied into the request, so an
/// idle pod reserved its whole 8Gi + 8Gi. Each container now asks for
/// no memory and has no CPU request or limit at all.
#[test]
fn test_pod_spec_reserves_nothing_and_limits_only_memory() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    let spec = pod.spec.expect("a pod spec");
    let containers = spec.containers.iter().chain(spec.init_containers.iter().flatten());
    for container in containers {
        let resources = container.resources.clone().unwrap_or_default();
        let limits = resources.limits.unwrap_or_default();
        let requests = resources.requests.unwrap_or_default();
        assert!(limits.contains_key("memory"), "{}: a memory limit", container.name);
        assert_eq!(
            requests.get("memory").map(|q| q.0.as_str()),
            Some("0"),
            "{}: an explicit zero memory request, not the limit copied over",
            container.name
        );
        assert!(!limits.contains_key("cpu") && !requests.contains_key("cpu"), "{}: no CPU", container.name);
    }
}

#[test]
fn test_pod_container_limits_include_the_docker_sidecar() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    assert_eq!(
        pod_container_limits(&pod),
        (vec!["1Gi".to_string(), "2Gi".to_string()], Vec::<String>::new())
    );
}

#[test]
fn test_a_missing_stream_is_a_no_stream_error_naming_it() {
    let err = require_stream::<u8>(None, "the exec's stdout").expect_err("no stream should be an error");
    assert!(
        matches!(&err, SandboxError::NoStream(what) if what == "the exec's stdout"),
        "expected NoStream naming the stream, got {err:?}"
    );
    assert_eq!(err.to_string(), "kube gave no stream for the exec's stdout");
    assert_eq!(
        require_stream(Some(7u8), "the exec's stdout").expect("a stream kube gave is passed through"),
        7
    );
}

#[test]
fn test_an_io_error_says_it_was_on_an_exec_or_port_forward_stream() {
    let err = SandboxError::Io(std::io::Error::other("broken pipe"));
    assert_eq!(err.to_string(), "I/O error on an exec's or port-forward's stream: broken pipe");
}

#[test]
fn test_a_metrics_request_for_an_invalid_namespace_is_an_error_not_a_panic() {
    let result = pod_metrics_request("bad namespace");
    assert!(
        matches!(result, Err(SandboxError::Kube(kube::Error::HttpError(_)))),
        "a space isn't a URI character: expected an HttpError, got {result:?}"
    );
}

#[test]
fn test_the_metrics_request_names_the_namespaces_pods() {
    let request = pod_metrics_request(NAMESPACE).expect("the namespace makes a valid request");
    assert_eq!(request.method(), http::Method::GET);
    assert_eq!(
        request.uri().to_string(),
        format!("/apis/metrics.k8s.io/v1beta1/namespaces/{NAMESPACE}/pods")
    );
}

#[test]
fn test_docker_pvc_spec_is_named_labelled_and_sized_for_the_conversation() {
    let [(pvc_name, pvc), (workspace_name, workspace)] = conversation_pvc_specs(42, TEST_INSTANCE);
    assert_eq!(workspace_name, "sandbox-workspace-42", "each claim's name comes beside its spec");
    assert_eq!(workspace.metadata.name.as_deref(), Some(workspace_name.as_str()));
    assert_eq!(workspace.metadata.labels, pvc.metadata.labels);
    assert_eq!(pvc_name, "sandbox-docker-42", "each claim's name comes beside its spec");
    assert_eq!(pvc.metadata.name.as_deref(), Some(pvc_name.as_str()));
    assert_eq!(
        pvc.metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(CONVERSATION_LABEL))
            .map(String::as_str),
        Some("42"),
        "the startup sweep finds a claim's conversation by this label"
    );
    let spec = pvc.spec.expect("claim should have a spec");
    assert_eq!(spec.access_modes, Some(vec!["ReadWriteOnce".to_string()]));
    let requested = spec
        .resources
        .and_then(|r| r.requests)
        .and_then(|r| r.get("storage").cloned());
    assert_eq!(requested, Some(Quantity(default_docker_storage_size())));
}

#[test]
fn test_resolve_mount_path_expands_bare_tilde() {
    assert_eq!(resolve_mount_path("~"), "/home/sandbox");
}

#[test]
fn test_resolve_mount_path_expands_tilde_slash_prefix() {
    assert_eq!(resolve_mount_path("~/.ssh"), "/home/sandbox/.ssh");
}

#[test]
fn test_resolve_mount_path_leaves_an_absolute_path_unchanged() {
    assert_eq!(resolve_mount_path("/data/cache"), "/data/cache");
}

#[test]
fn test_resolve_mount_path_leaves_a_non_slash_tilde_prefix_unchanged() {
    // No `~otheruser` support — there's only ever one sandbox user —
    // so this passes through as a literal path rather than expanding.
    assert_eq!(resolve_mount_path("~foo"), "~foo");
}

#[test]
fn test_volume_mounts_for_produces_a_pvc_backed_pair_per_volume() {
    let volumes = vec![
        db::SandboxVolume {
            id: 7,
            name: "ssh-key".to_string(),
            mount_path: "/home/sandbox/.ssh".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        },
        db::SandboxVolume {
            id: 12,
            name: "build-cache".to_string(),
            mount_path: "/data/cache".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        },
    ];

    let (vols, mounts) = volume_mounts_for(&volumes);
    assert_eq!(vols.len(), 2);
    assert_eq!(mounts.len(), 2);

    assert_eq!(vols[0].name, "volume-7");
    assert_eq!(
        vols[0]
            .persistent_volume_claim
            .as_ref()
            .expect("should be PVC-backed")
            .claim_name,
        "sandbox-volume-7"
    );
    assert_eq!(mounts[0].name, "volume-7");
    assert_eq!(mounts[0].mount_path, "/home/sandbox/.ssh");

    assert_eq!(vols[1].name, "volume-12");
    assert_eq!(
        vols[1]
            .persistent_volume_claim
            .as_ref()
            .expect("should be PVC-backed")
            .claim_name,
        "sandbox-volume-12"
    );
    assert_eq!(mounts[1].name, "volume-12");
    assert_eq!(mounts[1].mount_path, "/data/cache");
}

#[test]
fn test_check_pod_guard_allows_when_no_live_pod_exists() {
    assert!(check_pod_guard(&[]).is_ok());
}

#[test]
fn test_check_pod_guard_refuses_when_a_live_pod_already_exists() {
    let result = check_pod_guard(&[fake_pod(1)]);
    assert!(
        matches!(result, Err(SandboxError::PodAlreadyExists)),
        "expected PodAlreadyExists, got {result:?}"
    );
}

#[test]
fn test_resolve_pod_id_returns_the_one_live_pod() {
    assert_eq!(resolve_pod_id(&[fake_pod(42)]).expect("should resolve"), 42);
}

#[test]
fn test_resolve_pod_id_errors_with_no_pod_when_none_live() {
    let result = resolve_pod_id(&[]);
    assert!(
        matches!(result, Err(TerminalError::NoPod)),
        "expected NoPod, got {result:?}"
    );
}

use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
    PodStatus,
};

fn pod_waiting(reason: &str, message: &str) -> Pod {
    Pod {
        status: Some(PodStatus {
            phase: Some("Pending".to_string()),
            container_statuses: Some(vec![ContainerStatus {
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some(reason.to_string()),
                        message: Some(message.to_string()),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn test_pod_startup_failure_reports_a_container_that_cannot_start() {
    let failure = pod_startup_failure(&pod_waiting(
        "ImagePullBackOff",
        "Back-off pulling image \"smelt-sandbox:missing\"",
    ))
    .expect("a container stuck in ImagePullBackOff is a startup failure");
    assert!(failure.contains("ImagePullBackOff"), "got {failure}");
    assert!(failure.contains("Back-off pulling image"), "got {failure}");
}

/// The kubelet's own words for an image that isn't on the node, with
/// `imagePullPolicy: Never`: the pod would wait out the whole running
/// timeout, so it's a startup failure at once, naming the image and what
/// to do, which the model can't do itself (SME-121). The container's
/// status names the image too.
fn pod_never_pulling(image: &str) -> Pod {
    let message = format!("Container image \"{image}\" is not present with pull policy of Never");
    let mut pod = pod_waiting("ErrImageNeverPull", &message);
    for status in pod.status.as_mut().expect("status").container_statuses.iter_mut().flatten() {
        status.image = image.to_string();
    }
    pod
}

#[test]
fn test_a_missing_image_is_a_startup_failure_that_says_how_to_build_it() {
    let failure = pod_startup_failure(&pod_never_pulling(OWN_SANDBOX_IMAGE))
        .expect("an image that isn't on the node is a startup failure");
    assert!(failure.contains(OWN_SANDBOX_IMAGE), "names the image: {failure}");
    assert!(failure.contains("with scripts/build-sandbox-image.sh"), "says how to build it: {failure}");
    assert!(!failure.contains("--latest"), "{failure}");
    assert!(failure.contains("tell the user"), "the model can't build it itself: {failure}");
}

/// Review 1: the script builds only this tree's own image (and moves
/// `:latest` only with `--latest`), so advice for any other image says so.
#[test]
fn test_a_missing_image_the_script_doesnt_build_gets_its_own_advice() {
    let latest = pod_startup_failure(&pod_never_pulling("smelt-sandbox:latest")).expect("a startup failure");
    assert!(latest.contains("scripts/build-sandbox-image.sh --latest"), "{latest}");

    let other = "registry.example/team/sandbox:1";
    let failure = pod_startup_failure(&pod_never_pulling(other)).expect("a startup failure");
    assert!(failure.contains(other), "{failure}");
    assert!(failure.contains("SANDBOX_IMAGE or SANDBOX_DOCKER_IMAGE"), "{failure}");
    assert!(failure.contains("import"), "{failure}");
    assert!(!failure.contains("with scripts/build-sandbox-image.sh"), "{failure}");
}

/// Review 2: a container that ran before has its status's image from the
/// runtime (another tag of it, or a bare `sha256:` id), so the advice goes
/// by the image the pod's spec names for that container.
#[test]
fn test_the_missing_image_advice_goes_by_the_pods_spec() {
    let mut pod = pod_never_pulling("sha256:0123456789abcdef");
    for status in pod.status.as_mut().expect("status").container_statuses.iter_mut().flatten() {
        status.name = "sandbox".to_string();
    }
    pod.spec = Some(PodSpec {
        containers: vec![Container {
            name: "sandbox".to_string(),
            image: Some(OWN_SANDBOX_IMAGE.to_string()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let failure = pod_startup_failure(&pod).expect("a startup failure");
    assert!(failure.contains("with scripts/build-sandbox-image.sh"), "{failure}");
}

/// The Docker sidecar is an init container, delivered by the same script:
/// a missing `docker:29-dind` fails the same way.
#[test]
fn test_a_sidecar_whose_image_is_missing_is_a_startup_failure() {
    let mut pod = pod_never_pulling("docker.io/library/docker:29-dind");
    let status = pod.status.as_mut().expect("status");
    status.init_container_statuses = status.container_statuses.take();
    let failure = pod_startup_failure(&pod).expect("a sidecar that can't start is a startup failure");
    assert!(failure.contains("with scripts/build-sandbox-image.sh"), "{failure}");

    // A sidecar crashing while it starts is left to its own restarts.
    let mut crashing = pod_waiting("CrashLoopBackOff", "back-off restarting failed container");
    let status = crashing.status.as_mut().expect("status");
    status.init_container_statuses = status.container_statuses.take();
    assert_eq!(pod_startup_failure(&crashing), None);
}

/// What a pod waiting on a volume claim that doesn't exist actually
/// reports (seen on this cluster): no container status yet, just an
/// unschedulable `PodScheduled` condition.
fn pod_unschedulable(message: &str) -> Pod {
    Pod {
        status: Some(PodStatus {
            phase: Some("Pending".to_string()),
            conditions: Some(vec![k8s_openapi::api::core::v1::PodCondition {
                type_: "PodScheduled".to_string(),
                status: "False".to_string(),
                reason: Some("Unschedulable".to_string()),
                message: Some(message.to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn test_pod_pending_detail_explains_an_unschedulable_pod() {
    let detail = pod_pending_detail(&pod_unschedulable(
        "0/1 nodes are available: persistentvolumeclaim \"sandbox-volume-7\" not found.",
    ))
    .expect("an unschedulable pod has a reason to report");
    assert!(detail.contains("persistentvolumeclaim"), "got {detail}");
}

#[test]
fn test_pod_pending_detail_is_none_for_a_running_pod() {
    assert_eq!(pod_pending_detail(&pod_with_phase("Running")), None);
}

#[test]
fn test_pod_startup_failure_is_none_while_a_pod_is_still_starting() {
    assert_eq!(pod_startup_failure(&pod_waiting("ContainerCreating", "")), None);
    assert_eq!(pod_startup_failure(&pod_with_phase("Pending")), None);
    assert_eq!(pod_startup_failure(&pod_with_phase("Running")), None);
}

fn pod_with_phase(phase: &str) -> Pod {
    Pod {
        status: Some(PodStatus {
            phase: Some(phase.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn failed_pod_with_container_state(
    state: Option<ContainerState>,
    last_state: Option<ContainerState>,
) -> Pod {
    Pod {
        status: Some(PodStatus {
            phase: Some("Failed".to_string()),
            container_statuses: Some(vec![ContainerStatus {
                state,
                last_state,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn terminated(reason: Option<&str>) -> ContainerState {
    ContainerState {
        terminated: Some(ContainerStateTerminated {
            reason: reason.map(str::to_string),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// SME-51 B9: since the Docker sidecar (SME-33) the pod stays Running
/// after the sandbox container is killed, while dockerd shuts down. The
/// sandbox container's own end is the death, with its reason.
#[test]
fn test_decide_pod_death_reason_sandbox_container_ended_while_the_sidecar_runs() {
    let pod = Pod {
        status: Some(PodStatus {
            phase: Some("Running".to_string()),
            container_statuses: Some(vec![ContainerStatus {
                name: "sandbox".to_string(),
                state: Some(terminated(Some("OOMKilled"))),
                ..Default::default()
            }]),
            init_container_statuses: Some(vec![ContainerStatus {
                name: "docker".to_string(),
                state: Some(ContainerState {
                    running: Some(Default::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(decide_pod_death_reason(Some(pod)), Some(Some("OOMKilled".to_string())));
}

#[test]
fn test_decide_pod_death_reason_pod_gone_is_confirmed_dead_with_no_reason() {
    assert_eq!(decide_pod_death_reason(None), Some(None));
}

#[test]
fn test_decide_pod_death_reason_failed_with_reason_in_state() {
    let pod = failed_pod_with_container_state(Some(terminated(Some("OOMKilled"))), None);
    assert_eq!(
        decide_pod_death_reason(Some(pod)),
        Some(Some("OOMKilled".to_string()))
    );
}

#[test]
fn test_decide_pod_death_reason_failed_with_reason_only_in_last_state() {
    // `state` present but not itself `terminated` (e.g. mid-restart) —
    // the restart_policy: Never quirk this project actually hits puts
    // the reason in `state`, not `last_state`, but a differently-
    // configured pod could still show up this way.
    let pod = failed_pod_with_container_state(
        Some(ContainerState::default()),
        Some(terminated(Some("Error"))),
    );
    assert_eq!(
        decide_pod_death_reason(Some(pod)),
        Some(Some("Error".to_string()))
    );
}

#[test]
fn test_decide_pod_death_reason_failed_falls_back_to_pod_status_reason_without_container_status()
 {
    let pod = Pod {
        status: Some(PodStatus {
            phase: Some("Failed".to_string()),
            reason: Some("Evicted".to_string()),
            container_statuses: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        decide_pod_death_reason(Some(pod)),
        Some(Some("Evicted".to_string()))
    );
}

#[test]
fn test_decide_pod_death_reason_failed_with_no_reason_available_anywhere() {
    let pod = Pod {
        status: Some(PodStatus {
            phase: Some("Failed".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(decide_pod_death_reason(Some(pod)), Some(None));
}

#[test]
fn test_decide_pod_death_reason_running_is_inconclusive() {
    assert_eq!(
        decide_pod_death_reason(Some(pod_with_phase("Running"))),
        None
    );
}

#[test]
fn test_decide_pod_death_reason_pending_is_inconclusive() {
    assert_eq!(
        decide_pod_death_reason(Some(pod_with_phase("Pending"))),
        None
    );
}

#[test]
fn test_decide_pod_death_reason_no_status_at_all_is_inconclusive() {
    assert_eq!(decide_pod_death_reason(Some(Pod::default())), None);
}

/// DB-only — `create_pod`'s guard must fire *before* it ever touches
/// the process-global `MANAGER` (unset here, since this test doesn't
/// need a real cluster), so a refusal shows up as `PodAlreadyExists`,
/// not a panic from `get()`.
#[sqlx::test]
async fn test_create_pod_refuses_before_touching_the_manager_when_a_live_pod_exists(
    pool: PgPool,
) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create sandbox pod");

    let result = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await;
    assert!(
        matches!(result, Err(SandboxError::PodAlreadyExists)),
        "expected PodAlreadyExists, got {result:?}"
    );
}

/// DB-only: with no live pod there's nothing to connect to, and the
/// manager is never touched.
#[sqlx::test]
async fn test_open_pod_port_without_a_live_pod_is_no_pod(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let result = open_pod_target(&pool, conversation.id, PodHost::Localhost, 3000).await;
    assert!(matches!(result, Err(TerminalError::NoPod)), "expected NoPod");
}

/// DB-only: the sandbox agent's port is refused before anything is
/// looked up, so no route can reach it.
#[sqlx::test]
async fn test_open_pod_port_refuses_the_sandbox_agents_own_port(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let result = open_pod_target(&pool, conversation.id, PodHost::Localhost, AGENT_PORT).await;
    assert!(matches!(result, Err(TerminalError::AgentPort(_))), "the agent's port must be refused");
}

/// Sends a bare HTTP request down `stream` and returns whatever comes
/// back before the response stops arriving — never waits for the end
/// of the stream, which a port-forward reports about a second late.
async fn http_get_over(stream: &mut Box<dyn PodIo>, path: &str) -> String {
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .expect("write the request");
    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
        Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).to_string(),
        Ok(Err(e)) => panic!("read failed: {e}"),
        Err(_) => panic!("no response within 10s"),
    }
}

/// Real cluster: `open_pod_target` reaches a server inside the
/// conversation's own pod — one bound to `127.0.0.1` there, as dev
/// servers are by default — but never the sandbox agent's own port. A
/// conversation with no pod of its own can't borrow another's, and a
/// port nothing listens on ends at once with no bytes.
#[sqlx::test]
async fn test_open_pod_port_reaches_the_conversations_own_pod(pool: PgPool) {
    // Pod names are `sandbox-{pod_id}`, and every `#[sqlx::test]`
    // database restarts ids at 1 — start this one's ids far from the
    // low range.
    let first_id = (uuid_like().parse::<u128>().unwrap() % 1_000_000_000) as i64 + 1_000_000;
    sqlx::query("SELECT setval(pg_get_serial_sequence('sandbox_pods', 'id'), $1)")
        .bind(first_id)
        .execute(&pool)
        .await
        .expect("move the pod id sequence");
    // Its own client and manager, never the process-global one: a
    // manager set here would die with this test's runtime and break
    // every later test that uses `get()` (seen as `Kube(Service(Closed))`
    // on SME-42). The pod is created
    // under the name the database row gives it, as `create_pod` would.
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());

    let with_pod = db::create_conversation(&pool).await.expect("create conversation");
    let without_pod = db::create_conversation(&pool).await.expect("create conversation");
    let row = db::create_sandbox_pod(&pool, with_pod.id).await.expect("create the pod's row");
    // As this database's (SME-115): a port-forward goes only into a pod
    // labelled with the database's instance.
    let instance = db::smelt_instance(&pool).await.expect("instance");
    let docker = DockerSidecar { memory: "512Mi".to_string(), storage: PodStorage::Ephemeral };
    let sandbox = manager
        .create_with_docker(&row.id.to_string(), "128Mi", &docker, &[], &instance)
        .await
        .expect("create the pod");
    let open = |conversation_id: i64, port: u16| {
        let (client, pool) = (client.clone(), pool.clone());
        async move { open_pod_port_with(&pool, conversation_id, PodHost::Localhost, port, move || Ok(client)).await }
    };
    // Python's server speaks HTTP/1.0 and closes after every response,
    // so the preview below also meets an upstream that's gone between
    // requests on one kept-alive connection.
    let started_server = sandbox
        .exec(&["sh", "-c", "setsid nohup python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 </dev/null &"])
        .await;

    // The server binds a moment after it starts. Everything is gathered
    // before asserting, so a failure still deletes the pod below
    // instead of leaving it in the cluster.
    let mut reply = Err("never tried".to_string());
    for _ in 0..50 {
        reply = match open(with_pod.id, 8000).await {
            Ok(mut stream) => Ok(http_get_over(&mut stream, "/no-such-path").await),
            Err(e) => Err(e.to_string()),
        };
        if reply.as_ref().is_ok_and(|r| !r.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The user's preview proxy over the same real port-forward: three
    // requests on one kept-alive connection. Each would take over a
    // second if anything waited for the port-forward's late end of
    // stream (see `open_pod_target`).
    let through_preview = {
        use crate::egress_proxy::{DialFuture, SandboxDial};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let preview_addr = listener.local_addr().expect("local addr");
        let template = crate::preview::PreviewTemplate::parse(&format!(
            "http://{{port}}-{{conversation}}.preview.localhost:{}",
            preview_addr.port()
        ))
        .expect("a template");
        let (dial_client, dial_pool) = (client.clone(), pool.clone());
        let dial_for: crate::preview::DialFor = Arc::new(move |conversation| {
            let (client, pool) = (dial_client.clone(), dial_pool.clone());
            Arc::new(move |_host, port| {
                let (client, pool) = (client.clone(), pool.clone());
                Box::pin(async move {
                    open_pod_port_with(&pool, conversation, PodHost::Localhost, port, move || Ok(client))
                        .await
                        .map_err(|e| crate::egress_proxy::DialError::from(e.to_string()))
                }) as DialFuture
            }) as SandboxDial
        });
        let server = tokio::spawn(crate::preview::serve(listener, template.clone(), dial_for, None));
        let http = reqwest::Client::builder()
            .resolve(&format!("8000-{}.preview.localhost", with_pod.id), preview_addr)
            .build()
            .expect("a client");
        let started = std::time::Instant::now();
        let mut statuses = Vec::new();
        for _ in 0..3 {
            match http.get(format!("{}/no-such-path", template.url_for(with_pod.id, PodHost::Localhost, 8000))).send().await {
                Ok(response) => statuses.push(response.status().as_u16()),
                Err(_) => statuses.push(0),
            }
        }
        server.abort();
        (statuses, started.elapsed())
    };
    let (probe_client, probe_pool) = (client.clone(), pool.clone());
    let server_listening =
        pod_port_is_listening_with(&probe_pool, with_pod.id, PodHost::Localhost, 8000, || Ok(probe_client.clone())).await;
    let agent_port_result = open(with_pod.id, AGENT_PORT).await;
    let unused_listening = pod_port_is_listening_with(&probe_pool, with_pod.id, PodHost::Localhost, 9, || Ok(probe_client.clone())).await;
    let without_pod_result = open(without_pod.id, 8000).await;
    let closed_reply = match open(with_pod.id, 9).await {
        Ok(mut stream) => Ok(http_get_over(&mut stream, "/").await),
        Err(e) => Err(e.to_string()),
    };
    pods_api(&client).delete(&pod_name(row.id), &immediate_delete_params()).await.ok();
    std::mem::forget(sandbox);

    started_server.expect("start a server in the pod");
    let reply = reply.expect("open the server's port");
    assert!(reply.starts_with("HTTP/1.0 404"), "expected the server's 404, got {reply:?}");
    assert!(
        matches!(agent_port_result, Err(TerminalError::AgentPort(_))),
        "the sandbox agent's port must be refused even with a pod"
    );
    assert!(
        matches!(without_pod_result, Err(TerminalError::NoPod)),
        "a conversation without a pod must not reach any pod"
    );
    assert_eq!(closed_reply.expect("open an unused port"), "", "nothing listens on port 9");
    assert!(server_listening.expect("probe the server's port"), "the server listens on 8000");
    assert!(!unused_listening.expect("probe port 9"), "nothing listens on port 9");
    let (statuses, elapsed) = through_preview;
    assert_eq!(statuses, vec![404, 404, 404], "the server's 404 through the preview each time");
    assert!(elapsed < Duration::from_secs(2), "three preview requests took {elapsed:?}");
}

#[test]
fn test_validate_mount_path_accepts_absolute_paths() {
    for path in ["/data", "/home/dev/.cache", "/workspace/project"] {
        assert!(validate_mount_path(path).is_ok(), "{path} should be accepted");
    }
}

#[test]
fn test_validate_mount_path_rejects_relative_empty_and_escaping_paths() {
    for path in ["relative/path", "data", "", "/", "/data/../etc", "../x"] {
        assert!(validate_mount_path(path).is_err(), "{path:?} should be rejected");
    }
}

/// A relative mount path is mounted into every sandbox pod, and a
/// container can't start with one — so it must be refused before it's
/// saved (and before the cluster is touched: MANAGER is unset here).
#[sqlx::test]
async fn test_create_volume_refuses_a_relative_mount_path_before_saving_it(pool: PgPool) {
    let result = create_volume(&pool, "cache", "relative/path").await;
    assert!(
        matches!(result, Err(SandboxError::InvalidMountPath(_))),
        "expected InvalidMountPath, got {result:?}"
    );
    assert!(
        db::list_sandbox_volumes(&pool).await.expect("list volumes").is_empty(),
        "the refused volume was saved anyway"
    );
}

/// DB-only — no MANAGER touch, since `conversation_pod_id` never calls
/// `get()`.
#[sqlx::test]
async fn test_conversation_pod_id_resolves_the_live_pod_and_errors_with_no_pod_otherwise(
    pool: PgPool,
) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let before = conversation_pod_id(&pool, conversation.id).await;
    assert!(
        matches!(before, Err(TerminalError::NoPod)),
        "expected NoPod before any pod exists, got {before:?}"
    );

    let pod = db::create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create sandbox pod");
    let resolved = conversation_pod_id(&pool, conversation.id)
        .await
        .expect("should resolve");
    assert_eq!(resolved, pod.id);
}

/// The manager before `init()`: an error saying so, not a panic (SME-56).
/// Read from a cell of the test's own, so it holds whatever else runs.
#[test]
fn test_the_manager_before_init_is_not_initialized() {
    let cell = OnceLock::new();
    let result = manager_in(&cell);
    assert!(matches!(result, Err(SandboxError::NotInitialized)), "expected NotInitialized");
}

/// A client for a cluster that isn't there: nothing is sent until a
/// request is made, so it builds a manager without a cluster.
fn unreachable_client() -> kube::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = "http://127.0.0.1:9".parse().expect("a url");
    kube::Client::try_from(kube::Config::new(url)).expect("a client")
}

/// SME-94: a test's own manager is what `get()` returns on the test's
/// thread, where `#[sqlx::test]`'s current-thread runtime runs the test
/// and every task it spawns, and nowhere else.
#[tokio::test]
async fn test_a_test_manager_is_what_get_returns_on_its_own_thread_only() {
    let manager = use_test_manager(unreachable_client());
    let here = get().map(|got| std::ptr::eq(got, manager));
    let elsewhere = std::thread::spawn(|| matches!(get(), Err(SandboxError::NotInitialized)))
        .join()
        .expect("the other thread");
    assert!(matches!(here, Ok(true)), "get() on the test's thread isn't the test's manager");
    assert!(elsewhere, "another thread sees the test's manager");
}

pub(super) fn unique_session_id(label: &str) -> String {
    format!("test-{label}-{}", uuid_like())
}


/// A pod gets the user's SSH keys and commit identity where ssh and git
/// read them, and a reinstall after a key is deleted removes it
/// (SME-32).
#[tokio::test]
async fn test_git_files_reach_ssh_and_git_in_a_real_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let sandbox = manager
        .create(&unique_session_id("git-files"), "256Mi", &[])
        .await
        .expect("create pod");
    let shell = sandbox.shell();

    let checks = tokio::time::timeout(Duration::from_secs(120), async {
        let key = crate::git::generate_key("test-key");
        let identity = crate::git::GitIdentity {
            name: "Ada \"Countess\" Lovelace".to_string(),
            email: "ada@example.com".to_string(),
        };
        let files =
            crate::git::pod_git_files(&[("test-key".to_string(), key.private_key.clone())], &identity);
        crate::git::install_git_files(&shell, &files)
            .await
            .expect("install git files");

        let mode = sandbox
            .exec(&["stat", "-c", "%a %U", "/etc/smelt/keys/test-key"])
            .await
            .expect("exec stat");
        assert_eq!(mode.stdout.trim(), "600 sandbox", "key file mode and owner");

        // The file is the key, byte for byte: ssh derives the same public half.
        let derived = sandbox
            .exec(&["ssh-keygen", "-y", "-f", "/etc/smelt/keys/test-key"])
            .await
            .expect("exec ssh-keygen");
        let expected: Vec<&str> = key.public_key.split(' ').take(2).collect();
        let got: Vec<&str> = derived.stdout.trim().split(' ').take(2).collect();
        assert_eq!(got, expected, "derived public key");

        let ssh = sandbox
            .exec(&["ssh", "-G", "github.com"])
            .await
            .expect("exec ssh -G");
        assert!(
            ssh.stdout.contains("identityfile /etc/smelt/keys/test-key"),
            "ssh -G github.com: {}",
            ssh.stdout
        );
        let known = sandbox
            .exec(&["ssh-keygen", "-F", "github.com", "-f", "/etc/ssh/ssh_known_hosts"])
            .await
            .expect("exec ssh-keygen -F");
        assert_eq!(known.exit_code, 0, "github.com is a known host: {}", known.stdout);

        let name = sandbox
            .exec(&["git", "config", "user.name"])
            .await
            .expect("exec git config");
        assert_eq!(name.stdout.trim(), "Ada \"Countess\" Lovelace");

        // Reinstalling (a key added elsewhere, the identity saved)
        // never leaves the key missing, even for a moment: a push
        // running then would fail (SME-32 code review 2, finding 5).
        let watch = sandbox.exec(&[
            "sh",
            "-c",
            "for i in $(seq 1 300); do [ -e /etc/smelt/keys/test-key ] || echo missing; sleep 0.01; done",
        ]);
        let reinstall = async {
            for _ in 0..3 {
                crate::git::install_git_files(&shell, &files).await.expect("reinstall");
            }
        };
        let (watched, ()) = tokio::join!(watch, reinstall);
        let watched = watched.expect("exec watch");
        assert!(!watched.stdout.contains("missing"), "the key vanished during a reinstall");

        // A write that fails says why (SME-32 code review 7, finding 5).
        let locked = sandbox.exec(&["chmod", "500", "/etc/smelt/keys"]).await.expect("exec chmod");
        assert_eq!(locked.exit_code, 0);
        let failed = crate::git::install_git_files(&shell, &files).await.expect_err("keys dir not writable");
        assert!(failed.to_string().contains("Permission denied"), "{failed}");
        let unlocked = sandbox.exec(&["chmod", "700", "/etc/smelt/keys"]).await.expect("exec chmod");
        assert_eq!(unlocked.exit_code, 0);

        // The key is deleted: a reinstall without it removes the file.
        let files = crate::git::pod_git_files(&[], &identity);
        crate::git::install_git_files(&shell, &files)
            .await
            .expect("reinstall git files");
        let gone = sandbox
            .exec(&["test", "-e", "/etc/smelt/keys/test-key"])
            .await
            .expect("exec test");
        assert_eq!(gone.exit_code, 1, "a deleted key's file is removed");
        let ssh = sandbox
            .exec(&["ssh", "-G", "github.com"])
            .await
            .expect("exec ssh -G");
        assert!(!ssh.stdout.contains("/etc/smelt/keys/"), "{}", ssh.stdout);
    })
    .await;

    manager.delete(sandbox).await.expect("delete pod");
    checks.expect("checks finished within the timeout");
}

/// Sets up a bare repo at /tmp/origin.git inside `sandbox`'s pod, with
/// `main` holding an AGENTS.md and a `feature` branch on top. Returns
/// main's commit.
async fn make_origin_repo(sandbox: &Sandbox) -> String {
    let script = r#"set -e
            git init -q -b main /tmp/src && cd /tmp/src
            printf 'Run make test before committing.\n' > AGENTS.md
            git add AGENTS.md && git -c user.name=t -c user.email=t@t commit -qm first
            git branch feature && git checkout -q feature
            printf 'x\n' > feature.txt && mkdir web && printf 'Use pnpm.\n' > web/AGENTS.md
            git add feature.txt web/AGENTS.md
            git -c user.name=t -c user.email=t@t commit -qm feature && git checkout -q main
            git clone -q --bare /tmp/src /tmp/origin.git
            git rev-parse main"#;
    let made = sandbox.exec(&["sh", "-c", script]).await.expect("exec make origin");
    assert_eq!(made.exit_code, 0, "make origin repo: {}", made.stdout);
    made.stdout.trim().to_string()
}

#[tokio::test]
async fn test_clone_into_pod_checks_out_a_branch_and_reports_failures() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let sandbox = manager
        .create(&unique_session_id("git-clone"), "256Mi", &[])
        .await
        .expect("create pod");
    let shell = sandbox.shell();

    let checks = tokio::time::timeout(Duration::from_secs(120), async {
        let main_commit = make_origin_repo(&sandbox).await;

        let cloned = crate::git::clone_into_pod(&shell, "file:///tmp/origin.git", None, "origin")
            .await
            .expect("clone the default branch");
        assert_eq!(cloned.commit.as_deref(), Some(main_commit.as_str()));
        assert_eq!(cloned.branch, "main");
        let agents = sandbox
            .exec(&["cat", "/workspace/origin/AGENTS.md"])
            .await
            .expect("exec cat");
        assert_eq!(agents.stdout, "Run make test before committing.\n");

        let read = crate::git::read_instructions_file(&shell, "/workspace/origin", "/workspace/origin/AGENTS.md")
            .await
            .expect("read AGENTS.md")
            .expect("origin has an AGENTS.md");
        assert_eq!(read.content, "Run make test before committing.\n");
        assert_eq!(read.file_bytes, 33);
        assert_eq!(read.hash.len(), 64, "sha256 hex: {}", read.hash);
        let listed = crate::git::list_agents_files(&shell, "origin").await.expect("list");
        assert_eq!(listed, vec!["AGENTS.md".to_string()], "main has no nested files");

        let feature = crate::git::clone_into_pod(
            &shell,
            "file:///tmp/origin.git",
            Some("feature"),
            "origin-feature",
        )
        .await
        .expect("clone a branch");
        assert_eq!(feature.branch, "feature");
        assert_ne!(feature.commit.as_deref(), Some(main_commit.as_str()));
        let listed = crate::git::list_agents_files(&shell, "origin-feature").await.expect("list");
        assert_eq!(listed, vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()], "top-level first");

        let bare = sandbox
            .exec(&["git", "init", "-q", "/workspace/no-agents"])
            .await
            .expect("exec git init");
        assert_eq!(bare.exit_code, 0);
        let none = crate::git::read_instructions_file(&shell, "/workspace/no-agents", "/workspace/no-agents/AGENTS.md")
            .await
            .expect("read a missing file");
        assert_eq!(none, None);
        assert!(crate::git::list_agents_files(&shell, "no-agents").await.expect("list").is_empty());

        // Bytes that aren't UTF-8 (or a 1 MiB cut through a character)
        // still load, with the bad bytes replaced (SME-32 code review,
        // finding 6).
        let bad = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/bad-bytes && printf 'Use \\377 tabs.\\n' > /workspace/bad-bytes/AGENTS.md"])
            .await
            .expect("exec make bad bytes");
        assert_eq!(bad.exit_code, 0, "{}", bad.stderr);
        let read = crate::git::read_instructions_file(&shell, "/workspace/bad-bytes", "/workspace/bad-bytes/AGENTS.md")
            .await
            .expect("a file with invalid UTF-8 reads")
            .expect("it exists");
        assert_eq!(read.content, "Use \u{FFFD} tabs.\n");
        assert_eq!(read.file_bytes, 12);

        // Invalid bytes decode to 3-byte replacement characters: a file
        // well under 32 KiB can decode to more, and must load whole, not
        // be cut again (SME-32 code review 8, finding 3).
        let swollen = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/swollen && head -c 16000 /dev/zero | tr '\\0' '\\377' > /workspace/swollen/AGENTS.md && echo END >> /workspace/swollen/AGENTS.md"])
            .await
            .expect("exec make swollen");
        assert_eq!(swollen.exit_code, 0, "{}", swollen.stderr);
        let read = crate::git::read_instructions_file(&shell, "/workspace/swollen", "/workspace/swollen/AGENTS.md")
            .await
            .expect("read")
            .expect("it exists");
        assert_eq!(read.file_bytes, 16004);
        assert!(read.content.ends_with("END\n"), "the whole file loads: ...{:?}", read.content.chars().rev().take(5).collect::<String>());

        // A non-ASCII path is listed as it is, not in git's quoting,
        // so it can be loaded (SME-32 code review 7, finding 3).
        let unicode = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/unicode && cd /workspace/unicode && mkdir é && echo x > é/AGENTS.md && git add é/AGENTS.md"])
            .await
            .expect("exec make unicode");
        assert_eq!(unicode.exit_code, 0, "{}", unicode.stderr);
        let listed = crate::git::list_agents_files(&shell, "unicode").await.expect("list");
        assert_eq!(listed, vec!["é/AGENTS.md".to_string()]);

        // An AGENTS.md that's a symlink out of the checkout (to a key,
        // say) is refused, not read (SME-32 code review 7, finding 1).
        let linked = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/linked && ln -s /etc/passwd /workspace/linked/AGENTS.md"])
            .await
            .expect("exec make symlink");
        assert_eq!(linked.exit_code, 0, "{}", linked.stderr);
        let refused = crate::git::read_instructions_file(&shell, "/workspace/linked", "/workspace/linked/AGENTS.md")
            .await
            .expect_err("a symlink isn't read");
        assert!(refused.contains("isn't a regular file"), "{refused}");
        assert!(!refused.contains("root:"), "nothing of the target leaks: {refused}");

        let missing = crate::git::clone_into_pod(&shell, "file:///tmp/nope.git", None, "nope")
            .await
            .expect_err("a missing repo fails");
        assert!(missing.contains("does not appear to be a git repository"), "{missing}");

        // The directory is taken: git's own message, not a silent overwrite.
        let taken = crate::git::clone_into_pod(&shell, "file:///tmp/origin.git", None, "origin")
            .await
            .expect_err("an existing directory fails");
        assert!(taken.contains("already exists"), "{taken}");
        // Git refuses an existing directory; the error says what to do
        // when it's what a cut-off clone left (SME-32 code review 5).
        assert!(taken.contains("delete it"), "{taken}");
        let untouched = sandbox
            .exec(&["cat", "/workspace/origin/AGENTS.md"])
            .await
            .expect("exec cat");
        assert_eq!(untouched.stdout, "Run make test before committing.\n", "the existing checkout is left alone");
    })
    .await;

    manager.delete(sandbox).await.expect("delete pod");
    checks.expect("checks finished within the timeout");
}

#[test]
fn test_exec_with_no_status_is_a_failure_not_a_success() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
    // The connection closed before the API server said how it ended.
    assert_eq!(extract_exit_code(None), -1);
    // What a successful exec actually ends with: no causes at all.
    let success = Status {
        status: Some("Success".to_string()),
        ..Default::default()
    };
    assert_eq!(extract_exit_code(Some(success)), 0);
}

// Not a real UUID — just enough entropy to avoid pod-name collisions
// between concurrent test runs, without adding a `uuid` dependency.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Docker in a real sandbox pod behaves like Docker on a Linux machine,
/// and stays inside the pod (SME-33). One test so the pods, the claim
/// and the base image are made once.
#[tokio::test]
async fn test_docker_in_a_sandbox_pod_works_and_stays_inside_the_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let docker = DockerSidecar {
        memory: "1Gi".to_string(),
        storage: PodStorage::Conversation(conversation_id),
    };
    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("ensure docker claim");
    // Every pod this test makes, so cleanup below finds them even after
    // a failed assertion unwinds out of the checks.
    let created: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

    let checks = tokio::time::timeout(Duration::from_secs(300), async {
        let first = manager
            .create_with_docker(&unique_session_id("docker"), "256Mi", &docker, &[], &test_instance())
            .await
            .expect("create pod with docker");
        created.lock().expect("created").push(first.pod_name.clone());

        // The very first command works: the pod only runs once dockerd answers.
        let info = first
            .exec(&["docker", "info", "--format", "{{.ServerVersion}}"])
            .await
            .expect("exec docker info");
        assert_eq!(info.exit_code, 0, "docker info right after create: {}", info.stdout);
        assert!(info.stdout.starts_with("29."), "server version: {}", info.stdout);

        // A base image from the sandbox's own files: no Docker Hub.
        let import = first
            .exec(&[
                "bash",
                "-c",
                "sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/base",
            ])
            .await
            .expect("exec import");
        assert_eq!(import.exit_code, 0, "import base image: {}", import.stdout);

        let run = first
            .exec(&["docker", "run", "--rm", "local/base", "echo", "hello-from-docker"])
            .await
            .expect("exec docker run");
        assert_eq!(run.stdout.trim(), "hello-from-docker");

        // A relative bind mount from /workspace sees the sandbox's files.
        let compose = first
            .exec(&[
                "bash",
                "-c",
                "mkdir -p /workspace/proj/src && echo from-workspace > /workspace/proj/src/f \
                     && cd /workspace/proj \
                     && printf 'services:\\n  web:\\n    image: local/base\\n    command: [cat, /data/f]\\n    volumes: [./src:/data]\\n' > compose.yml \
                     && docker compose run --rm -q web 2>&1",
            ])
            .await
            .expect("exec compose");
        assert!(
            compose.stdout.contains("from-workspace"),
            "compose bind mount from /workspace: {}",
            compose.stdout
        );

        // Nested containers live under the sidecar's own cgroup, so its
        // memory limit applies, never at the node's cgroup root.
        let started = first
            .exec(&["docker", "run", "-d", "local/base", "sleep", "300"])
            .await
            .expect("exec docker run -d");
        let id = started.stdout.trim().to_string();
        let where_ = first
            .exec_in(
                "docker",
                &[
                    "sh",
                    "-c",
                    &format!(
                        "own=$(dirname $(sed -n 's/^0:://p' /proc/1/cgroup)); \
                             pid=$(docker --host={DOCKER_HOST} inspect -f '{{{{.State.Pid}}}}' {id}); \
                             echo \"$own\"; sed -n 's/^0:://p' /proc/$pid/cgroup"
                    ),
                ],
            )
            .await
            .expect("exec in docker sidecar");
        let mut lines = where_.stdout.lines();
        let own = lines.next().unwrap_or_default().to_string();
        let nested = lines.next().unwrap_or_default().to_string();
        assert!(
            !own.is_empty() && nested.starts_with(&format!("{own}/docker/")),
            "nested container cgroup {nested:?} should be under the sidecar's {own:?}"
        );

        // Both browsers' routes into the pod (SME-33): a published port
        // on localhost, any container port at its address through the
        // agent's relay, and nothing outside the Docker range.
        let serve = first
            .exec(&[
                "bash",
                "-c",
                "mkdir -p /workspace/www && echo from-a-container > /workspace/www/index.html \
                     && docker run -d -p 8000:8000 -v /workspace/www:/w -w /w local/base python3 -m http.server 8000 >/dev/null \
                     && docker run -d --name unpublished -v /workspace/www:/w -w /w local/base python3 -m http.server 8001 >/dev/null \
                     && sleep 2 && docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' unpublished",
            ])
            .await
            .expect("exec start servers");
        let container_ip: std::net::Ipv4Addr =
            serve.stdout.trim().parse().unwrap_or_else(|_| panic!("container ip: {:?}", serve.stdout));
        let fetch = |host: PodHost, port: u16| {
            let client = client.clone();
            let pod = first.pod_name.clone();
            async move {
                use tokio::io::AsyncWriteExt;
                let mut stream = dial_pod(&client, &pod, host, port).await.expect("dial_pod");
                stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.expect("send request");
                let mut reply = String::new();
                let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut reply)).await;
                reply
            }
        };
        assert!(fetch(PodHost::Localhost, 8000).await.contains("from-a-container"), "published port on localhost");
        assert!(
            fetch(PodHost::Container(container_ip), 8001).await.contains("from-a-container"),
            "unpublished container port through the relay"
        );
        assert_eq!(
            fetch(PodHost::Container(std::net::Ipv4Addr::new(10, 43, 0, 1)), 443).await,
            "",
            "the relay must refuse an address outside the Docker range"
        );
        // An address no container has yet (one still starting) isn't
        // listening: the relay mustn't look open while it's connecting.
        let nobody = dial_pod(&client, &first.pod_name, PodHost::Container(std::net::Ipv4Addr::new(172, 21, 200, 200)), 8001)
            .await
            .expect("dial_pod");
        assert!(!stream_is_listening(nobody).await, "nothing is at 172.21.200.200:8001");
        assert!(
            stream_is_listening(dial_pod(&client, &first.pod_name, PodHost::Container(container_ip), 8001).await.expect("dial_pod")).await,
            "the unpublished container is listening"
        );

        // A container can't reach the sandbox agent, whose WebSocket
        // runs commands with no login, at the bridge gateway.
        let agent = first
            .exec(&[
                "docker",
                "run",
                "--rm",
                "local/base",
                "bash",
                "-c",
                &format!("</dev/tcp/{}/{AGENT_PORT}", DOCKER_BRIDGE_IP.split('/').next().unwrap_or_default()),
            ])
            .await
            .expect("exec agent probe");
        assert_ne!(agent.exit_code, 0, "a container reached the sandbox agent at the gateway");

        // dockerd never listens on TCP.
        for port in [2375, 2376] {
            let tcp = first
                .exec(&["bash", "-c", &format!("</dev/tcp/127.0.0.1/{port}")])
                .await
                .expect("exec tcp probe");
            assert_ne!(tcp.exit_code, 0, "nothing should listen on {port}");
        }

        // Images live on the conversation's claim, so a new pod has them.
        let first_name = first.pod_name.clone();
        pods.delete(&first_name, &immediate_delete_params()).await.ok();
        std::mem::forget(first);
        while pods.get_opt(&first_name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let second = manager
            .create_with_docker(&unique_session_id("docker"), "256Mi", &docker, &[], &test_instance())
            .await
            .expect("recreate pod on the same claim");
        created.lock().expect("created").push(second.pod_name.clone());
        let images = second
            .exec(&["docker", "image", "ls", "-q", "local/base"])
            .await
            .expect("exec image ls");
        assert!(!images.stdout.trim().is_empty(), "local/base should survive a new pod");
        std::mem::forget(second);
    });
    let outcome = std::panic::AssertUnwindSafe(checks).catch_unwind().await;

    // Delete what this test made directly, not through the cleanup queue,
    // whether or not the checks passed.
    let created = created.into_inner().expect("created");
    for name in &created {
        pods.delete(name, &immediate_delete_params()).await.ok();
    }
    for name in &created {
        while pods.get_opt(name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    pvc_api(&client)
        .delete(&docker_pvc_name(conversation_id), &DeleteParams::default())
        .await
        .ok();
    match outcome {
        Ok(finished) => finished.expect("docker test should finish within the timeout"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// An OOM kill inside a nested container restarts only the Docker
/// sidecar: the sandbox container, its processes and /workspace carry
/// on, and the pod's status says why, as `docker_restart_to_report`
/// reads it (SME-33's spike). Written memory, not `bytearray(n)`, whose
/// untouched pages never count.
#[tokio::test]
async fn test_an_oom_in_a_nested_container_restarts_only_the_docker_sidecar() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let docker = DockerSidecar {
        memory: "512Mi".to_string(),
        storage: PodStorage::Ephemeral,
    };
    let sandbox = manager
        .create_with_docker(&unique_session_id("docker-oom"), "128Mi", &docker, &[], &test_instance())
        .await
        .expect("create pod");
    let name = sandbox.pod_name.clone();

    let checks = std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(240), async {
        let setup = sandbox
            .exec(&[
                "bash",
                "-c",
                "echo kept > /workspace/marker && (setsid sleep 1000 >/dev/null 2>&1 &) \
                     && sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null \
                        | docker import - local/base >/dev/null && echo ready",
            ])
            .await
            .expect("exec setup");
        assert_eq!(setup.stdout.trim(), "ready", "setup: {}", setup.stdout);
        // Detached: the container's memory outgrows the sidecar's limit.
        let started = sandbox
            .exec(&["docker", "run", "-d", "local/base", "sh", "-c", "head -c 900M /dev/zero | tail; sleep 600"])
            .await
            .expect("exec docker run");
        let container = started.stdout.trim().to_string();

        // Read as `watch_pods` reads it: every poll goes through
        // `note_docker_restarts`, and a restart no update explains yet
        // is re-read after `DOCKER_REASON_WAIT` (SME-85). The test pod
        // isn't named like a smelt pod, so it's watched as one here.
        let as_watched = |pod: Pod| named(pod, "sandbox-1");
        let mut seen = HashMap::new();
        let first = pods.get(&name).await.expect("get pod");
        note_docker_restarts(&mut seen, &as_watched(first), true);
        let restarted = loop {
            let pod = pods.get(&name).await.expect("get pod");
            let restarts = |pod: &Pod| docker_status(pod).map(|c| c.restart_count);
            match note_docker_restarts(&mut seen, &as_watched(pod.clone()), false) {
                Some(DockerRestart::Report { reason, .. }) => {
                    let count = restarts(&pod);
                    break (pod, (count, Some(reason)));
                }
                Some(DockerRestart::Recheck { .. }) => {
                    tokio::time::sleep(DOCKER_REASON_WAIT).await;
                    let pod = pods.get(&name).await.expect("re-read pod");
                    let reason = docker_restart_to_report(&pod, 0).and_then(|(_, reason)| reason);
                    let count = restarts(&pod);
                    break (pod, (count, reason));
                }
                None => {}
            }
            // A workload that exits on its own never reaches the limit:
            // say why now rather than at the timeout. Exit code 255 is
            // not that: it's how a restarted dockerd reports the
            // containers its OOM-killed predecessor ran, often before
            // the pod's status shows the restart.
            let state = sandbox
                .exec(&["docker", "inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", &container])
                .await
                .expect("exec docker inspect");
            if state.stdout.trim().starts_with("exited") && state.stdout.trim() != "exited 255" {
                let why = sandbox
                    .exec(&["sh", "-c", &format!("docker inspect -f '{{{{json .State}}}}' {container}; docker logs {container} 2>&1 | tail -5")])
                    .await
                    .expect("exec docker inspect");
                panic!("the workload exited without an OOM kill: {}", why.stdout);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        let (pod, report) = restarted;
        assert_eq!(report, (Some(1), Some("OOMKilled".to_string())));
        let sandbox_restarts = pod
            .status
            .and_then(|s| s.container_statuses)
            .and_then(|cs| cs.into_iter().find(|c| c.name == "sandbox"))
            .map(|c| c.restart_count);
        assert_eq!(sandbox_restarts, Some(0), "the sandbox container must not restart");

        let after = sandbox
            .exec(&["bash", "-c", "cat /workspace/marker; pgrep -x sleep >/dev/null && echo sleep-alive || \
                        (for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = sleep ] && echo sleep-alive && break; done)"])
            .await
            .expect("exec after restart");
        assert!(after.stdout.contains("kept"), "/workspace should survive: {}", after.stdout);
        assert!(after.stdout.contains("sleep-alive"), "sandbox processes should survive: {}", after.stdout);
    }))
    .catch_unwind()
    .await;

    pods.delete(&name, &immediate_delete_params()).await.ok();
    std::mem::forget(sandbox);
    while pods.get_opt(&name).await.ok().flatten().is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    match checks {
        Ok(finished) => finished.expect("the sidecar should be OOM-killed within the timeout"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// A new pod must not mount a conversation's Docker claim while the old
/// one is still stopping: two dockerds on one /var/lib/docker corrupt
/// it. The old pod gets a grace period so it lingers while terminating.
#[tokio::test]
async fn test_wait_for_conversation_pods_gone_waits_out_a_stopping_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let docker = DockerSidecar {
        memory: "256Mi".to_string(),
        storage: PodStorage::Conversation(conversation_id),
    };
    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("ensure docker claim");
    let sandbox = manager
        .create_with_docker(&unique_session_id("stopping"), "128Mi", &docker, &[], &test_instance())
        .await
        .expect("create pod");
    let name = sandbox.pod_name.clone();
    std::mem::forget(sandbox);

    // The same delete production makes (SME-51 B6): a forced one (grace
    // 0) removes the pod object at once while dockerd is still
    // stopping, and the wait below would see nothing to wait for.
    pods.delete(&name, &pod_delete_params())
        .await
        .expect("start deleting the pod");
    let listed_while_stopping = pods.get_opt(&name).await.expect("get pod").is_some();
    let waited =
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(60), TEST_INSTANCE).await;
    let still_there = pods.get_opt(&name).await.expect("get pod").is_some();

    pods.delete(&name, &immediate_delete_params()).await.ok();
    while pods.get_opt(&name).await.ok().flatten().is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    pvc_api(&client).delete(&docker_pvc_name(conversation_id), &DeleteParams::default()).await.ok();

    assert!(listed_while_stopping, "the delete removed {name} before its containers stopped");
    assert!(waited.is_ok(), "the wait should finish once the pod is gone: {waited:?}");
    assert!(!still_there, "the wait returned while {name} was still stopping");
}

/// The pod `test_a_stopping_pod_from_another_run_doesnt_hold_up_the_tier`
/// holds in Terminating. A fixed name, so a stopped run's leftover is easy
/// to find, and the next run clears it first.
const HELD_POD: &str = "sme-99-held-stopping-pod";

/// How long the held pod stays in Terminating once deleted: `sleep` as
/// PID 1 ignores SIGTERM, so the kubelet waits out the whole grace period
/// before killing it. Longer than the test's 10 s wait, so the wait meets
/// a stopping pod; short, so a run killed mid-test (no cleanup) leaves a
/// pod that goes by itself within this. A finalizer held it at first, and
/// a killed run would have left it Terminating for good, failing every
/// unit test that uses conversation 1 (SME-99's code review).
const HELD_POD_GRACE_SECS: i64 = 30;

/// The held pod's conversation label until it's already deleting: no
/// conversation's, so nothing waits on the pod while it starts.
const HELD_POD_PLACEHOLDER_LABEL: &str = "sme-99-not-yet";

/// A pod that stays in Terminating for `HELD_POD_GRACE_SECS` once deleted.
/// It's created under `HELD_POD_PLACEHOLDER_LABEL` and only relabelled as
/// conversation 1's once its delete has started, so a run killed at any
/// point leaves nothing that blocks conversation 1 for longer than the
/// grace period: before the delete the pod is no conversation's, and
/// after it the kubelet ends it within the grace (SME-99's second code
/// review: a run killed while waiting for it to start had left a running
/// pod labelled as conversation 1's for good).
fn held_pod_spec() -> Result<Pod, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": HELD_POD,
            "labels": { CONVERSATION_LABEL: HELD_POD_PLACEHOLDER_LABEL },
        },
        "spec": {
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": HELD_POD_GRACE_SECS,
            "containers": [{
                "name": "held",
                "image": default_sandbox_image(),
                "imagePullPolicy": "Never",
                "command": ["sleep", "3600"],
                "resources": {
                    "requests": { "cpu": "10m", "memory": "16Mi" },
                    "limits": { "cpu": "100m", "memory": "64Mi" },
                },
            }],
        },
    }))
}

/// Removes the held pod at once (grace 0) and waits, up to a minute, for
/// it to be gone. Errors are only logged: this runs on every exit path,
/// and before a run, for a previous run's leftover.
async fn release_held_pod(pods: &Api<Pod>) {
    let _ = pods.delete(HELD_POD, &immediate_delete_params()).await;
    for _ in 0..120 {
        match pods.get_opt(HELD_POD).await {
            Ok(None) => return,
            Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                eprintln!("couldn't check {HELD_POD} is gone: {e}");
                return;
            }
        }
    }
    eprintln!("{HELD_POD} is still there after a minute");
}

/// Waits up to a minute for the held pod to run: a pod deleted before its
/// container starts goes at once, with nothing to wait out.
async fn wait_held_pod_running(pods: &Api<Pod>) -> Result<(), String> {
    for _ in 0..120 {
        let pod = pods.get_opt(HELD_POD).await.map_err(|e| format!("read the held pod: {e}"))?;
        let phase = pod.and_then(|p| p.status).and_then(|s| s.phase);
        if phase.as_deref() == Some("Running") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("the held pod didn't start running within a minute".to_string())
}

/// SME-99: the browser tier's scratch database counted conversations from
/// 1, like every real-cluster test's, so its first sandbox waited out any
/// pod for conversation 1 another run was still stopping, and failed after
/// the wait's limit. Holds such a pod in Terminating on purpose, then
/// takes the tier's first conversation the way the tier does: its pod
/// must not wait on the held one. Its own client, never the process-global
/// manager (see `test_open_pod_port_reaches_the_conversations_own_pod`).
/// `#[ignore]`d so it runs with the browser tier and CI's ignored tests,
/// not alongside the unit tests that use conversation 1 for real.
#[sqlx::test]
#[ignore]
async fn test_a_stopping_pod_from_another_run_doesnt_hold_up_the_tier(pool: PgPool) {
    let client = test_client().await;
    let pods = pods_api(&client);
    release_held_pod(&pods).await;
    let spec = held_pod_spec().expect("the held pod's spec");
    pods.create(&PostParams::default(), &spec).await.expect("create the held pod");

    let outcome: Result<i64, String> = async {
        wait_held_pod_running(&pods).await?;
        pods.delete(HELD_POD, &DeleteParams::default())
            .await
            .map_err(|e| format!("start deleting the held pod: {e}"))?;
        let relabel = serde_json::json!({ "metadata": { "labels": { CONVERSATION_LABEL: "1" } } });
        pods.patch(HELD_POD, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&relabel))
            .await
            .map_err(|e| format!("label the stopping pod as conversation 1's: {e}"))?;
        let held = pods.get_opt(HELD_POD).await.map_err(|e| format!("read the held pod: {e}"))?;
        let stopping_as_1 = held.is_some_and(|pod| {
            pod.metadata.deletion_timestamp.is_some()
                && pod.metadata.labels.and_then(|l| l.get(CONVERSATION_LABEL).cloned()).as_deref() == Some("1")
        });
        if !stopping_as_1 {
            return Err("the held pod isn't held in Terminating as conversation 1's".to_string());
        }
        db::test_support::start_ids_clear_of_other_runs(&pool)
            .await
            .map_err(|e| format!("move the id sequences: {e}"))?;
        let conversation = db::create_conversation(&pool).await.map_err(|e| format!("conversation: {e}"))?;
        let instance = db::smelt_instance(&pool).await.map_err(|e| format!("instance: {e}"))?.id;
        wait_for_conversation_pods_gone(&client, conversation.id, Duration::from_secs(10), &instance)
            .await
            .map_err(|e| format!("conversation {}'s pod would wait: {e:?}", conversation.id))?;
        Ok(conversation.id)
    }
    .await;
    release_held_pod(&pods).await;

    let conversation = outcome.expect("the tier's first conversation shouldn't meet another run's stopping pod");
    assert_ne!(conversation, 1);
}

/// SME-51 B8: the listening probe (`sandbox_preview_url`) waited as
/// long as the Kubernetes client would for a port-forward to open,
/// minutes when the API server hangs, holding the turn.
#[sqlx::test]
async fn test_the_listening_probe_gives_up_on_a_hanging_cluster(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    // An "API server" that accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = kube::Config::new(format!("http://{addr}").parse().expect("url"));
    let client = kube::Client::try_from(config).expect("client");
    let probed = tokio::time::timeout(
        LISTEN_OPEN_TIMEOUT + Duration::from_secs(5),
        pod_port_is_listening_with(&pool, conversation.id, PodHost::Localhost, 8000, || Ok(client)),
    )
    .await;
    assert!(matches!(probed, Ok(Ok(false))), "the probe should give up and say nothing's listening: {probed:?}");
}

/// SME-62 B15: a pod start mustn't give up before the Docker sidecar's
/// own startup probe would (60 checks a second apart), with time left
/// for a first pod's claims to be provisioned.
#[test]
fn test_a_pod_start_waits_longer_than_the_docker_probe() {
    let probe = DOCKER_STARTUP_PROBE_CHECKS * DOCKER_STARTUP_PROBE_PERIOD_SECS;
    assert!(DEFAULT_RUNNING_WAIT_TIMEOUT_SECS >= probe as u64 + 30, "{DEFAULT_RUNNING_WAIT_TIMEOUT_SECS}s vs a {probe}s probe");
}

/// SME-51 code review 1: a pod start that fails after its conversation
/// was deleted may have re-created the conversation's claims after the
/// delete's teardown ran; they're removed. A live conversation's stay.
#[sqlx::test]
async fn test_a_failed_start_for_a_deleted_conversation_leaves_no_claims(pool: PgPool) {
    let client = test_client().await;
    let pvcs = pvc_api(&client);
    let deleted = db::create_conversation(&pool).await.expect("conversation");
    let live = db::create_conversation(&pool).await.expect("conversation");
    // Ids from a fresh test database are tiny; offset them away from
    // other runs' claims in the shared test namespace.
    let offset = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64 + 2_000_000_000;
    db::delete_conversation(&pool, deleted.id).await.expect("delete");
    let instance = db::smelt_instance(&pool).await.expect("instance");
    for id in [deleted.id, live.id] {
        ensure_conversation_pvcs(&client, id + offset, &instance).await.expect("claims");
    }

    clean_up_after_failed_start(&pool, &client, deleted.id, deleted.id + offset).await;
    clean_up_after_failed_start(&pool, &client, live.id, live.id + offset).await;

    let gone = pvcs.get_opt(&docker_pvc_name(deleted.id + offset)).await.expect("get").is_none_or(|p| p.metadata.deletion_timestamp.is_some());
    let kept = pvcs.get_opt(&docker_pvc_name(live.id + offset)).await.expect("get").is_some();
    delete_conversation_pvcs(&client, live.id + offset, &instance.id).await;
    delete_conversation_pvcs(&client, deleted.id + offset, &instance.id).await;
    assert!(gone, "the deleted conversation's re-created claim was left behind");
    assert!(kept, "a live conversation's claims must stay");
}

/// SME-51 B7: "Work on a repo" while the sandbox is still starting
/// waits for it, rather than taking the half-made pod's record and
/// cloning into a pod that isn't running.
#[sqlx::test]
async fn test_start_or_get_pod_waits_for_a_pod_still_starting(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // A pod being made: its record exists, and its start holds the lock.
    let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let starting = pod_start_lock(conversation.id);
    let guard = starting.lock().await;
    let early = tokio::time::timeout(Duration::from_millis(300), start_or_get_pod(&pool, conversation.id)).await;
    assert!(early.is_err(), "it took the pod before it had started");
    drop(guard);
    let pod = start_or_get_pod(&pool, conversation.id).await.expect("the started pod");
    assert_eq!(pod, row.id);
}

/// SME-51 B5: deleting a conversation removes every pod labelled with
/// it, including one whose record is already gone. That's what a
/// `create_pod` racing the delete leaves: its row cascades away with
/// the conversation, and a teardown that listed rows missed the pod.
#[tokio::test]
async fn test_teardown_deletes_a_conversations_pod_that_has_no_record() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    // Only the label matters, so the pod never needs to start: an image
    // that doesn't exist keeps it Pending and costs the cluster nothing.
    let name = format!("sandbox-{conversation_id}");
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {CONVERSATION_LABEL: conversation_id.to_string(), INSTANCE_LABEL: TEST_INSTANCE}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    // A language server pod of the same conversation (SME-35), under
    // its own label.
    let server = format!("lsp-{conversation_id}-x");
    let server_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": server, "labels": {crate::lsp::pods::LSP_OF_LABEL: conversation_id.to_string(), INSTANCE_LABEL: TEST_INSTANCE}},
        "spec": {"containers": [{"name": "server", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &server_pod).await.expect("create server pod");

    teardown_conversation_with(&client, conversation_id, &[], &test_instance()).await;
    let gone = tokio::time::timeout(Duration::from_secs(60), async {
        while pods.get_opt(&name).await.ok().flatten().is_some()
            || pods.get_opt(&server).await.ok().flatten().is_some()
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .is_ok();

    pods.delete(&name, &immediate_delete_params()).await.ok();
    pods.delete(&server, &immediate_delete_params()).await.ok();
    assert!(gone, "{name} or {server} survived its conversation's teardown");
}

/// SME-88: a pod without the conversation label (one from before
/// SME-33) is still deleted, found by its record's id.
#[tokio::test]
async fn test_teardown_deletes_our_pod_named_by_its_record_but_not_an_unlabelled_one() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    // Ours, with no conversation label (SME-88), and one from before
    // SME-115, with no label at all.
    let ours = pod_name(conversation_id + 1);
    let unlabelled = pod_name(conversation_id + 2);
    for (name, labels) in [
        (&ours, serde_json::json!({INSTANCE_LABEL: TEST_INSTANCE})),
        (&unlabelled, serde_json::json!({})),
    ] {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": labels},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        pods.create(&PostParams::default(), &pod).await.expect("create pod");
    }

    teardown_conversation_with(&client, conversation_id, &[conversation_id + 1, conversation_id + 2], &test_instance())
        .await;
    let ours_gone = tokio::time::timeout(Duration::from_secs(60), async {
        while pods.get_opt(&ours).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .is_ok();
    let unlabelled_kept = pods
        .get_opt(&unlabelled)
        .await
        .expect("read the pod")
        .is_some_and(|pod| pod.metadata.deletion_timestamp.is_none());

    pods.delete(&ours, &immediate_delete_params()).await.ok();
    pods.delete(&unlabelled, &immediate_delete_params()).await.ok();
    assert!(ours_gone, "{ours} survived its conversation's teardown");
    assert!(unlabelled_kept, "teardown deleted {unlabelled}, which has no instance label");
}

/// SME-115: a server deleting its conversation N leaves another database's
/// conversation N alone: its sandbox pod, its language server pod (found
/// by label), a pod its records happen to name, and its claims. Before
/// the fix teardown deleted them all.
#[tokio::test]
async fn test_teardown_leaves_another_databases_conversation_of_the_same_id() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let pvcs = pvc_api(&client);
    let theirs = "another-smelt-database";
    let conversation_id = unused_conversation_id();
    let sandbox = pod_name(conversation_id);
    let server = format!("lsp-{conversation_id}-x");
    for (name, label) in [(&sandbox, CONVERSATION_LABEL), (&server, crate::lsp::pods::LSP_OF_LABEL)] {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": {label: conversation_id.to_string(), INSTANCE_LABEL: theirs}},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        pods.create(&PostParams::default(), &pod).await.expect("create pod");
    }
    let claims = [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)];
    for claim in &claims {
        make_claim(&client, claim, conversation_id, Some(theirs)).await;
    }

    teardown_conversation_with(&client, conversation_id, &[conversation_id], &test_instance()).await;
    let mut deleted = Vec::new();
    for name in [&sandbox, &server] {
        if !pods.get_opt(name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none()) {
            deleted.push(name.clone());
        }
    }
    for claim in &claims {
        if !claim_kept(&client, claim).await {
            deleted.push(claim.clone());
        }
    }

    for name in [&sandbox, &server] {
        pods.delete(name, &immediate_delete_params()).await.ok();
    }
    for claim in &claims {
        pvcs.delete(claim, &DeleteParams::default()).await.ok();
    }
    assert!(deleted.is_empty(), "teardown deleted another database's {deleted:?}");
}

/// SME-115: a conversation's claims that are another database's (or from
/// before the fix, unadopted) are refused, never mounted, and left as
/// they were. Before the fix the start went ahead and mounted them.
#[tokio::test]
async fn test_another_databases_claims_are_never_mounted() {
    let client = test_client().await;
    let foreign = unused_conversation_id();
    let unlabelled = foreign + 1;
    make_claim(&client, &workspace_pvc_name(foreign), foreign, Some("another-smelt-database")).await;
    make_claim(&client, &workspace_pvc_name(unlabelled), unlabelled, None).await;

    let refused_foreign = ensure_conversation_pvcs(&client, foreign, &test_instance()).await;
    let refused_unlabelled = ensure_conversation_pvcs(&client, unlabelled, &test_instance()).await;
    let kept = [
        claim_kept(&client, &workspace_pvc_name(foreign)).await,
        claim_kept(&client, &workspace_pvc_name(unlabelled)).await,
    ];

    for id in [foreign, unlabelled] {
        for name in [docker_pvc_name(id), workspace_pvc_name(id)] {
            pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
        }
    }
    match refused_foreign {
        Err(SandboxError::NotOurs { name, ownership: Ownership::Foreign }) => {
            assert_eq!(name, workspace_pvc_name(foreign))
        }
        other => panic!("another database's /workspace wasn't refused: {other:?}"),
    }
    match refused_unlabelled {
        Err(e @ SandboxError::NotOurs { ownership: Ownership::Unlabelled, .. }) => {
            assert!(e.to_string().contains("SME-115"), "{e}")
        }
        other => panic!("an unadopted /workspace wasn't refused: {other:?}"),
    }
    assert_eq!(kept, [true, true], "a refused claim was changed");
}

/// SME-115: a pod named like ours that is another database's is refused,
/// never reused (its agent runs commands for whoever connects), and left.
#[tokio::test]
async fn test_another_databases_pod_of_the_same_name_is_never_reused() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let session_id = unique_session_id("foreign");
    let name = format!("sandbox-{session_id}");
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    let result = manager
        .create_with_running_timeout(
            &session_id,
            "128Mi",
            &DockerSidecar { memory: "512Mi".to_string(), storage: PodStorage::Ephemeral },
            &[],
            Duration::from_secs(5),
            &test_instance(),
        )
        .await;
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(
        matches!(result, Err(SandboxError::NotOurs { ownership: Ownership::Foreign, .. })),
        "another database's pod was used: {:?}",
        result.as_ref().map(|s| &s.pod_name)
    );
    assert!(kept, "another database's pod was deleted");
}

/// SME-115: another database's volume claim of the same name is refused,
/// never mounted, and deleting our volume of that id leaves it.
#[tokio::test]
async fn test_another_databases_volume_claim_is_refused_and_left() {
    let client = test_client().await;
    let pvcs = pvc_api(&client);
    let id = unused_conversation_id();
    let name = sandbox_volume_pvc_name(id);
    pvcs.create(&PostParams::default(), &build_volume_pvc_spec(id, "another-smelt-database"))
        .await
        .expect("create the claim");
    let volume = db::SandboxVolume {
        id,
        name: "sme-115".to_string(),
        mount_path: "/data".to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
    };

    let mounted = ensure_volume_claims(&client, std::slice::from_ref(&volume), &test_instance()).await;
    let deleted = delete_volume_claim(&client, id, &test_instance()).await;
    let kept = claim_kept(&client, &name).await;

    pvcs.delete(&name, &DeleteParams::default()).await.ok();
    assert!(
        matches!(mounted, Err(SandboxError::NotOurs { ownership: Ownership::Foreign, .. })),
        "another database's volume claim was used: {mounted:?}"
    );
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(kept, "deleting our volume deleted another database's claim");
}

/// SME-115: what reads as this database's pod. One from before the fix
/// counts only for the database that owns those (adoption may have missed
/// it); another database's never does.
#[test]
fn test_counts_as_ours_only_ours_and_unadopted_ones_for_their_owner() {
    let owner = db::SmeltInstance { id: "ours".to_string(), owns_unlabelled: true };
    let scratch = db::SmeltInstance { id: "ours".to_string(), owns_unlabelled: false };
    for (meta, for_owner, for_scratch) in [
        (meta_with_instance(Some("ours")), true, true),
        (meta_with_instance(None), true, false),
        (meta_with_instance(Some("theirs")), false, false),
    ] {
        assert_eq!(counts_as_ours(&meta, &owner), for_owner, "{:?}", meta.labels);
        assert_eq!(counts_as_ours(&meta, &scratch), for_scratch, "{:?}", meta.labels);
    }
}

/// SME-115: the pod watch is limited to this database's pods, so another
/// smelt's Docker restarts, language server stops and finished pods never
/// reach this database's records by their ids.
#[test]
fn test_the_pod_watch_sees_only_our_instance() {
    assert_eq!(watcher_config("ours").label_selector.as_deref(), Some("smelt/instance=ours"));
}

/// SME-115: a live record whose pod name is taken by another database's
/// pod (still running) is closed as gone, and that pod is left. Before the
/// fix the record stayed open on the other pod's status, and terminating
/// it deleted that pod.
#[sqlx::test]
async fn test_a_record_whose_pod_is_another_databases_is_closed_and_the_pod_left(pool: PgPool) {
    let client = test_client().await;
    let pods = pods_api(&client);
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let name = pod_name(row.id);
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    close_if_gone_with(&pool, &client, row.id).await;
    let closed = !db::sandbox_pod_is_live(&pool, row.id).await.expect("live?");
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(closed, "the record stayed open on another database's pod");
    assert!(kept, "closing the record deleted another database's pod");
}

/// SME-115 review 2: the adoption patch holds the object to its identity
/// (its uid: the API server refuses a patch carrying another uid, 422),
/// not its whole version: a new pod's status changes several times a
/// second, and a `resourceVersion` precondition failed both tries and
/// left it unadopted.
#[test]
fn test_the_adoption_patch_is_held_to_the_objects_uid_not_its_version() {
    let meta = ObjectMeta {
        name: Some("sandbox-401".to_string()),
        uid: Some("uid-401".to_string()),
        resource_version: Some("12345".to_string()),
        ..Default::default()
    };
    let patch = adoption_patch(&meta, "ours");
    assert_eq!(patch["metadata"]["labels"][INSTANCE_LABEL], "ours");
    assert_eq!(patch["metadata"]["uid"], "uid-401");
    assert!(patch["metadata"].get("resourceVersion").is_none(), "{patch}");
}

fn owner() -> db::SmeltInstance {
    db::SmeltInstance { id: "ours".to_string(), owns_unlabelled: true }
}

fn named_meta(name: &str, labels: &[(&str, &str)]) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
        ..Default::default()
    }
}

/// SME-115: adoption labels only what this database made before the fix:
/// never without `owns_unlabelled`, never an object already labelled
/// (another database's, or ours), and only one whose conversation or
/// volume this database still has.
#[test]
fn test_adoption_takes_only_unlabelled_objects_of_our_conversations_and_volumes() {
    let conversations = std::collections::HashSet::from([1528]);
    let volumes = std::collections::HashSet::from([3]);
    let adopts = |meta: &ObjectMeta, instance: &db::SmeltInstance| should_adopt(meta, instance, &conversations, &volumes);
    let claim = named_meta("sandbox-workspace-1528", &[(CONVERSATION_LABEL, "1528")]);
    let pod = named_meta("sandbox-401", &[(CONVERSATION_LABEL, "1528")]);
    let volume = named_meta("sandbox-volume-3", &[]);
    for meta in [&claim, &pod, &volume] {
        assert!(adopts(meta, &owner()), "{:?}", meta.name);
        assert!(!adopts(meta, &db::SmeltInstance { owns_unlabelled: false, ..owner() }), "a scratch database adopted {:?}", meta.name);
    }
    let gone = named_meta("sandbox-workspace-7", &[(CONVERSATION_LABEL, "7")]);
    let gone_volume = named_meta("sandbox-volume-4", &[]);
    let theirs = named_meta("sandbox-workspace-1528", &[(CONVERSATION_LABEL, "1528"), (INSTANCE_LABEL, "theirs")]);
    let already = named_meta("sandbox-workspace-1528", &[(CONVERSATION_LABEL, "1528"), (INSTANCE_LABEL, "ours")]);
    let stranger = named_meta("something-else", &[]);
    for meta in [&gone, &gone_volume, &theirs, &already, &stranger] {
        assert!(!adopts(meta, &owner()), "adopted {:?} {:?}", meta.name, meta.labels);
    }
}

/// SME-115: a language server pod is adopted with the sandbox pod it runs
/// next to, never otherwise.
#[test]
fn test_adoption_takes_a_server_pod_only_with_its_sandbox_pod() {
    let ours = std::collections::HashSet::from([401]);
    let server = |pod: &str, labels: &[(&str, &str)]| {
        let mut all = vec![(crate::lsp::pods::LSP_POD_LABEL, pod)];
        all.extend_from_slice(labels);
        named_meta(&format!("lsp-{pod}-x"), &all)
    };
    assert!(should_adopt_server(&server("401", &[]), &owner(), &ours));
    assert!(!should_adopt_server(&server("401", &[]), &db::SmeltInstance { owns_unlabelled: false, ..owner() }, &ours));
    assert!(!should_adopt_server(&server("402", &[]), &owner(), &ours), "its sandbox pod isn't ours");
    assert!(!should_adopt_server(&server("401", &[(INSTANCE_LABEL, "theirs")]), &owner(), &ours));
}

/// The instance label of `name`'s object, read back from the cluster.
async fn instance_of<K>(api: &Api<K>, name: &str) -> Option<String>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    api.get(name).await.expect("read").meta().labels.as_ref().and_then(|l| l.get(INSTANCE_LABEL).cloned())
}

/// The unlabelled objects of one conversation, as a database from before
/// SME-115 left them: its claim, its pod, and a language server pod next
/// to it; plus a claim of a conversation the database no longer has.
/// Returns (claim, pod, server pod, gone claim) names.
async fn make_pre_fix_objects(client: &kube::Client, conversation_id: i64, pod_id: i64) -> [String; 4] {
    let pods = pods_api(client);
    let claim = workspace_pvc_name(conversation_id);
    let gone = workspace_pvc_name(conversation_id + 1);
    make_claim(client, &claim, conversation_id, None).await;
    make_claim(client, &gone, conversation_id + 1, None).await;
    let pod = pod_name(pod_id);
    let server = format!("lsp-{pod_id}-x");
    for (name, labels) in [
        (&pod, serde_json::json!({CONVERSATION_LABEL: conversation_id.to_string()})),
        (&server, serde_json::json!({
            crate::lsp::pods::LSP_OF_LABEL: conversation_id.to_string(),
            crate::lsp::pods::LSP_POD_LABEL: pod_id.to_string(),
        })),
    ] {
        let spec: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": labels},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        pods.create(&PostParams::default(), &spec).await.expect("create pod");
    }
    [claim, pod, server, gone]
}

async fn remove_pre_fix_objects(client: &kube::Client, [claim, pod, server, gone]: &[String; 4]) {
    for name in [pod, server] {
        pods_api(client).delete(name, &immediate_delete_params()).await.ok();
    }
    for name in [claim, gone] {
        pvc_api(client).delete(name, &DeleteParams::default()).await.ok();
    }
}

/// SME-115: the database that made the objects from before the fix (the
/// dev database) labels its conversation's claim, pod and server pod as
/// its own at startup, so its watch, sweep and teardown see them again; a
/// claim of a conversation it no longer has is left as it was.
#[sqlx::test]
async fn test_the_owning_database_adopts_its_unlabelled_objects(pool: PgPool) {
    let client = test_client().await;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
    let ours = db::smelt_instance(&pool).await.expect("instance").id;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let pod_row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let names = make_pre_fix_objects(&client, conversation.id, pod_row.id).await;

    adopt_unlabelled_objects_with(&client, &pool).await;
    let labels = [
        instance_of(&pvc_api(&client), &names[0]).await,
        instance_of(&pods_api(&client), &names[1]).await,
        instance_of(&pods_api(&client), &names[2]).await,
        instance_of(&pvc_api(&client), &names[3]).await,
    ];

    remove_pre_fix_objects(&client, &names).await;
    assert_eq!(labels, [Some(ours.clone()), Some(ours.clone()), Some(ours), None]);
}

/// SME-115: a database that didn't make them (a scratch one) adopts
/// nothing, even for a conversation id it happens to have too.
#[sqlx::test]
async fn test_a_scratch_database_adopts_nothing(pool: PgPool) {
    let client = test_client().await;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let pod_row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let names = make_pre_fix_objects(&client, conversation.id, pod_row.id).await;

    adopt_unlabelled_objects_with(&client, &pool).await;
    let labels = [
        instance_of(&pvc_api(&client), &names[0]).await,
        instance_of(&pods_api(&client), &names[1]).await,
        instance_of(&pods_api(&client), &names[2]).await,
        instance_of(&pvc_api(&client), &names[3]).await,
    ];

    remove_pre_fix_objects(&client, &names).await;
    assert_eq!(labels, [None, None, None, None]);
}

/// SME-115: when startup's adoption missed a claim, the owning database
/// adopts it when a pod first needs it, instead of locking the
/// conversation out of its /workspace until a restart. A scratch database
/// still refuses it.
#[tokio::test]
async fn test_a_missed_claim_is_adopted_when_the_owner_needs_it() {
    let client = test_client().await;
    let owned = unused_conversation_id();
    let refused = owned + 1;
    let owner = db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: true };
    make_claim(&client, &workspace_pvc_name(owned), owned, None).await;
    make_claim(&client, &workspace_pvc_name(refused), refused, None).await;

    let adopted = ensure_conversation_pvcs(&client, owned, &owner).await;
    let label = instance_of(&pvc_api(&client), &workspace_pvc_name(owned)).await;
    let scratch = ensure_conversation_pvcs(&client, refused, &test_instance()).await;

    for id in [owned, refused] {
        for name in [docker_pvc_name(id), workspace_pvc_name(id)] {
            pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
        }
    }
    assert!(adopted.is_ok(), "{adopted:?}");
    assert_eq!(label.as_deref(), Some(TEST_INSTANCE));
    assert!(matches!(scratch, Err(SandboxError::NotOurs { ownership: Ownership::Unlabelled, .. })), "{scratch:?}");
}

/// SME-115 review 1: when startup's adoption missed a conversation's pod
/// and claims (a transient API error), the database that owns objects
/// from before the fix still deletes them with the conversation, instead
/// of leaving them for good. A database that doesn't own them leaves them
/// (`test_teardown_deletes_our_pod_named_by_its_record_but_not_an_unlabelled_one`).
#[tokio::test]
async fn test_the_owner_tears_down_a_conversation_adoption_missed() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = unused_conversation_id();
    let owner = db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: true };
    let pod = pod_name(conversation_id);
    let spec: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": pod, "labels": {CONVERSATION_LABEL: conversation_id.to_string()}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &spec).await.expect("create pod");
    let claims = [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)];
    for claim in &claims {
        make_claim(&client, claim, conversation_id, None).await;
    }

    teardown_conversation_with(&client, conversation_id, &[], &owner).await;
    let mut kept = Vec::new();
    if pods.get_opt(&pod).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none()) {
        kept.push(pod.clone());
    }
    for claim in &claims {
        if claim_kept(&client, claim).await {
            kept.push(claim.clone());
        }
    }

    pods.delete(&pod, &immediate_delete_params()).await.ok();
    for claim in &claims {
        pvc_api(&client).delete(claim, &DeleteParams::default()).await.ok();
    }
    assert!(kept.is_empty(), "the owning database left its own conversation's {kept:?}");
}

/// SME-115 review 1: stopping a pod startup's adoption missed deletes it
/// for the database that owns it, instead of closing the record and
/// leaving the pod running (every later start of the conversation would
/// then wait on it and time out).
#[sqlx::test]
async fn test_the_owner_terminates_a_pod_adoption_missed(pool: PgPool) {
    let client = test_client().await;
    let pods = pods_api(&client);
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let name = pod_name(row.id);
    let spec: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {CONVERSATION_LABEL: conversation.id.to_string()}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &spec).await.expect("create pod");

    let closed = force_terminate_pod_with(&pool, &client, row.id).await;
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(closed.is_ok(), "{closed:?}");
    assert!(!kept, "the owning database closed the record and left its pod running");
}

/// A pod from before SME-33 as the dev database left it: no labels at all.
async fn make_bare_pod(client: &kube::Client, name: &str) {
    let spec: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods_api(client).create(&PostParams::default(), &spec).await.expect("create pod");
}

/// Whether pod `name` is still there and not being deleted.
async fn pod_kept(client: &kube::Client, name: &str) -> bool {
    pods_api(client).get_opt(name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none())
}

/// SME-115 review 2: a pod from before SME-33 has no labels at all (SME-88).
/// The database that owns such objects still stops it, tears it down with
/// its conversation, and adopts it at startup while its record is live:
/// the record names it.
#[sqlx::test]
async fn test_the_owner_handles_a_pod_with_no_labels_named_by_its_record(pool: PgPool) {
    let client = test_client().await;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
    let owner = db::smelt_instance(&pool).await.expect("instance");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let stopped = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    make_bare_pod(&client, &pod_name(stopped.id)).await;
    let closed = force_terminate_pod_with(&pool, &client, stopped.id).await;
    let stopped_kept = pod_kept(&client, &pod_name(stopped.id)).await;

    // Torn down with its conversation, named by the record read before
    // the delete.
    let torn = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    make_bare_pod(&client, &pod_name(torn.id)).await;
    teardown_conversation_with(&client, conversation.id, &[torn.id], &owner).await;
    let torn_kept = pod_kept(&client, &pod_name(torn.id)).await;

    // Adopted at startup while its record is live.
    let other = db::create_conversation(&pool).await.expect("conversation");
    let live = db::create_sandbox_pod(&pool, other.id).await.expect("pod row");
    make_bare_pod(&client, &pod_name(live.id)).await;
    adopt_unlabelled_objects_with(&client, &pool).await;
    let label = instance_of(&pods_api(&client), &pod_name(live.id)).await;

    for id in [stopped.id, torn.id, live.id] {
        pods_api(&client).delete(&pod_name(id), &immediate_delete_params()).await.ok();
    }
    assert!(closed.is_ok(), "{closed:?}");
    assert!(!stopped_kept, "stopping its record left the pod running");
    assert!(!torn_kept, "deleting its conversation left the pod");
    assert_eq!(label.as_deref(), Some(owner.id.as_str()), "startup didn't adopt a live record's pod");
}

/// SME-115 review 2: deleting a volume whose claim startup's adoption
/// missed deletes the claim for the database that owns it, instead of
/// leaving the user's data in the cluster for good once the row is gone.
#[tokio::test]
async fn test_the_owner_deletes_a_volume_claim_adoption_missed() {
    let client = test_client().await;
    let id = unused_conversation_id();
    let name = sandbox_volume_pvc_name(id);
    let mut spec = build_volume_pvc_spec(id, "unused");
    spec.metadata.labels = None;
    pvc_api(&client).create(&PostParams::default(), &spec).await.expect("create the claim");
    let owner = db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: true };

    let deleted = delete_volume_claim(&client, id, &owner).await;
    let kept = claim_kept(&client, &name).await;

    pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(!kept, "the owning database left its own volume's claim");
}

/// SME-115: a new pod waits out the conversation's stopping pods, ours
/// and ones from before the fix, but never another database's pod
/// labelled with the same conversation.
#[tokio::test]
async fn test_the_wait_for_old_pods_ignores_another_databases() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = unused_conversation_id();
    let foreign = pod_name(conversation_id);
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": foreign, "labels": {CONVERSATION_LABEL: conversation_id.to_string(), INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");
    let foreign_ignored =
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(3), TEST_INSTANCE).await;

    // The same pod with no instance label: waited on.
    let unlabel = serde_json::json!({"metadata": {"labels": {INSTANCE_LABEL: null}}});
    pods.patch(&foreign, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&unlabel))
        .await
        .expect("drop the instance label");
    let unlabelled_waited =
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(2), TEST_INSTANCE).await;

    pods.delete(&foreign, &immediate_delete_params()).await.ok();
    assert!(foreign_ignored.is_ok(), "waited on another database's pod: {foreign_ignored:?}");
    assert!(
        matches!(unlabelled_waited, Err(SandboxError::Timeout(_))),
        "a pod from before the fix may be ours, and must be waited out: {unlabelled_waited:?}"
    );
}

/// A conversation id no other test or run uses: claims are named after it.
fn unused_conversation_id() -> i64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    3_000_000_000 + i64::try_from(nanos % 1_000_000_000).unwrap_or_default()
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

/// SME-115's regression test: a server whose database has no
/// conversations (B, a scratch check server) sweeps the shared namespace
/// at startup. It must delete only its own orphan, never another
/// database's claims (A, the dev server's) or one from before the fix.
/// Before the fix it deleted all three. Runs the sweep for real in the
/// shared test namespace: it can't reach another test's claims either.
#[sqlx::test]
async fn test_a_sweep_deletes_only_its_own_databases_orphans(pool: PgPool) {
    let client = test_client().await;
    let ours = db::smelt_instance(&pool).await.expect("instance").id;
    let base = unused_conversation_id();
    let theirs = format!("sandbox-workspace-{base}");
    let before_the_fix = format!("sandbox-workspace-{}", base + 1);
    let our_orphan = format!("sandbox-workspace-{}", base + 2);
    make_claim(&client, &theirs, base, Some("another-smelt-database")).await;
    make_claim(&client, &before_the_fix, base + 1, None).await;
    make_claim(&client, &our_orphan, base + 2, Some(&ours)).await;

    sweep_orphaned_conversation_claims_with(&client, &pool).await;

    let kept_theirs = claim_kept(&client, &theirs).await;
    let kept_unlabelled = claim_kept(&client, &before_the_fix).await;
    let kept_ours = claim_kept(&client, &our_orphan).await;
    for name in [&theirs, &before_the_fix, &our_orphan] {
        let _ = pvc_api(&client).delete(name, &DeleteParams::default()).await;
    }
    assert!(kept_theirs, "the sweep deleted another database's claim");
    assert!(kept_unlabelled, "the sweep deleted a claim with no instance label");
    assert!(!kept_ours, "the sweep left its own orphan");
}

/// A conversation's Docker claim is created once and reused, and
/// deleting it removes it. No pod: `local-path` binds on first use, so
/// an unused claim stays `Pending`, which is fine here.
#[tokio::test]
async fn test_ensure_docker_pvc_creates_once_and_delete_removes_it() {
    let client = test_client().await;
    let pvcs = pvc_api(&client);
    // Far above any id a fresh test database hands out, and unique per run.
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let name = docker_pvc_name(conversation_id);

    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("first ensure should create");
    let first = pvcs.get_opt(&name).await.expect("get claim");
    let first_uid = first.and_then(|p| p.metadata.uid);
    assert!(first_uid.is_some(), "ensure_docker_pvc should create {name}");

    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("second ensure should reuse");
    let second_uid = pvcs.get_opt(&name).await.expect("get claim").and_then(|p| p.metadata.uid);
    assert_eq!(first_uid, second_uid, "a second ensure must reuse the claim, not replace it");

    delete_conversation_pvcs(&client, conversation_id, TEST_INSTANCE).await;
    let gone = tokio::time::timeout(Duration::from_secs(30), async {
        while pvcs.get_opt(&name).await.expect("get claim").is_some() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    // Delete directly too, so a failed assertion never leaves it behind.
    pvcs.delete(&name, &DeleteParams::default()).await.ok();
    assert!(gone.is_ok(), "delete_docker_pvc should remove {name}");
}

#[tokio::test]
async fn test_create_reaches_running_from_clean_slate() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("create");

    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let pods = pods_api(&client);
    let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
    assert_eq!(pod.status.and_then(|s| s.phase).as_deref(), Some("Running"));

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// A pod that never reaches `Running` (real cause seen in this
/// environment: `local-path`'s `WaitForFirstConsumer` provisioning
/// stuck behind an unrelated pileup — see the precheck below) must not
/// be left behind: nothing else in the system ever cleans up a pod
/// `create` gave up waiting on, since `Sandbox::drop`'s cleanup queue
/// only fires for a `Sandbox` that was actually returned. An orphaned
/// pod here isn't just wasted — a caller that derives the same pod
/// name deterministically (`sandbox_volume_pvc_name`'s pod-name
/// counterpart) collides with it on every later attempt, exactly what
/// happened to `test_terminal_lifecycle_end_to_end`'s volume-mount pod
/// before this fix.
///
/// Drives `create_with_running_timeout` directly with a 1ms timeout
/// rather than the real (30s+) default, or mutating
/// `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` — that env var is process-global
/// and read by every other concurrently-running test in this suite, so
/// overriding it here would risk timing them out too. 1ms guarantees
/// the timeout fires before the first status check ever completes,
/// independent of how fast this cluster actually schedules pods.
#[tokio::test]
async fn test_create_deletes_the_pod_it_just_created_if_it_never_reaches_running() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("timeout-cleanup");
    let name = format!("sandbox-{session_id}");

    let result = manager
        .create_with_running_timeout(
            &session_id,
            "128Mi",
            &DockerSidecar {
                memory: "512Mi".to_string(),
                storage: PodStorage::Ephemeral,
            },
            &[],
            Duration::from_millis(1),
            &test_instance(),
        )
        .await;
    match result {
        Err(SandboxError::Timeout(_)) => {}
        Err(e) => panic!("expected a Timeout error, got a different error: {e}"),
        Ok(sandbox) => {
            std::mem::forget(sandbox);
            panic!("expected create to time out waiting for Running, but it succeeded");
        }
    }

    let pods = pods_api(&client);
    let gone = tokio::time::timeout(Duration::from_secs(15), async {
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "the pod create() gave up waiting on should have been cleaned up, not left orphaned"
    );
}

/// Every configured volume is mounted into every pod, but its claim can
/// be missing from the pod's namespace — the cluster was rebuilt, the
/// claim was deleted, or (as the browser tier found) the volume was
/// created in another namespace. The pod must still start.
#[tokio::test]
async fn test_create_recreates_a_missing_volume_claim() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pvcs = pvc_api(&client);
    let volume_id = 990_003;
    let claim = sandbox_volume_pvc_name(volume_id);
    pvcs.delete(&claim, &DeleteParams::default()).await.ok();
    let now = chrono::Utc::now().naive_utc();
    let volume = db::SandboxVolume {
        id: volume_id,
        name: "missing-claim".to_string(),
        mount_path: "/missing-claim".to_string(),
        created_at: now,
        updated_at: now,
    };

    let result = manager
        .create(&unique_session_id("missing-claim"), "128Mi", &[volume])
        .await;
    let claim_exists = matches!(pvcs.get_opt(&claim).await, Ok(Some(_)));
    if let Ok(sandbox) = &result {
        pods_api(&client)
            .delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .ok();
    }
    pvcs.delete(&claim, &DeleteParams::default()).await.ok();

    assert!(result.is_ok(), "the pod should start: {:?}", result.err().map(|e| e.to_string()));
    assert!(claim_exists, "the missing claim should have been recreated");
}

#[tokio::test]
async fn test_create_applies_the_memory_limit_and_reserves_nothing() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("limits");

    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let pods = pods_api(&client);
    let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
    let limits = pod
        .spec
        .expect("pod should have a spec")
        .containers
        .into_iter()
        .next()
        .expect("pod should have a container")
        .resources
        .expect("container should have resources");
    let requests = limits.requests.clone().unwrap_or_default();
    let limits = limits.limits.expect("resources should have limits");
    assert_eq!(limits.get("memory"), Some(&Quantity("128Mi".to_string())));
    assert_eq!(requests.get("memory"), Some(&Quantity("0".to_string())), "no memory reserved (SME-77)");
    assert!(!limits.contains_key("cpu") && !requests.contains_key("cpu"), "no CPU limit or request (SME-77)");

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// A `memory_limit` over the `smelt-park` namespace's `LimitRange` max
/// (`k8s/smelt-park-rbac.yaml`, `64Gi`) is rejected by Kubernetes
/// itself — no app-side comparison logic to test, just that the
/// rejection actually happens and surfaces as an ordinary error.
/// Requires the `LimitRange` to actually be applied to the cluster
/// (`k3s-bootstrap`, or the equivalent on `homelab`) — see SME-12's
/// "Per-pod limit overrides".
#[tokio::test]
async fn test_create_rejects_a_memory_limit_over_the_limitrange_max() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("over-limit");

    let result = manager.create(&session_id, "128Gi", &[]).await;
    assert!(
        matches!(result, Err(SandboxError::Kube(_))),
        "a memory_limit over the LimitRange's 64Gi max should be rejected by Kubernetes, got is_ok={}",
        result.is_ok()
    );
}

/// The one real, permanent OOM trigger in this suite (see SME-12's
/// "Testing" on why this isn't a full `cargo build` like the design
/// spike used — a bash builtin growing memory directly in its own
/// process, no fork, needs no `rust:*` image). Proves the whole real
/// path end to end: a genuine kernel OOM kill, Kubernetes actually
/// reporting `OOMKilled`, and `pod_death_reason` extracting it from a
/// *real* API response — the pure unit tests already cover the
/// branching logic exhaustively, but only against constructed data.
#[tokio::test]
async fn test_pod_death_reason_reports_oomkilled_from_a_real_oom_kill() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("real-oom");
    // Small on purpose — fast and reliable to trigger, using SME-12's
    // own per-pod override rather than the real 8Gi default.
    let sandbox = manager
        .create(&session_id, "64Mi", &[])
        .await
        .expect("create should succeed");
    let pods = pods_api(&client);

    // The whole `AttachedProcess` — not just its split-off stdout/
    // stderr handles — is moved into the spawned task, so the exec
    // session it owns stays open for as long as *that task* runs, not
    // tied to this function's own scope. Letting `exec` drop early
    // (e.g. a `{ ... }` block that only spawns readers off split
    // handles) closes the underlying connection, and since this
    // process is directly attached (not `setsid`-detached the way the
    // real agent injection is), the container runtime kills it right
    // along with the disconnect — the remote command never gets a
    // chance to actually run.
    if let Ok(mut exec) = pods
        .exec(
            &sandbox.pod_name,
            ["bash", "-c", "printf -v x '%*s' 200000000 ''; sleep 5"],
            &AttachParams::default(),
        )
        .await
    {
        tokio::spawn(async move {
            let mut out = String::new();
            let mut err = String::new();
            if let Some(mut so) = exec.stdout() {
                let _ = so.read_to_string(&mut out).await;
            }
            if let Some(mut se) = exec.stderr() {
                let _ = se.read_to_string(&mut err).await;
            }
            exec.join().await.ok();
        });
    }

    let reason = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(reason) = pod_death_reason(&pods, &sandbox.pod_name, &test_instance()).await {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;

    // Delete the pod directly before asserting, not through the cleanup
    // queue: a Failed pod is never removed by Kubernetes, and every run
    // used to leave one behind.
    let name = sandbox.pod_name.clone();
    std::mem::forget(sandbox);
    pods.delete(&name, &immediate_delete_params()).await.ok();
    while pods.get_opt(&name).await.ok().flatten().is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let reason = reason.expect("pod should be confirmed dead within 30s of a real OOM trigger");
    assert_eq!(
        reason,
        Some("OOMKilled".to_string()),
        "a real OOM kill should surface as OOMKilled via the real Kubernetes API, not a constructed one"
    );
}

#[tokio::test]
async fn test_create_reuses_existing_running_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("reuse");

    let first = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("first create should succeed");
    let second = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("second create should reuse, not error");

    assert_eq!(first.pod_name, second.pod_name);

    let pods = pods_api(&client);
    pods.delete(&first.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(first);
    std::mem::forget(second);
}

#[tokio::test]
async fn test_exec_captures_stdout_and_exit_code() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("exec");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let result = sandbox
        .exec(&["echo", "hello"])
        .await
        .expect("exec should succeed");
    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, 0);

    let failing = sandbox
        .exec(&["bash", "-c", "exit 7"])
        .await
        .expect("exec should succeed even for nonzero exit");
    assert_eq!(failing.exit_code, 7);

    let pods = pods_api(&client);
    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// New observable behavior from
/// SME-17's Phase 3 —
/// commands run as the pre-created, unprivileged `sandbox` user by
/// default (not root, unlike the old `debian:trixie-slim` image with
/// no `USER` set), with passwordless `sudo` still available for
/// anything that genuinely needs root.
#[tokio::test]
async fn test_sandbox_commands_run_as_non_root_with_sudo_available() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("non-root");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");

    let whoami = sandbox
        .exec(&["whoami"])
        .await
        .expect("exec should succeed");
    assert_eq!(
        whoami.stdout.trim(),
        "sandbox",
        "commands should run as the pre-created `sandbox` user, not root"
    );

    let sudo_whoami = sandbox
        .exec(&["sudo", "whoami"])
        .await
        .expect("exec should succeed");
    assert_eq!(
        sudo_whoami.stdout.trim(),
        "root",
        "sudo should still reach root for a command that genuinely needs it"
    );

    // The everyday tools are already there: a first coding task used
    // to fail with `python3: command not found` and spend a minute
    // installing it (SME-41 D10).
    for tool in ["python3", "git", "curl"] {
        let found = sandbox
            .exec(&["sh", "-c", &format!("command -v {tool}")])
            .await
            .expect("exec should succeed");
        assert!(!found.stdout.trim().is_empty(), "{tool} should be installed in the sandbox image");
    }

    let pods = pods_api(&client);
    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .ok();
    std::mem::forget(sandbox);
}

/// Characterization test for `pod_death_reason`'s real fetch, not
/// `decide_pod_death_reason`'s branching (already exhaustively covered
/// without a cluster) — proves the wrapper actually calls through to a
/// live pod correctly in both directions: inconclusive while it's
/// genuinely running, confirmed (with no reason to report) once it's
/// gone entirely.
#[tokio::test]
async fn test_pod_death_reason_reflects_a_real_pod_then_its_absence() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("death-reason");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");
    let pods = pods_api(&client);

    assert_eq!(
        pod_death_reason(&pods, &sandbox.pod_name, &test_instance()).await,
        None,
        "a genuinely Running pod is inconclusive"
    );

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .expect("delete should succeed");
    assert_eq!(
        pod_death_reason(&pods, &sandbox.pod_name, &test_instance()).await,
        Some(None),
        "a pod that's gone entirely is confirmed dead with no reason to report"
    );
    std::mem::forget(sandbox);
}

// No standalone `force_terminate_pod` test: it's reached through
// `terminate_pod`, which `pod_lifecycle_tests` exercise many times over.
// A test of it would never set the process-global `MANAGER` (a kube
// client works only while the runtime that built it runs, and each test
// has its own: on SME-42 that broke the lifecycle test with
// `Kube(Service(Closed))`); it would call `use_test_manager` instead.

#[tokio::test]
async fn test_manager_delete_removes_the_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("delete");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");
    let pod_name = sandbox.pod_name.clone();

    manager
        .delete(sandbox)
        .await
        .expect("delete should succeed");

    // Pods are deleted with a grace period (SME-51 B6), and the agent
    // exits on SIGTERM, so the pod goes well within it.
    let pods = pods_api(&client);
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        while pods.get_opt(&pod_name).await.expect("get_opt should not error").is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the pod should stop within seconds of manager.delete, not wait out its grace");
}

#[tokio::test]
async fn test_dropping_without_delete_still_cleans_up_via_drain_task() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("drop");
    let sandbox = manager
        .create(&session_id, "128Mi", &[])
        .await
        .expect("create should succeed");
    let pod_name = sandbox.pod_name.clone();

    drop(sandbox); // no manager.delete call — this is the path under test

    let pods = pods_api(&client);
    let gone = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if pods
                .get_opt(&pod_name)
                .await
                .expect("get_opt should not error")
                .is_none()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "drain task should have deleted the pod within the timeout"
    );
}

/// The futures of the sandbox calls that sit deepest in the dev server's
/// and the tests' call stacks stay small. A debug build runs them on a
/// 2 MB stack: on SME-115 holding a whole `Pod` across an await doubled
/// `force_terminate_pod`'s future and every caller's, and the first sign
/// was `test_terminal_lifecycle_end_to_end` overflowing its stack. Each
/// bound is about twice the size when this test was written; a failure
/// here means keep large values out of an await's scope or `Box::pin`
/// the large sub-future, not raise the bound. Nothing is polled, so this
/// needs neither a cluster nor a database.
#[tokio::test]
async fn test_sandbox_futures_stay_small() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/unused")
        .expect("lazy pool");
    let client = kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().expect("url"))).expect("client");
    let instance = db::SmeltInstance { id: "size-test".to_string(), owns_unlabelled: true };

    let sizes = [
        ("force_terminate_pod", std::mem::size_of_val(&force_terminate_pod(&pool, 1)), 1200),
        ("force_terminate_pod_with", std::mem::size_of_val(&force_terminate_pod_with(&pool, &client, 1)), 600),
        ("close_if_gone", std::mem::size_of_val(&watch::close_if_gone(&pool, 1)), 4200),
        ("close_if_gone_with", std::mem::size_of_val(&watch::close_if_gone_with(&pool, &client, 1)), 4000),
        ("create_pod", std::mem::size_of_val(&create_pod(&pool, 1, PodLimitOverrides::default())), 15000),
        ("ensure_conversation_pvcs", std::mem::size_of_val(&claims::ensure_conversation_pvcs(&client, 1, &instance)), 6700),
    ];
    for (name, size, bound) in sizes {
        println!("{name}: {size} bytes (bound {bound})");
    }
    for (name, size, bound) in sizes {
        assert!(size <= bound, "{name}'s future is {size} bytes, over its bound of {bound}");
    }
}

/// SME-94 review 1: `use_test_manager` refuses a multi-thread runtime,
/// where a task on another worker would read the process-wide manager.
#[test]
fn test_a_test_manager_on_a_multi_thread_runtime_is_refused() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let refused = std::thread::spawn(move || {
        runtime.block_on(async { std::panic::catch_unwind(|| use_test_manager(unreachable_client())).is_err() })
    })
    .join()
    .expect("the test thread");
    assert!(refused, "use_test_manager accepted a multi-thread runtime");
}

/// A server with no `SANDBOX_IMAGE` runs the image named after its own
/// agent sources, the one `scripts/sandbox-image-ref` prints and
/// `scripts/build-sandbox-image.sh` builds, never a `:latest` that only a
/// manual rebuild moves (SME-121). Compares with the script itself, so
/// `build.rs`'s hash can't drift from it.
#[test]
fn test_with_no_setting_the_image_is_the_one_named_after_this_trees_agent_sources() {
    let out = std::process::Command::new("scripts/sandbox-image-ref").output().expect("run scripts/sandbox-image-ref");
    assert!(out.status.success(), "scripts/sandbox-image-ref failed: {}", String::from_utf8_lossy(&out.stderr));
    let script = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(sandbox_image_from(None), script.trim());
    assert_eq!(sandbox_image_from(Some(String::new())), script.trim(), "an empty setting is no setting");
}

#[test]
fn test_a_sandbox_image_setting_names_the_image() {
    let image = "docker.io/library/smelt-sandbox:latest";
    assert_eq!(sandbox_image_from(Some(image.to_string())), image);
}

/// What `ClusterDialer::image` reads from a refused pod is the reference
/// `create_pod` gave it, so a pod made from the current image compares
/// equal to the one a new pod gets (SME-121).
#[test]
fn test_a_pods_sandbox_image_reads_back_as_the_one_it_was_built_with() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    assert_eq!(sandbox_container_image(&pod), Some(default_sandbox_image()));
    assert_eq!(sandbox_container_image(&Pod::default()), None);
}
