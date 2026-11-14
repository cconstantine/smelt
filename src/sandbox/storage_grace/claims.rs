//! The storage-grace pass's input (SME-521): this instance's conversation
//! claims in the cluster, grouped by conversation. The claims are what the
//! pass is about, so a claim with no pod row at all is seen too.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;

use super::*;

/// One conversation's claims, as the pass reads them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ClaimSet {
    pub names: Vec<String>,
    /// The oldest `creationTimestamp` (UTC) among the claims not being
    /// deleted: the clock of a conversation that never had a pod.
    pub oldest: Option<NaiveDateTime>,
    /// Every claim carries a deletion timestamp already: nothing left for
    /// the pass to do.
    pub all_deleting: bool,
}

/// This instance's conversation claims, by conversation id.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StoredClaims {
    pub by_conversation: BTreeMap<i64, ClaimSet>,
}

/// `claims` grouped by their conversation label. A claim whose name isn't
/// one of the conversation pair, or whose label doesn't parse, is left
/// out: nothing to group it under (the Pods page's `storage_rows` rule).
pub(crate) fn group_claims(claims: &[PersistentVolumeClaim]) -> StoredClaims {
    let mut by_conversation: BTreeMap<i64, (ClaimSet, bool)> = BTreeMap::new();
    for claim in claims {
        let Some(name) = claim.metadata.name.as_deref() else { continue };
        if conversation_claim_kind(name).is_none() {
            continue;
        }
        let Some(conversation_id) = claim
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(CONVERSATION_LABEL))
            .and_then(|value| value.parse::<i64>().ok())
        else {
            continue;
        };
        let (set, any_live) = by_conversation.entry(conversation_id).or_default();
        set.names.push(name.to_string());
        if claim.metadata.deletion_timestamp.is_some() {
            continue;
        }
        *any_live = true;
        let created = claim
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0))
            .map(|t| t.naive_utc());
        if let Some(created) = created {
            set.oldest = Some(set.oldest.map_or(created, |oldest| oldest.min(created)));
        }
    }
    let by_conversation = by_conversation
        .into_iter()
        .map(|(id, (mut set, any_live))| {
            set.all_deleting = !any_live;
            set.names.sort();
            (id, set)
        })
        .collect();
    StoredClaims { by_conversation }
}

/// This instance's conversation claims in the cluster, with the Pods
/// page's instance-scoped selector (SME-115): a check server's pass never
/// sees production's claims, nor the other way round.
pub(crate) async fn list_stored_claims(pool: &PgPool) -> Result<StoredClaims, String> {
    let instance = db::smelt_instance(pool).await.map_err(|e| e.to_string())?;
    let client = kube_client().map_err(|e| e.to_string())?;
    let selector = kube::api::ListParams::default()
        .labels(&format!("{},{}={}", CONVERSATION_LABEL, INSTANCE_LABEL, instance.id));
    let claims = pvc_api(&client).list(&selector).await.map_err(|e| e.to_string())?.items;
    Ok(group_claims(&claims))
}
