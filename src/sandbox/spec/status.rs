//! Why a pod isn't starting, read off its own status.

use super::*;

/// Why `pod` isn't running yet, from its first failing condition — e.g.
/// "PodScheduled: Unschedulable: 0/1 nodes are available: persistentvolumeclaim
/// … not found." Reported when waiting times out, so the error says why.
pub(in crate::sandbox) fn pod_pending_detail(pod: &Pod) -> Option<String> {
    let conditions = pod.status.as_ref()?.conditions.as_ref()?;
    conditions.iter().find(|c| c.status == "False").map(|c| {
        let reason = c.reason.as_deref().unwrap_or("");
        let message = c.message.as_deref().unwrap_or("");
        [c.type_.as_str(), reason, message]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(": ")
    })
}

/// Container waiting states that never resolve on their own — once a pod's
/// container is in one of these, waiting for `Running` only ends in a
/// timeout that hides the reason.
pub(in crate::sandbox) const FATAL_WAITING_REASONS: [&str; 8] = [
    "CreateContainerError",
    "CreateContainerConfigError",
    "RunContainerError",
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "CrashLoopBackOff",
    // `imagePullPolicy: Never` and the image isn't on the node: never
    // built for these sources, or deleted by the kubelet (SME-121).
    "ErrImageNeverPull",
];
/// What to do about `ErrImageNeverPull` for `image`. Sandbox pods' images
/// are delivered into the node from smelt's host, which the model can't
/// do, so it says to tell the user rather than retry.
fn image_never_pull_advice(image: &str) -> String {
    format!(
        "The cluster's node doesn't have this image, and sandbox pods never pull one. It can't be built \
         or imported from a sandbox, so don't retry create_pod: {}. If the image is meant to come from a \
         registry instead, that's SANDBOX_IMAGE_PULL_POLICY being unset.",
        how_to_build(image)
    )
}

/// The fatal reasons that are about the image, which no restart fixes.
/// Only these count for an init container (the Docker sidecar): a sidecar
/// that crashes while starting is left to its own restarts, as before.
const IMAGE_WAITING_REASONS: [&str; 4] = ["ErrImagePull", "ImagePullBackOff", "InvalidImageName", "ErrImageNeverPull"];

/// The image `pod`'s spec names for its container `name`, as smelt wrote
/// it, or `fallback` (the status's) if the spec has no such container. A
/// status's image comes from the runtime once the container has run: maybe
/// another tag of it, or a bare `sha256:` id (review 2).
fn spec_image(pod: &Pod, name: &str, fallback: &str) -> String {
    let spec = pod.spec.as_ref();
    let containers = spec.map(|s| s.containers.iter()).into_iter().flatten();
    let init = spec.and_then(|s| s.init_containers.as_ref()).into_iter().flatten();
    containers
        .chain(init)
        .find(|c| c.name == name)
        .and_then(|c| c.image.clone())
        .unwrap_or_else(|| fallback.to_string())
}

/// Why `pod` can't start, if one of its containers is stuck in a state that
/// won't recover (see `FATAL_WAITING_REASONS`), or its Docker sidecar (an
/// init container) has no image; `None` while it's still starting normally.
pub(in crate::sandbox) fn pod_startup_failure(pod: &Pod) -> Option<String> {
    let status = pod.status.as_ref()?;
    let init = status.init_container_statuses.iter().flatten().map(|s| (s, &IMAGE_WAITING_REASONS[..]));
    let main = status.container_statuses.iter().flatten().map(|s| (s, &FATAL_WAITING_REASONS[..]));
    init.chain(main).find_map(|(status, fatal)| {
        let waiting = status.state.as_ref()?.waiting.as_ref()?;
        let reason = waiting.reason.as_deref()?;
        fatal.contains(&reason).then(|| {
            let failure = match waiting.message.as_deref().filter(|m| !m.is_empty()) {
                Some(message) => format!("{reason}: {message}"),
                None => reason.to_string(),
            };
            if reason == "ErrImageNeverPull" {
                format!("{failure}. {}", image_never_pull_advice(&spec_image(pod, &status.name, &status.image)))
            } else {
                failure
            }
        })
    })
}
