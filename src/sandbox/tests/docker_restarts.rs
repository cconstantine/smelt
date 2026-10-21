use super::*;

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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
