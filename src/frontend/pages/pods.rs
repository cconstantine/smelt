use chrono::NaiveDateTime;
use dioxus::prelude::*;

use crate::api::pods::{PodActivity, PodUsage, get_pods, stop_pod};
use crate::frontend::Route;

/// A counter that goes up every time a pod is created or goes away, in
/// any conversation (`AppEvent::PodsChanged`). Read it inside a
/// `use_resource` to refetch on each change. Web only: on the server it
/// never changes. Reconnects if the stream drops.
pub(crate) fn use_pods_changed() -> Signal<u64> {
    #[allow(unused_mut)]
    let mut changed = use_signal(|| 0u64);
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(async move {
            loop {
                if let Ok(mut events) = crate::api::pods::subscribe_app_events().await {
                    // Anything that changed while disconnected.
                    *changed.write() += 1;
                    while let Some(Ok(crate::events::AppEvent::PodsChanged)) = events.recv().await {
                        *changed.write() += 1;
                    }
                }
                gloo_timers::future::TimeoutFuture::new(1500).await;
            }
        });
    });
    changed
}

/// Every live sandbox pod across all conversations, with a Stop button per
/// row (click once to arm, again to confirm, like conversation delete).
/// Refetches when a pod comes or goes, and every 30 seconds to keep ages
/// and usage current.
#[component]
pub fn PodsIndex() -> Element {
    let pods_changed = use_pods_changed();
    #[allow(unused_mut)]
    let mut tick = use_signal(|| 0u64);
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(30_000).await;
                *tick.write() += 1;
            }
        });
    });
    let fetched = use_resource(move || {
        let _ = (pods_changed(), tick());
        get_pods()
    });
    let mut pending_stop: Signal<Option<i64>> = use_signal(|| None);
    let mut stop_error: Signal<Option<String>> = use_signal(|| None);

    let mut request_stop = move |pod_id: i64| {
        if pending_stop() == Some(pod_id) {
            pending_stop.set(None);
            spawn(async move {
                if let Err(e) = stop_pod(pod_id).await {
                    stop_error.set(Some(super::chat::server_error_message(&e)));
                }
            });
        } else {
            pending_stop.set(Some(pod_id));
        }
    };

    rsx! {
        div { class: "pods-page",
            div { class: "pods-header",
                Link { to: Route::Home {}, class: "pods-back-link", "\u{2190} Back to conversations" }
                h1 { "Sandbox pods" }
                p { class: "muted",
                    "Every sandbox pod that's running, in any conversation. Stopping one loses its terminals and any files outside mounted volumes; the model is told, and makes a new pod when it needs one."
                }
            }
            if let Some(err) = stop_error() {
                p { class: "error", "{err}" }
            }
            match fetched() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { p { class: "error", "{super::chat::server_error_message(&e)}" } },
                Some(Ok(pods)) if pods.is_empty() => rsx! { p { class: "muted", "No pods are running." } },
                Some(Ok(pods)) => rsx! {
                    table { class: "pods-table",
                        thead {
                            tr {
                                th { "Conversation" }
                                th { "Status" }
                                th { "Up" }
                                th { "Activity" }
                                th { "Memory" }
                                th { "CPU" }
                                th { "Terminals" }
                                th {}
                            }
                        }
                        tbody {
                            for pod in pods {
                                tr { key: "{pod.pod_id}", "data-pod-id": "{pod.pod_id}",
                                    td {
                                        Link { to: Route::ConversationRoute { id: pod.conversation_id }, "{pod.conversation_title}" }
                                        span { class: "muted pod-id", " pod {pod.pod_id}" }
                                    }
                                    td { "{pod.status.clone().unwrap_or_else(|| \"unknown\".to_string())}" }
                                    td { "{format_age(age_seconds(pod.started_at, pod.observed_at))}" }
                                    td { "{activity_text(&pod.activity, pod.observed_at)}" }
                                    td { "{memory_text(pod.usage.as_ref(), pod.memory_limit.as_deref())}" }
                                    td { "{cpu_text(pod.usage.as_ref(), pod.cpu_limit.as_deref())}" }
                                    td { "{pod.terminals}" }
                                    td {
                                        button {
                                            class: if pending_stop() == Some(pod.pod_id) { "pod-stop confirm" } else { "pod-stop" },
                                            r#type: "button",
                                            onclick: move |_| request_stop(pod.pod_id),
                                            if pending_stop() == Some(pod.pod_id) { "Confirm stop?" } else { "Stop" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    p { class: "muted pods-footnote",
                        "Usage comes from the cluster's metrics and lags by up to a minute. \"unavailable\" means smelt can't read metrics on this cluster, or there's no sample yet."
                    }
                },
            }
        }
    }
}

/// Seconds from `since` to `now`, never negative.
fn age_seconds(since: NaiveDateTime, now: NaiveDateTime) -> i64 {
    (now - since).num_seconds().max(0)
}

/// A short age: `45s`, `12m`, `3h 5m`, `2d 4h`.
fn format_age(seconds: i64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds % 86_400 / 3600, seconds % 3600 / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// `busy`, or `idle 12m`.
fn activity_text(activity: &PodActivity, now: NaiveDateTime) -> String {
    match activity {
        PodActivity::Busy => "busy".to_string(),
        PodActivity::IdleSince(since) => format!("idle {}", format_age(age_seconds(*since, now))),
    }
}

/// Bytes as `512 MiB` or `1.5 GiB`: one decimal below 10, whole numbers
/// above.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{} {}", value.round() as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `used of limit`, `used` or `unavailable of limit`.
fn against_limit(used: Option<String>, limit: Option<&str>) -> String {
    let used = used.unwrap_or_else(|| "unavailable".to_string());
    match limit {
        Some(limit) => format!("{used} of {limit}"),
        None => used,
    }
}

/// Memory use against its limit: `512 MiB of 8Gi`, `unavailable of 8Gi`.
fn memory_text(usage: Option<&PodUsage>, limit: Option<&str>) -> String {
    against_limit(usage.map(|u| format_bytes(u.memory_bytes)), limit)
}

/// CPU use against its limit, in cores: `0.25 of 1`, `unavailable of 1`.
fn cpu_text(usage: Option<&PodUsage>, limit: Option<&str>) -> String {
    let cores = |millicores: u64| {
        let text = format!("{:.2}", millicores as f64 / 1000.0);
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    against_limit(usage.map(|u| cores(u.cpu_millicores)), limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: u32, second: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
            .expect("a valid date")
            .and_hms_opt(10, minute, second)
            .expect("a valid time")
    }

    #[test]
    fn test_age_seconds_is_never_negative() {
        assert_eq!(age_seconds(at(0, 0), at(1, 30)), 90);
        assert_eq!(age_seconds(at(1, 30), at(0, 0)), 0, "a clock skew shows as zero");
    }

    #[test]
    fn test_format_age_uses_the_two_largest_units() {
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(12 * 60 + 5), "12m");
        assert_eq!(format_age(3 * 3600 + 5 * 60), "3h 5m");
        assert_eq!(format_age(2 * 86400 + 4 * 3600 + 59), "2d 4h");
    }

    #[test]
    fn test_activity_text_says_busy_or_how_long_idle() {
        assert_eq!(activity_text(&PodActivity::Busy, at(30, 0)), "busy");
        assert_eq!(activity_text(&PodActivity::IdleSince(at(18, 0)), at(30, 0)), "idle 12m");
    }

    #[test]
    fn test_format_bytes_picks_a_readable_unit() {
        assert_eq!(format_bytes(900), "900 B");
        assert_eq!(format_bytes(3372 * 1024), "3.3 MiB");
        assert_eq!(format_bytes(512 * 1024 * 1024), "512 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
    }

    #[test]
    fn test_usage_text_shows_use_against_the_limit() {
        let usage = PodUsage { memory_bytes: 512 * 1024 * 1024, cpu_millicores: 250 };
        assert_eq!(memory_text(Some(&usage), Some("8Gi")), "512 MiB of 8Gi");
        assert_eq!(cpu_text(Some(&usage), Some("1")), "0.25 of 1");
        assert_eq!(memory_text(None, Some("8Gi")), "unavailable of 8Gi");
        assert_eq!(cpu_text(None, None), "unavailable");
        assert_eq!(memory_text(Some(&usage), None), "512 MiB");
    }
}
