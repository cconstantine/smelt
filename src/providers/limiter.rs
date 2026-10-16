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

use tokio::sync::Semaphore;

/// A provider's slots, and the count they were made for.
struct Pool {
    max: u32,
    slots: Arc<Semaphore>,
}

static POOLS: LazyLock<Mutex<HashMap<i64, Pool>>> = LazyLock::new(Default::default);

/// Sets how many requests at once a provider is sent, `None` for no
/// limit. The same count keeps its pool; another count, or none, closes
/// it, so the calls waiting on it look again (`current`).
pub fn set(provider_id: i64, max: Option<u32>) {
    // Nothing panics while it's held, so a poisoned lock's map is whole.
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    if pools.get(&provider_id).map(|pool| pool.max) == max {
        return;
    }
    let replaced = match max {
        Some(max) => pools.insert(provider_id, Pool { max, slots: Arc::new(Semaphore::new(max as usize)) }),
        None => pools.remove(&provider_id),
    };
    if let Some(old) = replaced {
        old.slots.close();
    }
}

/// The provider's pool now, `None` when it has no limit.
pub fn current(provider_id: i64) -> Option<Arc<Semaphore>> {
    let pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    pools.get(&provider_id).map(|pool| pool.slots.clone())
}

/// Forgets a deleted provider's pool. Requests holding its slots keep
/// them until they end.
pub fn forget(provider_id: i64) {
    set(provider_id, None);
}
