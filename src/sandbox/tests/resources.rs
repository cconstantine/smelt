//! SME-484: a persona's resources, their quantities, and the requests a
//! pod spec carries.

use super::*;

const GI: f64 = 1024.0 * 1024.0 * 1024.0;

#[test]
fn test_memory_quantities_parse_to_bytes_in_both_suffix_families() {
    assert_eq!(memory_bytes("0"), Some(0.0));
    assert_eq!(memory_bytes("512"), Some(512.0));
    assert_eq!(memory_bytes("256Mi"), Some(256.0 * 1024.0 * 1024.0));
    assert_eq!(memory_bytes("12Gi"), Some(12.0 * GI));
    assert_eq!(memory_bytes("1.5Gi"), Some(1.5 * GI));
    assert_eq!(memory_bytes("2G"), Some(2e9));
    assert_eq!(memory_bytes("64k"), Some(64e3));
}

#[test]
fn test_a_memory_quantity_that_isnt_one_is_refused() {
    for bad in ["", "Gi", "12GB", "12gi", "-1Gi", "1e9", "500m", "1..5Gi", ".Gi", " 1Gi", "1 Gi", "12Gi "] {
        assert_eq!(memory_bytes(bad), None, "{bad:?}");
    }
}

#[test]
fn test_cpu_quantities_parse_to_millicores() {
    assert_eq!(cpu_millis("500m"), Some(500.0));
    assert_eq!(cpu_millis("2"), Some(2000.0));
    assert_eq!(cpu_millis("0.25"), Some(250.0));
    for bad in ["", "m", "1.5m", "2Gi", "-1", "1 "] {
        assert_eq!(cpu_millis(bad), None, "{bad:?}");
    }
}

fn container(request: Option<&str>, limit: Option<&str>, cpu: Option<&str>) -> ContainerResources {
    ContainerResources {
        memory_request: request.map(str::to_string),
        memory_limit: limit.map(str::to_string),
        cpu_request: cpu.map(str::to_string),
    }
}

#[test]
fn test_a_request_within_its_limit_passes_the_check() {
    let resources = PodResources {
        sandbox: Some(container(Some("256Mi"), None, Some("500m"))),
        docker: Some(container(Some("12Gi"), Some("12Gi"), None)),
    };
    assert_eq!(resources.check("8Gi", "48Gi"), Ok(()));
    assert_eq!(PodResources::default().check("8Gi", "48Gi"), Ok(()));
}

#[test]
fn test_a_request_above_the_deployments_limit_is_refused_naming_both() {
    let resources = PodResources { sandbox: Some(container(Some("9Gi"), None, None)), docker: None };
    let error = resources.check("8Gi", "48Gi").expect_err("above the deployment's 8Gi");
    assert!(error.contains("smelt.resources.sandbox.memory_request") && error.contains("9Gi"), "{error}");
    assert!(error.contains("8Gi") && error.contains("SANDBOX_MEMORY_LIMIT"), "{error}");
}

#[test]
fn test_a_request_above_the_personas_own_limit_is_refused() {
    let resources = PodResources { sandbox: None, docker: Some(container(Some("16Gi"), Some("12Gi"), None)) };
    let error = resources.check("8Gi", "48Gi").expect_err("above its own 12Gi");
    assert!(error.contains("smelt.resources.docker.memory_request") && error.contains("12Gi"), "{error}");
    assert!(error.contains("smelt.resources.docker.memory_limit"), "{error}");
}

#[test]
fn test_a_bad_quantity_is_refused_naming_its_field() {
    for (resources, field) in [
        (PodResources { sandbox: Some(container(Some("lots"), None, None)), docker: None }, "sandbox.memory_request"),
        (PodResources { sandbox: None, docker: Some(container(None, Some("24GB"), None)) }, "docker.memory_limit"),
        (PodResources { sandbox: Some(container(None, None, Some("2Gi"))), docker: None }, "sandbox.cpu_request"),
    ] {
        let error = resources.check("8Gi", "48Gi").expect_err("a bad quantity");
        assert!(error.contains(&format!("smelt.resources.{field}")), "{field}: {error}");
    }
}

/// The deployment's default may itself be a quantity this parser doesn't
/// read (a deployment's own spelling); a request is then left to
/// Kubernetes rather than refused for it.
#[test]
fn test_an_unreadable_deployment_limit_refuses_nothing() {
    let resources = PodResources { sandbox: Some(container(Some("1Gi"), None, None)), docker: None };
    assert_eq!(resources.check("8e9", "48Gi"), Ok(()));
}

#[test]
fn test_a_container_that_asks_for_nothing_gets_todays_resources_byte_for_byte() {
    assert_eq!(container_resources("8Gi", &Requests::default()), memory_only("8Gi"));
}

#[test]
fn test_a_containers_requests_reach_its_resources_with_its_limit() {
    let quantity = |q: &str| Quantity(q.to_string());
    let asked = container_resources("24Gi", &Requests { memory: Some("12Gi".to_string()), cpu: Some("500m".to_string()) });
    assert_eq!(asked.limits.as_ref().and_then(|l| l.get("memory")), Some(&quantity("24Gi")));
    assert_eq!(asked.limits.as_ref().map(|l| l.len()), Some(1), "no CPU limit (SME-77)");
    let requests = asked.requests.expect("requests");
    assert_eq!(requests.get("memory"), Some(&quantity("12Gi")));
    assert_eq!(requests.get("cpu"), Some(&quantity("500m")));

    // A CPU request alone keeps today's explicit zero memory request.
    let cpu_only = container_resources("8Gi", &Requests { memory: None, cpu: Some("1".to_string()) });
    let requests = cpu_only.requests.expect("requests");
    assert_eq!((requests.get("memory"), requests.get("cpu")), (Some(&quantity("0")), Some(&quantity("1"))));
}

#[test]
fn test_pod_resources_round_trip_through_json_leaving_out_what_isnt_set() {
    let resources = PodResources { sandbox: None, docker: Some(container(Some("12Gi"), None, None)) };
    let json = serde_json::to_value(&resources).expect("to json");
    assert_eq!(json, serde_json::json!({"docker": {"memory_request": "12Gi"}}));
    assert_eq!(serde_json::from_value::<PodResources>(json).expect("from json"), resources);
}

fn resources_of<'a>(pod: &'a Pod, name: &str) -> &'a ResourceRequirements {
    let spec = pod.spec.as_ref().expect("spec");
    let all = spec.containers.iter().chain(spec.init_containers.iter().flatten());
    all.into_iter()
        .find(|c| c.name == name)
        .and_then(|c| c.resources.as_ref())
        .unwrap_or_else(|| panic!("no resources on `{name}`"))
}

/// No persona resources: both containers are exactly what they were
/// before SME-484, a request of `0` and the limit they were given.
#[test]
fn test_a_pod_with_no_requests_has_todays_container_resources() {
    let pod = build_sized_pod_spec("sandbox-1", "8Gi", &docker_for_conversation(42), &PodRequests::default(), &[], &[], TEST_INSTANCE);
    assert_eq!(resources_of(&pod, "sandbox"), &memory_only("8Gi"));
    assert_eq!(resources_of(&pod, "docker"), &memory_only("2Gi"));
}

#[test]
fn test_each_containers_requests_reach_its_own_container() {
    let requests = PodRequests {
        sandbox: Requests { memory: Some("256Mi".to_string()), cpu: None },
        docker: Requests { memory: Some("12Gi".to_string()), cpu: Some("2".to_string()) },
    };
    let pod = build_sized_pod_spec("sandbox-1", "8Gi", &docker_for_conversation(42), &requests, &[], &[], TEST_INSTANCE);
    assert_eq!(resources_of(&pod, "sandbox"), &container_resources("8Gi", &requests.sandbox));
    assert_eq!(resources_of(&pod, "docker"), &container_resources("2Gi", &requests.docker));
    let docker = resources_of(&pod, "docker").requests.as_ref().expect("requests");
    assert_eq!(docker.get("memory"), Some(&Quantity("12Gi".to_string())));
}

/// The spread rule: soft, over nodes, counting only this instance's
/// sandbox pods, and leaving tainted nodes (ryzen's) out — no toleration.
#[test]
fn test_every_sandbox_pod_spreads_softly_over_nodes_among_its_instances_sandbox_pods() {
    let pod = build_sized_pod_spec("sandbox-1", "8Gi", &docker_for_conversation(42), &PodRequests::default(), &[], &[], "instance-a");
    let spec = pod.spec.as_ref().expect("spec");
    let constraints = spec.topology_spread_constraints.as_deref().expect("a spread constraint");
    assert_eq!(constraints.len(), 1);
    let spread = &constraints[0];
    assert_eq!(spread.topology_key, "kubernetes.io/hostname");
    assert_eq!(spread.when_unsatisfiable, "ScheduleAnyway", "soft: it never blocks scheduling");
    assert_eq!(spread.max_skew, 1);
    assert_eq!(spread.node_taints_policy.as_deref(), Some("Honor"), "a tainted node isn't a place to spread to");
    assert_eq!(spec.tolerations, None, "no toleration reaches a tainted node");
    let selector = spread.label_selector.as_ref().expect("a selector");
    assert_eq!(
        selector.match_labels.as_ref().and_then(|l| l.get(INSTANCE_LABEL)).map(String::as_str),
        Some("instance-a"),
        "this instance's pods only"
    );
    let expressions = selector.match_expressions.as_deref().expect("expressions");
    assert_eq!(expressions.len(), 1);
    assert_eq!((expressions[0].key.as_str(), expressions[0].operator.as_str()), (CONVERSATION_LABEL, "Exists"),
        "sandbox pods only: language-server pods carry no conversation label");
    // The pod itself matches its own selector, as a sandbox pod must.
    let labels = pod.metadata.labels.as_ref().expect("labels");
    assert!(labels.contains_key(CONVERSATION_LABEL) && labels.get(INSTANCE_LABEL).map(String::as_str) == Some("instance-a"));
}

fn persona(sandbox: ContainerResources, docker: ContainerResources) -> PodResources {
    PodResources { sandbox: Some(sandbox), docker: Some(docker) }
}

#[test]
fn test_resolving_with_no_persona_resources_gives_the_deployments_limits_and_no_requests() {
    let (memory, docker, requests) = PodLimitOverrides::default().resolve(7, None).expect("resolves");
    assert_eq!((memory, docker.memory), (default_memory_limit(), default_docker_memory_limit()));
    assert_eq!(requests, PodRequests::default());
}

#[test]
fn test_a_personas_limits_and_requests_reach_the_pod_and_a_tools_limit_beats_the_personas() {
    let asks = persona(container(Some("256Mi"), Some("12Gi"), Some("500m")), container(Some("1Gi"), Some("6Gi"), None));
    let (memory, docker, requests) = PodLimitOverrides::default().resolve(7, Some(&asks)).expect("resolves");
    assert_eq!((memory.as_str(), docker.memory.as_str()), ("12Gi", "6Gi"));
    assert_eq!(requests.sandbox, Requests { memory: Some("256Mi".to_string()), cpu: Some("500m".to_string()) });
    assert_eq!(requests.docker, Requests { memory: Some("1Gi".to_string()), cpu: None });

    let overrides = PodLimitOverrides { memory: None, docker_memory: Some("4Gi".to_string()) };
    let (memory, docker, _) = overrides.resolve(7, Some(&asks)).expect("resolves");
    assert_eq!((memory.as_str(), docker.memory.as_str()), ("12Gi", "4Gi"));
}

#[test]
fn test_a_tools_limit_below_the_personas_request_is_refused_naming_both() {
    let asks = persona(container(None, None, None), container(Some("12Gi"), None, None));
    let overrides = PodLimitOverrides { memory: None, docker_memory: Some("4Gi".to_string()) };
    let error = overrides.resolve(7, Some(&asks)).expect_err("4Gi can't hold a 12Gi request").to_string();
    assert!(error.contains("12Gi") && error.contains("4Gi") && error.contains("docker"), "{error}");
}

/// A real scheduler's answer to a request no node has room for: a CPU
/// request of 100,000 cores. The wait's error leads with the scheduler's
/// reason, and the pod it made is deleted, as any that never starts is.
#[tokio::test]
async fn test_a_pod_no_node_has_room_for_times_out_with_the_schedulers_reason() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let session_id = unique_session_id("no-room");
    let requests = PodRequests {
        sandbox: Requests { memory: None, cpu: Some("100000".to_string()) },
        docker: Requests::default(),
    };
    let docker = DockerSidecar { memory: "512Mi".to_string(), storage: PodStorage::Ephemeral };
    let result = manager
        .create_with_running_timeout(&session_id, "128Mi", &docker, &requests, &[], &[], Duration::from_secs(8), &test_instance())
        .await;
    match result {
        Err(SandboxError::Timeout(Some(detail))) => {
            assert!(detail.starts_with("no node can take it now (the scheduler: "), "{detail}");
            assert!(detail.contains("Insufficient cpu"), "{detail}");
        }
        Err(e) => panic!("expected a timeout naming the scheduler, got {e}"),
        Ok(sandbox) => {
            std::mem::forget(sandbox);
            panic!("a 100,000-core pod started");
        }
    }
}
