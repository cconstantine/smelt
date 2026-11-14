//! The one storage-grace rule (SME-521): from what the database says about
//! a conversation, whether its stored claims are in use, kept, or removed
//! at a time, and under which grace. The pass and the Pods page both ask
//! it, so the page can't say one thing while the pass does another. Pure:
//! its tests need no database.

use chrono::NaiveDateTime;

pub(crate) use crate::api::pods::{GraceClass, StorageFate};
pub(crate) use crate::api::sandbox::GraceSettings;

/// One conversation's grace inputs, read from the database
/// (`db::storage_grace_inputs`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GraceInput {
    /// Its latest pod row; `None` when it never had one.
    pub latest: Option<LatestPod>,
    /// A sub-agent whose `agents.state` is `finished`, `stopped` or
    /// `failed`.
    pub agent_done: bool,
}

/// The fields of a conversation's latest `sandbox_pods` row the rule reads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LatestPod {
    pub pod_id: i64,
    /// `None` while the pod is live.
    pub terminated_at: Option<NaiveDateTime>,
    /// `true` for the user's Stop, `false` for any other end, `None` for a
    /// live row or one ended before the column existed.
    pub stopped_by_user: Option<bool>,
    /// The row carries a quiet stop's mark (`quietly_stopped_at`).
    pub quiet: bool,
}

/// The fate of a conversation's stored claims, `oldest_claim` being the
/// creation time of its oldest claim (on the database's clock), the clock
/// of a conversation that never had a pod. In order: a live latest row is
/// in use; a finished sub-agent gets the agent grace, whatever ended its
/// pod; then the user's Stop, a pre-SME-521 end that can't be told (the
/// Stop's grace, or the default when the Stop's is 0), and any other end
/// the default. A class whose days are 0 is kept; a default of 0 turns
/// automatic removal off for every class.
pub(crate) fn fate(input: &GraceInput, oldest_claim: NaiveDateTime, settings: &GraceSettings) -> StorageFate {
    let (clock, class) = match &input.latest {
        Some(LatestPod { terminated_at: None, .. }) => return StorageFate::Live,
        Some(pod) => {
            let class = if input.agent_done {
                GraceClass::Agent
            } else {
                match pod.stopped_by_user {
                    Some(true) => GraceClass::UserStop,
                    Some(false) => GraceClass::Default,
                    None if pod.quiet => GraceClass::Default,
                    None => GraceClass::Unknown,
                }
            };
            (pod.terminated_at.unwrap_or(oldest_claim), class)
        }
        None if input.agent_done => (oldest_claim, GraceClass::Agent),
        None => (oldest_claim, GraceClass::NoPod),
    };
    if settings.default_days == 0 {
        return StorageFate::Kept;
    }
    let days = match class {
        GraceClass::Agent => settings.agent_days,
        GraceClass::UserStop => settings.user_days,
        GraceClass::Unknown if settings.user_days == 0 => settings.default_days,
        GraceClass::Unknown => settings.user_days,
        GraceClass::Default | GraceClass::NoPod => settings.default_days,
    };
    if days == 0 {
        return StorageFate::Kept;
    }
    StorageFate::RemovesAt { at: clock + chrono::Duration::days(i64::from(days)), days, class }
}
