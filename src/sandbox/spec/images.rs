//! A pod's images: names, defaults, pull policy and pull credentials.

use super::*;

/// Who can make `image`, for advice the model passes on to the user: the
/// build script makes only this tree's own image and `docker:29-dind`, and
/// moves `smelt-sandbox:latest` only with `--latest`; any other image is
/// one a setting names, which the script can't help with (review 1, 2).
pub(in crate::sandbox) fn how_to_build(image: &str) -> &'static str {
    let full = full_image_name(image);
    let built_by_script = [OWN_SANDBOX_IMAGE, "docker.io/library/docker:29-dind"].map(full_image_name);
    if built_by_script.contains(&full) {
        "tell the user to build it with scripts/build-sandbox-image.sh, from the checkout smelt runs from."
    } else if full == "docker.io/library/smelt-sandbox:latest" {
        "tell the user to build it with scripts/build-sandbox-image.sh --latest, from the checkout smelt \
         runs from, or to unset SANDBOX_IMAGE."
    } else {
        "SANDBOX_IMAGE or SANDBOX_DOCKER_IMAGE names it, and scripts/build-sandbox-image.sh doesn't make \
         it: tell the user to replace or import that image, or to unset the setting."
    }
}

/// `SANDBOX_IMAGE`, default `OWN_SANDBOX_IMAGE` (the image named after
/// this build's agent sources) — same pattern as `default_memory_limit`.
/// The custom image `scripts/build-sandbox-image.sh` builds and delivers
/// with no registry involved (see SME-17) — its own `ENTRYPOINT` is the
/// sandbox agent, which is what makes the agent the pod's real PID 1
/// rather than something injected and launched after the fact. The
/// fully-qualified name (not just `smelt-sandbox:src-…`) matches exactly
/// what `ctr images import` registers the image as — confirmed by spike,
/// not assumed. A server with no setting (the dev server) runs its own
/// sources' image, so it can't drift onto an older agent the way a
/// `:latest` that only a manual rebuild moved did (SME-121); a node without
/// that image fails `create_pod` with `ErrImageNeverPull` and the command
/// that builds it. `scripts/check.sh`, `browser-tier`, `check-server` and
/// CI set `SANDBOX_IMAGE` to the working tree's image
/// (`scripts/sandbox-image-ref`, SME-102), and tests that build their own
/// pod specs read it here too.
pub(crate) fn default_sandbox_image() -> String {
    sandbox_image_from(std::env::var("SANDBOX_IMAGE").ok())
}

/// The image for a `SANDBOX_IMAGE` of `setting`: it, unless unset or empty.
pub(in crate::sandbox) fn sandbox_image_from(setting: Option<String>) -> String {
    setting
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OWN_SANDBOX_IMAGE.to_string())
}

/// The sandbox image named after the agent sources this server was built
/// from, `docker.io/library/smelt-sandbox:src-<hash>` (`build.rs`, the same
/// name `scripts/sandbox-image-ref` prints).
pub(crate) const OWN_SANDBOX_IMAGE: &str = env!("SMELT_SANDBOX_IMAGE");
/// `SANDBOX_DOCKER_IMAGE`, default `"docker.io/library/docker:29-dind"` —
/// see `default_sandbox_image`. Delivered into the node like the sandbox
/// image, and only its `dockerd` is used: never its entrypoint.
pub(in crate::sandbox) fn default_docker_image() -> String {
    std::env::var("SANDBOX_DOCKER_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "docker.io/library/docker:29-dind".to_string())
}

/// The pull policies Kubernetes accepts on a container's image.
const IMAGE_PULL_POLICIES: [&str; 3] = ["Never", "IfNotPresent", "Always"];

/// `SANDBOX_IMAGE_PULL_POLICY`, default `Never` — the policy both of a pod's
/// containers fetch their image with, so one setting covers the pair.
/// `Never` is right for the images `scripts/build-sandbox-image.sh` delivers
/// straight into the node's containerd: they're in no registry a pod could
/// reach, and a real pull would only fail. A deployment that publishes its
/// images to a registry sets `IfNotPresent` (or `Always`) instead — see
/// `image_pull_policy_from` for how the setting is read, and docs/setup.md
/// for the pull credentials a private registry needs.
pub(in crate::sandbox) fn default_image_pull_policy() -> &'static str {
    let setting = std::env::var("SANDBOX_IMAGE_PULL_POLICY").ok().map(|s| s.trim().to_string());
    let policy = image_pull_policy_from(setting.as_deref());
    // A typo here otherwise shows up as a pod that simply won't start, so
    // say what was ignored — the same "fall back and log" posture as
    // SMELT_PREVIEW_URL's invalid value.
    if let Some(ignored) = setting.filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(policy)) {
        tracing::warn!(
            "SANDBOX_IMAGE_PULL_POLICY={ignored} isn't a pull policy, so it's ignored: use Never, \
             IfNotPresent or Always"
        );
    }
    policy
}

/// The pull policy a `SANDBOX_IMAGE_PULL_POLICY` of `setting` asks for, in
/// Kubernetes' own spelling whatever case it was written in — or `Never`,
/// when it's unset, empty, or not one of `IMAGE_PULL_POLICIES`.
pub(in crate::sandbox) fn image_pull_policy_from(setting: Option<&str>) -> &'static str {
    setting
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| IMAGE_PULL_POLICIES.iter().find(|p| p.eq_ignore_ascii_case(s)).copied())
        .unwrap_or("Never")
}

/// `SANDBOX_IMAGE_PULL_SECRET` — the names, comma-separated, of the pull
/// credentials a sandbox pod fetches its images with. A registry that asks to
/// be authenticated first (GHCR's private packages among them) otherwise gives
/// the pod nothing to present, and both pulls fail with `pull access denied`.
/// The secrets themselves are `kubernetes.io/dockerconfigjson` objects in
/// `smelt-park`, where the pods run — see docs/setup.md. Unset, as it is for
/// images built straight into the node, and the pod pulls however the
/// namespace's own service account does, which is how it has always worked.
pub(in crate::sandbox) fn default_image_pull_secrets() -> Option<Vec<LocalObjectReference>> {
    let secrets = image_pull_secrets_from(std::env::var("SANDBOX_IMAGE_PULL_SECRET").ok().as_deref());
    (!secrets.is_empty()).then_some(secrets)
}

/// The pull credentials a `SANDBOX_IMAGE_PULL_SECRET` of `setting` names: one
/// per comma-separated entry, spaces trimmed, blanks dropped. Empty when the
/// setting names nothing at all.
pub(in crate::sandbox) fn image_pull_secrets_from(setting: Option<&str>) -> Vec<LocalObjectReference> {
    setting
        .into_iter()
        .flat_map(|setting| setting.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| LocalObjectReference {
            name: name.to_string(),
            ..Default::default()
        })
        .collect()
}
