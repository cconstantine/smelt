use super::*;

fn at(hour: u32) -> NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
        .expect("a valid date")
        .and_hms_opt(hour, 0, 0)
        .expect("a valid time")
}

fn row(created: u32, idle_since: u32, busy: bool) -> db::LivePodRow {
    db::LivePodRow {
        pod_id: 1,
        conversation_id: 1,
        conversation_title: "t".to_string(),
        created_at: at(created),
        live_terminals: 1,
        running_commands: i64::from(busy),
        idle_since: at(idle_since),
        busy,
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
const CAPTURED_POD_METRICS: &str = include_str!("../pod_metrics_fixture.json");

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
fn test_pod_activity_is_busy_while_the_database_says_so_or_a_turn_runs() {
    assert_eq!(activity_of(&row(1, 3, true), false), PodActivity::Busy, "a command or a clone");
    assert_eq!(activity_of(&row(1, 3, false), true), PodActivity::Busy, "a turn running");
}

#[test]
fn test_pod_activity_is_idle_since_the_databases_idle_since() {
    assert_eq!(activity_of(&row(1, 3, false), false), PodActivity::IdleSince(at(3)));
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
            .insert(crate::sandbox::CONVERSATION_LABEL.into(), conversation.into());
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
fn test_conversation_claim_kind_reads_the_generated_names() {
    // Belt: the two names the specs generate are the two kinds, and
    // every other claim's name is no kind at all — the generic
    // volume's shares no prefix with these.
    assert_eq!(crate::sandbox::conversation_claim_kind(&crate::sandbox::workspace_pvc_name(7)), Some(StorageKind::Workspace));
    assert_eq!(crate::sandbox::conversation_claim_kind(&crate::sandbox::docker_pvc_name(7)), Some(StorageKind::Docker));
    for name in ["sandbox-volume-7", "agent-volume-7", "sandbox-workspaces-7", "sandbox-docker", "workspace-7"] {
        assert_eq!(crate::sandbox::conversation_claim_kind(name), None, "{name} is not the pair's");
    }
}

#[test]
fn test_storage_rows_groups_a_conversations_two_claims() {
    let titles = std::collections::HashMap::from([(7i64, "seat a".to_string())]);
    let rows = storage_rows(
        &[
            stored_claim(&crate::sandbox::docker_pvc_name(7), Some("7"), None, Some("20Gi"), true),
            stored_claim(&crate::sandbox::workspace_pvc_name(7), Some("7"), Some("20Gi"), None, false),
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
            stored_claim(&crate::sandbox::workspace_pvc_name(7), Some("7"), Some("20Gi"), Some("1Gi"), false),
            stored_claim(&crate::sandbox::docker_pvc_name(7), Some("7"), None, None, false),
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
            stored_claim(&crate::sandbox::workspace_pvc_name(9), Some("9"), Some("20Gi"), None, false),
            // A global sandbox volume: right label, not this pair's name.
            stored_claim("sandbox-volume-7", Some("7"), Some("10Gi"), None, false),
            // Its conversation label is no id.
            stored_claim(&crate::sandbox::docker_pvc_name(7), Some("x"), None, Some("20Gi"), false),
            // And one the list shouldn't have returned: no label at all.
            stored_claim(&crate::sandbox::workspace_pvc_name(7), None, Some("20Gi"), None, false),
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
            stored_claim(&crate::sandbox::workspace_pvc_name(9), Some("9"), None, None, false),
            stored_claim(&crate::sandbox::workspace_pvc_name(7), Some("7"), None, None, false),
        ],
        &titles,
    );
    assert_eq!(
        rows.iter().map(|row| row.conversation_id).collect::<Vec<_>>(),
        vec![7, 9],
        "stable order for the page's rows"
    );
}
