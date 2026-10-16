//! How many model requests each provider is sent at once (SME-162).
//!
//! A provider with a max concurrent requests has a pool of that many
//! slots, kept here by provider id. A model call looks its provider's
//! pool up when it starts (`Endpoint::slot`, `anthropic::stream`) and
//! holds a slot from before it sends until it ends, so a request over the
//! limit waits in smelt instead of queueing, silently, on a busy server
//! until it times out. The pools are in memory, set from the column and
//! the server's report (`resolved_max`) by whatever changes either, and by
//! every endpoint build, so a restart rebuilds them as they're used.
//!
//! A change of limit applies from each running turn's next model call,
//! not its next turn (SME-162 review 1): the old pool is closed, so a call
//! waiting on it looks again and waits on the new one, while calls
//! already holding its slots finish on them.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::NaiveDateTime;
use tokio::sync::Semaphore;

/// A provider's limit as of a version of its row, and its slots.
struct Limit {
    /// The row's `updated_at` it was set from: a set from an older row (a
    /// turn that read it before a save) is ignored (SME-162 review 1).
    version: NaiveDateTime,
    /// The count and its slots; `None` for no limit.
    pool: Option<(u32, Arc<Semaphore>)>,
}

static LIMITS: LazyLock<Mutex<HashMap<i64, Limit>>> = LazyLock::new(Default::default);

/// Sets how many requests at once a provider is sent, `None` for no
/// limit, as of `version` (its row's `updated_at`); a set from an older
/// version than the last is ignored. The same count keeps its pool;
/// another count, or none, closes it, so the calls waiting on it look
/// again (`current`).
pub fn set(provider_id: i64, version: NaiveDateTime, max: Option<u32>) {
    // Nothing panics while it's held, so a poisoned lock's map is whole.
    let mut limits = LIMITS.lock().unwrap_or_else(|e| e.into_inner());
    let known = limits.get(&provider_id);
    if known.is_some_and(|limit| limit.version > version) {
        return;
    }
    let kept = known.and_then(|limit| limit.pool.clone()).filter(|(count, _)| Some(*count) == max);
    let pool = kept.or_else(|| max.map(|count| (count, Arc::new(Semaphore::new(count as usize)))));
    let replaced = limits.insert(provider_id, Limit { version, pool: pool.clone() });
    if let Some((_, old)) = replaced.and_then(|limit| limit.pool)
        && !pool.is_some_and(|(_, new)| Arc::ptr_eq(&new, &old))
    {
        old.close();
    }
}

/// The provider's pool now, `None` when it has no limit.
pub fn current(provider_id: i64) -> Option<Arc<Semaphore>> {
    let limits = LIMITS.lock().unwrap_or_else(|e| e.into_inner());
    limits.get(&provider_id).and_then(|limit| limit.pool.as_ref()).map(|(_, slots)| slots.clone())
}

/// Forgets a deleted provider's pool. It isn't closed: requests holding
/// its slots keep them until they end, and requests waiting on it still
/// take their turn (SME-162 review 2), rather than all going at once to
/// a server that has as few slots as before.
pub fn forget(provider_id: i64) {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner()).remove(&provider_id);
}
