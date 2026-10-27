//! Mount paths, the names a pod's shared volumes go by, and their pairs.

use super::*;

/// The sandbox user's home directory — fixed by Phase 1's `useradd -m
/// sandbox` in `docker/sandbox/Dockerfile`, not derived from anything at
/// runtime.
pub(in crate::sandbox) const SANDBOX_HOME: &str = "/home/sandbox";
/// Expands a leading `~` in a user-typed mount path to the sandbox user's
/// home directory — smelt's own expansion, not something deferred to the
/// pod spec (Kubernetes' `volumeMounts.mountPath` has no concept of `~`).
/// Only a bare `~` or a `~/...` prefix expands, matching ordinary shell
/// semantics for the single-user case (no `~otheruser` support — there's
/// only ever one sandbox user). Anything else passes through unchanged.
/// A volume's mount path is mounted into every sandbox pod, and a container
/// can't start with a relative (or root, or `..`-escaping) mount
/// destination — one bad volume would stop every new sandbox. Checked after
/// `resolve_mount_path` expands `~`.
pub(in crate::sandbox) fn validate_mount_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("{path:?} must be an absolute path (start with / or ~)"));
    }
    if path == "/" {
        return Err("a volume can't be mounted over the root directory".to_string());
    }
    if path.split('/').any(|part| part == "..") {
        return Err(format!("{path:?} must not contain .."));
    }
    Ok(())
}

pub(in crate::sandbox) fn resolve_mount_path(path: &str) -> String {
    if path == "~" {
        SANDBOX_HOME.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{SANDBOX_HOME}/{rest}")
    } else {
        path.to_string()
    }
}

/// The PVC name backing a given `sandbox_volumes` row — deterministic,
/// same `sandbox-{id}` naming convention `pod_name` already uses.
pub(in crate::sandbox) fn sandbox_volume_pvc_name(volume_id: i64) -> String {
    format!("sandbox-volume-{volume_id}")
}

/// Every configured volume's `Volume`/`VolumeMount` pair for a pod spec —
/// pulled out as a pure function over an already-fetched list, same
/// testability reason as `check_pod_guard`/`resolve_pod_id`. Every pod
/// gets every volume, unconditionally — see the idea's "not something
/// chosen per conversation."
pub(in crate::sandbox) fn volume_mounts_for(volumes: &[db::SandboxVolume]) -> (Vec<Volume>, Vec<VolumeMount>) {
    volumes
        .iter()
        .map(|v| {
            let name = format!("volume-{}", v.id);
            let volume = Volume {
                name: name.clone(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: sandbox_volume_pvc_name(v.id),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mount = VolumeMount {
                name,
                mount_path: v.mount_path.clone(),
                ..Default::default()
            };
            (volume, mount)
        })
        .unzip()
}

/// Shared by the sandbox and Docker sidecar containers: projects that
/// bind-mount host paths go here, and terminals start here.
pub const WORKSPACE_DIR: &str = "/workspace";
pub(in crate::sandbox) const WORKSPACE_VOLUME: &str = "workspace";
/// Holds only dockerd's socket, never dockerd's own `/run/docker`: that
/// holds containerd's state and pid file, and a copy shared with the
/// sandbox survives a sidecar restart and makes the next dockerd fail
/// (SME-33's spike).
pub(in crate::sandbox) const DOCKER_SOCK_DIR: &str = "/run/docker-sock";
pub(in crate::sandbox) const DOCKER_SOCK_VOLUME: &str = "docker-sock";
pub(in crate::sandbox) const DOCKER_HOST: &str = "unix:///run/docker-sock/docker.sock";
pub(in crate::sandbox) const DOCKER_DATA_VOLUME: &str = "docker-data";
/// The `docker` group's GID in the sandbox image
/// (docker/sandbox/Dockerfile); dockerd gives it the socket.
pub(in crate::sandbox) const DOCKER_GID: u32 = 2375;
/// The sandbox user's uid:gid, pinned in the image
/// (docker/sandbox/Dockerfile); the Docker sidecar gives it /workspace.
pub(in crate::sandbox) const SANDBOX_OWNER: &str = "1000:1000";
pub(in crate::sandbox) fn mount(volume: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: volume.to_string(),
        mount_path: path.to_string(),
        ..Default::default()
    }
}

pub(in crate::sandbox) fn empty_dir_volume(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

/// /workspace: the conversation's own claim, so a new pod finds it as the
/// last one left it (SME-32).
pub(in crate::sandbox) fn workspace_volume(storage: &PodStorage) -> Volume {
    match storage {
        PodStorage::Conversation(conversation_id) => Volume {
            name: WORKSPACE_VOLUME.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_pvc_name(*conversation_id),
                ..Default::default()
            }),
            ..Default::default()
        },
        #[cfg(test)]
        PodStorage::Ephemeral => empty_dir_volume(WORKSPACE_VOLUME),
    }
}

pub(in crate::sandbox) fn docker_data_volume(storage: &PodStorage) -> Volume {
    match storage {
        PodStorage::Conversation(conversation_id) => Volume {
            name: DOCKER_DATA_VOLUME.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: docker_pvc_name(*conversation_id),
                ..Default::default()
            }),
            ..Default::default()
        },
        #[cfg(test)]
        PodStorage::Ephemeral => empty_dir_volume(DOCKER_DATA_VOLUME),
    }
}

/// A model-created volume's claim, `agent-volume-<id>` (SME-239) — not a
/// global volume's `sandbox-volume-<id>`: `agent_volumes` and `sandbox_volumes`
/// count from their own sequences, and one pod can mount both, so the two names
/// must differ for the same number.
pub(in crate::sandbox) fn agent_volume_pvc_name(volume_id: i64) -> String {
    format!("agent-volume-{volume_id}")
}

/// Every volume one conversation mounts, as its `Volume`/`VolumeMount` pair —
/// the same pure shape as `volume_mounts_for`, over one conversation's mounts
/// instead of every configured volume: a model volume reaches only the
/// conversations that hold it (SME-239).
pub(in crate::sandbox) fn agent_volume_mounts_for(volumes: &[db::AgentVolume]) -> (Vec<Volume>, Vec<VolumeMount>) {
    volumes
        .iter()
        .map(|v| {
            let name = agent_volume_pvc_name(v.id);
            let volume = Volume {
                name: name.clone(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: name.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mount = VolumeMount {
                name,
                mount_path: v.mount_path.clone(),
                ..Default::default()
            };
            (volume, mount)
        })
        .unzip()
}

/// A path with its `.` segments, repeated `/` and trailing `/` gone and
/// otherwise unchanged — a relative path included, so `validate_mount_path`
/// stays the one that refuses it. Kubernetes takes a mount path exactly as
/// given, and smelt compares one volume's path against every other one the
/// same pod mounts, so what's stored has to be the one canonical form:
/// `/data//cache/.` and `/data/cache` are one place in the pod, not two
/// claims sitting on each other (SME-239).
pub(in crate::sandbox) fn normalize_mount_path(path: &str) -> String {
    if !path.starts_with('/') {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// Directories a model volume can't be mounted at or under. The first five
/// are smelt's own places in the pod: the conversation's `/workspace` claim,
/// over which a volume would hide the checkout every terminal starts in; the
/// git key directory, whose value this keeps as a literal so the module stays
/// free of `crate::git` — a test in `sandbox::tests` keeps the two equal; the
/// Docker socket directory, over which a volume would break every command the
/// pod runs through its sidecar, the one the design leaves out of its list and
/// this ticket's plan adds; and dockerd's own `/run/docker` and containerd's
/// `/run/containerd` — a model volume mounts in the sidecar too (pods.rs's
/// shared mounts), and over either it would hold the runtime state and pid
/// files that outlive the sidecar's automatic restart and make the next
/// dockerd fail: the same SME-33 finding that keeps `DOCKER_SOCK_DIR` down to
/// the socket alone, widened to the directories a model could pick (B-F4).
/// The rest are the system directories the design refuses (SME-239).
const RESERVED_MODEL_MOUNT_DIRS: &[&str] = &[
    WORKSPACE_DIR,
    "/etc/smelt",
    DOCKER_SOCK_DIR,
    "/run/docker",
    "/run/containerd",
    "/etc",
    "/usr",
    "/bin",
    "/lib",
    "/proc",
    "/sys",
    "/dev",
    "/var/lib/docker",
];

/// The reserved directory `path` is at or under, the narrowest when several
/// match: `/etc/smelt/keys` is refused as `/etc/smelt`, not `/etc`, because
/// that's the reason it's refused.
pub(in crate::sandbox) fn reserved_mount_path(path: &str) -> Option<&'static str> {
    RESERVED_MODEL_MOUNT_DIRS
        .iter()
        .filter(|dir| path == **dir || path.starts_with(&format!("{dir}/")))
        .max_by_key(|dir| dir.len())
        .copied()
}

/// The mount path a model's `create_volume` has stored: `~` expanded,
/// normalized, then the checks a global volume's path already gets, then the
/// reserved-directory rule only a model volume needs — a global volume is a
/// user's choice, a model volume the model's. The error is the sentence the
/// model gets.
pub(crate) fn model_volume_mount_path(path: &str) -> Result<String, String> {
    let path = normalize_mount_path(&resolve_mount_path(path));
    validate_mount_path(&path)?;
    if let Some(dir) = reserved_mount_path(&path) {
        return Err(format!("{path:?} is at or under {dir:?}, which a volume can't be mounted over"));
    }
    Ok(path)
}
