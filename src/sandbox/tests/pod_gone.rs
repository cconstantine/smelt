//! A deleted pod's wait to be gone, traced (SME-489): what the pod did
//! between the delete and the end of the wait, so a timed-out wait says
//! which container was still running, and since when.

use super::*;
use k8s_openapi::api::core::v1::Event;

/// One poll's view of the pod: whether its deletion is recorded, and each
/// container's state, as `pod_gone_state` writes it.
fn pod_gone_state(pod: &Pod) -> String {
    let status = pod.status.as_ref();
    let statuses = [
        status.and_then(|s| s.init_container_statuses.as_deref()).unwrap_or_default(),
        status.and_then(|s| s.container_statuses.as_deref()).unwrap_or_default(),
    ];
    let containers = statuses
        .iter()
        .flat_map(|s| s.iter())
        .map(|c| {
            let state = c.state.as_ref();
            let what = if state.and_then(|s| s.running.as_ref()).is_some() {
                "running".to_string()
            } else if let Some(done) = state.and_then(|s| s.terminated.as_ref()) {
                let at = done.finished_at.as_ref().map(|t| format!(" at {}", t.0)).unwrap_or_default();
                format!("terminated ({}, exit {}{at})", done.reason.as_deref().unwrap_or("no reason"), done.exit_code)
            } else if let Some(waiting) = state.and_then(|s| s.waiting.as_ref()) {
                format!("waiting ({})", waiting.reason.as_deref().unwrap_or("no reason"))
            } else {
                "no state".to_string()
            };
            format!("{} {what}", c.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let deletion = match (&pod.metadata.deletion_timestamp, pod.metadata.deletion_grace_period_seconds) {
        (Some(at), grace) => format!("deleting since {} (grace {})", at.0, grace.map_or("unset".to_string(), |g| format!("{g} s"))),
        (None, _) => "no deletionTimestamp".to_string(),
    };
    let phase = status.and_then(|s| s.phase.as_deref()).unwrap_or("no phase");
    format!("{phase}, {deletion}; {containers}")
}

/// The pod's events, oldest first, each as "reason: message".
fn describe_gone_events(events: &[Event]) -> String {
    let mut sorted: Vec<&Event> = events.iter().collect();
    let at = |e: &Event| {
        e.last_timestamp
            .as_ref()
            .map(|t| t.0)
            .or_else(|| e.event_time.as_ref().map(|t| t.0))
            .or_else(|| e.first_timestamp.as_ref().map(|t| t.0))
    };
    sorted.sort_by_key(|e| at(e));
    sorted
        .iter()
        .map(|e| {
            let when = at(e).map(|t| format!("{t} ")).unwrap_or_default();
            format!("{when}{}: {}", e.reason.as_deref().unwrap_or("no reason"), e.message.as_deref().unwrap_or(""))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Why `wait_for_pod_gone` gave up on `name` after `elapsed`: how long it
/// waited, which containers the last poll saw still running (or that
/// none were), every change the polls saw at its offset from the wait's
/// start (each container's state, exit code and finish time included),
/// and the pod's events.
fn pod_gone_report(name: &str, elapsed: Duration, seen: &[String], last_pod: Option<&Pod>, events: &str) -> String {
    let running = last_pod.and_then(|pod| pod.status.as_ref()).map(|status| {
        let names: Vec<&str> = [status.init_container_statuses.as_deref(), status.container_statuses.as_deref()]
            .into_iter()
            .flatten()
            .flatten()
            .filter(|c| c.state.as_ref().is_some_and(|s| s.running.is_some()))
            .map(|c| c.name.as_str())
            .collect();
        if names.is_empty() { "none".to_string() } else { names.join(", ") }
    });
    let last = last_pod.map_or_else(|| "never read".to_string(), pod_gone_state);
    format!(
        "pod {name} still there after {:.1} s; still running: {}; last: {last}; seen: {}; events: {events}",
        elapsed.as_secs_f64(),
        running.as_deref().unwrap_or("unknown (no status read)"),
        if seen.is_empty() { "nothing".to_string() } else { seen.join(" → ") },
    )
}

/// Waits up to `window` for `name` to be gone, polling every 200 ms: `Ok`
/// once it is, `Err` with `pod_gone_report`'s account of the wait when
/// `window` runs out, written at that moment.
pub(super) async fn wait_for_pod_gone(pods: &Api<Pod>, name: &str, window: Duration) -> Result<(), String> {
    let start = std::time::Instant::now();
    let mut seen: Vec<String> = Vec::new();
    let mut last_state = String::new();
    let mut last_pod = None;
    while start.elapsed() < window {
        match pods.get_opt(name).await {
            Ok(None) => return Ok(()),
            Ok(Some(pod)) => {
                let state = pod_gone_state(&pod);
                if state != last_state {
                    seen.push(format!("+{:.1} s {state}", start.elapsed().as_secs_f64()));
                    last_state = state;
                }
                last_pod = Some(pod);
            }
            Err(e) => seen.push(format!("+{:.1} s read failed: {e}", start.elapsed().as_secs_f64())),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let events = match &last_pod {
        Some(pod) => match pod_events(pods, pod).await {
            Ok(events) => describe_gone_events(&events),
            Err(e) => format!("unavailable: {e}"),
        },
        None => "none read".to_string(),
    };
    Err(pod_gone_report(name, start.elapsed(), &seen, last_pod.as_ref(), &events))
}

/// The real wait on a pod that never goes: it gives up at its window and
/// reports the pod as the cluster has it, events included. Harmless if
/// killed: a unique name, and an image that doesn't exist keeps it
/// Pending, so it costs the cluster nothing.
#[tokio::test]
async fn test_a_pod_gone_wait_reports_a_pod_that_stays() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let name = format!("sandbox-{}", unique_session_id("stays"));
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {INSTANCE_LABEL: TEST_INSTANCE}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    let waited = std::time::Instant::now();
    let gone = wait_for_pod_gone(&pods, &name, Duration::from_secs(2)).await;
    let took = waited.elapsed();
    pods.delete(&name, &immediate_delete_params()).await.ok();

    let report = gone.expect_err("the pod was never deleted, so the wait should time out");
    assert!(took < Duration::from_secs(5), "the wait should give up at its window, took {took:?}");
    for part in [format!("pod {name} still there after 2."), "Pending, no deletionTimestamp".to_string(), "events: ".to_string()] {
        assert!(report.contains(&part), "the report lacks {part:?}: {report}");
    }
    assert!(!report.contains("events: none read"), "the pod was read, so its events were asked for: {report}");
}

/// A pod deleted with a 20 s grace whose agent has exited and whose Docker
/// sidecar hasn't: the report names the sidecar as what's still running,
/// with the agent's exit code, and keeps every change and the events.
#[test]
fn test_a_timed_out_pod_gone_wait_names_what_still_runs() {
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "sandbox-x", "deletionTimestamp": "2026-11-13T06:23:47Z", "deletionGracePeriodSeconds": 20},
        "status": {
            "phase": "Running",
            "initContainerStatuses": [{"name": "docker", "image": "d", "imageID": "", "ready": true, "restartCount": 0,
                "state": {"running": {"startedAt": "2026-11-13T06:23:10Z"}}}],
            "containerStatuses": [{"name": "sandbox", "image": "s", "imageID": "", "ready": false, "restartCount": 0,
                "state": {"terminated": {"exitCode": 0, "reason": "Completed", "finishedAt": "2026-11-13T06:23:29Z"}}}],
        },
    }))
    .expect("pod");
    let seen = vec![
        "+0.1 s Running, no deletionTimestamp; docker running, sandbox running".to_string(),
        format!("+0.3 s {}", pod_gone_state(&pod)),
    ];
    let report = pod_gone_report("sandbox-x", Duration::from_millis(10_050), &seen, Some(&pod), "Killing: Stopping container sandbox");

    for part in [
        "pod sandbox-x still there after 10.1 s",
        "still running: docker",
        "+0.1 s Running, no deletionTimestamp",
        "deleting since 2026-11-13T06:23:47Z (grace 20 s)",
        "sandbox terminated (Completed, exit 0 at 2026-11-13T06:23:29Z)",
        "events: Killing: Stopping container sandbox",
    ] {
        assert!(report.contains(part), "the report lacks {part:?}: {report}");
    }
}

/// Every container exited, and the pod is still listed: the report says
/// nothing runs, which points at the kubelet's finalisation.
#[test]
fn test_a_timed_out_pod_gone_wait_says_when_nothing_still_runs() {
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "sandbox-y"},
        "status": {"phase": "Succeeded", "containerStatuses": [{"name": "sandbox", "image": "s", "imageID": "", "ready": false,
            "restartCount": 0, "state": {"terminated": {"exitCode": 137, "reason": "Error"}}}]},
    }))
    .expect("pod");
    let report = pod_gone_report("sandbox-y", Duration::from_secs(15), &[], Some(&pod), "none");
    assert!(report.contains("still running: none"), "{report}");
    assert!(report.contains("no deletionTimestamp"), "the last state is reported: {report}");
    assert!(report.contains("exit 137"), "{report}");
}
