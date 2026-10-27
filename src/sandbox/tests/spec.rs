use super::*;

#[test]
fn test_pod_spec_runs_dockerd_in_a_privileged_native_sidecar() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    let spec = pod.spec.expect("pod should have a spec");
    let main = Some(spec.containers);
    let sandbox = container(&main, "sandbox");
    assert_ne!(
        sandbox.security_context.as_ref().and_then(|s| s.privileged),
        Some(true)
    );
}

/// Every image the system runs is built linux/amd64 only
/// (`publish-images.yml` has no `platforms:`), so a sandbox pod must never
/// be scheduled onto a node of another architecture, where its pull would
/// fail on a platform mismatch. `kubernetes.io/arch: amd64` is that pin;
/// x86_64 is what Kubernetes calls amd64.
#[test]
fn test_pod_spec_pins_pods_to_amd64_nodes() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    let selector = pod
        .spec
        .as_ref()
        .and_then(|s| s.node_selector.as_ref())
        .expect("a pod should say the arch it may land on");
    assert_eq!(
        selector.get("kubernetes.io/arch").map(String::as_str),
        Some("amd64"),
        "every image is linux/amd64; a pod must never reach another arch"
    );
}

#[test]
fn test_pod_spec_shares_workspace_and_socket_but_keeps_docker_data_in_the_sidecar() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker, &[], &[], TEST_INSTANCE);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &volumes, &[], TEST_INSTANCE);
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
    let labelled = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    assert_eq!(
        labelled.metadata.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)).map(String::as_str),
        Some("42"),
        "create_pod finds a conversation's still-stopping pods by this label"
    );
    let ephemeral = DockerSidecar {
        storage: PodStorage::Ephemeral,
        ..docker_for_conversation(42)
    };
    let unlabelled = build_pod_spec("sandbox-1", "1Gi", &ephemeral, &[], &[], TEST_INSTANCE);
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
        let pod = build_pod_spec("sandbox-1", "1Gi", &docker, &[], &[], instance);
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

/// SME-77: a limit with no request is copied into the request, so an
/// idle pod reserved its whole 8Gi + 8Gi. Each container now asks for
/// no memory and has no CPU request or limit at all.
#[test]
fn test_pod_spec_reserves_nothing_and_limits_only_memory() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    assert_eq!(
        pod_container_limits(&pod),
        (vec!["1Gi".to_string(), "2Gi".to_string()], Vec::<String>::new())
    );
}

/// The setting is a deployment's, not a test's, so nothing here sets it: what
/// the spec asks for has to be whatever `default_image_pull_policy` resolves
/// to, for both containers, which is the whole point of one setting covering
/// the pair.
#[test]
fn test_both_containers_fetch_their_image_with_the_same_policy() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    let spec = pod.spec.expect("a pod spec");
    for container in spec.containers.iter().chain(spec.init_containers.iter().flatten()) {
        assert_eq!(
            container.image_pull_policy.as_deref(),
            Some(default_image_pull_policy()),
            "{}: the pod's two images are fetched the same way",
            container.name
        );
    }
}

#[test]
fn test_an_unset_or_unrecognised_pull_policy_never_pulls() {
    assert_eq!(image_pull_policy_from(None), "Never");
    assert_eq!(image_pull_policy_from(Some("")), "Never", "an empty setting is no setting");
    // A typo is the default, not a pod Kubernetes refuses to create;
    // default_image_pull_policy logs what it ignored.
    assert_eq!(image_pull_policy_from(Some("registry")), "Never");
}

#[test]
fn test_a_pull_policy_is_read_in_any_case_with_the_spaces_trimmed() {
    assert_eq!(image_pull_policy_from(Some("ifnotpresent")), "IfNotPresent", "Kubernetes spells it");
    assert_eq!(image_pull_policy_from(Some(" ALWAYS ")), "Always");
}

/// A sandbox pod carries pull credentials only if the deployment names some,
/// and names them for the pod as a whole rather than per container — the same
/// one-setting-covers-both-images shape as the pull policy.
#[test]
fn test_a_pod_carries_the_pull_credentials_the_deployment_names() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    let named = |secrets: Option<Vec<LocalObjectReference>>| {
        secrets.unwrap_or_default().into_iter().map(|secret| secret.name).collect::<Vec<_>>()
    };
    assert_eq!(
        named(pod.spec.expect("a pod spec").image_pull_secrets),
        named(default_image_pull_secrets()),
        "the pod asks for exactly the credentials the setting names"
    );
}

#[test]
fn test_a_pull_secret_setting_names_no_credentials_when_it_names_nothing() {
    assert!(image_pull_secrets_from(None).is_empty());
    assert!(image_pull_secrets_from(Some("")).is_empty(), "an empty setting is no setting");
    assert!(image_pull_secrets_from(Some(" , ")).is_empty(), "a blank entry is no entry");
}

#[test]
fn test_pull_secrets_are_named_one_per_comma_with_the_spaces_trimmed() {
    let named = |setting: &str| {
        image_pull_secrets_from(Some(setting))
            .into_iter()
            .map(|secret| secret.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(named("ghcr"), ["ghcr".to_string()]);
    assert_eq!(
        named("ghcr, smelt-park-harbor ,"),
        ["ghcr".to_string(), "smelt-park-harbor".to_string()],
        "several registries, and the trailing comma is no secret"
    );
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
