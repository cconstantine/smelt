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
    /// `rebuild_image` when the pod already runs the image a new pod would
    /// get, so replacing it won't help until that image is rebuilt, and is
    /// needed after (SME-121).
    RestartRequired {
        version: Option<String>,
        #[serde(default)]
        rebuild_image: bool,
    },
}

impl AgentStatus {
    /// One line, for the Sandboxes page and the model's `list_pods`.
    pub fn describe(&self) -> String {
        match self {
            AgentStatus::Current { version } => format!("agent {version}"),
            AgentStatus::RestartRecommended { version } => format!("agent {version}, restart for new features"),
            AgentStatus::RestartRequired { version, rebuild_image } => {
                let agent = match version {
                    Some(version) => format!("agent {version}"),
                    None => "old agent".to_string(),
                };
                // The pod runs the old build even once the image is rebuilt
                // under the same name, so it still needs a restart after.
                let fix = if *rebuild_image { "rebuild the image, then restart" } else { "restart required" };
                format!("{agent}, {fix}")
            }
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

/// One conversation's stored sandbox volumes on the Sandboxes page, and
/// the conversation's sandbox panel (SME-253): present whether or not a
/// pod runs — stopped conversations holding disk is the whole point.
#[cfg_attr(not(feature = "server"), expect(dead_code, reason = "the browser side reads them from SME-253's UI commit; the server's list endpoint constructs them now"))]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredStorage {
    pub conversation_id: i64,
    pub conversation_title: String,
    pub volumes: Vec<StoredVolume>,
}

/// One of a conversation's two conversation claims.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredVolume {
    pub kind: StorageKind,
    /// The claim's size as a Kubernetes quantity string (`"20Gi"`):
    /// bound capacity, else the request. Under local-path both equal the
    /// *request*, never usage — views label it "claim", never "used".
    pub size: Option<String>,
    /// The claim carries a deletion timestamp: it goes when its last
    /// mounting pod does. Views read it "removing…".
    pub deleting: bool,
}

/// Which claim of the pair, by name prefix.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    Workspace,
    Docker,
}

/// What one run of `sandbox::remove_conversation_storage` did. Counts,
/// not assertions: a halfway removal is a real state (a claim another
/// server made between list and delete), and the view's refetch — driven
/// by the `PodsChanged` the removal publishes — is what shows the rest.
#[cfg_attr(not(feature = "server"), expect(dead_code, reason = "the browser side reads it from SME-253's UI commit; the remove endpoint returns it now"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageRemoval {
    /// Whether a running sandbox pod was stopped first.
    pub pod_stopped: bool,
    /// How many of the conversation's language-server pods were deleted.
    pub lsp_pods_stopped: u8,
    /// How many of the two named claims were deleted here.
    pub claims_deleted: u8,
    /// How many were left behind (unlabelled, replaced, a failed
    /// delete): the view will still show them.
    pub claims_failed: u8,
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

/// Which claim of the conversation pair a claim name is (SME-253);
/// `None` for anything else — belt after the label guard, since agent
/// volumes and the global sandbox volumes share no prefix with these.
#[cfg(feature = "server")]
fn storage_kind(name: &str) -> Option<StorageKind> {
    if name.starts_with("sandbox-workspace-") {
        Some(StorageKind::Workspace)
    } else if name.starts_with("sandbox-docker-") {
        Some(StorageKind::Docker)
    } else {
        None
    }
}

/// The stored-volumes view's rows: `claims` (already narrowed to this
/// instance's conversation claims) grouped per conversation, `deleting`
/// from the deletion timestamp, `size` from bound capacity else the
/// request. Claims whose conversation label doesn't parse are dropped
/// (nothing to group them under), as are conversations missing from
/// `titles` — deleted between listing and this read, no row to title.
/// Conversations sort by id, workspaces before dockers.
#[cfg(feature = "server")]
fn storage_rows(
    claims: &[k8s_openapi::api::core::v1::PersistentVolumeClaim],
    titles: &std::collections::HashMap<i64, String>,
) -> Vec<StoredStorage> {
    let mut by_id: std::collections::BTreeMap<i64, Vec<StoredVolume>> = Default::default();
    for claim in claims {
        let Some(kind) = claim.metadata.name.as_deref().and_then(storage_kind) else { continue };
        let Some(value) = claim.metadata.labels.as_ref().and_then(|labels| labels.get("smelt/conversation")) else {
            continue;
        };
        let Ok(conversation_id) = value.parse::<i64>() else { continue };
        let size = claim
            .status
            .as_ref()
            .and_then(|status| status.capacity.as_ref())
            .and_then(|capacity| capacity.get("storage"))
            .or_else(|| {
                claim
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.resources.as_ref())
                    .and_then(|resources| resources.requests.as_ref())
                    .and_then(|requests| requests.get("storage"))
            })
            .map(|quantity| quantity.0.clone());
        by_id.entry(conversation_id).or_default().push(StoredVolume {
            kind,
            size,
            deleting: claim.metadata.deletion_timestamp.is_some(),
        });
    }
    by_id
        .into_iter()
        .filter_map(|(conversation_id, mut volumes)| {
            let conversation_title = titles.get(&conversation_id).cloned()?;
            volumes.sort_by_key(|volume| matches!(volume.kind, StorageKind::Docker));
            Some(StoredStorage { conversation_id, conversation_title, volumes })
        })
        .collect()
}

/// The claims and titles behind `get_stored_storage`. The list carries
/// the startup sweep's selector — instance-scoped (SME-115), so a check
/// server's list can never show or act on production's claims, or vice
/// versa.
#[cfg(feature = "server")]
pub(crate) async fn stored_storage(pool: &sqlx::PgPool) -> Result<Vec<StoredStorage>, String> {
    let instance = db::smelt_instance(pool).await.map_err(|e| e.to_string())?;
    let client = crate::sandbox::kube_client().map_err(|e| e.to_string())?;
    let selector = kube::api::ListParams::default()
        .labels(&format!("smelt/conversation,{}={}", crate::sandbox::INSTANCE_LABEL, instance.id));
    let claims = crate::sandbox::pvc_api(&client)
        .list(&selector)
        .await
        .map_err(|e| e.to_string())?
        .items;
    let titles = db::list_conversations(pool)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|conversation| (conversation.id, conversation.title))
        .collect();
    Ok(storage_rows(&claims, &titles))
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
    #[expect(clippy::expect_used, reason = "the array's first element is always Some, so the max is too")]
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
        // `Ok(None)`: no pod of this database's by that name (SME-115),
        // so a same-named pod's usage isn't shown against this row either.
        let (details, ours) = match crate::sandbox::pod_details(pool, row.pod_id).await {
            Ok(details) => (details.clone().unwrap_or_default(), details.is_some()),
            Err(e) => {
                tracing::warn!(pod_id = row.pod_id, error = %e, "couldn't read pod details");
                (crate::sandbox::PodDetails::default(), true)
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
            usage: ours.then(|| usage.get(&crate::sandbox::pod_name(row.pod_id)).cloned()).flatten(),
            terminals: row.live_terminals,
            agent: crate::sandbox::agent_status(row.pod_id),
            language_servers: server_overviews(pool, row.conversation_id, row.pod_id, &usage).await,
            observed_at: row.observed_at,
        });
    }
    Ok(overviews)
}

/// The language servers next to sandbox pod `pod_id`; none when the
/// cluster can't be asked.
#[cfg(feature = "server")]
async fn server_overviews(
    pool: &sqlx::PgPool,
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
    let instance = match db::smelt_instance(pool).await {
        Ok(instance) => instance.id,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't list language server pods");
            return Vec::new();
        }
    };
    let servers = match pods::list_with(&client, conversation_id, &instance).await {
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

/// The Sandboxes page's stored-volumes rows (SME-253): which
/// conversations hold sandbox disk — the /workspace and Docker-data
/// claims — whether or not a pod runs, plus what the panel shows as its
/// storage line.
#[get("/api/storage")]
pub async fn get_stored_storage() -> ServerFnResult<Vec<StoredStorage>> {
    stored_storage(db::get()).await.map_err(ServerFnError::new)
}

/// Removes a conversation's sandbox volumes, keeping the conversation:
/// see `sandbox::remove_conversation_storage`. A running pod is stopped
/// first. The returned counts say what went and what was left; views
/// refetch on the `PodsChanged` that publishes.
#[post("/api/storage/{conversation_id}/remove")]
pub async fn remove_stored_storage(conversation_id: i64) -> ServerFnResult<StorageRemoval> {
    crate::sandbox::remove_conversation_storage(db::get(), conversation_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// The always-open stream of app-wide events (`AppEvent`) for the sidebar
/// and the pods view.
#[get("/api/app-events")]
pub async fn subscribe_app_events() -> ServerFnResult<ServerEvents<AppEvent>> {
    Ok(app_events_response(crate::events::subscribe_app()))
}

/// The response `subscribe_app_events` sends, relaying `rx`. Takes the
/// receiver so a test can hand it a channel of its own: the app-wide
/// channel's count moves with every other test subscribing (SME-135).
#[cfg(feature = "server")]
fn app_events_response(rx: tokio::sync::broadcast::Receiver<AppEvent>) -> ServerEvents<AppEvent> {
    // `from_stream`, not `ServerEvents::new`, for the same reason as
    // `subscribe_conversation_events`: dropping the response (the tab
    // going away) drops the subscription.
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
    ServerEvents::from_stream(stream)
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
    /// dropped (the tab closed or reloaded), it stops listening. Counted
    /// on a channel of the test's own: the app-wide one is shared with
    /// every test running alongside (SME-135).
    #[tokio::test]
    async fn test_a_dropped_app_event_subscription_stops_listening() {
        let (sender, rx) = tokio::sync::broadcast::channel(16);
        // Another test subscribing to app events meanwhile, as the pod
        // lifecycle and providers tests do: it mustn't move the count.
        let _alongside = crate::events::subscribe_app();
        let subscription = app_events_response(rx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(sender.receiver_count(), 1);
        drop(subscription);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(sender.receiver_count(), 0, "a dropped subscription is still listening");
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
            AgentStatus::RestartRequired { version: Some("2.0".into()), rebuild_image: false }.describe(),
            "agent 2.0, restart required"
        );
        assert_eq!(
            AgentStatus::RestartRequired { version: None, rebuild_image: false }.describe(),
            "old agent, restart required"
        );
        // SME-121: a new pod would run the same image, so a restart can't help.
        assert_eq!(
            AgentStatus::RestartRequired { version: Some("2.0".into()), rebuild_image: true }.describe(),
            "agent 2.0, rebuild the image, then restart"
        );
        assert_eq!(
            AgentStatus::RestartRequired { version: None, rebuild_image: true }.describe(),
            "old agent, rebuild the image, then restart"
        );
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
            Some(AgentStatus::RestartRequired { version: Some("2.1".into()), rebuild_image: false }),
            Some(AgentStatus::RestartRequired { version: None, rebuild_image: false }),
            Some(AgentStatus::RestartRequired { version: None, rebuild_image: true }),
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

    /// One claim of the stored-volumes list, as the cluster's JSON would
    /// have it: `conversation` labels it (as listing would), `capacity`
    /// and `request` are the two size sources, `deleting` the timestamp.
    #[cfg(feature = "server")]
    fn stored_claim(
        name: &str,
        conversation: Option<&str>,
        capacity: Option<&str>,
        request: Option<&str>,
        deleting: bool,
    ) -> k8s_openapi::api::core::v1::PersistentVolumeClaim {
        let mut metadata = serde_json::json!({
            "name": name,
            "labels": { "smelt/instance": "this-instance" },
        });
        if let Some(conversation) = conversation {
            metadata.as_object_mut().expect("object")["labels"].as_object_mut().expect("labels")
                .insert("smelt/conversation".into(), conversation.into());
        }
        if deleting {
            metadata.as_object_mut().expect("object")
                .insert("deletionTimestamp".into(), "2026-10-01T00:00:00Z".into());
        }
        let spec = request.map(|request| serde_json::json!({ "resources": { "requests": { "storage": request } } }));
        let status = capacity.map(|capacity| serde_json::json!({ "capacity": { "storage": capacity } }));
        serde_json::from_value(serde_json::json!({
            "metadata": metadata,
            "spec": spec.unwrap_or_else(|| serde_json::json!({})),
            "status": status.unwrap_or_else(|| serde_json::json!({})),
        }))
        .expect("claim")
    }

    #[test]
    fn test_storage_rows_groups_a_conversations_two_claims() {
        let titles = std::collections::HashMap::from([(7i64, "seat a".to_string())]);
        let rows = storage_rows(
            &[
                stored_claim("sandbox-docker-7", Some("7"), None, Some("20Gi"), true),
                stored_claim("sandbox-workspace-7", Some("7"), Some("20Gi"), None, false),
            ],
            &titles,
        );
        assert_eq!(
            rows,
            vec![StoredStorage {
                conversation_id: 7,
                conversation_title: "seat a".into(),
                volumes: vec![
                    StoredVolume { kind: StorageKind::Workspace, size: Some("20Gi".into()), deleting: false },
                    StoredVolume { kind: StorageKind::Docker, size: Some("20Gi".into()), deleting: true },
                ],
            }],
            "workspace first however they listed, docker's size from the request, its deleting flagged"
        );
    }

    #[test]
    fn test_storage_rows_show_bound_capacity_over_the_request_and_none_at_all() {
        let titles = std::collections::HashMap::from([(7i64, "seat".to_string())]);
        let rows = storage_rows(
            &[
                stored_claim("sandbox-workspace-7", Some("7"), Some("20Gi"), Some("1Gi"), false),
                stored_claim("sandbox-docker-7", Some("7"), None, None, false),
            ],
            &titles,
        );
        assert_eq!(rows[0].volumes[0].size.as_deref(), Some("20Gi"), "bound capacity over request");
        assert_eq!(rows[0].volumes[1].size, None, "neither source says nothing");
    }

    #[test]
    fn test_storage_rows_drop_claims_of_no_known_conversation() {
        let titles = std::collections::HashMap::from([(7i64, "seat".to_string())]);
        let rows = storage_rows(
            &[
                // Its conversation row is gone (deleted while these listed).
                stored_claim("sandbox-workspace-9", Some("9"), Some("20Gi"), None, false),
                // A global sandbox volume: right label, not this pair's name.
                stored_claim("sandbox-volume-7", Some("7"), Some("10Gi"), None, false),
                // Its conversation label is no id.
                stored_claim("sandbox-docker-x", Some("x"), None, Some("20Gi"), false),
                // And one the list shouldn't have returned: no label at all.
                stored_claim("sandbox-workspace-7", None, Some("20Gi"), None, false),
            ],
            &titles,
        );
        assert!(rows.is_empty(), "conversation 7 had only an unlabelled claim: {rows:?}");
    }

    #[test]
    fn test_storage_rows_sort_conversations_by_id() {
        let titles = std::collections::HashMap::from([
            (9i64, "b".to_string()),
            (7i64, "a".to_string()),
        ]);
        let rows = storage_rows(
            &[
                stored_claim("sandbox-workspace-9", Some("9"), None, None, false),
                stored_claim("sandbox-workspace-7", Some("7"), None, None, false),
            ],
            &titles,
        );
        assert_eq!(
            rows.iter().map(|row| row.conversation_id).collect::<Vec<_>>(),
            vec![7, 9],
            "stable order for the page's rows"
        );
    }
}
