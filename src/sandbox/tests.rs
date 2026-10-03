use super::*;

async fn test_client() -> kube::Client {
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[]);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[]);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[]);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker, &[]);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &volumes);
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
    let labelled = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[]);
    assert_eq!(
        labelled.metadata.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)).map(String::as_str),
        Some("42"),
        "create_pod finds a conversation's still-stopping pods by this label"
    );
    let ephemeral = DockerSidecar {
        storage: PodStorage::Ephemeral,
        ..docker_for_conversation(42)
    };
    let unlabelled = build_pod_spec("sandbox-1", "1Gi", &ephemeral, &[]);
    assert!(unlabelled.metadata.labels.and_then(|l| l.get(CONVERSATION_LABEL).cloned()).is_none());
}

fn claim(name: &str, conversation: Option<&str>) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: conversation
                .map(|c| [(CONVERSATION_LABEL.to_string(), c.to_string())].into()),
            ..Default::default()
        },
        ..Default::default()
    }
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
    assert_eq!(orphaned_docker_claims(&claims, &live), vec![2]);
    // A conversation has two claims (Docker data and /workspace): it's
    // named once.
    let both = vec![
        claim("sandbox-docker-4", Some("4")),
        claim("sandbox-workspace-4", Some("4")),
    ];
    assert_eq!(orphaned_docker_claims(&both, &live), vec![4]);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[]);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[]);
    assert_eq!(
        pod_container_limits(&pod),
        (vec!["1Gi".to_string(), "2Gi".to_string()], Vec::<String>::new())
    );
}

#[test]
fn test_docker_pvc_spec_is_named_labelled_and_sized_for_the_conversation() {
    let [pvc, workspace] = conversation_pvc_specs(42);
    assert_eq!(workspace.metadata.name.as_deref(), Some("sandbox-workspace-42"));
    assert_eq!(workspace.metadata.labels, pvc.metadata.labels);
    assert_eq!(pvc.metadata.name.as_deref(), Some("sandbox-docker-42"));
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
    // low range `test_terminal_lifecycle_end_to_end` wipes and uses.
    let first_id = (uuid_like().parse::<u128>().unwrap() % 1_000_000_000) as i64 + 1_000_000;
    sqlx::query("SELECT setval(pg_get_serial_sequence('sandbox_pods', 'id'), $1)")
        .bind(first_id)
        .execute(&pool)
        .await
        .expect("move the pod id sequence");
    // Its own client and manager, never the process-global one: a
    // manager set here would die with this test's runtime and break
    // every later test that uses `get()` (seen as `Kube(Service(Closed))`
    // in `test_terminal_lifecycle_end_to_end`). The pod is created
    // under the name the database row gives it, as `create_pod` would.
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());

    let with_pod = db::create_conversation(&pool).await.expect("create conversation");
    let without_pod = db::create_conversation(&pool).await.expect("create conversation");
    let row = db::create_sandbox_pod(&pool, with_pod.id).await.expect("create the pod's row");
    let sandbox = manager
        .create(&row.id.to_string(), "128Mi", &[])
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
/// Read from a cell of the test's own, since
/// `test_terminal_lifecycle_end_to_end` sets the process-wide one.
#[test]
fn test_the_manager_before_init_is_not_initialized() {
    let cell = OnceLock::new();
    let result = manager_in(&cell);
    assert!(matches!(result, Err(SandboxError::NotInitialized)), "expected NotInitialized");
}

fn unique_session_id(label: &str) -> String {
    format!("test-{label}-{}", uuid_like())
}

/// The `unique_session_id` label `test_terminal_lifecycle_end_to_end`'s
/// own volume-mount pod uses — pulled out as a constant so the
/// precheck below (which needs the *prefix*, sans the pod's own
/// unpredictable nanosecond suffix) can't drift out of sync with the
/// actual pod-creation call site.
const VOLUME_MOUNT_SESSION_LABEL: &str = "volume-mount";

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
    ensure_conversation_pvcs(&client, conversation_id).await.expect("ensure docker claim");
    // Every pod this test makes, so cleanup below finds them even after
    // a failed assertion unwinds out of the checks.
    let created: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

    let checks = tokio::time::timeout(Duration::from_secs(300), async {
        let first = manager
            .create_with_docker(&unique_session_id("docker"), "256Mi", &docker, &[])
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
            .create_with_docker(&unique_session_id("docker"), "256Mi", &docker, &[])
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
        .create_with_docker(&unique_session_id("docker-oom"), "128Mi", &docker, &[])
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
    ensure_conversation_pvcs(&client, conversation_id).await.expect("ensure docker claim");
    let sandbox = manager
        .create_with_docker(&unique_session_id("stopping"), "128Mi", &docker, &[])
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
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(60)).await;
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
    for id in [deleted.id, live.id] {
        ensure_conversation_pvcs(&client, id + offset).await.expect("claims");
    }

    clean_up_after_failed_start(&pool, &client, deleted.id, deleted.id + offset).await;
    clean_up_after_failed_start(&pool, &client, live.id, live.id + offset).await;

    let gone = pvcs.get_opt(&docker_pvc_name(deleted.id + offset)).await.expect("get").is_none_or(|p| p.metadata.deletion_timestamp.is_some());
    let kept = pvcs.get_opt(&docker_pvc_name(live.id + offset)).await.expect("get").is_some();
    delete_conversation_pvcs(&client, live.id + offset).await;
    delete_conversation_pvcs(&client, deleted.id + offset).await;
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
        "metadata": {"name": name, "labels": {CONVERSATION_LABEL: conversation_id.to_string()}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    // A language server pod of the same conversation (SME-35), under
    // its own label.
    let server = format!("lsp-{conversation_id}-x");
    let server_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": server, "labels": {crate::lsp::pods::LSP_OF_LABEL: conversation_id.to_string()}},
        "spec": {"containers": [{"name": "server", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &server_pod).await.expect("create server pod");

    teardown_conversation_with(&client, conversation_id, &[]).await;
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
async fn test_teardown_deletes_an_unlabelled_pod_named_by_its_record() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let pod_id = conversation_id + 1;
    let name = pod_name(pod_id);
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    teardown_conversation_with(&client, conversation_id, &[pod_id]).await;
    let gone = tokio::time::timeout(Duration::from_secs(60), async {
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .is_ok();

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(gone, "{name} survived its conversation's teardown");
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

    ensure_conversation_pvcs(&client, conversation_id).await.expect("first ensure should create");
    let first = pvcs.get_opt(&name).await.expect("get claim");
    let first_uid = first.and_then(|p| p.metadata.uid);
    assert!(first_uid.is_some(), "ensure_docker_pvc should create {name}");

    ensure_conversation_pvcs(&client, conversation_id).await.expect("second ensure should reuse");
    let second_uid = pvcs.get_opt(&name).await.expect("get claim").and_then(|p| p.metadata.uid);
    assert_eq!(first_uid, second_uid, "a second ensure must reuse the claim, not replace it");

    delete_conversation_pvcs(&client, conversation_id).await;
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

/// A single, comprehensive, real-cluster-and-real-Postgres integration
/// test covering the terminal *and* file-tool lifecycle end to end,
/// including the one-pod-per-conversation guard — deliberately one
/// large test, not many small ones: the free functions (`create_pod`,
/// `create_terminal`, ...) reach through the process-global `MANAGER`
/// singleton (mirroring `db::init()`/`db::get()`), and initializing it
/// from more than one `#[tokio::test]`/`#[sqlx::test]` function would
/// risk the same cross-runtime-reuse hazard `docs/testing.md` documents
/// for `PgPool` (each test gets its own tokio runtime) — this is the
/// one place in the whole suite that touches `MANAGER` at all, so
/// there's nothing to race with. Pod isolation, previously shown via
/// two pods in one conversation, now uses two separate conversations —
/// a conversation can have at most one live pod, see
/// SME-11's "One pod per conversation."
#[sqlx::test]
async fn test_terminal_lifecycle_end_to_end(pool: PgPool) {
    // Every terminal command that finishes during this test triggers a
    // `turn::notify` wake (see SME-13). This
    // test's database has no model provider (SME-72), so each one
    // fails at once without reaching any model.
    // Those wakes touch process-wide turn state keyed by conversation
    // id, as the chat tests' turns do.
    let _turns = crate::providers::test_support::lock_turn_tests();

    let client = test_client().await;
    MANAGER.set(SandboxManager::new(client.clone())).ok();

    // pod_id/terminal_id are now DB-generated (see SME-9's "How") —
    // each `#[sqlx::test]` run gets a *fresh* isolated Postgres database
    // whose identity sequences restart at 1, but this test still talks
    // to the one *real, shared* k3s cluster, so a from-scratch pod_id
    // sequence would collide with k8s pod names ("sandbox-1",
    // "sandbox-2", ...) left over from a previous or concurrent run of
    // *this specific test* — no other test function creates pods this
    // way (the rest all use `manager.create` directly with
    // nanosecond-entropy session ids, producing "sandbox-test-*"
    // names, untouched by this). A small pre-emptive wipe of the low
    // integer range this run will actually use is enough.
    let pods_precheck = pods_api(&client);
    for n in 1..=30i64 {
        pods_precheck
            .delete(&pod_name(n), &immediate_delete_params())
            .await
            .ok();
    }
    // Same reasoning as the pod-name wipe above, for
    // `create_volume`/`delete_volume`'s PVCs (`sandbox-volume-{id}`) —
    // this test's own `sandbox_volumes` row always lands on a
    // low integer id in a fresh `#[sqlx::test]` database, but a
    // previous run's PVC of the same k8s name can still be sitting
    // around if that run failed before reaching its own cleanup.
    let pvcs_precheck = pvc_api(&client);
    for n in 1..=30i64 {
        pvcs_precheck
            .delete(&sandbox_volume_pvc_name(n), &DeleteParams::default())
            .await
            .ok();
        pvcs_precheck
            .delete(&docker_pvc_name(n), &DeleteParams::default())
            .await
            .ok();
        pvcs_precheck
            .delete(&workspace_pvc_name(n), &DeleteParams::default())
            .await
            .ok();
    }
    // Same reasoning again, for the volume-mount pod itself
    // (`unique_session_id(VOLUME_MOUNT_SESSION_LABEL)`) — its name
    // carries a nanosecond suffix, not a low integer, so a fixed-range
    // wipe like the two above can't cover it; list and filter by
    // prefix instead. A previous run's pod here is what actually
    // starved the PVC precheck above of its point: `local-path`'s
    // `WaitForFirstConsumer` binding waits on *a* pod using the claim
    // reaching `Scheduled`, and a pile of these left `Pending` forever
    // (nothing schedules them — SandboxManager::create now cleans up
    // its own timeout, but this covers every prior run before that
    // fix, and any future failure mode that leaves one behind again)
    // starves that wait indefinitely. Safe to sweep unconditionally:
    // this is the only place in the whole suite that uses this label,
    // so nothing concurrently running can collide with it.
    let volume_mount_pod_prefix = format!("sandbox-test-{VOLUME_MOUNT_SESSION_LABEL}-");
    if let Ok(existing) = pods_precheck.list(&ListParams::default()).await {
        for pod in existing.items {
            if let Some(name) = pod.metadata.name.as_deref() {
                if name.starts_with(&volume_mount_pod_prefix) {
                    pods_precheck
                        .delete(name, &immediate_delete_params())
                        .await
                        .ok();
                }
            }
        }
    }

    let conversation_a = db::create_conversation(&pool)
        .await
        .expect("create conversation a");
    let conversation_b = db::create_conversation(&pool)
        .await
        .expect("create conversation b");
    let conversation_c = db::create_conversation(&pool)
        .await
        .expect("create conversation c");
    let conversation_d = db::create_conversation(&pool)
        .await
        .expect("create conversation d");

    let outcome = tokio::time::timeout(Duration::from_secs(400), async {
        // --- Guards fire before there's anything to guard against yet ---
        let too_early = create_terminal(&pool, conversation_a.id).await;
        assert!(
            matches!(too_early, Err(TerminalError::NoPod)),
            "create_terminal before any pod exists should fail with NoPod, got {too_early:?}"
        );

        // --- One pod per conversation: create_pod refuses a second
        // live pod, list reflects reality ---
        // A stored key and commit identity reach every new pod (SME-32).
        let key = crate::git::generate_key("lifecycle");
        db::create_ssh_key(&pool, "lifecycle", &key.public_key, &key.private_key)
            .await
            .expect("store key");
        db::set_git_identity(
            &pool,
            &crate::git::GitIdentity {
                name: "Lifecycle Test".to_string(),
                email: "lifecycle@example.com".to_string(),
            },
        )
        .await
        .expect("store identity");
        let pod_a = create_pod(&pool, conversation_a.id, PodLimitOverrides::default()).await.expect("create_pod (a) should succeed");
        let git_check = exec_with(
            &client,
            &pod_name(pod_a),
            "sandbox",
            &["sh", "-c", "test -s /etc/smelt/keys/lifecycle && git config user.email"],
            None,
        )
        .await
        .expect("exec git check");
        assert_eq!(
            (git_check.exit_code, git_check.stdout.trim()),
            (0, "lifecycle@example.com"),
            "a new pod has the stored key and identity"
        );
        // /workspace is the conversation's claim, whose root the
        // provisioner owns; the sandbox user gets it (SME-32 code
        // review 2, finding 6).
        let owner = exec_with(&client, &pod_name(pod_a), "sandbox", &["stat", "-c", "%U:%G", "/workspace"], None)
            .await
            .expect("exec stat");
        assert_eq!(owner.stdout.trim(), "sandbox:sandbox", "/workspace belongs to the sandbox user");

        // clone_repo records the repo and checks it out in the pod.
        let origin = exec_with(
            &client,
            &pod_name(pod_a),
            "sandbox",
            &["sh", "-c", "set -e; git init -q -b main /tmp/src; cd /tmp/src; echo 'Run make test.' > AGENTS.md; git add AGENTS.md; git commit -qm one; git clone -q --bare /tmp/src /tmp/origin.git; git rev-parse HEAD"],
            None,
        )
        .await
        .expect("exec make origin");
        assert_eq!(origin.exit_code, 0, "make origin: {}{}", origin.stdout, origin.stderr);
        let repo = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, None)
            .await
            .expect("clone_repo");
        assert_eq!(repo.path, "/workspace/origin");
        assert_eq!(repo.status, crate::git::RepoStatus::Ready);
        assert_eq!(repo.commit.as_deref(), Some(origin.stdout.trim()));
        assert_eq!(repo.branch.as_deref(), Some("main"));
        // The clone lists its AGENTS.md; nothing loads by itself.
        assert_eq!(repo.agents_files, vec!["AGENTS.md".to_string()]);
        assert!(crate::git::project_instructions(&pool, conversation_a.id).await.expect("loaded").is_empty());
        // The model asks to load it: a remote the user hasn't decided
        // about, so they're asked, with exactly that file.
        let asked = crate::git::load_instructions(&pool, conversation_a.id, "/workspace/origin/AGENTS.md")
            .await
            .expect("load_instructions");
        assert!(asked.contains("being asked"), "{asked}");
        let repos = crate::git::list_repos(&pool, conversation_a.id).await.expect("list repos");
        assert_eq!(repos[0].trust_requests.len(), 1);
        assert_eq!(repos[0].trust_requests[0].content, "Run make test.\n");
        // Trusted, that file is in the model's context.
        let shown = &repos[0].trust_requests[0];
        crate::git::decide_trust(&pool, conversation_a.id, shown.id, &shown.hash, true)
            .await
            .expect("trust");
        let loaded = crate::git::project_instructions(&pool, conversation_a.id)
            .await
            .expect("project instructions");
        assert_eq!(loaded.len(), 1, "{loaded:?}");
        assert_eq!(loaded[0].content, "Run make test.\n");
        assert_eq!(loaded[0].path, "/workspace/origin/AGENTS.md");
        assert_eq!(loaded[0].commit, repo.commit);
        // A failed clone retried with another branch (or URL) clones
        // what's asked for now, not what failed (SME-32 code review,
        // finding 1).
        let failed = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", Some("nope"), Some("retry"))
            .await
            .expect_err("there's no branch nope");
        assert!(failed.contains("nope"), "{failed}");
        let retried = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, Some("retry"))
            .await
            .expect("the retry clones the default branch");
        assert_eq!(retried.requested_branch, None);
        assert_eq!(retried.branch.as_deref(), Some("main"));
        assert_eq!(retried.status, crate::git::RepoStatus::Ready);

        // A clone refused because the directory already holds work
        // doesn't delete that work when retried; only a clone that was
        // interrupted gets its directory replaced (SME-32 code review
        // 2, finding 1).
        let work = exec_with(
            &client,
            &pod_name(pod_a),
            "sandbox",
            &["sh", "-c", "mkdir -p /workspace/mywork && echo precious > /workspace/mywork/notes.txt"],
            None,
        )
        .await
        .expect("exec make work");
        assert_eq!(work.exit_code, 0, "{}", work.stderr);
        for attempt in ["first", "retry"] {
            let refused = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, Some("mywork"))
                .await
                .expect_err("the directory holds work");
            assert!(refused.contains("already exists"), "{attempt}: {refused}");
        }
        let kept = exec_with(&client, &pod_name(pod_a), "sandbox", &["cat", "/workspace/mywork/notes.txt"], None)
            .await
            .expect("exec cat");
        assert_eq!(kept.stdout, "precious\n", "a retry must not delete the work");
        let mywork = db::list_conversation_repos(&pool, conversation_a.id)
            .await
            .expect("list")
            .into_iter()
            .find(|r| r.dir == "mywork")
            .expect("the mywork repo");
        // Even marked interrupted: that directory held work before any
        // clone, so a retry leaves it (SME-32 code review 3).
        db::set_repo_failed(&pool, mywork.id, mywork.attempt, crate::git::CLONE_INTERRUPTED).await.expect("mark interrupted");
        let refused = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, Some("mywork"))
            .await
            .expect_err("the directory still holds work");
        assert!(refused.contains("already exists"), "{refused}");
        let kept = exec_with(&client, &pod_name(pod_a), "sandbox", &["cat", "/workspace/mywork/notes.txt"], None)
            .await
            .expect("exec cat");
        assert_eq!(kept.stdout, "precious\n");

        // A brand-new empty repo clones: its branch, no commit yet.
        let empty = exec_with(&client, &pod_name(pod_a), "sandbox", &["git", "init", "-q", "--bare", "-b", "main", "/tmp/empty.git"], None)
            .await
            .expect("exec init empty");
        assert_eq!(empty.exit_code, 0, "{}", empty.stderr);
        let cloned_empty = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/empty.git", None, None)
            .await
            .expect("an empty repo clones");
        assert_eq!(cloned_empty.status, crate::git::RepoStatus::Ready);
        assert_eq!((cloned_empty.branch.as_deref(), cloned_empty.commit.as_deref()), (Some("main"), None));

        // Asking again returns the same checkout rather than a second clone.
        let again = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, None)
            .await
            .expect("clone_repo again");
        assert_eq!(again.id, repo.id);
        let wrote = exec_with(
            &client,
            &pod_name(pod_a),
            "sandbox",
            &["sh", "-c", "echo 'not pushed yet' > /workspace/origin/uncommitted.txt"],
            None,
        )
        .await
        .expect("exec write");
        assert_eq!(wrote.exit_code, 0, "{}", wrote.stderr);

        // AGENTS.md changes in the checkout: what's loaded stays until
        // the model loads it again, which a trusted repo allows at once.
        let edited = exec_with(
            &client,
            &pod_name(pod_a),
            "sandbox",
            &["sh", "-c", "echo 'Run make lint too.' >> /workspace/origin/AGENTS.md"],
            None,
        )
        .await
        .expect("exec edit");
        assert_eq!(edited.exit_code, 0, "{}", edited.stderr);
        let loaded = crate::git::project_instructions(&pool, conversation_a.id).await.expect("loaded");
        assert_eq!(loaded[0].content, "Run make test.\n", "still the loaded version");
        let reloaded = crate::git::load_instructions(&pool, conversation_a.id, "/workspace/origin/AGENTS.md")
            .await
            .expect("load again");
        assert!(reloaded.starts_with("Loaded"), "{reloaded}");
        let loaded = crate::git::project_instructions(&pool, conversation_a.id).await.expect("loaded");
        assert_eq!(loaded.len(), 1, "replaced, not added: {loaded:?}");
        assert_eq!(loaded[0].content, "Run make test.\nRun make lint too.\n");
        let duplicate = create_pod(&pool, conversation_a.id, PodLimitOverrides::default()).await;
        assert!(
            matches!(duplicate, Err(SandboxError::PodAlreadyExists)),
            "create_pod should refuse a second live pod for the same conversation, got {duplicate:?}"
        );
        let pods_listed = list_pods(&pool, conversation_a.id).await.expect("list_pods should succeed");
        assert_eq!(pods_listed.len(), 1, "exactly the one pod should be listed");
        assert_eq!(pods_listed[0].status, "Running");

        // --- Terminal: N per pod, no idempotency ---
        let terminal_a1 = create_terminal(&pool, conversation_a.id).await.expect("create_terminal (a1) should succeed");
        let terminal_a2 = create_terminal(&pool, conversation_a.id).await.expect("create_terminal (a2) should succeed");
        assert_ne!(terminal_a1, terminal_a2, "every create_terminal call should mint a distinct terminal");

        let terminals_listed = list_terminals(&pool, conversation_a.id).await.expect("list_terminals");
        assert_eq!(terminals_listed.len(), 2, "both terminals in the one pod should be listed");
        assert!(terminals_listed.iter().all(|t| t.status == "connected"));
        assert!(terminals_listed.iter().all(|t| t.pod_id == pod_a));

        // terminate_pod must now refuse for conversation_a — it still has terminals.
        let blocked = terminate_pod(&pool, conversation_a.id).await;
        assert!(
            matches!(blocked, Err(TerminalError::TerminalStillExists)),
            "terminate_pod should refuse while a terminal exists, got {blocked:?}"
        );

        // --- Two terminals in the same pod are genuinely independent ---
        run_and_wait(&pool, conversation_a.id, terminal_a1, "cd-a1", "cd /tmp").await;
        run_and_wait(&pool, conversation_a.id, terminal_a2, "cd-a2", "cd /var").await;
        let pwd_a1 = run_and_wait(&pool, conversation_a.id, terminal_a1, "pwd-a1", "pwd").await;
        let pwd_a2 = run_and_wait(&pool, conversation_a.id, terminal_a2, "pwd-a2", "pwd").await;
        assert_eq!(first_stdout_line(&pool, &pwd_a1).await, "/tmp");
        assert_eq!(first_stdout_line(&pool, &pwd_a2).await, "/var");

        // --- Concurrency: a long command in one terminal doesn't block
        // a sibling terminal in the same pod from running its own ---
        let long_id = "long-a1";
        db::create_terminal_command(&pool, conversation_a.id, terminal_a1, long_id, "sleep 5")
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal_a1, long_id, "sleep 5").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await; // let it actually start

        let quick_id = "quick-a2";
        db::create_terminal_command(&pool, conversation_a.id, terminal_a2, quick_id, "echo still_alive")
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal_a2, quick_id, "echo still_alive").await.expect("send_command");
        let quick_status = poll_until_finished(&pool, quick_id).await;
        assert_eq!(
            quick_status.status, "finished",
            "terminal_a2 should complete a command while terminal_a1's sleep is still running"
        );

        send_signal(&pool, terminal_a1, long_id, "KILL").await.expect("send_signal");
        poll_until_finished(&pool, long_id).await;

        // --- A terminal command's own exit event actively wakes the
        // model — no other trigger needed (see
        // SME-13). This test's database has no model provider, so the
        // resulting wake_conversation call fails before any model call — but the
        // notification text is drained and durably persisted *before*
        // that failing call, and the failure itself publishes a visible
        // event; both are observable without ever touching a real (or
        // mock) Anthropic endpoint, and without this test doing anything
        // else that would otherwise trigger the passive backlog drain. ---
        let mut wake_events = events::subscribe(conversation_a.id);
        let wake_command_id = "wake-a1";
        db::create_terminal_command(&pool, conversation_a.id, terminal_a1, wake_command_id, "true")
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal_a1, wake_command_id, "true").await.expect("send_command");

        let saw_wake_failure = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match wake_events.recv().await {
                    Ok(events::ConversationEvent::NotificationDeliveryFailed { .. }) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            saw_wake_failure,
            "the command's own exit event should have actively woken the model \
                 (surfacing as NotificationDeliveryFailed since this test's database has \
                 no model provider), with no other trigger — not just sat unnotified waiting \
                 for something else to happen to the conversation"
        );

        let messages_after_wake = db::list_messages(&pool, conversation_a.id).await.expect("list_messages");
        assert!(
            messages_after_wake.iter().any(|m| {
                m.blocks().ok().is_some_and(|blocks| {
                    blocks.iter().any(|b| matches!(
                        b,
                        crate::anthropic::ContentBlock::Text { text }
                            if text.contains(wake_command_id) && text.contains("finished")
                    ))
                })
            }),
            "the notification message itself should be durably persisted even though \
                 the follow-up API call failed, got: {messages_after_wake:?}"
        );

        // --- Pod isolation: a file in conversation_a's pod is
        // invisible from conversation_b's — two separate conversations
        // now, since one conversation can't have two live pods. ---
        create_pod(&pool, conversation_b.id, PodLimitOverrides::default()).await.expect("create_pod (b) should succeed");
        let terminal_b1 = create_terminal(&pool, conversation_b.id).await.expect("create_terminal (b1) should succeed");
        run_and_wait(&pool, conversation_a.id, terminal_a1, "write-marker", "echo marker > /tmp/isolation-marker").await;
        let check = run_and_wait(&pool, conversation_b.id, terminal_b1, "check-marker", "cat /tmp/isolation-marker 2>&1; echo EXIT:$?").await;
        let lines = db::read_terminal_output(&pool, &check, &["stdout"], 0, 10)
            .await
            .expect("read_terminal_output");
        let joined = lines.iter().map(|l| l.data.as_str()).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("EXIT:1") || joined.contains("No such file"),
            "conversation_b's pod should not see a file written in conversation_a's, got: {joined}"
        );
        terminate_terminal(&pool, terminal_b1).await.expect("terminate_terminal (b1)");
        terminate_pod(&pool, conversation_b.id).await.expect("terminate_pod (b) should succeed");

        // --- terminate_terminal is guarded on a running command, per terminal ---
        let blocking_id = "blocking-a2";
        db::create_terminal_command(&pool, conversation_a.id, terminal_a2, blocking_id, "sleep 30")
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal_a2, blocking_id, "sleep 30").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let blocked_terminate = terminate_terminal(&pool, terminal_a2).await;
        assert!(
            matches!(blocked_terminate, Err(TerminalError::CommandStillRunning)),
            "terminate_terminal should refuse while a command is running, got {blocked_terminate:?}"
        );
        send_signal(&pool, terminal_a2, blocking_id, "KILL").await.expect("send_signal");
        poll_until_finished(&pool, blocking_id).await;

        // --- terminate_terminal on a2 leaves a1 (same pod) untouched ---
        terminate_terminal(&pool, terminal_a2)
            .await
            .expect("terminate_terminal (a2) should succeed once no command is running");
        // Idempotent on repeat.
        terminate_terminal(&pool, terminal_a2).await.expect("terminate_terminal (a2) should be idempotent");

        let still_pwd = run_and_wait(&pool, conversation_a.id, terminal_a1, "pwd-a1-again", "pwd").await;
        assert_eq!(
            first_stdout_line(&pool, &still_pwd).await, "/tmp",
            "terminal_a1 should be completely unaffected by terminating its sibling terminal_a2"
        );

        // --- list_commands is scoped per terminal, history survives termination ---
        let a2_history = db::list_terminal_commands(&pool, terminal_a2, 10)
            .await
            .expect("list_terminal_commands should still work for a terminated terminal");
        assert!(a2_history.iter().any(|c| c.command_id == blocking_id));
        assert!(
            !a2_history.iter().any(|c| c.command_id == "cd-a1"),
            "list_commands should not leak another terminal's history"
        );

        // --- File tools: write_file/read_file/edit_file/list_directory,
        // all pod-scoped (no terminal_id needed) against conversation_a's
        // still-live pod. ---
        let file_path = "/tmp/file-tools-test/example.txt";

        // write_file creates a new file — no expected_hash needed.
        let hash1 = write_file(&pool, conversation_a.id, file_path, "line one\nline two\n", None)
            .await
            .expect("write_file (create) should succeed");

        // read_file returns numbered lines, total_lines, and the same
        // hash write_file just returned.
        let read1 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read1.lines, vec!["line one", "line two"]);
        assert_eq!(read1.total_lines, 2);
        assert_eq!(read1.hash, hash1);

        // edit_file with the correct expected_hash succeeds and returns a new hash.
        let hash2 = edit_file(&pool, conversation_a.id, file_path, "line one", "line ONE", false, hash1.clone(), None)
            .await
            .expect("edit_file should succeed");
        assert_ne!(hash2, hash1);
        let read2 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read2.lines, vec!["line ONE", "line two"]);
        assert_eq!(read2.hash, hash2);

        // --- Staleness: a terminal command changes the file out from
        // under a stale hash — edit_file/write_file must refuse rather
        // than clobber it. ---
        run_and_wait(&pool, conversation_a.id, terminal_a1, "external-write", &format!("echo changed_externally > {file_path}")).await;
        let stale_edit = edit_file(&pool, conversation_a.id, file_path, "line ONE", "line X", false, hash2.clone(), None).await;
        assert!(
            matches!(stale_edit, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "edit_file should refuse a stale expected_hash, got {stale_edit:?}"
        );
        let stale_write = write_file(&pool, conversation_a.id, file_path, "clobber", Some(hash2.clone())).await;
        assert!(
            matches!(stale_write, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "write_file should refuse a stale expected_hash, got {stale_write:?}"
        );

        let read3 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read3.lines, vec!["changed_externally"]);

        // write_file with the *current* hash succeeds (a legitimate overwrite).
        let hash4 = write_file(&pool, conversation_a.id, file_path, "alpha\nalpha\nbeta\n", Some(read3.hash.clone()))
            .await
            .expect("write_file (overwrite) with the current hash should succeed");

        // --- expected_line targets one specific occurrence among
        // identical repeated lines. ---
        let hash5 = edit_file(&pool, conversation_a.id, file_path, "alpha", "ALPHA", false, hash4.clone(), Some(2))
            .await
            .expect("edit_file with expected_line should succeed");
        let read5 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read5.lines, vec!["alpha", "ALPHA", "beta"], "expected_line should have targeted only the second occurrence");
        assert_eq!(read5.hash, hash5);

        // --- replace_all replaces every occurrence ---
        let hash6 = write_file(&pool, conversation_a.id, file_path, "dup\ndup\ndup\n", Some(read5.hash.clone()))
            .await
            .expect("write_file (overwrite) should succeed");
        edit_file(&pool, conversation_a.id, file_path, "dup", "rep", true, hash6.clone(), None)
            .await
            .expect("edit_file with replace_all should succeed");
        let read7 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read7.lines, vec!["rep", "rep", "rep"]);

        // --- Ambiguous match without replace_all/expected_line is a clear error ---
        let ambiguous = edit_file(&pool, conversation_a.id, file_path, "rep", "x", false, read7.hash.clone(), None).await;
        assert!(
            matches!(ambiguous, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "expected an ambiguous-match error, got {ambiguous:?}"
        );

        // --- read_file on a nonexistent path is a clear error, not a panic ---
        let missing = read_file(&pool, conversation_a.id, "/tmp/file-tools-test/does-not-exist.txt", 1, 10).await;
        assert!(matches!(missing, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))), "expected a file error, got {missing:?}");

        // --- Oversized content is refused, not silently truncated ---
        let oversized_content = "x".repeat(300 * 1024); // over the 256 KiB cap
        let oversized = write_file(&pool, conversation_a.id, "/tmp/file-tools-test/big.txt", &oversized_content, None).await;
        assert!(matches!(oversized, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))), "expected a size-limit error, got {oversized:?}");

        // --- list_directory: non-recursive, sorted, correct type/size,
        // and (via a nested path) proves write_file creates parent
        // directories. ---
        write_file(&pool, conversation_a.id, "/tmp/file-tools-test/subdir/nested.txt", "nested", None)
            .await
            .expect("write_file should create parent directories");
        let listing = list_directory(&pool, conversation_a.id, "/tmp/file-tools-test").await.expect("list_directory should succeed");
        let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["example.txt", "subdir"], "entries should be sorted alphabetically, size-limit failure excluded");
        let example_entry = listing.iter().find(|e| e.name == "example.txt").expect("example.txt should be listed");
        assert!(!example_entry.is_dir);
        assert!(example_entry.size.unwrap_or(0) > 0);
        let subdir_entry = listing.iter().find(|e| e.name == "subdir").expect("subdir should be listed");
        assert!(subdir_entry.is_dir);

        // --- glob/grep: pattern-based file discovery and content
        // search — see SME-19. A fresh
        // subdirectory, kept separate from example.txt's own
        // (by-now-heavily-edited) content above so this section's
        // expectations don't depend on tracking that history. ---
        write_file(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep/one.rs", "fn one() {}\n", None)
            .await
            .expect("write_file should succeed");
        write_file(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep/nested/two.rs", "fn two() {}\n", None)
            .await
            .expect("write_file should create parent directories");
        write_file(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep/notes.txt", "just plain prose, nothing to match\n", None)
            .await
            .expect("write_file should succeed");

        let recursive_glob = glob(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep", "**/*.rs", 1, 50)
            .await
            .expect("glob should succeed");
        let mut recursive_paths = recursive_glob.paths.clone();
        recursive_paths.sort();
        assert_eq!(
            recursive_paths,
            vec![
                "/tmp/file-tools-test/glob-grep/nested/two.rs".to_string(),
                "/tmp/file-tools-test/glob-grep/one.rs".to_string(),
            ],
            "**/*.rs should match .rs files at every depth"
        );
        assert_eq!(recursive_glob.total, 2);
        assert!(!recursive_glob.scan_capped);

        let shallow_glob = glob(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep", "*.rs", 1, 50)
            .await
            .expect("glob should succeed");
        assert_eq!(
            shallow_glob.paths,
            vec!["/tmp/file-tools-test/glob-grep/one.rs".to_string()],
            "a bare * shouldn't cross into nested/ the way ** does"
        );

        let grep_result = grep(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep", "fn ", None, false, 1, 20)
            .await
            .expect("grep should succeed");
        let mut grep_paths: Vec<&str> = grep_result.matches.iter().map(|m| m.path.as_str()).collect();
        grep_paths.sort();
        assert_eq!(
            grep_paths,
            vec![
                "/tmp/file-tools-test/glob-grep/nested/two.rs",
                "/tmp/file-tools-test/glob-grep/one.rs",
            ],
            "grep should find matches across every file under path, not just the top level"
        );
        assert_eq!(grep_result.total, 2);
        assert!(!grep_result.scan_capped);
        assert!(grep_result.skipped.is_empty());

        let filtered_grep = grep(
            &pool,
            conversation_a.id,
            "/tmp/file-tools-test/glob-grep",
            "fn ",
            Some("nested/*.rs".to_string()),
            false,
            1,
            20,
        )
        .await
        .expect("grep should succeed");
        assert_eq!(
            filtered_grep.matches.len(), 1,
            "grep's glob filter should narrow the search to just the matching file"
        );
        assert_eq!(filtered_grep.matches[0].path, "/tmp/file-tools-test/glob-grep/nested/two.rs");

        // --- Pod vanishes out from under us (deleted, evicted, node
        // lost) while smelt still thinks it's live — connect_with_retry
        // should run the same crash cleanup a failed `connect()` already
        // did, not just error out and leave the terminal wedged forever
        // — and now also fully terminate the pod itself (not just its
        // terminals), so a confirmed crash (OOM-killed, or any other
        // early exit) always leaves `sandbox_pods` correctly marked
        // terminated, the same as an exhausted-retries "gave up
        // without ever confirming" crash already did. A separate
        // conversation, since conversation_a's one pod slot is already
        // in use. ---
        let pod_c = create_pod(&pool, conversation_c.id, PodLimitOverrides::default()).await.expect("create_pod (c) should succeed");
        let terminal_c1 = create_terminal(&pool, conversation_c.id).await.expect("create_terminal (c1) should succeed");

        let pods = pods_api(&get().expect("set above").client);
        pods.delete(&pod_name(pod_c), &immediate_delete_params()).await.expect("delete pod_c directly");
        deregister(pod_c);

        let err = terminate_terminal(&pool, terminal_c1).await;
        assert!(matches!(err, Err(TerminalError::AgentUnreachable)), "expected AgentUnreachable, got {err:?}");

        let live_c = db::list_sandbox_terminals_for_pod(&pool, pod_c).await.expect("list_sandbox_terminals_for_pod");
        assert!(
            live_c.is_empty(),
            "crash cleanup should have marked terminal_c1 terminated even though the pod was never found, not just on a failed connect"
        );

        // The confirmed crash above already fully terminated pod_c
        // itself (not just its terminal) — terminate_pod now correctly
        // finds no live pod left for conversation_c to resolve.
        let already_gone = terminate_pod(&pool, conversation_c.id).await;
        assert!(
            matches!(already_gone, Err(TerminalError::NoPod)),
            "terminate_pod (c) should find no live pod left — a confirmed crash already terminated it, got {already_gone:?}"
        );

        // --- try_reconnect: a still-healthy pod that just lost its
        // in-memory registry entry (e.g. a smelt restart, simulated
        // here with `forget_connection` — the k8s pod itself is left
        // alone) should flip back to "connected" once try_reconnect
        // runs, not need an unrelated tool call to happen first.
        // Another separate conversation, same reason as pod_c. ---
        let pod_d = create_pod(&pool, conversation_d.id, PodLimitOverrides::default()).await.expect("create_pod (d) should succeed");
        let terminal_d1 = create_terminal(&pool, conversation_d.id).await.expect("create_terminal (d1) should succeed");
        forget_connection(pod_d);

        let disconnected = list_terminals(&pool, conversation_d.id).await.expect("list_terminals");
        assert_eq!(
            disconnected.iter().find(|t| t.terminal_id == terminal_d1).map(|t| t.status.as_str()),
            Some("disconnected"),
            "deregistering should make list_terminals report this terminal as disconnected"
        );

        try_reconnect(&pool, pod_d).await;

        let reconnected = list_terminals(&pool, conversation_d.id).await.expect("list_terminals");
        assert_eq!(
            reconnected.iter().find(|t| t.terminal_id == terminal_d1).map(|t| t.status.as_str()),
            Some("connected"),
            "try_reconnect should have re-established the connection to the still-healthy pod"
        );

        terminate_terminal(&pool, terminal_d1).await.expect("terminate_terminal (d1)");
        terminate_pod(&pool, conversation_d.id).await.expect("terminate_pod (d)");

        // --- Full teardown: terminate remaining terminal, then the pod.
        // terminate_pod is no longer idempotent on repeat now that it's
        // resolved via conversation_pod_id (live pods only) instead of
        // a caller-supplied pod_id — once terminated, there's no live
        // pod left for this conversation to resolve, so a second call
        // is genuinely NoPod, same as a conversation that never had one. ---
        terminate_terminal(&pool, terminal_a1).await.expect("terminate_terminal (a1)");
        terminate_pod(&pool, conversation_a.id).await.expect("terminate_pod (a) should succeed once its terminal is gone");
        let repeat = terminate_pod(&pool, conversation_a.id).await;
        assert!(
            matches!(repeat, Err(TerminalError::NoPod)),
            "terminate_pod should no longer be idempotent — a second call resolves to NoPod, got {repeat:?}"
        );

        // /workspace is the conversation's own: the next pod has the
        // checkout, uncommitted work included.
        let pod_a2 = create_pod(&pool, conversation_a.id, PodLimitOverrides::default())
            .await
            .expect("a second pod for conversation a");
        let kept = exec_with(
            &client,
            &pod_name(pod_a2),
            "sandbox",
            &["sh", "-c", "cat /workspace/origin/uncommitted.txt && git -C /workspace/origin status --porcelain"],
            None,
        )
        .await
        .expect("exec check workspace");
        assert_eq!(
            (kept.exit_code, kept.stdout.as_str()),
            (0, "not pushed yet\n M AGENTS.md\n?? uncommitted.txt\n"),
            "the checkout and its uncommitted file survive the pod: {}",
            kept.stderr
        );
        let repos = crate::git::list_repos(&pool, conversation_a.id).await.expect("list repos");
        assert_eq!(repos.len(), 4, "origin, retry, mywork, empty: {repos:?}");
        assert_eq!(repos[0].status, crate::git::RepoStatus::Ready, "{repos:?}");
        assert_eq!(repos[0].loaded_instructions, vec!["AGENTS.md".to_string()], "{repos:?}");
        assert_eq!(repos[1].status, crate::git::RepoStatus::Ready, "{repos:?}");
        // An origin that outlives the pod, for the model's clone below.
        let seeded = exec_with(
            &client,
            &pod_name(pod_a2),
            "sandbox",
            &["git", "clone", "-q", "--bare", "/workspace/origin", "/workspace/seed.git"],
            None,
        )
        .await
        .expect("exec seed origin");
        assert_eq!(seeded.exit_code, 0, "{}", seeded.stderr);
        terminate_pod(&pool, conversation_a.id).await.expect("terminate the second pod (a)");

        let pods_after = list_pods(&pool, conversation_a.id).await.expect("list_pods");
        assert!(pods_after.is_empty(), "no pods should be listed after terminating it, got {pods_after:?}");

        // "Work on a repo" on a conversation with no sandbox starts one.
        // The repo is recorded first, so a message sent while the
        // sandbox starts waits for the clone (SME-32 code review,
        // finding 3).
        let attaching = tokio::spawn({
            let pool = pool.clone();
            let id = conversation_a.id;
            async move { crate::git::attach_repo(&pool, id, "file:///tmp/missing.git", None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !crate::git::wait_for_clones(&pool, conversation_a.id, Duration::ZERO).await,
            "a turn started while the sandbox starts should see the clone coming"
        );
        let attached = attaching
            .await
            .expect("attach task")
            .expect_err("this origin doesn't exist");
        assert!(attached.contains("does not appear to be a git repository"), "{attached}");
        // The user named it, so it counts as trusted.
        assert_eq!(
            db::get_repo_trust(&pool, "file/tmp/missing").await.expect("trust"),
            Some(true)
        );
        assert_eq!(
            list_pods(&pool, conversation_a.id).await.expect("list_pods").len(),
            1,
            "attach_repo started a sandbox"
        );
        terminate_pod(&pool, conversation_a.id).await.expect("terminate the attach pod (a)");

        // The model's clone_repo on a conversation with no sandbox starts
        // one too, recording the repo first (SME-49).
        let cloning = tokio::spawn({
            let pool = pool.clone();
            let id = conversation_a.id;
            async move { crate::git::clone_repo(&pool, id, "file:///workspace/seed.git", None, None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !crate::git::wait_for_clones(&pool, conversation_a.id, Duration::ZERO).await,
            "a turn started while the sandbox starts should see the clone coming"
        );
        let seed = cloning
            .await
            .expect("clone task")
            .expect("clone_repo starts the sandbox and clones");
        assert_eq!(seed.path, "/workspace/seed");
        assert_eq!(seed.status, crate::git::RepoStatus::Ready, "{seed:?}");
        assert_eq!(
            list_pods(&pool, conversation_a.id).await.expect("list_pods").len(),
            1,
            "clone_repo started a sandbox"
        );
        terminate_pod(&pool, conversation_a.id).await.expect("terminate the clone pod (a)");
        let terminals_after = list_terminals(&pool, conversation_a.id).await.expect("list_terminals");
        assert!(terminals_after.is_empty(), "no terminals should be listed after terminating all of them");

        // A conversation that never had a pod at all: the same NoPod, not a panic.
        let unknown = terminate_pod(&pool, 999_999_999).await;
        assert!(
            matches!(unknown, Err(TerminalError::NoPod)),
            "terminate_pod on a conversation that never had a pod should error, got {unknown:?}"
        );

        // --- Proactive crash detection: deleting a pod out from under a
        // live connection is noticed and reported without any further
        // tool call — see SME-12. ---
        let conversation_e = db::create_conversation(&pool).await.expect("create conversation e");
        let pod_e = create_pod(&pool, conversation_e.id, PodLimitOverrides::default()).await.expect("create_pod (e) should succeed");
        let terminal_e = create_terminal(&pool, conversation_e.id).await.expect("create_terminal (e) should succeed");

        pods_api(&client).delete(&pod_name(pod_e), &immediate_delete_params()).await.expect("delete pod (e) directly");

        // Poll for the pod-level message itself, not `list_terminals`'
        // "disconnected" status — that status flips (via
        // `deregister_if_current`) *before* `handle_crash_cleanup`
        // finishes the rest of its work (marking the terminal
        // terminated, persisting this message), so it's an earlier,
        // racier signal than the thing this phase actually cares about.
        let detected = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if any_message_contains(&pool, conversation_e.id, "stopped unexpectedly").await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        assert!(
            detected.is_ok(),
            "an unattributed crash (pod simply gone, no reason to report) should get the plain pod-level notification without any further tool call"
        );
        // `list_terminals` only returns *live* terminals
        // (`terminated_at IS NULL`) — once crash-cleanup has actually
        // terminated it, it's simply absent, not present-with-a-
        // disconnected-status.
        let terminals_e = list_terminals(&pool, conversation_e.id).await.expect("list_terminals");
        assert!(
            terminals_e.iter().all(|t| t.terminal_id != terminal_e),
            "terminal (e) should be terminated (and so absent from list_terminals) by the same crash-cleanup pass"
        );

        // --- Deliberate termination is never misreported as a crash —
        // the Arc::ptr_eq identity check in `deregister_if_current`
        // is what makes this race-safe against the reader task noticing
        // the same connection drop for a different reason. ---
        let conversation_f = db::create_conversation(&pool).await.expect("create conversation f");
        create_pod(&pool, conversation_f.id, PodLimitOverrides::default()).await.expect("create_pod (f) should succeed");
        terminate_pod(&pool, conversation_f.id).await.expect("terminate_pod (f) should succeed");
        tokio::time::sleep(Duration::from_secs(2)).await; // give a wrongly-firing reader task a chance to misfire
        assert!(
            !any_message_contains(&pool, conversation_f.id, "stopped unexpectedly").await,
            "a deliberate terminate_pod must never produce a crash notification"
        );

        // --- The user stops a pod that has an open terminal and a
        // running command: everything is torn down, the command is
        // marked lost, and the model gets a user-stop notice, not a
        // crash notice. See SME-26. ---
        let conversation_g = db::create_conversation(&pool).await.expect("create conversation g");
        let mut app_events = events::subscribe_app();
        let pod_g = create_pod(&pool, conversation_g.id, PodLimitOverrides::default()).await.expect("create_pod (g) should succeed");
        assert!(
            received_pods_changed(&mut app_events).await,
            "creating a pod should tell app-wide listeners"
        );
        let terminal_g = create_terminal(&pool, conversation_g.id).await.expect("create_terminal (g) should succeed");
        db::create_terminal_command(&pool, conversation_g.id, terminal_g, "long-g", "sleep 300")
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal_g, "long-g", "sleep 300").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await;

        let overviews = crate::api::pods::pod_overviews(&pool).await.expect("pod_overviews");
        let overview_g = overviews
            .iter()
            .find(|o| o.pod_id == pod_g)
            .expect("the pods view should list pod (g)");
        assert_eq!(overview_g.conversation_id, conversation_g.id);
        assert_eq!(overview_g.status.as_deref(), Some("Running"));
        // The sandbox container's and the Docker sidecar's, added up (SME-33).
        assert_eq!(
            overview_g.memory_limit,
            crate::api::pods::sum_memory_limits(&[default_memory_limit(), default_docker_memory_limit()])
        );
        assert_eq!(overview_g.cpu_limit, None, "sandbox pods have no CPU limit (SME-77)");
        assert_eq!(overview_g.terminals, 1);
        assert_eq!(overview_g.activity, crate::api::pods::PodActivity::Busy, "a command is running");
        // Live usage, end to end: metrics-server samples a new pod
        // within a minute or so, and smelt's RBAC lets it read that.
        let usage_g = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let overviews = crate::api::pods::pod_overviews(&pool).await.expect("pod_overviews");
                if let Some(usage) = overviews.iter().find(|o| o.pod_id == pod_g).and_then(|o| o.usage.clone()) {
                    return usage;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        })
        .await
        .expect("the pods view should show pod (g)'s live usage once metrics-server has sampled it");
        assert!(usage_g.memory_bytes > 0, "a running pod uses some memory: {usage_g:?}");

        stop_pod_for_user(&pool, pod_g).await.expect("stop_pod_for_user (g) should succeed");
        assert!(
            received_pods_changed(&mut app_events).await,
            "stopping a pod should tell app-wide listeners"
        );

        let command_g = db::get_terminal_command(&pool, "long-g").await.expect("get").expect("the command");
        assert_eq!(command_g.status, "lost", "a running command should be marked lost");
        assert!(
            list_terminals(&pool, conversation_g.id).await.expect("list_terminals").is_empty(),
            "the pod's terminal should be closed"
        );
        assert!(
            db::list_sandbox_pods(&pool, conversation_g.id).await.expect("list pods").is_empty(),
            "the pod should be marked terminated"
        );
        assert!(
            pods_api(&client).get_opt(&pod_name(pod_g)).await.expect("get pod").is_none_or(|p| p.metadata.deletion_timestamp.is_some()),
            "the Kubernetes pod should be gone or going"
        );
        let notified = tokio::time::timeout(Duration::from_secs(10), async {
            while !any_message_contains(&pool, conversation_g.id, "The user stopped sandbox pod").await {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        assert!(notified.is_ok(), "the model should be told the user stopped the pod");
        assert!(
            !any_message_contains(&pool, conversation_g.id, "stopped unexpectedly").await,
            "a user stop must not be reported as a crash"
        );

        // --- Keeping records in step with the cluster (`watch_pods`):
        // a record whose pod vanished before the watch started is closed
        // when the watch's first listing completes; a pod deleted while
        // the watch runs is closed after the grace period; a young
        // record whose pod may still be starting is left alone. All
        // quietly. See SME-26. ---
        let updated_at = |id: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, chrono::NaiveDateTime>("SELECT updated_at FROM conversations WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("the conversation's updated_at")
            }
        };
        let wait_until_closed = |pod_id: i64| {
            let pool = pool.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(30), async {
                    while db::sandbox_pod_is_live(&pool, pod_id).await.expect("is live") {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                })
                .await
                .is_ok()
            }
        };
        let wait_until_gone_from_kubernetes = |pod_id: i64| {
            let client = client.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(60), async {
                    while pods_api(&client).get_opt(&pod_name(pod_id)).await.ok().flatten().is_some() {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                })
                .await
                .is_ok()
            }
        };

        // Before the watch starts: pod (h) is gone from Kubernetes, and
        // old enough that its record should have a pod.
        let conversation_h = db::create_conversation(&pool).await.expect("create conversation h");
        let pod_h = create_pod(&pool, conversation_h.id, PodLimitOverrides::default()).await.expect("create_pod (h) should succeed");
        let terminal_h = db::create_sandbox_terminal(&pool, pod_h).await.expect("a terminal record");
        db::create_terminal_command(&pool, conversation_h.id, terminal_h.id, "stale-h", "sleep 300")
            .await
            .expect("a running command record");
        sqlx::query("UPDATE sandbox_pods SET created_at = now() - interval '10 minutes' WHERE id = $1")
            .bind(pod_h)
            .execute(&pool)
            .await
            .expect("age the pod past the reconciliation cut-off");
        pods_api(&client).delete(&pod_name(pod_h), &immediate_delete_params()).await.expect("delete pod (h) directly");
        assert!(wait_until_gone_from_kubernetes(pod_h).await, "pod (h) should leave Kubernetes");
        let conversation_young = db::create_conversation(&pool).await.expect("create conversation");
        let young = db::create_sandbox_pod(&pool, conversation_young.id).await.expect("a young pod record");
        let updated_before = updated_at(conversation_h.id).await;

        let watch = tokio::spawn(watch_pods(pool.clone()));

        assert!(wait_until_closed(pod_h).await, "the watch's first listing should close pod (h)'s record");
        let command_h = db::get_terminal_command(&pool, "stale-h").await.expect("get").expect("the command");
        assert_eq!(command_h.status, "lost", "its running command should be marked lost");
        assert!(
            db::list_sandbox_terminals_for_pod(&pool, pod_h).await.expect("terminals").is_empty(),
            "its terminal should be closed"
        );
        assert!(
            db::list_messages(&pool, conversation_h.id).await.expect("messages").is_empty(),
            "records are closed quietly, with no notice"
        );
        assert_eq!(updated_at(conversation_h.id).await, updated_before, "the conversation shouldn't move in the sidebar");

        // While the watch runs: pod (i) is deleted outside smelt, with no
        // connection open, so crash detection never sees it.
        let conversation_i = db::create_conversation(&pool).await.expect("create conversation i");
        let pod_i = create_pod(&pool, conversation_i.id, PodLimitOverrides::default()).await.expect("create_pod (i) should succeed");
        pods_api(&client).delete(&pod_name(pod_i), &immediate_delete_params()).await.expect("delete pod (i) directly");
        assert!(wait_until_closed(pod_i).await, "a pod deleted while the watch runs should have its record closed");
        assert!(
            db::list_messages(&pool, conversation_i.id).await.expect("messages").is_empty(),
            "closed quietly, with no notice"
        );

        assert!(db::sandbox_pod_is_live(&pool, young.id).await.expect("is live"), "a young record is left alone");
        watch.abort();
        db::terminate_sandbox_pod(&pool, young.id).await.expect("clean up the young record");

        // --- Exhausting reconnect attempts without Kubernetes ever
        // confirming death still cleans up *and* force-terminates the
        // pod — verified by create_pod succeeding again immediately
        // after, not staying blocked behind a stale live-pod row. A
        // hand-built pod with no agent in it (not `create_pod`, which
        // now always has one running the instant the pod is `Running`
        // — the agent is the image's own `ENTRYPOINT`, see Phase 2 of
        // SME-17) is what
        // makes every connect() attempt genuinely and deterministically
        // fail while the pod itself stays Running throughout, exactly
        // the "stuck reporting Running but unreachable" case this path
        // exists for. ---
        let conversation_g = db::create_conversation(&pool).await.expect("create conversation g");
        let pod_g_row = db::create_sandbox_pod(&pool, conversation_g.id).await.expect("create_sandbox_pod (g)");
        let pod_g = pod_g_row.id;
        // Explicit and small, on purpose — see
        // `sandbox_image_import::loader_pod_spec`'s comment: an
        // unspecified request/limit here would implicitly default to
        // `smelt-park`'s `LimitRange` `max` (64Gi/16 cores), which no
        // CI runner can schedule.
        let mut no_agent_limits = std::collections::BTreeMap::new();
        no_agent_limits.insert("cpu".to_string(), Quantity("250m".to_string()));
        no_agent_limits.insert("memory".to_string(), Quantity("128Mi".to_string()));
        let no_agent_pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name(pod_g)),
                namespace: Some(NAMESPACE.to_string()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "sandbox".to_string(),
                    image: Some("busybox:1.36".to_string()),
                    command: Some(vec!["sleep".to_string(), "300".to_string()]),
                    resources: Some(ResourceRequirements { limits: Some(no_agent_limits), ..Default::default() }),
                    ..Default::default()
                }],
                restart_policy: Some("Never".to_string()),
                ..Default::default()
            }),
            status: None,
        };
        pods_api(&client).create(&PostParams::default(), &no_agent_pod).await.expect("create no-agent pod (g)");
        wait_for_running(&pods_api(&client), &pod_name(pod_g)).await.expect("no-agent pod (g) should reach Running");

        let gave_up = connect_with_retry(pool.clone(), pod_g, ConnectMode::Reconnect).await;
        assert!(
            matches!(gave_up, Err(TerminalError::AgentUnreachable)),
            "should give up as AgentUnreachable once reconnect attempts are exhausted, got {:?}",
            gave_up.is_ok()
        );
        // Deleted, though this agentless pod's `sleep` may take its grace
        // period to stop (SME-51 B6).
        let pod_g_now = pods_api(&client).get_opt(&pod_name(pod_g)).await.expect("get_opt");
        assert!(
            pod_g_now.is_none_or(|p| p.metadata.deletion_timestamp.is_some()),
            "exhausting reconnect attempts should delete the k8s pod"
        );
        let recreated = create_pod(&pool, conversation_g.id, PodLimitOverrides::default()).await;
        assert!(
            recreated.is_ok(),
            "create_pod should succeed again immediately, not stay blocked behind a stale live-pod row, got {recreated:?}"
        );

        // --- An idle terminal (nothing running in it) still gets
        // covered by the pod-level message, even though it has no
        // per-command message of its own. ---
        let conversation_h = db::create_conversation(&pool).await.expect("create conversation h");
        let pod_h = create_pod(&pool, conversation_h.id, PodLimitOverrides::default()).await.expect("create_pod (h) should succeed");
        create_terminal(&pool, conversation_h.id).await.expect("create_terminal (h) should succeed");
        pods_api(&client).delete(&pod_name(pod_h), &immediate_delete_params()).await.expect("delete pod (h) directly");
        let detected_h = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if any_message_contains(&pool, conversation_h.id, "stopped unexpectedly").await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        assert!(detected_h.is_ok(), "an idle terminal's pod crashing should still produce the pod-level notification");

        // --- Docker data PVC (SME-33): create_pod gives the pod the
        // conversation's own claim, so its images outlive the pod, and
        // teardown_conversation removes the claim with the pod. ---
        let conversation_j = db::create_conversation(&pool).await.expect("create conversation j");
        let pod_j = create_pod(&pool, conversation_j.id, PodLimitOverrides::default()).await.expect("create_pod (j) should succeed");
        let docker_pvcs = pvc_api(&client);
        let docker_claim = docker_pvc_name(conversation_j.id);
        assert!(
            docker_pvcs.get_opt(&docker_claim).await.expect("get_opt").is_some(),
            "create_pod should create the conversation's docker data claim {docker_claim}"
        );
        let spec_j = pods_api(&client).get(&pod_name(pod_j)).await.expect("get pod j").spec.expect("spec");
        let data_claim = spec_j
            .volumes
            .unwrap_or_default()
            .into_iter()
            .find(|v| v.name == DOCKER_DATA_VOLUME)
            .and_then(|v| v.persistent_volume_claim)
            .map(|c| c.claim_name);
        assert_eq!(data_claim.as_deref(), Some(docker_claim.as_str()), "pod j should mount its conversation's claim");
        // And /workspace is on a claim of its own (SME-32).
        let workspace_claim = workspace_pvc_name(conversation_j.id);
        assert!(
            docker_pvcs.get_opt(&workspace_claim).await.expect("get_opt").is_some(),
            "create_pod should create the conversation's workspace claim {workspace_claim}"
        );
        let spec_j = pods_api(&client).get(&pod_name(pod_j)).await.expect("get pod j").spec.expect("spec");
        let mounted_workspace = spec_j
            .volumes
            .unwrap_or_default()
            .into_iter()
            .find(|v| v.name == WORKSPACE_VOLUME)
            .and_then(|v| v.persistent_volume_claim)
            .map(|c| c.claim_name);
        assert_eq!(mounted_workspace.as_deref(), Some(workspace_claim.as_str()), "pod j's /workspace is its conversation's claim");

        teardown_conversation(conversation_j.id, &[]).await;
        // The claim's `pvc-protection` finalizer holds it until the pod
        // is really gone.
        let docker_claim_gone = tokio::time::timeout(Duration::from_secs(60), async {
            while docker_pvcs.get_opt(&docker_claim).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(docker_claim_gone.is_ok(), "teardown_conversation should delete the docker data claim");
        let workspace_claim_gone = tokio::time::timeout(Duration::from_secs(60), async {
            while docker_pvcs.get_opt(&workspace_claim).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(workspace_claim_gone.is_ok(), "teardown_conversation should delete the workspace claim");

        // --- SME-51 B7: a create_pod whose caller goes away (a Stop
        // drops the turn mid-call) still finishes: the pod gets its git
        // setup and is announced Running, rather than being left with
        // whatever step it had reached. ---
        let conversation_k = db::create_conversation(&pool).await.expect("create conversation k");
        let mut events_k = events::subscribe(conversation_k.id);
        let dropped = tokio::time::timeout(
            Duration::from_secs(2),
            create_pod(&pool, conversation_k.id, PodLimitOverrides::default()),
        )
        .await;
        assert!(dropped.is_err(), "the create should still be under way after 2s");
        let announced = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                if let Ok(events::ConversationEvent::SandboxPodUpdate { status, .. }) = events_k.recv().await
                    && status == "Running"
                {
                    return;
                }
            }
        })
        .await;
        teardown_conversation(conversation_k.id, &[]).await;
        assert!(announced.is_ok(), "a create_pod whose caller went away never finished");

        // --- Generic volumes: create_volume/delete_volume manage a
        // real PVC alongside the sandbox_volumes row, and a volume
        // mounted into a real pod is genuinely writable/readable —
        // see SME-17's
        // Phase 4. Folded in here rather than a separate test function
        // for the same MANAGER-singleton reason as everything else in
        // this test (see docs/testing.md). ---
        let volume_id = create_volume(&pool, "test-volume", "/data/testvol").await.expect("create_volume should succeed");
        let pvcs = pvc_api(&client);
        let pvc_name = sandbox_volume_pvc_name(volume_id);
        assert!(
            pvcs.get_opt(&pvc_name).await.expect("get_opt").is_some(),
            "create_volume should create the backing PVC"
        );

        let volumes = db::list_sandbox_volumes(&pool).await.expect("list_sandbox_volumes");
        let volume_session_id = unique_session_id(VOLUME_MOUNT_SESSION_LABEL);
        let volume_sandbox =
            get().expect("set above").create(&volume_session_id, "128Mi", &volumes).await.expect("create with a volume should succeed");
        let write =
            volume_sandbox.exec(&["sh", "-c", "echo hello > /data/testvol/marker.txt"]).await.expect("exec should succeed");
        assert_eq!(write.exit_code, 0, "writing into the mounted volume should succeed");
        let read = volume_sandbox.exec(&["cat", "/data/testvol/marker.txt"]).await.expect("exec should succeed");
        assert_eq!(read.stdout.trim(), "hello", "the mounted volume should be genuinely writable and readable");

        let volume_pods = pods_api(&client);
        volume_pods.delete(&volume_sandbox.pod_name, &immediate_delete_params()).await.ok();
        let volume_pod_name = volume_sandbox.pod_name.clone();
        std::mem::forget(volume_sandbox);
        // The PVC has Kubernetes' own `kubernetes.io/pvc-protection`
        // finalizer while a pod actively mounts it — the finalizer only
        // releases once the pod is genuinely gone, not just marked for
        // deletion, so this waits for that before deleting the volume.
        tokio::time::timeout(Duration::from_secs(30), async {
            while volume_pods.get_opt(&volume_pod_name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await
        .expect("the volume-mounting pod should actually disappear, not just be marked for deletion");

        delete_volume(&pool, volume_id).await.expect("delete_volume should succeed");
        let pvc_gone = tokio::time::timeout(Duration::from_secs(15), async {
            while pvcs.get_opt(&pvc_name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(pvc_gone.is_ok(), "delete_volume should delete the backing PVC");
    })
    .await;

    // Best-effort cleanup regardless of pass/fail, matching this file's
    // existing convention (real-cluster tests, no automatic isolation).
    let pods = pods_api(&get().expect("set above").client);
    for n in 1..=20i64 {
        pods.delete(&pod_name(n), &immediate_delete_params())
            .await
            .ok();
    }
    // Each conversation's docker data claim (SME-33); ids are small here.
    let pvcs = pvc_api(&get().expect("set above").client);
    for n in 1..=30i64 {
        pvcs.delete(&docker_pvc_name(n), &DeleteParams::default()).await.ok();
    }

    outcome.expect(
        "terminal lifecycle integration test should complete within the timeout, not hang",
    );
}

/// Creates and sends a command in one step, waits for it to finish,
/// returns its `command_id` — most of this test's steps are this exact
/// shape, this just cuts the repetition.
async fn run_and_wait(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> String {
    db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command)
        .await
        .expect("create_terminal_command");
    send_command(pool, terminal_id, command_id, command)
        .await
        .expect("send_command");
    poll_until_finished(pool, command_id).await;
    command_id.to_string()
}

async fn first_stdout_line(pool: &PgPool, command_id: &str) -> String {
    let lines = db::read_terminal_output(pool, command_id, &["stdout"], 0, 1)
        .await
        .expect("read_terminal_output");
    lines.first().map(|l| l.data.clone()).unwrap_or_default()
}

async fn poll_until_finished(pool: &PgPool, command_id: &str) -> db::TerminalCommandStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = db::terminal_command_status(pool, command_id)
            .await
            .expect("terminal_command_status")
            .expect("command should exist");
        if status.status != "running" {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "command {command_id} did not finish in time"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Whether a `PodsChanged` arrives on `rx` within a few seconds,
/// skipping any older ones still queued from earlier scenarios.
async fn received_pods_changed(
    rx: &mut tokio::sync::broadcast::Receiver<events::AppEvent>,
) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(events::AppEvent::PodsChanged) => return true,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn any_message_contains(pool: &PgPool, conversation_id: i64, needle: &str) -> bool {
    db::list_messages(pool, conversation_id)
        .await
        .expect("list_messages")
        .iter()
        .any(|m| {
            m.blocks().ok().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| matches!(b, crate::anthropic::ContentBlock::Text { text } if text.contains(needle)))
            })
        })
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
            if let Some(reason) = pod_death_reason(&pods, &sandbox.pod_name).await {
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
        pod_death_reason(&pods, &sandbox.pod_name).await,
        None,
        "a genuinely Running pod is inconclusive"
    );

    pods.delete(&sandbox.pod_name, &immediate_delete_params())
        .await
        .expect("delete should succeed");
    assert_eq!(
        pod_death_reason(&pods, &sandbox.pod_name).await,
        Some(None),
        "a pod that's gone entirely is confirmed dead with no reason to report"
    );
    std::mem::forget(sandbox);
}

// No standalone `force_terminate_pod` test: it's a private helper only
// reachable through `terminate_pod`, which `test_terminal_lifecycle_end_to_end`
// already exercises six times over. A second test independently racing
// to set the process-global `MANAGER` singleton is actively harmful,
// not just redundant — `kube::Client`'s internals are tied to whichever
// tokio runtime first constructed it, and `#[sqlx::test]`/`#[tokio::test]`
// each get their own runtime; whichever test's runtime tears down first
// kills the shared client for every other test still relying on it via
// `get()`. Confirmed live: adding this test back made
// `test_terminal_lifecycle_end_to_end` fail with `Kube(Service(Closed))`
// every time it ran after this one, even though neither test touches
// the other's data.

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
