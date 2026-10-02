//! The pods view: every live sandbox pod across all conversations, and
//! stopping one. See SME-26.

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
    /// Its sandbox agent's protocol version, once smelt has connected to it
    /// (SME-53). `None` until then.
    pub agent: Option<AgentStatus>,
    /// The language servers running next to it (SME-35).
    pub language_servers: Vec<ServerPodOverview>,
    /// The database's clock when this was read; ages are measured against
    /// it, not the browser's clock.
    pub observed_at: NaiveDateTime,
}

/// A sandbox pod's agent, against the protocol this smelt speaks (SME-53).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum AgentStatus {
    /// Smelt's own version, or a newer minor.
    Current { version: String },
    /// An older minor: everything works except features added since, which
    /// ask for a restart.
    RestartRecommended { version: String },
    /// Another major, or (`None`) an agent from before versioning: the
    /// terminals and file tools don't work until the pod is replaced.
    RestartRequired { version: Option<String> },
}

impl AgentStatus {
    /// One line, for the Sandboxes page and the model's `list_pods`.
    pub fn describe(&self) -> String {
        match self {
            AgentStatus::Current { version } => format!("agent {version}"),
            AgentStatus::RestartRecommended { version } => format!("agent {version}, restart for new features"),
            AgentStatus::RestartRequired { version: Some(version) } => format!("agent {version}, restart required"),
            AgentStatus::RestartRequired { version: None } => "old agent, restart required".to_string(),
        }
    }
}

/// A language server's pod, next to a sandbox pod.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServerPodOverview {
    pub name: String,
    /// `installing`, `ready`, or `stopped (OOMKilled)`.
    pub state: String,
    pub memory_limit: Option<String>,
    pub usage: Option<PodUsage>,
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

/// A pod's containers' memory limits added up, as a quantity: `"16Gi"`,
/// `"1536Mi"`. `None` if there are none or one doesn't parse.
#[cfg(feature = "server")]
pub(crate) fn sum_memory_limits(limits: &[String]) -> Option<String> {
    if limits.is_empty() {
        return None;
    }
    let bytes = limits.iter().map(|l| parse_memory_bytes(l)).sum::<Option<u64>>()?;
    const MI: u64 = 1024 * 1024;
    const GI: u64 = 1024 * MI;
    Some(if bytes % GI == 0 {
        format!("{}Gi", bytes / GI)
    } else if bytes % MI == 0 {
        format!("{}Mi", bytes / MI)
    } else {
        bytes.to_string()
    })
}

/// A pod's containers' CPU limits added up, as a quantity: `"2"`, `"1250m"`.
#[cfg(feature = "server")]
pub(crate) fn sum_cpu_limits(limits: &[String]) -> Option<String> {
    if limits.is_empty() {
        return None;
    }
    let millicores = limits
        .iter()
        .map(|l| parse_cpu_nanocores(l).map(|nanos| (nanos / 1_000_000.0).round() as u64))
        .sum::<Option<u64>>()?;
    Some(if millicores % 1000 == 0 {
        (millicores / 1000).to_string()
    } else {
        format!("{millicores}m")
    })
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
            memory_limit: sum_memory_limits(&details.memory_limits),
            cpu_limit: sum_cpu_limits(&details.cpu_limits),
            usage: usage.get(&crate::sandbox::pod_name(row.pod_id)).cloned(),
            terminals: row.live_terminals,
            agent: crate::sandbox::agent_status(row.pod_id),
            language_servers: server_overviews(row.conversation_id, row.pod_id, &usage).await,
            observed_at: row.observed_at,
        });
    }
    Ok(overviews)
}

/// The language servers next to sandbox pod `pod_id`; none when the
/// cluster can't be asked.
#[cfg(feature = "server")]
async fn server_overviews(
    conversation_id: i64,
    pod_id: i64,
    usage: &std::collections::HashMap<String, PodUsage>,
) -> Vec<ServerPodOverview> {
    use crate::lsp::pods::{self, ServerState};
    let client = match crate::sandbox::kube_client() {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't list language server pods");
            return Vec::new();
        }
    };
    let servers = match pods::list_with(&client, conversation_id).await {
        Ok(servers) => servers,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't list language server pods");
            return Vec::new();
        }
    };
    servers
        .into_iter()
        .filter(|server| server.pod_name == pods::server_pod_name(pod_id, &server.name))
        .map(|server| ServerPodOverview {
            state: match &server.state {
                ServerState::Installing => "installing".to_string(),
                ServerState::Ready => "ready".to_string(),
                ServerState::Stopped(Some(reason)) => format!("stopped ({reason})"),
                ServerState::Stopped(None) => "stopped".to_string(),
            },
            usage: usage.get(&server.pod_name).cloned(),
            memory_limit: server.memory_limit,
            name: server.name,
        })
        .collect()
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
    fn test_agent_status_says_whether_a_restart_is_needed() {
        assert_eq!(AgentStatus::Current { version: "1.0".into() }.describe(), "agent 1.0");
        assert_eq!(
            AgentStatus::RestartRecommended { version: "1.2".into() }.describe(),
            "agent 1.2, restart for new features"
        );
        assert_eq!(
            AgentStatus::RestartRequired { version: Some("2.0".into()) }.describe(),
            "agent 2.0, restart required"
        );
        assert_eq!(AgentStatus::RestartRequired { version: None }.describe(), "old agent, restart required");
    }

    /// `PodOverview` crosses to the browser, so each agent status must
    /// survive JSON.
    #[test]
    fn test_a_pod_overview_round_trips_through_json_with_each_agent_status() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 9, 29)
            .expect("date")
            .and_hms_opt(1, 2, 3)
            .expect("time");
        for agent in [
            None,
            Some(AgentStatus::Current { version: "1.0".into() }),
            Some(AgentStatus::RestartRecommended { version: "1.0".into() }),
            Some(AgentStatus::RestartRequired { version: Some("2.1".into()) }),
            Some(AgentStatus::RestartRequired { version: None }),
        ] {
            let overview = PodOverview {
                pod_id: 7,
                conversation_id: 3,
                conversation_title: "t".into(),
                status: Some("Running".into()),
                started_at: at,
                activity: PodActivity::Busy,
                memory_limit: None,
                cpu_limit: None,
                usage: None,
                terminals: 1,
                agent,
                language_servers: vec![],
                observed_at: at,
            };
            let json = serde_json::to_string(&overview).expect("serializes");
            let back: PodOverview = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, overview);
        }
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
    fn test_sum_limits_adds_a_pods_containers() {
        let q = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(sum_memory_limits(&q(&["8Gi", "8Gi"])), Some("16Gi".to_string()));
        assert_eq!(sum_memory_limits(&q(&["1Gi", "512Mi"])), Some("1536Mi".to_string()));
        assert_eq!(sum_memory_limits(&q(&["128Mi"])), Some("128Mi".to_string()));
        assert_eq!(sum_memory_limits(&q(&["8Gi", "lots"])), None);
        assert_eq!(sum_memory_limits(&[]), None);
        assert_eq!(sum_cpu_limits(&q(&["1", "1"])), Some("2".to_string()));
        assert_eq!(sum_cpu_limits(&q(&["1", "250m"])), Some("1250m".to_string()));
        assert_eq!(sum_cpu_limits(&q(&["x"])), None);
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

    /// A real `PodMetricsList` from the local k3s's metrics-server,
    /// captured 2026-09-26 for a sandbox-image pod in the test namespace.
    const CAPTURED_POD_METRICS: &str = include_str!("pod_metrics_fixture.json");

    #[test]
    fn test_parse_pod_metrics_reads_a_real_response() {
        let list: serde_json::Value = serde_json::from_str(CAPTURED_POD_METRICS).expect("valid JSON");
        let usage = parse_pod_metrics(&list);
        assert_eq!(
            usage.get("metrics-fixture-capture"),
            Some(&PodUsage { memory_bytes: 1156 * 1024, cpu_millicores: 0 }),
            "736969 nanocores is under one millicore"
        );
        assert_eq!(usage.len(), 1);
    }

    /// Hand-made, for what the captured response doesn't show: a pod with
    /// several containers, and one whose numbers don't parse.
    const EDGE_CASE_POD_METRICS: &str = r#"{
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
        let list: serde_json::Value = serde_json::from_str(EDGE_CASE_POD_METRICS).expect("valid JSON");
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
