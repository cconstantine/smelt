//! How many model requests each provider is sent at once (SME-162).
//!
//! A provider with a max concurrent requests has a pool of that many
//! slots, kept here by provider id and handed out in its `Endpoint`
//! (`providers::endpoint`); a model call holds a slot from before it
//! sends until it ends (`anthropic::stream`), so a request over the limit
//! waits in smelt instead of queueing, silently, on a busy server until
//! it times out. The pools are in memory, built when an endpoint is,
//! from the column and the server's report (`resolved_max`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::Semaphore;

/// Each provider's slots, and the count they were built for.
static POOLS: LazyLock<Mutex<HashMap<i64, (u32, Arc<Semaphore>)>>> = LazyLock::new(Default::default);

/// The slots for a provider allowed `max` requests at once, `None` for no
/// limit. The same pool while `max` stays the same; a new one when it
/// changes, while requests already holding the old pool's slots keep
/// them. No limit forgets any pool, so a limit cleared and set again
/// starts afresh.
pub fn slots(provider_id: i64, max: Option<u32>) -> Option<Arc<Semaphore>> {
    // Nothing panics while it's held, so a poisoned lock's map is whole.
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(max) = max else {
        pools.remove(&provider_id);
        return None;
    };
    if let Some((size, pool)) = pools.get(&provider_id)
        && *size == max
    {
        return Some(pool.clone());
    }
    let pool = Arc::new(Semaphore::new(max as usize));
    pools.insert(provider_id, (max, pool.clone()));
    Some(pool)
}

/// Forgets a deleted provider's pool. Requests holding its slots keep
/// them until they end.
pub fn forget(provider_id: i64) {
    POOLS.lock().unwrap_or_else(|e| e.into_inner()).remove(&provider_id);
}
