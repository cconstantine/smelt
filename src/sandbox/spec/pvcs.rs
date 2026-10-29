//! The claims behind a pod: conversation and volume PVCs, and their sizes.

use super::*;

/// The conversation's Docker data PVC, `sandbox-docker-<id>`.
pub(crate) fn docker_pvc_name(conversation_id: i64) -> String {
    format!("{DOCKER_PVC_PREFIX}{conversation_id}")
}

/// The conversation's /workspace PVC, `sandbox-workspace-<id>` (SME-32).
pub(crate) fn workspace_pvc_name(conversation_id: i64) -> String {
    format!("{WORKSPACE_PVC_PREFIX}{conversation_id}")
}

/// The two claims' name prefixes — the single place they're spelled, so
/// a view that tells the pair's claims apart can't drift from the names
/// these make (SME-253 review 1, finding 5). Neither shares a prefix
/// with the generic `sandbox-volume-<id>` or `agent-volume-<id>` names.
const WORKSPACE_PVC_PREFIX: &str = "sandbox-workspace-";
const DOCKER_PVC_PREFIX: &str = "sandbox-docker-";

/// Which claim of the conversation pair a claim name is (SME-253), the
/// decision behind the stored-volumes view's rows. `None` for anything
/// else — belt after the label guard, since agent volumes and the global
/// sandbox volumes share no prefix with these.
pub(crate) fn conversation_claim_kind(name: &str) -> Option<crate::api::pods::StorageKind> {
    if name.starts_with(WORKSPACE_PVC_PREFIX) {
        Some(crate::api::pods::StorageKind::Workspace)
    } else if name.starts_with(DOCKER_PVC_PREFIX) {
        Some(crate::api::pods::StorageKind::Docker)
    } else {
        None
    }
}

/// `SANDBOX_DOCKER_STORAGE_SIZE`, default `"20Gi"` — see
/// `default_memory_limit`. Each conversation's Docker data PVC requests
/// this much.
pub(in crate::sandbox) fn default_docker_storage_size() -> String {
    std::env::var("SANDBOX_DOCKER_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "20Gi".to_string())
}

/// `SANDBOX_WORKSPACE_STORAGE_SIZE`, default `"20Gi"` — see
/// `default_memory_limit`. Each conversation's /workspace PVC requests
/// this much.
pub(in crate::sandbox) fn default_workspace_storage_size() -> String {
    std::env::var("SANDBOX_WORKSPACE_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "20Gi".to_string())
}

/// A conversation's claims: its Docker data and its /workspace (SME-32),
/// both kept across its pods and deleted with it. Each comes with its
/// name, so a caller needn't read it back out of the spec's `Option`.
pub(in crate::sandbox) fn conversation_pvc_specs(conversation_id: i64, instance: &str) -> [(String, PersistentVolumeClaim); 2] {
    [
        (docker_pvc_name(conversation_id), default_docker_storage_size()),
        (workspace_pvc_name(conversation_id), default_workspace_storage_size()),
    ]
    .map(|(name, size)| {
        let spec = build_conversation_pvc_spec(name.clone(), conversation_id, size, instance);
        (name, spec)
    })
}

pub(in crate::sandbox) fn build_conversation_pvc_spec(
    name: String,
    conversation_id: i64,
    size: String,
    instance: &str,
) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert("storage".to_string(), Quantity(size));
    let mut labels = instance_labels(instance);
    labels.insert(
        CONVERSATION_LABEL.to_string(),
        conversation_id.to_string(),
    );

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// --- Generic volumes ---

/// `SANDBOX_VOLUME_STORAGE_SIZE`, default `"10Gi"` — same pattern as
/// `default_memory_limit`. Every generic volume's PVC requests this much
/// capacity; not currently configurable per volume (nothing in
/// SME-17 calls for that), just
/// a documented fixed default.
pub(in crate::sandbox) fn default_volume_storage_size() -> String {
    std::env::var("SANDBOX_VOLUME_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "10Gi".to_string())
}

pub(in crate::sandbox) fn build_volume_pvc_spec(volume_id: i64, instance: &str) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert(
        "storage".to_string(),
        Quantity(default_volume_storage_size()),
    );

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(sandbox_volume_pvc_name(volume_id)),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some(instance_labels(instance)),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A model-created volume's claim: one per volume, mounted by every pod whose
/// conversation holds it. RWO across pods is what smelt's single-node k3s
/// requires (the design's spike S2); `smelt/instance` keeps another
/// deployment's same-numbered volume from ever being mounted (SME-115), and
/// `smelt/agent-volume-owner` names the creator for the startup sweep
/// (SME-239).
pub(in crate::sandbox) fn build_agent_volume_pvc_spec(
    volume_id: i64,
    owner_conversation_id: i64,
    instance: &str,
) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert(
        "storage".to_string(),
        Quantity(default_volume_storage_size()),
    );
    let mut labels = instance_labels(instance);
    labels.insert(AGENT_VOLUME_LABEL.to_string(), owner_conversation_id.to_string());

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(agent_volume_pvc_name(volume_id)),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
