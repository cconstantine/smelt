use super::*;

fn sandbox() -> SandboxRef {
    SandboxRef {
        conversation_id: 42,
        pod_id: 7,
        pod_name: "sandbox-7".to_string(),
        pod_uid: "uid-7".to_string(),
        node: "node-a".to_string(),
        instance: "instance-a".to_string(),
    }
}

fn config() -> LanguageServerConfig {
    LanguageServerConfig {
        name: "rust-analyzer".to_string(),
        image: "rust:1".to_string(),
        install_command: "rustup component add rust-analyzer".to_string(),
        command: "rust-analyzer".to_string(),
        env: [("RA_LOG".to_string(), "info".to_string())].into(),
        file_types: [("rs".to_string(), "rust".to_string())].into(),
        memory_limit: "3Gi".to_string(),
        cpu_limit: "2".to_string(),
        enabled: true,
        ..Default::default()
    }
}

#[test]
fn test_a_server_pod_sits_next_to_its_sandbox_and_goes_with_it() {
    let pod = server_pod_spec(&sandbox(), &config(), "2026-09-28T08:00:00");
    let meta = &pod.metadata;
    assert_eq!(meta.name.as_deref(), Some("lsp-7-rust-analyzer"));
    let labels = meta.labels.as_ref().expect("labels");
    assert_eq!(labels.get(LSP_OF_LABEL).map(String::as_str), Some("42"));
    assert_eq!(labels.get(LSP_SERVER_LABEL).map(String::as_str), Some("rust-analyzer"));
    assert_eq!(labels.get(LSP_POD_LABEL).map(String::as_str), Some("7"));
    assert!(!labels.contains_key("smelt/conversation"), "the sandbox pod's own label would make create_pod wait on it");
    assert_eq!(labels.get(crate::sandbox::INSTANCE_LABEL).map(String::as_str), Some("instance-a"), "SME-115");
    assert_eq!(
        meta.annotations.as_ref().and_then(|a| a.get(CONFIG_VERSION_ANNOTATION)).map(String::as_str),
        Some("2026-09-28T08:00:00")
    );
    let owner = &meta.owner_references.as_ref().expect("owner")[0];
    assert_eq!((owner.kind.as_str(), owner.name.as_str(), owner.uid.as_str()), ("Pod", "sandbox-7", "uid-7"));

    let spec = pod.spec.as_ref().expect("spec");
    assert_eq!(spec.node_name.as_deref(), Some("node-a"), "local-path claims can only be shared on one node");
    assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
    let security = spec.security_context.as_ref().expect("security context");
    assert_eq!((security.run_as_user, security.run_as_group), (Some(1000), Some(1000)));
    let volume = &spec.volumes.as_ref().expect("volumes")[0];
    assert_eq!(
        volume.persistent_volume_claim.as_ref().map(|c| c.claim_name.as_str()),
        Some("sandbox-workspace-42")
    );

    let container = &spec.containers[0];
    assert_eq!(container.image.as_deref(), Some("rust:1"));
    assert_eq!(container.volume_mounts.as_ref().expect("mounts")[0].mount_path, "/workspace");
    let env: Vec<(String, String)> = container
        .env
        .as_ref()
        .expect("env")
        .iter()
        .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
        .collect();
    assert!(env.contains(&("HOME".to_string(), POD_HOME.to_string())));
    assert!(env.contains(&("RA_LOG".to_string(), "info".to_string())));
    let resources = container.resources.as_ref().expect("resources");
    let limits = resources.limits.as_ref().expect("limits");
    assert_eq!(limits.get("memory").map(|q| q.0.as_str()), Some("3Gi"));
    assert_eq!(limits.get("cpu").map(|q| q.0.as_str()), Some("2"));
    // Small requests: with only the namespace's defaults the spike's
    // pods couldn't be scheduled.
    assert_eq!(resources.requests.as_ref().and_then(|r| r.get("memory")).map(|q| q.0.as_str()), Some("64Mi"));
    let script = container.command.as_ref().expect("command").join(" ");
    assert!(script.contains("rustup component add rust-analyzer"), "{script}");
    assert!(script.contains(INSTALL_RC) && script.contains("trap 'exit 0' TERM"), "{script}");
}

fn server_pod(uid: &str, phase: &str, reason: Option<&str>, deleting: bool) -> Pod {
    let mut pod = unready_server_pod(uid, phase, reason, deleting);
    pod.metadata.annotations = Some([(READY_ANNOTATION.to_string(), "true".to_string())].into());
    pod
}

/// A server pod whose install never finished.
fn unready_server_pod(uid: &str, phase: &str, reason: Option<&str>, deleting: bool) -> Pod {
    serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": "lsp-7-rust-analyzer",
            "uid": uid,
            "labels": {LSP_OF_LABEL: "42", LSP_SERVER_LABEL: "rust-analyzer", LSP_POD_LABEL: "7"},
            "deletionTimestamp": if deleting { Some("2026-09-28T00:00:00Z") } else { None },
        },
        "status": {
            "phase": phase,
            "containerStatuses": [{
                "name": "server", "image": "rust:1", "imageID": "", "ready": false, "restartCount": 0,
                "state": {"terminated": {"exitCode": 137, "reason": reason}},
            }],
        },
    }))
    .expect("a pod")
}

#[test]
fn test_a_server_pod_that_stops_is_reported_once() {
    let mut seen = std::collections::HashSet::new();
    assert_eq!(note_server_stop(&mut seen, &server_pod("a", "Running", None, false), false), None);
    let stopped = note_server_stop(&mut seen, &server_pod("a", "Failed", Some("OOMKilled"), false), false).expect("reported");
    assert_eq!(stopped.conversation_id, 42);
    assert!(stopped.notice.contains("rust-analyzer") && stopped.notice.contains("OOMKilled"), "{}", stopped.notice);
    assert!(stopped.notice.contains("memory limit"), "an OOM says what to change: {}", stopped.notice);
    assert!(stopped.notice.contains("start_language_server"), "{}", stopped.notice);
    assert_eq!(note_server_stop(&mut seen, &server_pod("a", "Failed", Some("OOMKilled"), false), false), None);
    // A new pod for the same server is reported again.
    assert!(note_server_stop(&mut seen, &server_pod("b", "Failed", Some("Error"), false), false).is_some());
}

#[test]
fn test_a_server_pod_stopped_by_smelt_or_before_is_not_reported() {
    let mut seen = std::collections::HashSet::new();
    assert_eq!(note_server_stop(&mut seen, &server_pod("a", "Failed", Some("Error"), true), false), None);
    assert_eq!(note_server_stop(&mut seen, &server_pod("b", "Failed", Some("Error"), false), true), None);
    assert_eq!(note_server_stop(&mut seen, &server_pod("b", "Failed", Some("Error"), false), false), None);
    // One that never got through its install: its start already said
    // what went wrong.
    assert_eq!(note_server_stop(&mut seen, &unready_server_pod("c", "Failed", Some("StartError"), false), false), None);
}

/// SME-171: what a start waiting on an install makes of its pod.
#[test]
fn test_a_running_install_is_not_interrupted() {
    assert_eq!(install_interrupted("lsp-7-ra", "a", Some(&unready_server_pod("a", "Running", None, false))), None);
    assert_eq!(install_interrupted("lsp-7-ra", "a", Some(&unready_server_pod("a", "Pending", None, false))), None);
}

#[test]
fn test_an_install_killed_for_memory_says_so_and_what_to_change() {
    let error = install_interrupted("lsp-7-ra", "a", Some(&unready_server_pod("a", "Failed", Some("OOMKilled"), false)))
        .expect("a stopped pod ends the wait");
    assert!(error.contains("lsp-7-ra") && error.contains("stopped during its install"), "{error}");
    assert!(error.contains("OOMKilled") && error.contains("memory limit"), "{error}");
}

#[test]
fn test_an_install_that_stopped_any_other_way_gives_its_reason() {
    let error = install_interrupted("lsp-7-ra", "a", Some(&unready_server_pod("a", "Failed", Some("Error"), false)))
        .expect("a stopped pod ends the wait");
    assert!(error.contains("(Error)"), "{error}");
    let error = install_interrupted("lsp-7-ra", "a", Some(&unready_server_pod("a", "Succeeded", None, false)))
        .expect("a pod whose container exited ends the wait");
    assert!(error.contains("stopped during its install") && !error.contains("()"), "{error}");
}

#[test]
fn test_an_install_whose_pod_went_or_is_going_says_it_was_deleted() {
    for (what, pod) in [
        ("gone", None),
        ("being deleted", Some(unready_server_pod("a", "Running", None, true))),
        ("replaced", Some(unready_server_pod("b", "Running", None, false))),
    ] {
        let error = install_interrupted("lsp-7-ra", "a", pod.as_ref()).unwrap_or_else(|| panic!("{what}: kept waiting"));
        assert!(error.contains("lsp-7-ra was deleted during its install"), "{what}: {error}");
    }
}

/// A pod stopped by its node (`Evicted`) says so in its own status, not
/// its container's: the container's reason, if it has one, is only how it
/// was stopped (SME-171).
fn evicted_server_pod(container_reason: Option<&str>) -> Pod {
    let containers = match container_reason {
        Some(reason) => serde_json::json!([{
            "name": "server", "image": "rust:1", "imageID": "", "ready": false, "restartCount": 0,
            "state": {"terminated": {"exitCode": 137, "reason": reason}},
        }]),
        None => serde_json::json!([]),
    };
    serde_json::from_value(serde_json::json!({
        "metadata": {"name": "lsp-7-rust-analyzer", "uid": "a"},
        "status": {
            "phase": "Failed",
            "reason": "Evicted",
            "message": "The node was low on resource: memory.",
            "containerStatuses": containers,
        },
    }))
    .expect("a pod")
}

#[test]
fn test_an_evicted_server_says_it_was_evicted() {
    assert_eq!(stop_reason(&evicted_server_pod(None)).as_deref(), Some("Evicted"));
    assert_eq!(stop_reason(&evicted_server_pod(Some("Error"))).as_deref(), Some("Evicted"));
    assert_eq!(stop_reason(&unready_server_pod("a", "Failed", Some("OOMKilled"), false)).as_deref(), Some("OOMKilled"));
    let error = install_interrupted("lsp-7-ra", "a", Some(&evicted_server_pod(None))).expect("an evicted pod ends the wait");
    assert!(error.contains("(Evicted)"), "{error}");
}

/// Review 1: a read error the next read recovered from isn't what a
/// timed-out install is blamed on.
#[test]
fn test_a_timed_out_install_names_only_a_read_error_that_is_still_the_latest() {
    let running = || Ok(Some(unready_server_pod("a", "Running", None, false)));
    let mut watch = InstallWatch { name: "lsp-7-ra", uid: "a", read_error: None };
    assert_eq!(watch.read(Err("the API server is busy".to_string())), None, "a failed read isn't a death");
    assert!(watch.timed_out().contains("(last error reading the pod: the API server is busy)"), "{}", watch.timed_out());
    assert_eq!(watch.read(running()), None);
    assert!(!watch.timed_out().contains("last error"), "{}", watch.timed_out());
    assert!(watch.timed_out().starts_with("The install didn't finish within 10 minutes"), "{}", watch.timed_out());
    assert!(watch.read(Ok(None)).is_some_and(|e| e.contains("deleted")));
}

/// A server pod with `status`, a status captured live.
fn captured_server_pod(status: &str) -> Pod {
    Pod {
        metadata: unready_server_pod("a", "Running", None, false).metadata,
        status: Some(serde_json::from_str(status).expect("the captured status")),
        ..Default::default()
    }
}

/// Review 1: a server pod this cluster's kubelet really evicted (its
/// ephemeral storage over its limit) *after* its install, as `/pods` and
/// `note_server_stop` see it, its status captured live
/// (`evicted_status.json`). Kubernetes says "Evicted" only in an event:
/// the pod script's TERM trap exits 0, so the pod reads Succeeded, its
/// container "Completed", with no pod-level reason. Nothing else stops
/// that script with 0 short of deleting the pod.
#[test]
fn test_a_server_pod_really_evicted_says_it_was_most_likely_evicted() {
    let pod = captured_server_pod(include_str!("tests/evicted_status.json"));
    assert_eq!(stop_reason(&pod).as_deref(), Some("evicted by its node, most likely"));
    let error = install_interrupted("lsp-7-ra", "a", Some(&pod)).expect("an evicted pod ends the wait");
    assert!(error.ends_with("stopped during its install (evicted by its node, most likely)"), "{error}");
}

/// Review 2: the same eviction landing *during* the install, as the
/// start waiting on it sees it, captured live
/// (`evicted_during_install_status.json`, the pod script of the time).
/// The trap isn't set yet, so the kubelet's TERM goes unanswered and its
/// KILL fails the pod: the container reads "Error" (137), and only the
/// pod's own reason says "Evicted".
#[test]
fn test_a_server_pod_evicted_during_its_install_says_it_was_evicted() {
    let pod = captured_server_pod(include_str!("tests/evicted_during_install_status.json"));
    let error = install_interrupted("lsp-7-ra", "a", Some(&pod)).expect("an evicted pod ends the wait");
    assert!(error.ends_with("stopped during its install (Evicted)"), "{error}");
}

#[test]
fn test_a_stop_explains_an_oom_and_names_any_other_reason() {
    assert!(stop_explanation(Some("OOMKilled")).contains("memory limit on the Language servers page"));
    assert_eq!(stop_explanation(Some("Error")), " (Error)");
    assert_eq!(stop_explanation(None), "");
}

#[test]
fn test_an_empty_install_still_records_success() {
    assert!(pod_script("  ").contains("( true )"));
}

/// Real-cluster tests: a stand-in sandbox pod (the real image, idle)
/// and its conversation's workspace claim, with `cat` as the "server".
pub(crate) mod cluster;
