use chrono::NaiveDateTime;
use dioxus::prelude::*;

use crate::api::pods::{
    AgentStatus, PodActivity, PodUsage, ServerPodOverview, StorageFate, StorageKind, StoredStorage, StoredSummary,
    get_pods, get_stored_storage, get_stored_summary, remove_stored_storage, stop_pod,
};
use crate::api::sandbox::{ClaimClass, GraceSettings, NO_PROVISIONER, StorageReport, get_storage_report};
use crate::frontend::Route;
use crate::frontend::pages::SettingsCrumbs;
use crate::frontend::pages::STORAGE_REMOVE_WARNING;

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
                    crate::frontend::check_build_id().await;
                    // Other app-wide events (a turn starting, say) aren't
                    // about pods; skip them rather than ending the loop.
                    while let Some(Ok(event)) = events.recv().await {
                        match event {
                            crate::events::AppEvent::PodsChanged => *changed.write() += 1,
                            // Added since this page loaded (SME-43).
                            crate::events::AppEvent::Unknown => *crate::frontend::STALE_BUNDLE.write() = true,
                            _ => {}
                        }
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

    let mut confirm_stop = move |pod_id: i64| {
        pending_stop.set(None);
        spawn(async move {
            stop_error.set(stop_pod(pod_id).await.err().map(|e| super::server_error_message(&e)));
        });
    };

    rsx! {
        div { class: "pods-page",
            div { class: "pods-header",
                SettingsCrumbs {}
                h1 { "Sandboxes" }
                p { class: "muted",
                    "Every sandbox that's running, in any conversation. Stopping one loses its terminals and any files outside /workspace and mounted volumes; the model is told, and starts a new one when it needs to."
                }
            }
            super::ErrorText { message: stop_error() }
            match fetched() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { super::ErrorText { message: Some(super::server_error_message(&e)) } },
                Some(Ok(pods)) if pods.is_empty() => rsx! { p { class: "muted", "No sandboxes are running." } },
                Some(Ok(pods)) => rsx! {
                    table { class: "pods-table",
                        thead {
                            tr {
                                th { "Conversation" }
                                th { "Status" }
                                th { "Node" }
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
                                    td { "{status_text(pod.status.as_deref(), pod.agent.as_ref())}" }
                                    td { class: "pod-node", "data-label": "Node", "{pod.node.clone().unwrap_or_default()}" }
                                    td { "{format_age(age_seconds(pod.started_at, pod.observed_at))}" }
                                    td { "{activity_text(&pod.activity, pod.observed_at)}" }
                                    td { "{memory_text(pod.usage.as_ref(), pod.memory_limit.as_deref())}" }
                                    td { "{cpu_text(pod.usage.as_ref(), pod.cpu_limit.as_deref())}" }
                                    td { "{pod.terminals}" }
                                    td {
                                        super::TwoStepButton {
                                            armed: pending_stop() == Some(pod.pod_id),
                                            class: "pod-stop",
                                            idle: "Stop",
                                            confirm: "Confirm stop?",
                                            on_arm: move |_| pending_stop.set(Some(pod.pod_id)),
                                            on_confirm: move |_| confirm_stop(pod.pod_id),
                                        }
                                    }
                                }
                                if !pod.language_servers.is_empty() {
                                    tr { key: "{pod.pod_id}-servers", class: "pod-servers",
                                        td { colspan: "9", span { class: "muted", "Language servers: " } "{servers_text(&pod.language_servers)}" }
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
            IdleStopNote {}
            StoredVolumes { pods_changed, tick }
        }
    }
}

/// The stored-volumes section under the pods table (SME-253): which
/// conversations hold sandbox disk — the two claims that outlive their
/// pods. Stopped conversations are the ones to look for: their pods are
/// gone and their `/workspace` and Docker data stay until removed here
/// or the conversation is deleted. Refetches on the same signals as the
/// pods table (a removal publishes `PodsChanged`, and the removal's own
/// `removing…` state rides the 30 s tick).
#[component]
fn StoredVolumes(pods_changed: Signal<u64>, tick: Signal<u64>) -> Element {
    let fetched = use_resource(move || {
        let _ = (pods_changed(), tick());
        get_stored_storage()
    });
    // Its own read, so `get_stored_storage` keeps its type for older tabs;
    // nothing shown until it loads, or if it fails.
    let summary = use_resource(move || {
        let _ = (pods_changed(), tick());
        get_stored_summary()
    });
    let summary = match summary() {
        Some(Ok(summary)) if summary.conversations > 0 || summary.orphaned > 0 => Some(summary_text(&summary)),
        _ => None,
    };
    // Where removed storage goes (SME-521): read on the same signals.
    let report = use_resource(move || {
        let _ = (pods_changed(), tick());
        get_storage_report()
    });
    let report_lines = match report() {
        Some(Ok(report)) => storage_report_lines(&report),
        _ => Vec::new(),
    };
    let mut pending_remove: Signal<Option<i64>> = use_signal(|| None);
    let mut remove_error: Signal<Option<String>> = use_signal(|| None);
    let mut confirm_remove = move |conversation_id: i64| {
        pending_remove.set(None);
        spawn(async move {
            remove_error
                .set(remove_stored_storage(conversation_id).await.err().map(|e| super::server_error_message(&e)));
        });
    };

    rsx! {
        section { class: "stored-volumes",
            h2 { "Stored volumes" }
            p { class: "muted",
                "Each conversation's sandbox keeps its /workspace and its Docker images in two volumes that outlive its pods; they hold node disk until removed here or when the conversation is deleted."
            }
            if let Some(summary) = summary {
                p { class: "muted storage-summary", "{summary}" }
            }
            for line in report_lines {
                p { class: "muted storage-report", "{line}" }
            }
            super::ErrorText { message: remove_error() }
            match fetched() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { super::ErrorText { message: Some(super::server_error_message(&e)) } },
                Some(Ok(rows)) if rows.is_empty() => rsx! { p { class: "muted", "No stored volumes." } },
                Some(Ok(rows)) => rsx! {
                    table { class: "pods-table storage-table",
                        thead {
                            tr {
                                th { "Conversation" }
                                th { "Stopped" }
                                th { "/workspace" }
                                th { "Docker" }
                                th {}
                            }
                        }
                        tbody {
                            for row in rows {
                                tr { key: "{row.conversation_id}", "data-storage-conversation": "{row.conversation_id}",
                                    td { class: "storage-conversation",
                                        Link { to: Route::ConversationRoute { id: row.conversation_id }, "{row.conversation_title}" }
                                    }
                                    td { class: "storage-stopped", "data-label": "Stopped", "{stopped_text(row.stopped.as_ref(), &row.fate, row.observed_at)}" }
                                    td { "data-label": "/workspace", "{volume_cell_text(&row, StorageKind::Workspace)}" }
                                    td { "data-label": "Docker", "{volume_cell_text(&row, StorageKind::Docker)}" }
                                    td {
                                        super::TwoStepButton {
                                            armed: pending_remove() == Some(row.conversation_id),
                                            class: "storage-remove",
                                            idle: "Remove volumes",
                                            confirm: "Confirm remove?",
                                            title: Some(STORAGE_REMOVE_WARNING),
                                            on_arm: move |_| pending_remove.set(Some(row.conversation_id)),
                                            on_confirm: move |_| confirm_remove(row.conversation_id),
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

/// When a row's sandbox stopped (SME-123 DR9) and what becomes of its
/// storage, as the server's rule says (SME-521): removed in so many days,
/// kept until removed here, or nothing while in use or not known. Claims
/// no pod ever used have no stop, only their fate.
fn stopped_text(stop: Option<&crate::api::pods::PodStop>, fate: &StorageFate, now: NaiveDateTime) -> String {
    let stopped = stop.map(|stop| format!("stopped {} ago", format_age(age_seconds(stop.at, now))));
    let fate = match fate {
        StorageFate::Kept => Some("kept until you remove it".to_string()),
        StorageFate::RemovesAt { at, .. } => Some(match ((*at - now).num_seconds() + 86_399) / 86_400 {
            ..=0 => "removed within the hour".to_string(),
            1 => "removed in 1 day".to_string(),
            n => format!("removed in {n} days"),
        }),
        StorageFate::Live | StorageFate::Unknown => None,
    };
    [stopped, fate].into_iter().flatten().collect::<Vec<_>>().join(" \u{b7} ")
}

/// `n` and its noun, singular or plural.
fn count_of(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The stored-storage section's summary line (SME-521).
fn summary_text(summary: &StoredSummary) -> String {
    let holds = if summary.conversations == 1 { "holds" } else { "hold" };
    format!(
        "{} {holds} {} ({} requested) \u{b7} {} past their grace, removed over the next passes \u{b7} {} orphaned",
        count_of(summary.conversations, "conversation", "conversations"),
        count_of(summary.claims, "claim", "claims"),
        format_bytes(summary.requested_bytes),
        summary.past_grace,
        summary.orphaned,
    )
}

/// The stored-volumes section's lines on where removed storage goes
/// (SME-521): the claims' class, and the volumes left Released. Nothing
/// for a read the server couldn't make.
fn storage_report_lines(report: &StorageReport) -> Vec<String> {
    let mut lines = Vec::new();
    match &report.class {
        ClaimClass::Unavailable => {}
        ClaimClass::NoDefault => lines.push(
            "Sandbox claims have no storage class: the cluster has no default and SANDBOX_STORAGE_CLASS isn't set."
                .to_string(),
        ),
        ClaimClass::Missing { name } => {
            lines.push(format!("Sandbox claims name storage class {name}, which the cluster doesn't have."))
        }
        ClaimClass::Found { name, provisioner, reclaim_policy, .. } => {
            // Review 1, B-L1: static volumes have no one to reclaim them.
            let static_volumes = provisioner == NO_PROVISIONER;
            let kind = if static_volumes { ", no provisioner" } else { "" };
            let frees =
                if reclaim_policy == "Delete" && !static_volumes { "" } else { "; removing storage frees no disk" };
            lines.push(format!("Sandbox claims use {name} (reclaim: {reclaim_policy}{kind}){frees}."));
        }
    }
    if let Some(released) = &report.released {
        lines.push(format!(
            "Released volumes not yet reclaimed: {} ({}).",
            released.count,
            format_bytes(released.capacity_bytes)
        ));
    }
    lines
}

/// A stored-volumes cell: the claim's size (`"20Gi"` — its capacity,
/// which under local-path is the request, never usage),
/// `"removing…"` while it carries a deletion timestamp, `"stored"` if
/// it has no size to show, and `"none"` if that column's claim isn't
/// there (one half of a halfway removal that failed).
fn volume_cell_text(row: &StoredStorage, kind: StorageKind) -> String {
    match row.volumes.iter().find(|volume| volume.kind == kind) {
        Some(volume) if volume.deleting => "removing\u{2026}".to_string(),
        Some(volume) => {
            let size = volume.size.clone().unwrap_or_else(|| "stored".to_string());
            // Which host's disk holds it (SME-304).
            match &volume.node {
                Some(node) => format!("{size} on {node}"),
                None => size,
            }
        }
        None => "none".to_string(),
    }
}

/// A sandbox's language servers: `rust-analyzer ready, 512 MiB of 2Gi`,
/// one per server, joined with ` · `.
fn servers_text(servers: &[ServerPodOverview]) -> String {
    servers
        .iter()
        .map(|server| format!("{} {}, {}", server.name, server.state, memory_text(server.usage.as_ref(), server.memory_limit.as_deref())))
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
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

/// Kubernetes' phase, then the agent's status once smelt knows it:
/// `Running · agent 1.0`, `Running · old agent, restart required`.
fn status_text(phase: Option<&str>, agent: Option<&AgentStatus>) -> String {
    let phase = phase.unwrap_or("unknown");
    match agent {
        Some(agent) => format!("{phase} \u{b7} {}", agent.describe()),
        None => phase.to_string(),
    }
}

/// The footnote's sentence on automatic stops (SME-78), for an idle
/// threshold of `minutes` (`0`: idle stops off).
fn idle_stop_note(minutes: u32) -> String {
    match minutes {
        0 => "Idle sandboxes aren't stopped automatically; a finished agent's is, unless a command is still running.".to_string(),
        minutes => format!(
            "Sandboxes idle for {minutes} minutes with no command running are stopped, and a finished agent's at once unless a command is still running; /workspace is kept."
        ),
    }
}

/// The footnote's sentence on automatic storage removal, naming the three
/// graces (SME-521); a default of `0` turns removal off, and any other
/// grace of `0` keeps that storage until it's removed by hand.
fn storage_grace_note(graces: &GraceSettings) -> String {
    if graces.default_days == 0 {
        return "Storage is never removed automatically.".to_string();
    }
    let after = |days: u32| match days {
        0 => "is kept until you remove it".to_string(),
        days => format!("is removed {} later", count_of(days, "day", "days")),
    };
    format!(
        "A stopped sandbox's storage {}; a finished agent's {}; one you stopped {}.",
        after(graces.default_days),
        after(graces.agent_days),
        after(graces.user_days),
    )
}

/// The footnote on automatic stops and what becomes of the storage they
/// keep, once the server says what's in force.
#[component]
fn IdleStopNote() -> Element {
    let minutes = use_resource(crate::api::sandbox::get_idle_stop_minutes);
    let graces = use_resource(crate::api::sandbox::get_storage_grace);
    match (minutes(), graces()) {
        (Some(Ok(minutes)), Some(Ok(graces))) => rsx! {
            p { class: "muted pods-footnote", "{idle_stop_note(minutes)} {storage_grace_note(&graces)}" }
        },
        _ => rsx! {},
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
    fn test_servers_text_gives_each_servers_state_and_memory() {
        let servers = vec![
            ServerPodOverview {
                name: "rust-analyzer".to_string(),
                state: "ready".to_string(),
                memory_limit: Some("2Gi".to_string()),
                usage: Some(PodUsage { memory_bytes: 512 * 1024 * 1024, cpu_millicores: 10 }),
            },
            ServerPodOverview { name: "pyright".to_string(), state: "stopped (OOMKilled)".to_string(), memory_limit: Some("1Gi".to_string()), usage: None },
        ];
        assert_eq!(servers_text(&servers), "rust-analyzer ready, 512 MiB of 2Gi \u{b7} pyright stopped (OOMKilled), unavailable of 1Gi");
    }

    #[test]
    fn test_status_text_adds_the_agent_once_known() {
        assert_eq!(status_text(Some("Running"), None), "Running");
        assert_eq!(status_text(None, None), "unknown");
        assert_eq!(
            status_text(Some("Running"), Some(&AgentStatus::Current { version: "1.0".into() })),
            "Running \u{b7} agent 1.0"
        );
        assert_eq!(
            status_text(Some("Running"), Some(&AgentStatus::RestartRequired { version: None, rebuild_image: false })),
            "Running \u{b7} old agent, restart required"
        );
        assert_eq!(
            status_text(Some("Running"), Some(&AgentStatus::RestartRequired { version: None, rebuild_image: true })),
            "Running \u{b7} old agent, rebuild the image, then restart"
        );
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
    fn test_the_idle_stop_note_says_what_is_stopped_automatically() {
        assert_eq!(
            idle_stop_note(60),
            "Sandboxes idle for 60 minutes with no command running are stopped, and a finished agent's at once unless a command is still running; /workspace is kept."
        );
        assert_eq!(
            idle_stop_note(0),
            "Idle sandboxes aren't stopped automatically; a finished agent's is, unless a command is still running."
        );
    }

    /// SME-521: the note names the three graces, a 0 one as kept, and a
    /// default of 0 as no automatic removal at all.
    #[test]
    fn test_the_storage_note_names_the_three_graces() {
        let graces = GraceSettings { default_days: 7, agent_days: 1, user_days: 30 };
        assert_eq!(
            storage_grace_note(&graces),
            "A stopped sandbox's storage is removed 7 days later; a finished agent's is removed 1 day later; one you stopped is removed 30 days later."
        );
        assert_eq!(
            storage_grace_note(&GraceSettings { user_days: 0, ..graces }),
            "A stopped sandbox's storage is removed 7 days later; a finished agent's is removed 1 day later; one you stopped is kept until you remove it."
        );
        assert_eq!(
            storage_grace_note(&GraceSettings { default_days: 0, ..graces }),
            "Storage is never removed automatically."
        );
    }

    #[test]
    fn test_the_storage_report_says_the_class_and_the_released_volumes() {
        use crate::api::sandbox::ReleasedVolumes;
        let class = |policy: &str| ClaimClass::Found {
            name: "local-path".to_string(),
            provisioner: "rancher.io/local-path".to_string(),
            reclaim_policy: policy.to_string(),
            configured: false,
        };
        let released = Some(ReleasedVolumes { count: 3, capacity_bytes: 60 << 30, oldest: None });
        assert_eq!(
            storage_report_lines(&StorageReport { class: class("Delete"), released: released.clone() }),
            ["Sandbox claims use local-path (reclaim: Delete).", "Released volumes not yet reclaimed: 3 (60 GiB)."]
        );
        assert_eq!(
            storage_report_lines(&StorageReport { class: class("Retain"), released: None }),
            ["Sandbox claims use local-path (reclaim: Retain); removing storage frees no disk."]
        );
        // Review 1, B-L1: no provisioner, no one to reclaim it.
        let static_class = ClaimClass::Found {
            name: "static".to_string(),
            provisioner: "kubernetes.io/no-provisioner".to_string(),
            reclaim_policy: "Delete".to_string(),
            configured: true,
        };
        assert_eq!(
            storage_report_lines(&StorageReport { class: static_class, released: None }),
            ["Sandbox claims use static (reclaim: Delete, no provisioner); removing storage frees no disk."]
        );
        assert_eq!(
            storage_report_lines(&StorageReport { class: ClaimClass::Missing { name: "fast".to_string() }, released: None }),
            ["Sandbox claims name storage class fast, which the cluster doesn't have."]
        );
        assert!(storage_report_lines(&StorageReport { class: ClaimClass::Unavailable, released: None }).is_empty());
    }

    #[test]
    fn test_the_summary_line_counts_what_the_claims_hold() {
        let summary = StoredSummary {
            conversations: 34,
            claims: 68,
            requested_bytes: 1_429_365_116_108,
            past_grace: 12,
            orphaned: 0,
        };
        assert_eq!(
            summary_text(&summary),
            "34 conversations hold 68 claims (1.3 TiB requested) \u{b7} 12 past their grace, removed over the next passes \u{b7} 0 orphaned"
        );
        let one = StoredSummary { conversations: 1, claims: 1, requested_bytes: 20 << 30, past_grace: 0, orphaned: 2 };
        assert_eq!(
            summary_text(&one),
            "1 conversation holds 1 claim (20 GiB requested) \u{b7} 0 past their grace, removed over the next passes \u{b7} 2 orphaned"
        );
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

    #[test]
    fn test_volume_cell_text_reads_the_claim_or_says_its_absence() {
        use crate::api::pods::{StoredStorage, StoredVolume};
        let volume = |kind, size: Option<&str>, deleting| StoredVolume {
            kind,
            size: size.map(str::to_string),
            deleting,
            node: None,
        };
        let row = |volumes| StoredStorage {
            conversation_id: 7,
            conversation_title: "seat".to_string(),
            volumes,
            stopped: None,
            observed_at: chrono::NaiveDateTime::default(),
            fate: StorageFate::Unknown,
        };
        let both = row(vec![
            volume(StorageKind::Workspace, Some("20Gi"), false),
            volume(StorageKind::Docker, Some("20Gi"), true),
        ]);
        assert_eq!(volume_cell_text(&both, StorageKind::Workspace), "20Gi");
        assert_eq!(volume_cell_text(&both, StorageKind::Docker), "removing\u{2026}");
        // A halfway removal: the docker claim's delete failed and this
        // row lists only the workspace; and a claim with no size reads
        // "stored" rather than an empty cell.
        let half = row(vec![volume(StorageKind::Workspace, None, false)]);
        assert_eq!(volume_cell_text(&half, StorageKind::Workspace), "stored");
        assert_eq!(volume_cell_text(&half, StorageKind::Docker), "none");
        // SME-304: the node whose disk holds the claim, once bound.
        let mut on_node = volume(StorageKind::Workspace, Some("20Gi"), false);
        on_node.node = Some("ryzen".to_string());
        assert_eq!(volume_cell_text(&row(vec![on_node]), StorageKind::Workspace), "20Gi on ryzen");
    }

    /// SME-521: the row says what the server's rule says, whatever the
    /// stop was — the page does no grace math of its own.
    #[test]
    fn test_a_rows_stop_says_when_and_what_its_fate_is() {
        use crate::api::pods::{GraceClass, PodStop};
        let now = chrono::NaiveDate::from_ymd_opt(2026, 11, 10).expect("date").and_hms_opt(12, 0, 0).expect("time");
        let days_ago = |d: i64| now - chrono::Duration::days(d);
        let removes = |in_days: i64, class| StorageFate::RemovesAt { at: now + chrono::Duration::days(in_days), days: 7, class };
        let stop = PodStop { at: days_ago(3), quiet: false };
        assert_eq!(
            stopped_text(Some(&stop), &removes(27, GraceClass::UserStop), now),
            "stopped 3d 0h ago \u{b7} removed in 27 days",
            "the user's Stop is removed too now"
        );
        let in_a_day = StorageFate::RemovesAt { at: now + chrono::Duration::hours(20), days: 1, class: GraceClass::Agent };
        assert_eq!(stopped_text(Some(&stop), &in_a_day, now), "stopped 3d 0h ago \u{b7} removed in 1 day");
        assert_eq!(
            stopped_text(Some(&stop), &removes(-1, GraceClass::Default), now),
            "stopped 3d 0h ago \u{b7} removed within the hour"
        );
        assert_eq!(stopped_text(Some(&stop), &StorageFate::Kept, now), "stopped 3d 0h ago \u{b7} kept until you remove it");
        assert_eq!(stopped_text(Some(&stop), &StorageFate::Unknown, now), "stopped 3d 0h ago", "no fate from an older server");
        assert_eq!(stopped_text(None, &StorageFate::Live, now), "", "in use");
        assert_eq!(
            stopped_text(None, &removes(5, GraceClass::NoPod), now),
            "removed in 5 days",
            "claims no pod used have a fate but no stop"
        );
    }
}
