//! The pods view: every live sandbox pod across all conversations, and
//! stopping one. See docs/projects/completed/20260925-pod-management.md.

use chrono::NaiveDateTime;
use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::events::AppEvent;

#[cfg(feature = "server")]
use crate::db;

/// One row of the pods view.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PodOverview {
    pub pod_id: i64,
    pub conversation_id: i64,
    pub conversation_title: String,
    /// Kubernetes' phase (`Running`, `Pending`, ...), or `None` when the
    /// pod object couldn't be read.
    pub status: Option<String>,
    pub started_at: NaiveDateTime,
    pub activity: PodActivity,
    /// Kubernetes quantity strings, as configured (`"8Gi"`, `"1"`).
    pub memory_limit: Option<String>,
    pub cpu_limit: Option<String>,
    /// `None` when the metrics API can't be read (not allowed, not
    /// installed) or has no numbers for this pod yet.
    pub usage: Option<PodUsage>,
    pub terminals: i64,
    /// The database's clock when this was read; ages are measured against
    /// it, not the browser's clock.
    pub observed_at: NaiveDateTime,
}

/// A pod's current resource use, from the cluster's metrics API. Lags
/// real use by up to about a minute.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PodUsage {
    pub memory_bytes: u64,
    pub cpu_millicores: u64,
}

/// A Kubernetes CPU quantity in nanocores: `"250m"`, `"2"` (cores),
/// `"1203981n"` (nanocores, what metrics-server reports) or `"5u"`. A
/// float, so fractional cores (`"0.5"`) and sums across containers keep
/// their precision until converted to millicores at the end.
#[cfg(feature = "server")]
fn parse_cpu_nanocores(quantity: &str) -> Option<f64> {
    let (number, scale) = match quantity.char_indices().last()? {
        (i, 'n') => (&quantity[..i], 1.0),
        (i, 'u') => (&quantity[..i], 1_000.0),
        (i, 'm') => (&quantity[..i], 1_000_000.0),
        _ => (quantity, 1_000_000_000.0),
    };
    let value: f64 = number.parse().ok()?;
    (value >= 0.0).then_some(value * scale)
}

/// A Kubernetes memory quantity in bytes: `"3372Ki"`, `"512Mi"`, `"1Gi"`,
/// `"1500k"`, `"2M"`, `"1G"` or plain bytes.
#[cfg(feature = "server")]
fn parse_memory_bytes(quantity: &str) -> Option<u64> {
    const UNITS: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    let (number, scale) = UNITS
        .iter()
        .find_map(|(suffix, scale)| quantity.strip_suffix(suffix).map(|n| (n, *scale)))
        .unwrap_or((quantity, 1.0));
    let value: f64 = number.parse().ok()?;
    (value >= 0.0).then(|| (value * scale) as u64)
}

/// Usage per pod name from a `PodMetricsList` (`metrics.k8s.io/v1beta1`),
/// summed over each pod's containers. A pod whose numbers don't parse is
/// left out rather than shown wrong.
#[cfg(feature = "server")]
fn parse_pod_metrics(list: &serde_json::Value) -> std::collections::HashMap<String, PodUsage> {
    let items = list.get("items").and_then(|items| items.as_array());
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = item.pointer("/metadata/name")?.as_str()?.to_string();
            let mut nanocores = 0.0;
            let mut memory_bytes = 0;
            for container in item.get("containers")?.as_array()? {
                nanocores += parse_cpu_nanocores(container.pointer("/usage/cpu")?.as_str()?)?;
                memory_bytes += parse_memory_bytes(container.pointer("/usage/memory")?.as_str()?)?;
            }
            let cpu_millicores = (nanocores / 1_000_000.0) as u64;
            Some((name, PodUsage { memory_bytes, cpu_millicores }))
        })
        .collect()
}

/// Whether a pod is doing anything. Busy while any of its terminals has a
/// command running; otherwise idle since its last sign of activity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PodActivity {
    Busy,
    IdleSince(NaiveDateTime),
}

/// `row`'s activity: busy while a command runs, and otherwise idle since
/// the latest of its last command finishing, its conversation's last
/// message (which stands in for file and web tool calls, which leave no
/// per-pod record), and the pod starting.
#[cfg(feature = "server")]
fn pod_activity(row: &db::LivePodRow) -> PodActivity {
    if row.running_commands > 0 {
        return PodActivity::Busy;
    }
    let latest = [Some(row.created_at), Some(row.conversation_updated_at), row.last_command_finished_at]
        .into_iter()
        .flatten()
        .max()
        .expect("created_at is always present");
    PodActivity::IdleSince(latest)
}

/// Every live pod, assembled from the database, each pod object in
/// Kubernetes, and one metrics list. Metrics failing (most often a 403,
/// until the RBAC change is applied) leaves every `usage` empty; a pod
/// object that can't be read leaves its status and limits empty. Neither
/// fails the whole view.
#[cfg(feature = "server")]
pub(crate) async fn pod_overviews(pool: &sqlx::PgPool) -> Result<Vec<PodOverview>, sqlx::Error> {
    let rows = db::list_live_pods(pool).await?;
    let usage = match crate::sandbox::pod_metrics_list().await {
        Ok(list) => parse_pod_metrics(&list),
        Err(e) => {
            tracing::debug!(error = %e, "pod metrics unavailable");
            std::collections::HashMap::new()
        }
    };
    let mut overviews = Vec::with_capacity(rows.len());
    for row in rows {
        let details = match crate::sandbox::pod_details(row.pod_id).await {
            Ok(details) => details.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(pod_id = row.pod_id, error = %e, "couldn't read pod details");
                crate::sandbox::PodDetails::default()
            }
        };
        overviews.push(PodOverview {
            pod_id: row.pod_id,
            conversation_id: row.conversation_id,
            conversation_title: row.conversation_title.clone(),
            status: details.phase,
            started_at: row.created_at,
            activity: pod_activity(&row),
            memory_limit: details.memory_limit,
            cpu_limit: details.cpu_limit,
            usage: usage.get(&crate::sandbox::kubernetes_pod_name(row.pod_id)).cloned(),
            terminals: row.live_terminals,
            observed_at: row.observed_at,
        });
    }
    Ok(overviews)
}

#[get("/api/pods")]
pub async fn get_pods() -> ServerFnResult<Vec<PodOverview>> {
    pod_overviews(db::get()).await.map_err(ServerFnError::new)
}

/// Stops a pod for the user. See `sandbox::stop_pod_for_user`.
#[post("/api/pods/{pod_id}/stop")]
pub async fn stop_pod(pod_id: i64) -> ServerFnResult<()> {
    crate::sandbox::stop_pod_for_user(db::get(), pod_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Which conversations have a live pod, for the sidebar's markers.
#[get("/api/pods/conversations")]
pub async fn get_live_pod_conversations() -> ServerFnResult<Vec<i64>> {
    db::conversations_with_live_pods(db::get())
        .await
        .map_err(ServerFnError::new)
}

/// The always-open stream of app-wide events (`AppEvent`) for the sidebar
/// and the pods view.
#[get("/api/app-events")]
pub async fn subscribe_app_events() -> ServerFnResult<ServerEvents<AppEvent>> {
    // `from_stream`, not `ServerEvents::new`, for the same reason as
    // `subscribe_conversation_events`: dropping the response (the tab
    // going away) drops the subscription.
    let rx = crate::events::subscribe_app();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => return Some((Ok::<_, axum::BoxError>(event), rx)),
                // A listener that fell behind just refetches on the next one.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Ok(ServerEvents::from_stream(stream))
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    fn at(hour: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
            .expect("a valid date")
            .and_hms_opt(hour, 0, 0)
            .expect("a valid time")
    }

    fn row(created: u32, message: u32, finished: Option<u32>, running: i64) -> db::LivePodRow {
        db::LivePodRow {
            pod_id: 1,
            conversation_id: 1,
            conversation_title: "t".to_string(),
            conversation_updated_at: at(message),
            created_at: at(created),
            live_terminals: 1,
            running_commands: running,
            last_command_finished_at: finished.map(at),
            observed_at: at(12),
        }
    }

    /// A subscription belongs to its connection: once the response is
    /// dropped (the tab closed or reloaded), it stops listening.
    #[tokio::test]
    async fn test_a_dropped_app_event_subscription_stops_listening() {
        let before = crate::events::app_subscriber_count();
        let subscription = subscribe_app_events().await.expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(crate::events::app_subscriber_count(), before + 1);
        drop(subscription);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            crate::events::app_subscriber_count(),
            before,
            "a dropped subscription is still listening"
        );
    }

    fn parse_cpu_millicores(quantity: &str) -> Option<u64> {
        parse_cpu_nanocores(quantity).map(|nanos| (nanos / 1_000_000.0) as u64)
    }

    #[test]
    fn test_parse_cpu_millicores_handles_each_unit() {
        assert_eq!(parse_cpu_millicores("250m"), Some(250));
        assert_eq!(parse_cpu_millicores("2"), Some(2000));
        assert_eq!(parse_cpu_millicores("1203981n"), Some(1));
        assert_eq!(parse_cpu_millicores("5500000n"), Some(5));
        assert_eq!(parse_cpu_millicores("2500u"), Some(2));
        assert_eq!(parse_cpu_millicores("0.5"), Some(500));
        assert_eq!(parse_cpu_millicores("lots"), None);
    }

    #[test]
    fn test_parse_memory_bytes_handles_each_unit() {
        assert_eq!(parse_memory_bytes("3372Ki"), Some(3372 * 1024));
        assert_eq!(parse_memory_bytes("512Mi"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("8Gi"), Some(8 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("1500k"), Some(1_500_000));
        assert_eq!(parse_memory_bytes("2M"), Some(2_000_000));
        assert_eq!(parse_memory_bytes("1G"), Some(1_000_000_000));
        assert_eq!(parse_memory_bytes("4096"), Some(4096));
        assert_eq!(parse_memory_bytes("big"), None);
    }

    /// Shape as documented for `metrics.k8s.io/v1beta1` `PodMetricsList`;
    /// to be replaced by a real captured response once smelt's service
    /// account is allowed to read metrics (see the plan).
    const DOCUMENTED_POD_METRICS: &str = r#"{
        "kind": "PodMetricsList",
        "apiVersion": "metrics.k8s.io/v1beta1",
        "metadata": {},
        "items": [
            {
                "metadata": {"name": "sandbox-12", "namespace": "smelt-park"},
                "timestamp": "2026-09-25T05:00:00Z",
                "window": "10.052s",
                "containers": [
                    {"name": "sandbox", "usage": {"cpu": "1203981n", "memory": "3372Ki"}},
                    {"name": "sidecar", "usage": {"cpu": "2000000n", "memory": "1Mi"}}
                ]
            },
            {
                "metadata": {"name": "sandbox-13", "namespace": "smelt-park"},
                "timestamp": "2026-09-25T05:00:00Z",
                "window": "10.052s",
                "containers": [{"name": "sandbox", "usage": {"cpu": "weird", "memory": "1Mi"}}]
            }
        ]
    }"#;

    #[test]
    fn test_parse_pod_metrics_sums_containers_and_skips_unparseable_pods() {
        let list: serde_json::Value = serde_json::from_str(DOCUMENTED_POD_METRICS).expect("valid JSON");
        let usage = parse_pod_metrics(&list);
        assert_eq!(
            usage.get("sandbox-12"),
            Some(&PodUsage { memory_bytes: 3372 * 1024 + 1024 * 1024, cpu_millicores: 3 })
        );
        assert!(!usage.contains_key("sandbox-13"), "unparseable numbers should be left out");
    }

    #[test]
    fn test_pod_activity_is_busy_while_a_command_runs() {
        assert_eq!(pod_activity(&row(1, 2, Some(3), 1)), PodActivity::Busy);
    }

    #[test]
    fn test_pod_activity_is_idle_since_the_latest_sign_of_activity() {
        assert_eq!(pod_activity(&row(1, 2, Some(3), 0)), PodActivity::IdleSince(at(3)), "last command");
        assert_eq!(pod_activity(&row(1, 4, Some(3), 0)), PodActivity::IdleSince(at(4)), "last message");
        assert_eq!(pod_activity(&row(5, 4, None, 0)), PodActivity::IdleSince(at(5)), "pod start");
    }
}
