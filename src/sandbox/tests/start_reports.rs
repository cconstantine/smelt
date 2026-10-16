use super::*;

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

/// When `pod_start_report`'s fixtures were "created".
const REPORT_CREATED: &str = "2026-10-09T00:00:00Z";

/// `REPORT_CREATED` plus `seconds`, as the API writes a timestamp.
fn report_at(seconds: i64) -> String {
    let created: k8s_openapi::jiff::Timestamp = REPORT_CREATED.parse().expect("a timestamp");
    (created + k8s_openapi::jiff::SignedDuration::from_secs(seconds)).to_string()
}

/// "Now" for `pod_start_report`'s fixtures: 90 s after creation, when a
/// 90 s wait gives up.
fn report_now() -> k8s_openapi::jiff::Timestamp {
    report_at(90).parse().expect("a timestamp")
}

/// A smelt pod's shape (init container `docker`, container `sandbox`)
/// with the given conditions, as (type, status, seconds after creation,
/// reason, message), and container statuses.
fn report_pod(
    conditions: &[(&str, &str, i64, &str, &str)],
    init_statuses: serde_json::Value,
    statuses: serde_json::Value,
) -> Pod {
    let conditions: Vec<_> = conditions
        .iter()
        .map(|(type_, status, at, reason, message)| {
            let mut condition = serde_json::json!({"type": type_, "status": status, "lastTransitionTime": report_at(*at)});
            if !reason.is_empty() {
                condition["reason"] = serde_json::json!(reason);
            }
            if !message.is_empty() {
                condition["message"] = serde_json::json!(message);
            }
            condition
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "metadata": {"name": "sandbox-report", "creationTimestamp": REPORT_CREATED},
        "spec": {
            "initContainers": [{"name": "docker"}],
            "containers": [{"name": "sandbox"}],
        },
        "status": {
            "phase": "Pending",
            "conditions": conditions,
            "initContainerStatuses": init_statuses,
            "containerStatuses": statuses,
        },
    }))
    .expect("a pod fixture")
}

/// An event on the fixture pod: (type, reason, count, seconds after
/// creation it last happened, message).
fn report_event(type_: &str, reason: &str, count: i32, at: i64, message: &str) -> k8s_openapi::api::core::v1::Event {
    serde_json::from_value(serde_json::json!({
        "metadata": {"name": format!("sandbox-report.{reason}.{at}")},
        "involvedObject": {"kind": "Pod", "name": "sandbox-report"},
        "type": type_,
        "reason": reason,
        "count": count,
        "firstTimestamp": report_at(at),
        "lastTimestamp": report_at(at),
        "message": message,
    }))
    .expect("an event fixture")
}

/// The flake's stage (SME-132): a pod the kubelet can't mount volumes
/// for. The report says the stage, for how long, that neither container
/// exists, and the `FailedMount` warning ahead of newer `Normal` events.
#[test]
fn test_pod_start_report_for_a_pod_stuck_at_its_volumes() {
    let pod = report_pod(
        &[
            ("PodReadyToStartContainers", "False", 2, "", ""),
            ("Initialized", "False", 2, "ContainersNotInitialized", "containers with incomplete status: [docker]"),
            ("Ready", "False", 2, "ContainersNotReady", "containers with unready status: [sandbox]"),
            ("ContainersReady", "False", 2, "ContainersNotReady", "containers with unready status: [sandbox]"),
            ("PodScheduled", "True", 0, "", ""),
        ],
        serde_json::json!([]),
        serde_json::json!([]),
    );
    let events = [
        report_event("Normal", "Scheduled", 1, 0, "Successfully assigned smelt-park-test/sandbox-report to node"),
        report_event("Warning", "FailedMount", 4, 30, "MountVolume.SetUp failed for volume \"missing\" : configmap \"sme-132-missing\" not found"),
        report_event("Normal", "TaintManagerEviction", 1, 60, "Cancelling deletion of Pod"),
    ];
    let report = pod_start_report(&pod, Ok(&events), report_now());

    assert!(report.contains("PodReadyToStartContainers (volumes and sandbox) for 88 s"), "got {report}");
    assert!(report.contains("pod 90 s old"), "got {report}");
    assert!(report.contains("scheduled +0 s"), "got {report}");
    assert!(report.contains("docker not created"), "got {report}");
    assert!(report.contains("sandbox not created"), "got {report}");
    assert!(report.contains("FailedMount (×4): MountVolume.SetUp failed"), "got {report}");
    let failed_mount = report.find("FailedMount").expect("FailedMount listed");
    let newer_normal = report.find("TaintManagerEviction").expect("the newer Normal event listed");
    let older_normal = report.find("Scheduled (").expect("the older Normal event listed");
    assert!(failed_mount < newer_normal, "a Warning comes before a Normal event: {report}");
    assert!(newer_normal < older_normal, "newer Normal events come first: {report}");
}

/// A pod past its volumes, waiting on its Docker sidecar's startup probe:
/// the timeline shows how far it got, and each container's state shows.
#[test]
fn test_pod_start_report_for_a_pod_waiting_on_its_sidecar() {
    let pod = report_pod(
        &[
            ("PodReadyToStartContainers", "True", 1, "", ""),
            ("Initialized", "False", 1, "ContainersNotInitialized", "containers with incomplete status: [docker]"),
            ("Ready", "False", 1, "ContainersNotReady", "containers with unready status: [sandbox]"),
            ("ContainersReady", "False", 1, "ContainersNotReady", "containers with unready status: [sandbox]"),
            ("PodScheduled", "True", 0, "", ""),
        ],
        serde_json::json!([{"name": "docker", "image": "docker", "imageID": "", "ready": false, "restartCount": 0,
            "started": false, "state": {"running": {"startedAt": report_at(2)}}}]),
        serde_json::json!([{"name": "sandbox", "image": "sandbox", "imageID": "", "ready": false, "restartCount": 0,
            "state": {"waiting": {"reason": "PodInitializing"}}}]),
    );
    let events = [report_event("Warning", "Unhealthy", 40, 89, "Startup probe failed: Cannot connect to the Docker daemon")];
    let report = pod_start_report(&pod, Ok(&events), report_now());

    assert!(report.contains("Initialized: ContainersNotInitialized"), "got {report}");
    assert!(report.contains("(Docker sidecar starting) for 89 s"), "got {report}");
    assert!(report.contains("scheduled +0 s, sandbox ready +1 s"), "got {report}");
    assert!(report.contains("docker running, not started"), "got {report}");
    assert!(report.contains("sandbox waiting (PodInitializing)"), "got {report}");
    assert!(report.contains("Unhealthy (×40): Startup probe failed"), "got {report}");
}

/// A pod with no conditions yet says so, rather than nothing.
#[test]
fn test_pod_start_report_for_a_pod_not_scheduled_yet() {
    let pod = report_pod(&[], serde_json::json!([]), serde_json::json!([]));
    let report = pod_start_report(&pod, Ok(&[]), report_now());
    assert!(report.contains("not scheduled yet"), "got {report}");
    assert!(report.contains("pod 90 s old"), "got {report}");
    assert!(report.contains("events: none"), "got {report}");
}

/// SME-132 review 2: a pod that reached `Running` between the last poll
/// and the timeout's fresh read has started; the wait succeeds rather
/// than report a timeout and have the caller delete a running pod.
#[test]
fn test_a_pod_running_by_the_timeouts_read_is_a_start() {
    let mut running = report_pod(&[("Ready", "True", 30, "", "")], serde_json::json!([]), serde_json::json!([]));
    running.status.as_mut().expect("a status").phase = Some("Running".to_string());
    assert!(matches!(timed_out_outcome(&running, Ok(&[]), report_now()), Ok(())));

    let pending = report_pod(&[("PodReadyToStartContainers", "False", 2, "", "")], serde_json::json!([]), serde_json::json!([]));
    match timed_out_outcome(&pending, Ok(&[]), report_now()) {
        Err(SandboxError::Timeout(Some(detail))) => assert!(detail.contains("PodReadyToStartContainers"), "got {detail}"),
        other => panic!("expected a timeout with the report, got {other:?}"),
    }
}

/// SME-132 review 2: a pending pod with no `False` condition says where it
/// is rather than "no stage false": scheduled with nothing from the
/// kubelet yet, or past every stage and not yet `Running`.
#[test]
fn test_pod_start_report_names_a_pending_pod_with_no_false_stage() {
    let scheduled = report_pod(&[("PodScheduled", "True", 0, "", "")], serde_json::json!([]), serde_json::json!([]));
    let report = pod_start_report(&scheduled, Ok(&[]), report_now());
    assert!(report.starts_with("scheduled, nothing from the kubelet yet;"), "got {report}");

    let passed = report_pod(
        &[("PodScheduled", "True", 0, "", ""), ("PodReadyToStartContainers", "True", 1, "", ""), ("Ready", "True", 9, "", "")],
        serde_json::json!([]),
        serde_json::json!([]),
    );
    let report = pod_start_report(&passed, Ok(&[]), report_now());
    assert!(report.starts_with("every stage passed, phase Pending;"), "got {report}");
}

/// SME-132 review 2: `char::is_control` misses the line and paragraph
/// separators and the bidi controls, which also break the report's one
/// line or reorder what's shown; each becomes a space too.
#[test]
fn test_pod_start_report_replaces_separators_and_bidi_controls() {
    let pod = report_pod(&[("Initialized", "False", 0, "", "")], serde_json::json!([]), serde_json::json!([]));
    let message = "a\u{2028}b\u{2029}c\u{202e}d\u{2066}e\u{200f}f";
    let events = [report_event("Warning", "Unhealthy", 1, 10, message)];
    let report = pod_start_report(&pod, Ok(&events), report_now());
    assert!(report.contains("Unhealthy (×1): a b c d e f"), "got {report:?}");
}

/// At most 5 events, each message cut to 200 characters, on a char
/// boundary.
#[test]
fn test_pod_start_report_caps_its_events() {
    let pod = report_pod(&[("PodReadyToStartContainers", "False", 0, "", "")], serde_json::json!([]), serde_json::json!([]));
    let long = "é".repeat(300);
    let events: Vec<_> = (0..7).map(|i| report_event("Warning", &format!("Reason{i}"), 1, i, &long)).collect();
    let report = pod_start_report(&pod, Ok(&events), report_now());
    assert_eq!(report.matches(" (×").count(), 5, "got {report}");
    assert!(report.contains(&"é".repeat(200)), "got {report}");
    assert!(!report.contains(&"é".repeat(201)), "got {report}");
}

/// SME-132 review 1: "now" is smelt's clock and the pod's times are the
/// node's, so a skewed clock can put "now" before them. The report says
/// 0 s rather than a negative time.
#[test]
fn test_pod_start_report_never_shows_a_negative_time() {
    let pod = report_pod(
        &[("PodReadyToStartContainers", "False", 3, "", ""), ("PodScheduled", "True", 2, "", "")],
        serde_json::json!([]),
        serde_json::json!([]),
    );
    let before_creation = report_at(-5).parse().expect("a timestamp");
    let report = pod_start_report(&pod, Ok(&[]), before_creation);
    assert!(!report.contains('-'), "got {report}");
    assert!(report.contains("for 0 s; pod 0 s old"), "got {report}");
}

/// SME-132 review 1: the API's times are whole seconds, so stages often
/// tie, and the API lists conditions in another order than they happen.
/// Tied stages come out in the order the kubelet passes them.
#[test]
fn test_pod_start_timeline_orders_tied_stages_as_they_happen() {
    let pod = report_pod(
        &[
            ("PodReadyToStartContainers", "True", 1, "", ""),
            ("Initialized", "True", 65, "", ""),
            ("Ready", "True", 65, "", ""),
            ("ContainersReady", "True", 65, "", ""),
            ("PodScheduled", "True", 1, "", ""),
        ],
        serde_json::json!([]),
        serde_json::json!([]),
    );
    let report = pod_start_report(&pod, Ok(&[]), report_now());
    assert!(
        report.contains("timeline: scheduled +1 s, sandbox ready +1 s, sidecar started +65 s, containers ready +65 s, ready +65 s;"),
        "got {report}"
    );
}

/// SME-132 review 1: an event recorded through `events.k8s.io` (the
/// scheduler's) reads back through core/v1 with no `count` or
/// `lastTimestamp`, its repeats in `series`. The report counts and orders
/// it by the series.
#[test]
fn test_pod_start_report_reads_an_event_series() {
    let pod = report_pod(&[("PodScheduled", "False", 0, "Unschedulable", "")], serde_json::json!([]), serde_json::json!([]));
    let series: k8s_openapi::api::core::v1::Event = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "sandbox-report.series"},
        "involvedObject": {"kind": "Pod", "name": "sandbox-report"},
        "type": "Warning",
        "reason": "FailedScheduling",
        "eventTime": report_at(0).replace('Z', ".000000Z"),
        "series": {"count": 12, "lastObservedTime": report_at(80).replace('Z', ".000000Z")},
        "message": "0/1 nodes are available",
    }))
    .expect("an event fixture");
    let newer_once = report_event("Warning", "FailedMount", 1, 40, "once");
    let report = pod_start_report(&pod, Ok(&[newer_once, series]), report_now());
    assert!(report.contains("FailedScheduling (×12)"), "got {report}");
    let scheduling = report.find("FailedScheduling").expect("listed");
    let mount = report.find("FailedMount").expect("listed");
    assert!(scheduling < mount, "the series was last seen later, so it comes first: {report}");
}

/// SME-132 review 1: probe and CNI messages are often multi-line; the
/// report stays on one line, each control character a space.
#[test]
fn test_pod_start_report_keeps_a_multi_line_message_on_one_line() {
    let pod = report_pod(&[("Initialized", "False", 0, "", "")], serde_json::json!([]), serde_json::json!([]));
    let events = [report_event("Warning", "Unhealthy", 3, 10, "Startup probe failed: line one\nline two\r\tend")];
    let report = pod_start_report(&pod, Ok(&events), report_now());
    assert!(!report.chars().any(char::is_control), "got {report:?}");
    assert!(report.contains("Startup probe failed: line one line two  end"), "got {report:?}");
}

/// A start that succeeds after more than half its timeout is slow, and
/// logged; one at or under half isn't (SME-132).
#[test]
fn test_a_start_past_half_its_timeout_is_slow() {
    let timeout = Duration::from_secs(90);
    assert!(is_slow_start(Duration::from_secs(46), timeout));
    assert!(is_slow_start(Duration::from_millis(45_001), timeout));
    assert!(!is_slow_start(Duration::from_secs(45), timeout));
    assert!(!is_slow_start(Duration::from_secs(2), timeout));
}

/// When the events can't be listed, the rest of the report is kept.
#[test]
fn test_pod_start_report_keeps_the_rest_when_events_are_unavailable() {
    let pod = report_pod(&[("PodReadyToStartContainers", "False", 2, "", "")], serde_json::json!([]), serde_json::json!([]));
    let report = pod_start_report(&pod, Err("forbidden"), report_now());
    assert!(report.contains("events unavailable: forbidden"), "got {report}");
    assert!(report.contains("PodReadyToStartContainers (volumes and sandbox) for 88 s"), "got {report}");
}

#[test]
fn test_pod_startup_failure_is_none_while_a_pod_is_still_starting() {
    assert_eq!(pod_startup_failure(&pod_waiting("ContainerCreating", "")), None);
    assert_eq!(pod_startup_failure(&pod_with_phase("Pending")), None);
    assert_eq!(pod_startup_failure(&pod_with_phase("Running")), None);
}
