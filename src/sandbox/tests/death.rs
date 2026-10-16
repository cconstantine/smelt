use super::*;

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
