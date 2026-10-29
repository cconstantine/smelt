//! Who a cluster object belongs to: the conversation and instance labels.

use super::*;

/// Label naming a conversation, on its Docker data PVC and on every pod
/// that mounts that PVC.
pub(crate) const CONVERSATION_LABEL: &str = "smelt/conversation";

/// Label naming the conversation that *created* a model volume, on its claim
/// (SME-239). Not `smelt/conversation`: every conversation that mounts the
/// volume mounts the one claim, so the sweep needs the creator's id in
/// particular — that's the conversation whose deletion takes the volume.
pub(in crate::sandbox) const AGENT_VOLUME_LABEL: &str = "smelt/agent-volume-owner";

/// Label naming the database an object belongs to: its `smelt_instance`
/// id (SME-115). Every smelt server shares the namespace and names its
/// objects after its own database's ids, so this is what tells one
/// server's `sandbox-workspace-7` from another's.
pub(crate) const INSTANCE_LABEL: &str = "smelt/instance";

/// The instance tests' own ephemeral pods carry: they have no database.
#[cfg(test)]
pub(crate) const TEST_INSTANCE: &str = "smelt-tests";

/// The instance of the tests' own pods, as a database's would read.
#[cfg(test)]
pub(crate) fn test_instance() -> db::SmeltInstance {
    db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: false }
}

/// Whose a cluster object is, from one server's side (SME-115).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Labelled with this server's instance.
    Ours,
    /// No instance label: made before SME-115. Only adoption touches it.
    Unlabelled,
    /// Another database's.
    Foreign,
}

/// Whose `meta`'s object is, for a server of `instance`. Only `Ours` may
/// be deleted, reused or mounted.
pub fn ownership(meta: &ObjectMeta, instance: &str) -> Ownership {
    match meta.labels.as_ref().and_then(|labels| labels.get(INSTANCE_LABEL)) {
        None => Ownership::Unlabelled,
        // An empty instance (never read) is nobody's.
        Some(label) if !instance.is_empty() && label == instance => Ownership::Ours,
        Some(_) => Ownership::Foreign,
    }
}

/// Whether a pod counts as this database's for reading it and for keeping
/// its record open: ours, or one from before SME-115 while this database
/// owns those (adoption missed it: it may still be ours). Another
/// database's pod reads as absent.
pub fn counts_as_ours(meta: &ObjectMeta, instance: &db::SmeltInstance) -> bool {
    match ownership(meta, &instance.id) {
        Ownership::Ours => true,
        Ownership::Unlabelled => instance.owns_unlabelled,
        Ownership::Foreign => false,
    }
}

/// The labels every object a server of `instance` creates carries.
pub(in crate::sandbox) fn instance_labels(instance: &str) -> std::collections::BTreeMap<String, String> {
    [(INSTANCE_LABEL.to_string(), instance.to_string())].into()
}
