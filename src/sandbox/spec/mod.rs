//! Pod and claim specs: names, defaults, mounts, limits and the pod spec
//! itself. Pure: nothing here talks to the cluster.
//!
//! Split from one `spec.rs` under SME-239: it sat 2,591 B under the 40,000 B
//! file ceiling, and this ticket's claim builders, mount mapping and
//! reserved-path checks land inside the margin that left — the SME-179 shape,
//! where review fixes cross the ceiling. Each submodule keeps its items'
//! visibility: `pub(super)` read "visible in `crate::sandbox`" from one level
//! up, so what was `pub(super)` there is `pub(in crate::sandbox)` here, and the
//! re-exports below are what keep `spec::X` inside sandbox and `sandbox::X`
//! outside it meaning what they meant before.

mod images;
mod limits;
mod mounts;
mod ownership;
mod pods;
mod pvcs;
mod status;

// What the submodules' own `use super::*` reaches: this module's re-exports
// of each other, and everything `sandbox` itself imports — the kube types,
// `db`, the namespace constants — which a child module sees through its
// ancestors exactly as `spec.rs` saw them through its own `use super::*`.
use super::*;

pub use limits::PodLimitOverrides;
pub use mounts::WORKSPACE_DIR;
pub use ownership::{counts_as_ours, ownership, Ownership};
pub use pods::{pod_name, DockerSidecar, PodStorage};

pub(crate) use images::default_sandbox_image;
#[cfg(test)]
pub(crate) use images::OWN_SANDBOX_IMAGE;
pub(crate) use mounts::model_volume_mount_path;
pub(crate) use ownership::INSTANCE_LABEL;
#[cfg(test)]
pub(crate) use ownership::{test_instance, TEST_INSTANCE};
pub(crate) use pods::pod_delete_params;
pub(crate) use pvcs::workspace_pvc_name;

pub(in crate::sandbox) use images::{default_docker_image, default_image_pull_policy, default_image_pull_secrets, how_to_build};
#[cfg(test)]
pub(in crate::sandbox) use images::{image_pull_policy_from, image_pull_secrets_from, sandbox_image_from};
pub(in crate::sandbox) use limits::{memory_only, pod_container_limits, running_wait_timeout};
#[cfg(test)]
pub(in crate::sandbox) use limits::{default_docker_memory_limit, default_memory_limit};
pub(in crate::sandbox) use mounts::{agent_volume_mounts_for, agent_volume_pvc_name, docker_data_volume, empty_dir_volume, mount, resolve_mount_path, sandbox_volume_pvc_name, validate_mount_path, volume_mounts_for, workspace_volume, DOCKER_DATA_VOLUME, DOCKER_GID, DOCKER_HOST, DOCKER_SOCK_DIR, DOCKER_SOCK_VOLUME, SANDBOX_OWNER, WORKSPACE_VOLUME};
pub(in crate::sandbox) use ownership::{instance_labels, AGENT_VOLUME_LABEL, CONVERSATION_LABEL};
pub(in crate::sandbox) use pods::{build_pod_spec, sandbox_delete_params};
#[cfg(test)]
pub(in crate::sandbox) use pods::{immediate_delete_params, START_DOCKERD_SCRIPT};
pub(in crate::sandbox) use pvcs::{build_agent_volume_pvc_spec, build_volume_pvc_spec, conversation_pvc_specs, docker_pvc_name};
#[cfg(test)]
pub(in crate::sandbox) use pvcs::{build_conversation_pvc_spec, default_docker_storage_size, default_volume_storage_size};
pub(in crate::sandbox) use status::{pod_pending_detail, pod_startup_failure};
