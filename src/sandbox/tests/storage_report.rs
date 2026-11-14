//! SME-521's storage report: which class the conversation claims get and
//! whether removing one frees disk, and the namespace's volumes left
//! Released. Pure reads of API objects, and one live read.

use super::*;
use crate::api::sandbox::{ClaimClass, ReleasedVolumes};
use crate::sandbox::storage_report::{
    claim_class, class_problem, released_overdue, released_volumes, storage_report_with,
};
use k8s_openapi::api::core::v1::PersistentVolume;
use k8s_openapi::api::storage::v1::StorageClass;

fn class(json: serde_json::Value) -> StorageClass {
    serde_json::from_value(json).expect("a storage class")
}

fn volume(json: serde_json::Value) -> PersistentVolume {
    serde_json::from_value(json).expect("a persistent volume")
}

fn classes() -> Vec<StorageClass> {
    vec![
        class(serde_json::json!({
            "metadata": {"name": "local-path", "creationTimestamp": "2025-01-01T00:00:00Z",
                         "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}},
            "provisioner": "rancher.io/local-path", "reclaimPolicy": "Delete",
        })),
        class(serde_json::json!({
            "metadata": {"name": "nfs-keep"},
            "provisioner": "nfs.csi.k8s.io", "reclaimPolicy": "Retain",
        })),
    ]
}

#[test]
fn test_unconfigured_claims_get_the_default_class() {
    let found = claim_class(None, &classes());
    assert_eq!(
        found,
        ClaimClass::Found {
            name: "local-path".to_string(),
            provisioner: "rancher.io/local-path".to_string(),
            reclaim_policy: "Delete".to_string(),
            configured: false,
        }
    );
    assert_eq!(class_problem(&found), None);
}

#[test]
fn test_the_newest_of_two_default_classes_wins() {
    let mut classes = classes();
    classes.push(class(serde_json::json!({
        "metadata": {"name": "newer", "creationTimestamp": "2026-01-01T00:00:00Z",
                     "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}},
        "provisioner": "example.com/newer",
    })));
    let ClaimClass::Found { name, reclaim_policy, .. } = claim_class(None, &classes) else { panic!("no class") };
    // An unset reclaimPolicy is the API's default, Delete.
    assert_eq!((name.as_str(), reclaim_policy.as_str()), ("newer", "Delete"));
}

#[test]
fn test_a_configured_retain_class_is_a_problem() {
    let found = claim_class(Some("nfs-keep"), &classes());
    assert!(matches!(&found, ClaimClass::Found { configured: true, reclaim_policy, .. } if reclaim_policy == "Retain"));
    let problem = class_problem(&found).expect("a Retain class is a problem");
    assert!(problem.contains("nfs-keep") && problem.contains("Retain"), "{problem}");
}

/// Review 1, B-L1: a class with no provisioner (static volumes) has no
/// one to reclaim a released volume, whatever its reclaimPolicy says.
#[test]
fn test_a_class_with_no_provisioner_is_a_problem() {
    let mut classes = classes();
    classes.push(class(serde_json::json!({
        "metadata": {"name": "static"},
        "provisioner": "kubernetes.io/no-provisioner", "reclaimPolicy": "Delete",
    })));
    let found = claim_class(Some("static"), &classes);
    let problem = class_problem(&found).expect("a class with no provisioner is a problem");
    assert!(problem.contains("static") && problem.contains("no-provisioner"), "{problem}");
}

#[test]
fn test_a_missing_class_or_no_default_is_a_problem() {
    let missing = claim_class(Some("gone"), &classes());
    assert_eq!(missing, ClaimClass::Missing { name: "gone".to_string() });
    assert!(class_problem(&missing).is_some_and(|p| p.contains("gone")));
    let none = claim_class(None, &classes()[1..]);
    assert_eq!(none, ClaimClass::NoDefault);
    assert!(class_problem(&none).is_some());
    assert_eq!(class_problem(&ClaimClass::Unavailable), None);
}

fn pv(namespace: &str, phase: &str, size: &str, since: &str) -> PersistentVolume {
    volume(serde_json::json!({
        "metadata": {"name": format!("pvc-{namespace}-{phase}-{since}")},
        "spec": {"capacity": {"storage": size}, "claimRef": {"namespace": namespace, "name": "x"}},
        "status": {"phase": phase, "lastPhaseTransitionTime": since},
    }))
}

#[test]
fn test_released_and_failed_volumes_of_the_namespace_are_counted() {
    let volumes = vec![
        pv("smelt-park", "Released", "20Gi", "2026-11-10T00:00:00Z"),
        pv("smelt-park", "Failed", "10Gi", "2026-11-12T00:00:00Z"),
        pv("smelt-park", "Bound", "20Gi", "2026-11-01T00:00:00Z"),
        pv("other", "Released", "20Gi", "2026-11-01T00:00:00Z"),
        volume(serde_json::json!({"metadata": {"name": "unclaimed"}, "status": {"phase": "Available"}})),
    ];
    let released = released_volumes("smelt-park", &volumes);
    let oldest = chrono::NaiveDate::from_ymd_opt(2026, 11, 10).and_then(|d| d.and_hms_opt(0, 0, 0));
    assert_eq!(released, ReleasedVolumes { count: 2, capacity_bytes: 30 << 30, oldest });
}

#[test]
fn test_a_volume_released_over_a_day_ago_is_overdue() {
    let at = |day: u32, hour: u32| chrono::NaiveDate::from_ymd_opt(2026, 11, day).and_then(|d| d.and_hms_opt(hour, 0, 0));
    let released = ReleasedVolumes { count: 1, capacity_bytes: 1, oldest: at(10, 0) };
    assert!(!released_overdue(&released, at(10, 23).expect("a time")));
    assert!(released_overdue(&released, at(11, 1).expect("a time")));
    assert!(!released_overdue(&ReleasedVolumes::default(), at(30, 0).expect("a time")));
}

/// The test cluster's `park` account has the `smelt-storage-reader`
/// grant (`k8s/smelt-storage-reader.yaml`, which `k3s-bootstrap` applies), and k3s's default class is
/// `local-path` with `Delete`.
#[tokio::test]
async fn test_the_test_cluster_reports_its_default_class_and_released_volumes() {
    let client = test_client().await;
    let report = storage_report_with(&client).await;
    assert!(
        matches!(&report.class, ClaimClass::Found { name, reclaim_policy, configured: false, .. }
            if name == "local-path" && reclaim_policy == "Delete"),
        "{report:?}"
    );
    assert!(report.released.is_some(), "{report:?}");
}
