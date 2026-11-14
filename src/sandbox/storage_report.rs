//! Where removed sandbox storage actually goes (SME-521). Removing a
//! conversation's claims frees disk only when their storage class's
//! `reclaimPolicy` is `Delete`: with `Retain` the volume and its data
//! stay, `Released`, until someone deletes it by hand. smelt can't see
//! that from its namespace, since PersistentVolumes and StorageClasses
//! are cluster-wide; the read-only `smelt-storage-reader` ClusterRole
//! (`k8s/smelt-park-rbac.yaml`, `terraform/smelt/rbac.tf`) lets it read
//! both. Without the grant each read is a 403, which reads as
//! "unavailable" and is logged once: smelt never needs these to work.
//!
//! The storage pass logs the class once at startup and the Released
//! count each pass, as a `warn!` when something is wrong; the pods page
//! shows both.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use k8s_openapi::api::core::v1::PersistentVolume;
use k8s_openapi::api::storage::v1::StorageClass;

use super::*;
use crate::api::sandbox::{ClaimClass, ReleasedVolumes, StorageReport};

/// The annotation that marks the cluster's default storage class.
const DEFAULT_CLASS_ANNOTATION: &str = "storageclass.kubernetes.io/is-default-class";

/// How long a volume may sit `Released` before the pass warns: a `Delete`
/// class's provisioner reclaims one in seconds to minutes.
const RELEASED_WARN_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(1);

/// The class a claim with no `storageClassName` gets: the one annotated
/// default, the newest if several are (as the API server picks).
fn default_class(classes: &[StorageClass]) -> Option<&StorageClass> {
    classes
        .iter()
        .filter(|class| {
            class.metadata.annotations.as_ref().and_then(|a| a.get(DEFAULT_CLASS_ANNOTATION)).map(String::as_str)
                == Some("true")
        })
        .max_by_key(|class| class.metadata.creation_timestamp.as_ref().map(|t| t.0.as_second()))
}

/// The class the conversation claims get, `configured` being
/// `SANDBOX_STORAGE_CLASS`, out of the cluster's `classes`.
pub(in crate::sandbox) fn claim_class(configured: Option<&str>, classes: &[StorageClass]) -> ClaimClass {
    let found = |class: &StorageClass| ClaimClass::Found {
        name: class.metadata.name.clone().unwrap_or_default(),
        provisioner: class.provisioner.clone(),
        reclaim_policy: class.reclaim_policy.clone().unwrap_or_else(|| "Delete".to_string()),
        configured: configured.is_some(),
    };
    match configured {
        Some(name) => classes
            .iter()
            .find(|class| class.metadata.name.as_deref() == Some(name))
            .map_or_else(|| ClaimClass::Missing { name: name.to_string() }, found),
        None => default_class(classes).map_or(ClaimClass::NoDefault, found),
    }
}

/// `namespace`'s volumes among `volumes` whose claim is gone and whose
/// disk isn't: phase `Released`, or `Failed` (a reclaim that failed).
pub(in crate::sandbox) fn released_volumes(namespace: &str, volumes: &[PersistentVolume]) -> ReleasedVolumes {
    let mut released = ReleasedVolumes::default();
    for volume in volumes {
        let ours = volume.spec.as_ref().and_then(|spec| spec.claim_ref.as_ref()).and_then(|r| r.namespace.as_deref())
            == Some(namespace);
        let status = volume.status.as_ref();
        let phase = status.and_then(|status| status.phase.as_deref());
        if !ours || !matches!(phase, Some("Released" | "Failed")) {
            continue;
        }
        released.count += 1;
        let capacity: Option<&BTreeMap<String, Quantity>> = volume.spec.as_ref().and_then(|spec| spec.capacity.as_ref());
        let bytes = capacity.and_then(|c| c.get("storage")).and_then(|q| memory_bytes(&q.0)).unwrap_or(0.0);
        released.capacity_bytes += bytes as u64;
        let since = status
            .and_then(|status| status.last_phase_transition_time.as_ref())
            .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0))
            .map(|t| t.naive_utc());
        if let Some(since) = since {
            released.oldest = Some(released.oldest.map_or(since, |oldest: NaiveDateTime| oldest.min(since)));
        }
    }
    released
}

/// What's wrong with `class` for a claim removal to free disk, if
/// anything: the warning the pass logs.
pub(in crate::sandbox) fn class_problem(class: &ClaimClass) -> Option<String> {
    match class {
        ClaimClass::Unavailable => None,
        ClaimClass::NoDefault => Some(
            "the cluster has no default storage class and SANDBOX_STORAGE_CLASS isn't set: sandbox claims can't be bound"
                .to_string(),
        ),
        ClaimClass::Missing { name } => {
            Some(format!("SANDBOX_STORAGE_CLASS names {name}, which the cluster doesn't have: sandbox claims can't be bound"))
        }
        ClaimClass::Found { name, reclaim_policy, .. } if reclaim_policy != "Delete" => Some(format!(
            "sandbox claims use storage class {name}, whose reclaimPolicy is {reclaim_policy}: removing a claim frees no disk; \
             set SANDBOX_STORAGE_CLASS to a class with reclaimPolicy Delete"
        )),
        ClaimClass::Found { .. } => None,
    }
}

/// Whether one of `released` has sat there past `RELEASED_WARN_AFTER`
/// at `now` (UTC).
pub(in crate::sandbox) fn released_overdue(released: &ReleasedVolumes, now: NaiveDateTime) -> bool {
    released.oldest.is_some_and(|oldest| now - oldest > RELEASED_WARN_AFTER)
}

async fn read_claim_class(client: &kube::Client) -> Result<ClaimClass, kube::Error> {
    let classes = Api::<StorageClass>::all(client.clone()).list(&ListParams::default()).await?.items;
    Ok(claim_class(sandbox_storage_class().as_deref(), &classes))
}

async fn read_released(client: &kube::Client) -> Result<ReleasedVolumes, kube::Error> {
    let volumes = Api::<PersistentVolume>::all(client.clone()).list(&ListParams::default()).await?.items;
    Ok(released_volumes(NAMESPACE, &volumes))
}

/// Both reads with `client`, a failure of either reading as unavailable.
pub(in crate::sandbox) async fn storage_report_with(client: &kube::Client) -> StorageReport {
    let (class, released) = tokio::join!(read_claim_class(client), read_released(client));
    StorageReport { class: class.unwrap_or(ClaimClass::Unavailable), released: released.ok() }
}

/// Both reads, for the pods page.
pub(crate) async fn storage_report() -> StorageReport {
    match kube_client() {
        Ok(client) => storage_report_with(&client).await,
        Err(_) => StorageReport { class: ClaimClass::Unavailable, released: None },
    }
}

/// The storage pass's log of these reads: a missing grant is said once,
/// not each pass.
#[derive(Debug, Default)]
pub(crate) struct StorageReportLog {
    said_forbidden: bool,
}

impl StorageReportLog {
    /// Logs a failed read: a 403 once, as the missing grant; anything
    /// else each time.
    fn failed(&mut self, what: &str, error: &kube::Error) {
        if matches!(error, kube::Error::Api(e) if e.code == 403) {
            if !std::mem::replace(&mut self.said_forbidden, true) {
                tracing::info!(
                    "smelt can't read {what} (no smelt-storage-reader grant), so it can't tell whether removed storage frees disk"
                );
            }
        } else {
            tracing::warn!(error = %error, "couldn't read {what}");
        }
    }

    /// Once, at startup: the class the claims get, as a `warn!` when
    /// removing one won't free disk.
    pub(crate) async fn log_class(&mut self) {
        let Ok(client) = kube_client() else { return };
        match read_claim_class(&client).await {
            Ok(class) => match class_problem(&class) {
                Some(problem) => tracing::warn!("{problem}"),
                None => tracing::info!(?class, "sandbox claims' storage class"),
            },
            Err(e) => self.failed("storage classes", &e),
        }
    }

    /// Each pass: how many of this namespace's volumes are Released, as a
    /// `warn!` when one has sat there over a day.
    pub(crate) async fn log_released(&mut self) {
        let Ok(client) = kube_client() else { return };
        match read_released(&client).await {
            Ok(released) if released_overdue(&released, chrono::Utc::now().naive_utc()) => tracing::warn!(
                count = released.count,
                bytes = released.capacity_bytes,
                oldest = ?released.oldest,
                "released sandbox volumes are still holding disk a day on: their storage class doesn't reclaim them"
            ),
            Ok(released) => tracing::info!(count = released.count, bytes = released.capacity_bytes, "released sandbox volumes"),
            Err(e) => self.failed("persistent volumes", &e),
        }
    }
}
