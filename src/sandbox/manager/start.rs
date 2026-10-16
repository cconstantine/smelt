use super::*;

// Only called by the "no-agent pod" real-cluster test in `tests.rs` — `create`
// itself now goes straight to `create_with_running_timeout`.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::sandbox) async fn wait_for_running(pods: &Api<Pod>, name: &str) -> Result<(), SandboxError> {
    wait_for_running_with_timeout(pods, name, running_wait_timeout()).await
}

/// Waits up to `timeout` for pod `name` to reach `Running`, in a
/// `wait_for_running` span (SME-137) marked failed, with the wait's detail,
/// when it doesn't.
pub(in crate::sandbox) async fn wait_for_running_with_timeout(
    pods: &Api<Pod>,
    name: &str,
    timeout: Duration,
) -> Result<(), SandboxError> {
    let span = tracing::info_span!(
        "wait_for_running",
        pod = %name,
        timeout_s = timeout.as_secs() as i64,
        otel.status_code = tracing::field::Empty,
        otel.status_description = tracing::field::Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        // Boxed: the wait's future is large, and this span's would hold it
        // (see `test_sandbox_futures_stay_small`).
        let result = Box::pin(wait_for_running_unspanned(pods, name, timeout)).await;
        if let Err(e) = &result {
            crate::telemetry::mark_error(&span, &e.to_string());
        }
        result
    })
    .await
}

pub(in crate::sandbox) async fn wait_for_running_unspanned(
    pods: &Api<Pod>,
    name: &str,
    timeout: Duration,
) -> Result<(), SandboxError> {
    let mut last_detail = None;
    let started = std::time::Instant::now();
    let waited = tokio::time::timeout(timeout, async {
        loop {
            let pod = pods.get(name).await?;
            if let Some(reason) = pod_startup_failure(&pod) {
                return Err(SandboxError::StartFailed(reason));
            }
            last_detail = pod_pending_detail(&pod);
            if pod_phase_is_running(&pod) {
                let took = started.elapsed();
                if is_slow_start(took, timeout) {
                    tracing::warn!(
                        pod = %name,
                        took_secs = took.as_secs(),
                        timeout_secs = timeout.as_secs(),
                        timeline = %pod_start_timeline(&pod),
                        "a sandbox pod took more than half its start timeout to reach Running"
                    );
                }
                return Ok(());
            }
            // Not needed across the sleep. (Holding it adds nothing today:
            // its space is shared with the `get` it came from.)
            drop(pod);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    match waited {
        Ok(result) => result,
        // Boxed: a `Pod` and its events are large, and this path is rare
        // (see `test_sandbox_futures_stay_small`).
        Err(_) => Box::pin(timed_out(pods, name, timeout, last_detail)).await,
    }
}

/// Whether a start that took `took` came close enough to `timeout` to log.
/// A start past half its timeout is logged (SME-132), so a dev server's
/// log shows how close real starts come to the limit.
pub(in crate::sandbox) fn is_slow_start(took: Duration, timeout: Duration) -> bool {
    took > timeout / 2
}

/// How long `timed_out_detail` waits for each of its two reads.
const REPORT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A timed-out wait's result, from a fresh read of the pod and its events
/// (SME-132): `pod_start_report` as the timeout's detail, before
/// `create_with_running_timeout` deletes the pod. When the pod can't be
/// read, the last poll's `pod_pending_detail` stands. A pod that reached
/// `Running` since the last poll has started (review 2), and is logged as
/// a slow start.
async fn timed_out(pods: &Api<Pod>, name: &str, timeout: Duration, last_detail: Option<String>) -> Result<(), SandboxError> {
    let Ok(Ok(pod)) = tokio::time::timeout(REPORT_READ_TIMEOUT, pods.get(name)).await else {
        return Err(SandboxError::Timeout(last_detail));
    };
    if pod_phase_is_running(&pod) {
        tracing::warn!(
            pod = %name,
            timeout_secs = timeout.as_secs(),
            timeline = %pod_start_timeline(&pod),
            "a sandbox pod reached Running only as its start timed out"
        );
        return Ok(());
    }
    let events = pod_events(pods, &pod).await;
    timed_out_outcome(&pod, events.as_deref().map_err(String::as_str), Timestamp::now())
}

/// Whether `pod`'s phase is `Running`.
fn pod_phase_is_running(pod: &Pod) -> bool {
    pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running")
}

/// What a timed-out wait returns, given the fresh read of its pod: a
/// start if it's `Running` by now, otherwise a timeout whose detail is
/// `pod_start_report`.
pub(in crate::sandbox) fn timed_out_outcome(pod: &Pod, events: Result<&[Event], &str>, now: Timestamp) -> Result<(), SandboxError> {
    if pod_phase_is_running(pod) {
        return Ok(());
    }
    Err(SandboxError::Timeout(Some(pod_start_report(pod, events, now))))
}

/// `pod`'s events, or why they couldn't be listed. The `park` Role may
/// list events in both namespaces (`k8s/smelt-park-rbac.yaml`).
async fn pod_events(pods: &Api<Pod>, pod: &Pod) -> Result<Vec<Event>, String> {
    let client = pods.clone().into_client();
    let events: Api<Event> = match pods.namespace() {
        Some(namespace) => Api::namespaced(client, namespace),
        None => Api::all(client),
    };
    let mut selector = format!("involvedObject.name={}", pod.metadata.name.as_deref().unwrap_or_default());
    if let Some(uid) = &pod.metadata.uid {
        selector.push_str(&format!(",involvedObject.uid={uid}"));
    }
    match tokio::time::timeout(REPORT_READ_TIMEOUT, events.list(&ListParams::default().fields(&selector))).await {
        Ok(Ok(list)) => Ok(list.items),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no answer in {} s", REPORT_READ_TIMEOUT.as_secs())),
    }
}

/// The most events `pod_start_report` lists.
const REPORT_MAX_EVENTS: usize = 5;

/// The most characters of an event's message `pod_start_report` keeps.
const REPORT_MAX_MESSAGE_CHARS: usize = 200;

/// What a pod that hasn't started is stuck on, for a timed-out wait's
/// error (SME-132): the stage (its first `False` condition, as
/// `pod_pending_detail` gives it) and for how long, the pod's age, the
/// stages it passed, each container's state, and up to 5 of its events,
/// `Warning`s first, then the newest. `events` is `Err` with why when
/// they couldn't be listed; the rest is reported anyway.
pub(in crate::sandbox) fn pod_start_report(pod: &Pod, events: Result<&[Event], &str>, now: Timestamp) -> String {
    let created = pod.metadata.creation_timestamp.as_ref().map(|t| t.0);
    let conditions = pod.status.as_ref().and_then(|s| s.conditions.as_deref()).unwrap_or_default();
    let stage = match conditions.iter().find(|c| c.status == "False") {
        Some(stuck) => {
            let detail = pod_pending_detail(pod).unwrap_or_else(|| stuck.type_.clone());
            let gloss = stage_gloss(&stuck.type_).map(|g| format!(" ({g})")).unwrap_or_default();
            let since = stuck.last_transition_time.as_ref().map(|t| format!(" for {}", whole_seconds(t.0, now)));
            format!("{detail}{gloss}{}", since.unwrap_or_default())
        }
        None if conditions.is_empty() => "not scheduled yet".to_string(),
        // Review 2: where a pending pod with no `False` condition is.
        None if conditions.iter().all(|c| c.type_ == "PodScheduled") => {
            "scheduled, nothing from the kubelet yet".to_string()
        }
        None => format!(
            "every stage passed, phase {}",
            pod.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("unknown")
        ),
    };
    let mut parts = vec![stage];
    if let Some(created) = created {
        parts.push(format!("pod {} old", whole_seconds(created, now)));
    }
    let timeline = pod_start_timeline(pod);
    if !timeline.is_empty() {
        parts.push(format!("timeline: {timeline}"));
    }
    parts.push(format!("containers: {}", container_states(pod)));
    parts.push(match events {
        Ok([]) => "events: none".to_string(),
        Ok(events) => format!("events: {}", describe_events(events)),
        Err(e) => format!("events unavailable: {e}"),
    });
    parts.join("; ")
}

/// What the kubelet does while one of its pod conditions is `False`.
fn stage_gloss(condition: &str) -> Option<&'static str> {
    match condition {
        "PodReadyToStartContainers" => Some("volumes and sandbox"),
        "Initialized" => Some("Docker sidecar starting"),
        "ContainersReady" | "Ready" => Some("sandbox container starting"),
        _ => None,
    }
}

/// `from` to `to` in whole seconds, as "88 s". The API's timestamps are
/// to the second. Never negative: `to` may be smelt's clock and `from` the
/// node's, and a skew between them would otherwise show as "-3 s"
/// (SME-132 review 1).
fn whole_seconds(from: Timestamp, to: Timestamp) -> String {
    format!("{} s", to.duration_since(from).as_secs().max(0))
}

/// Each condition that went `True`, as an offset from the pod's creation,
/// in the order they happened: "scheduled +0 s, sandbox ready +1 s".
fn pod_start_timeline(pod: &Pod) -> String {
    let Some(created) = pod.metadata.creation_timestamp.as_ref().map(|t| t.0) else {
        return String::new();
    };
    let conditions = pod.status.as_ref().and_then(|s| s.conditions.as_deref()).unwrap_or_default();
    let mut passed: Vec<_> = conditions
        .iter()
        .filter(|c| c.status == "True")
        .filter_map(|c| Some((c.last_transition_time.as_ref()?.0, c.type_.as_str())))
        .collect();
    // Times are whole seconds, so stages tie often; a tie goes in the
    // order the kubelet passes them, not the API's list order (SME-132
    // review 1). Conditions it doesn't know go after, by name.
    passed.sort_by_key(|(at, condition)| (*at, stage_rank(condition), *condition));
    passed
        .iter()
        .map(|(at, condition)| {
            let name = match *condition {
                "PodScheduled" => "scheduled",
                "PodReadyToStartContainers" => "sandbox ready",
                "Initialized" => "sidecar started",
                "ContainersReady" => "containers ready",
                "Ready" => "ready",
                other => other,
            };
            format!("{name} +{}", whole_seconds(created, *at))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where a pod condition comes in a start, for ordering stages that
/// happened in the same second.
fn stage_rank(condition: &str) -> u8 {
    match condition {
        "PodScheduled" => 0,
        "PodReadyToStartContainers" => 1,
        "Initialized" => 2,
        "ContainersReady" => 3,
        "Ready" => 4,
        _ => 5,
    }
}

/// Each of the pod spec's containers, init containers first, with its
/// state: "docker running, not started, sandbox not created".
fn container_states(pod: &Pod) -> String {
    let spec = pod.spec.as_ref();
    let status = pod.status.as_ref();
    let init = spec.and_then(|s| s.init_containers.as_deref()).unwrap_or_default();
    let main = spec.map(|s| s.containers.as_slice()).unwrap_or_default();
    let statuses = [
        status.and_then(|s| s.init_container_statuses.as_deref()).unwrap_or_default(),
        status.and_then(|s| s.container_statuses.as_deref()).unwrap_or_default(),
    ];
    init.iter()
        .chain(main)
        .map(|container| {
            let found = statuses.iter().flat_map(|s| s.iter()).find(|s| s.name == container.name);
            format!("{} {}", container.name, found.map_or_else(|| "not created".to_string(), container_state))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One container's state, as `container_states` writes it.
fn container_state(status: &k8s_openapi::api::core::v1::ContainerStatus) -> String {
    let state = status.state.as_ref();
    if let Some(waiting) = state.and_then(|s| s.waiting.as_ref()) {
        return format!("waiting ({})", waiting.reason.as_deref().unwrap_or("no reason"));
    }
    if state.and_then(|s| s.running.as_ref()).is_some() {
        return match status.started {
            Some(false) => "running, not started".to_string(),
            _ => "running".to_string(),
        };
    }
    if let Some(done) = state.and_then(|s| s.terminated.as_ref()) {
        return format!("terminated ({}, exit {})", done.reason.as_deref().unwrap_or("no reason"), done.exit_code);
    }
    "not created".to_string()
}

/// Whether `c` would break the report's one line or reorder what's shown:
/// a control character, the line and paragraph separators (U+2028,
/// U+2029), or a bidi control (U+200E/F, U+202A-E, U+2066-9), which
/// `char::is_control` misses (SME-132 review 2).
fn breaks_a_line(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Up to `REPORT_MAX_EVENTS` events, `Warning`s first and then the newest,
/// as "FailedMount (×4): …", each message cut to
/// `REPORT_MAX_MESSAGE_CHARS`.
fn describe_events(events: &[Event]) -> String {
    // An event recorded through `events.k8s.io` (the scheduler's) has no
    // `count` or `lastTimestamp` here: its repeats are in `series`
    // (SME-132 review 1).
    let last_seen = |e: &Event| {
        e.series
            .as_ref()
            .and_then(|s| s.last_observed_time.as_ref())
            .map(|t| t.0)
            .or_else(|| e.last_timestamp.as_ref().map(|t| t.0))
            .or_else(|| e.event_time.as_ref().map(|t| t.0))
            .or_else(|| e.first_timestamp.as_ref().map(|t| t.0))
    };
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|e| (e.type_.as_deref() != Some("Warning"), std::cmp::Reverse(last_seen(e))));
    sorted
        .iter()
        .take(REPORT_MAX_EVENTS)
        .map(|e| {
            let message = e.message.as_deref().unwrap_or("");
            // One line: probe and CNI messages are often several (SME-132
            // review 1).
            let mut cut: String = message
                .chars()
                .map(|c| if breaks_a_line(c) { ' ' } else { c })
                .take(REPORT_MAX_MESSAGE_CHARS)
                .collect();
            if message.chars().nth(REPORT_MAX_MESSAGE_CHARS).is_some() {
                cut.push('…');
            }
            format!("{} (×{}): {cut}", e.reason.as_deref().unwrap_or("no reason"), e.series.as_ref().and_then(|s| s.count).or(e.count).unwrap_or(1))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}
